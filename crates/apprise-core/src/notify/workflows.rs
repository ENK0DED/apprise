//! Microsoft Power Automate / Workflows for Teams (upstream `NotifyWorkflows`).
//!
//! A webhook such as
//! `https://HOST:443/workflows/WORKFLOW/triggers/manual/paths/invoke?api-version=...&sig=SIGNATURE`
//! (or the Power Automate form under `/powerautomate/automations/direct`,
//! optionally with a `/cu/ROUTING_ID`) is written
//! `workflows://HOST:443/WORKFLOW/SIGNATURE`.

use std::sync::OnceLock;

use async_trait::async_trait;
use indexmap::IndexMap;
use regex_lite::Regex;
use serde_json::{Map, Value, json};

use crate::asset::NotifyImageSize;
use crate::conversion::pystr;
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::plugin_args::{qsd_bool, split_path};
use crate::utils::templates::{TemplateFile, TemplateType, app_tokens, apply_template};

/// API versions (upstream `APIVersion`).
const API_VERSION_WORKFLOW: &str = "2016-06-01";
const API_VERSION_POWER_AUTOMATE: &str = "2022-03-01-preview";

/// Largest template accepted, in bytes (upstream `max_workflows_template_size`).
const MAX_TEMPLATE_SIZE: usize = 35000;

const ADAPTIVE_CARD_VERSION: &str = "1.4";

/// Teams `<at>...</at>` mentions (upstream `WORKFLOWS_MENTION_RE`).
/// `[^<]+` stops before a nested opening tag, so a valid inner mention
/// survives a malformed outer one.
fn mention_re() -> &'static Regex {
  static RE: OnceLock<Regex> = OnceLock::new();
  RE.get_or_init(|| Regex::new(r"(?i)<at>([^<]+)</at>").expect("valid mention pattern"))
}

pub struct Workflows {
  host: String,
  port: Option<u16>,
  workflow: String,
  signature: String,
  include_image: bool,
  power_automate: bool,
  routing_id: Option<String>,
  wrap: bool,
  api_version: String,
  template: Option<TemplateFile>,
  tokens: IndexMap<String, String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `validate_regex()` for `^[A-Z0-9_-]+$` (any case), after strip.
fn token(value: Option<&str>) -> Option<String> {
  let value = pystr::strip(value?);
  (!value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then(|| value.to_string())
}

impl Workflows {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream parses this URL with host verification
    let host = url.verified_host()?;
    let mut entries = split_path(url).into_iter();

    let workflow = match url.get("workflow").filter(|w| !w.is_empty()).or_else(|| url.get("id").filter(|w| !w.is_empty())) {
      Some(w) => Some(w.to_string()),
      None => entries.next(),
    };
    let Some(workflow) = token(workflow.as_deref()) else {
      tracing::warn!("An invalid Workflows ID was specified.");
      return None;
    };

    let signature = match url.get("signature").filter(|s| !s.is_empty()).or_else(|| url.get("sig").filter(|s| !s.is_empty())) {
      Some(s) => Some(s.to_string()),
      None => entries.next(),
    };
    let Some(signature) = token(signature.as_deref()) else {
      tracing::warn!("An invalid Signature was specified.");
      return None;
    };

    let power_automate = url.get("powerautomate").or_else(|| url.get("pa")).map(parse_bool).unwrap_or(false);

    // Microsoft may route Power Automate URLs through a numeric CU ID
    let routing_id = match url.get("route").filter(|r| !r.is_empty()).or_else(|| url.get("routeid").filter(|r| !r.is_empty())) {
      None => None,
      Some(r) => {
        let r = pystr::strip(r);
        if r.is_empty() || !r.chars().all(|c| c.is_ascii_digit()) {
          tracing::warn!("An invalid Workflows Routing ID ({}) was specified.", r);
          return None;
        }
        Some(r.to_string())
      }
    };

    let version = url.get("api-version").filter(|v| !v.is_empty()).or_else(|| url.get("ver").filter(|v| !v.is_empty()));
    let api_version =
      version.map(str::to_string).unwrap_or_else(|| (if power_automate { API_VERSION_POWER_AUTOMATE } else { API_VERSION_WORKFLOW }).to_string());

    let template = match url.get("template").filter(|t| !t.is_empty()) {
      Some(t) => match TemplateFile::parse(t, MAX_TEMPLATE_SIZE) {
        Some(file) => Some(file),
        None => {
          tracing::warn!("The Workflows template specified could not be loaded.");
          return None;
        }
      },
      None => None,
    };

    Some(Self {
      host,
      port: url.port,
      workflow,
      signature,
      include_image: qsd_bool(url, "image", true),
      power_automate,
      routing_id,
      wrap: qsd_bool(url, "wrap", true),
      api_version,
      template,
      tokens: url.qsd_colon.clone(),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Power Automate / Workflows (for MSTeams)",
      service_url: Some("https://www.microsoft.com/power-platform/products/power-automate"),
      setup_url: Some("https://appriseit.com/services/workflows/"),
      protocols: vec!["workflow", "workflows"],
      description: "Send Microsoft Teams messages via Power Automate Workflows.",
      attachment_support: false,
    }
  }

  /// The webhook URL (without its query).
  fn notify_url(&self) -> String {
    let mut path = String::new();
    if self.power_automate {
      path.push_str("/powerautomate/automations/direct");
      if let Some(id) = &self.routing_id {
        path.push_str(&format!("/cu/{id}"));
      }
    }
    let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
    // service_base() only lets tests redirect the user's host
    format!("{}{}/workflows/{}/triggers/manual/paths/invoke", service_base(&format!("https://{}{}", self.host, port)), path, self.workflow)
  }

  /// The Adaptive Card Apprise sends, or the payload `?template=` builds
  /// (upstream `gen_payload()`). `None` (already logged) when the template
  /// cannot be used.
  async fn gen_payload(&self, ctx: &NotifyContext) -> Option<Value> {
    let image_url = if self.include_image { ctx.image_url(NotifyImageSize::Xy32) } else { None };

    let Some(template) = &self.template else {
      let mut body = Vec::new();
      if let Some(url) = &image_url {
        body.push(json!({ "type": "Image", "url": url, "height": "32px", "altText": ctx.notify_type.as_str() }));
      }
      if !ctx.title.is_empty() {
        body.push(json!({ "type": "TextBlock", "text": ctx.title, "style": "heading", "weight": "Bolder", "size": "Large", "id": "title" }));
      }
      body.push(json!({ "type": "TextBlock", "text": ctx.body, "style": "default", "wrap": self.wrap, "id": "body" }));

      // Teams needs an entity for each mention, once per ID in body order
      let mut entities: IndexMap<String, Value> = IndexMap::new();
      for m in mention_re().captures_iter(&ctx.body) {
        let key = pystr::strip(&m[1]).to_string();
        if key.is_empty() || entities.contains_key(&key) {
          continue;
        }
        // Teams matches the tag verbatim, case and padding included
        entities.insert(key.clone(), json!({ "type": "mention", "text": &m[0], "mentioned": { "id": key, "name": key } }));
      }
      let mut msteams = Map::new();
      msteams.insert("width".into(), json!("full"));
      if !entities.is_empty() {
        msteams.insert("entities".into(), Value::Array(entities.into_values().collect()));
      }

      return Some(json!({
        "type": "message",
        "attachments": [{
          "contentType": "application/vnd.microsoft.card.adaptive",
          "contentUrl": null,
          "content": {
            "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
            "type": "AdaptiveCard",
            "version": ADAPTIVE_CARD_VERSION,
            "body": body,
            "msteams": msteams,
          },
        }],
      }));
    };

    let text = template.read("Workflow").await?;
    let tokens = app_tokens(&self.tokens, &ctx.asset, &ctx.notify_type, &ctx.title, &ctx.body, image_url.as_deref());
    let content: Value = match serde_json::from_str(&apply_template(&text, TemplateType::Json, &tokens)) {
      Ok(v) => v,
      Err(e) => {
        tracing::error!("Workflow template {} contains invalid JSON.", template.loggable());
        tracing::debug!("JSONDecodeError: {}", e);
        return None;
      }
    };

    // The payload must be a Workflows/Power Automate message
    let Some(object) = content.as_object() else {
      tracing::error!("Workflow template {} must be a JSON object.", template.loggable());
      return None;
    };
    if object.get("type").and_then(Value::as_str) != Some("message") {
      let got = object.get("type").map(Value::to_string).unwrap_or_else(|| "None".into());
      tracing::error!("Workflow template {} must have 'type': 'message' (got {}).", template.loggable(), got);
      return None;
    }
    let Some(attachments) = object.get("attachments").and_then(Value::as_array).filter(|a| !a.is_empty()) else {
      tracing::error!("Workflow template {} must contain a non-empty 'attachments' list.", template.loggable());
      return None;
    };
    if !attachments.iter().all(|a| a.get("contentType").is_some_and(Value::is_string)) {
      tracing::error!("Workflow template {} contains an attachment missing a 'contentType' string.", template.loggable());
      return None;
    }
    Some(content)
  }
}

#[async_trait]
impl Notify for Workflows {
  fn schemas(&self) -> &[&str] {
    &["workflow", "workflows"]
  }
  fn service_name(&self) -> &str {
    "Power Automate / Workflows (for MSTeams)"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn body_maxlen(&self) -> usize {
    1000
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    // Upstream hashes str() of each value, so a missing one is "None"
    let none = || "None".to_string();
    Some(vec![
      "workflow".to_string(),
      self.host.clone(),
      self.port.map_or_else(none, |p| p.to_string()),
      self.workflow.clone(),
      self.signature.clone(),
      self.routing_id.clone().unwrap_or_else(none),
    ])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let Some(payload) = self.gen_payload(ctx).await else { return Ok(false) };
    let params = [("api-version", self.api_version.as_str()), ("sp", "/triggers/manual/run"), ("sv", "1.0"), ("sig", self.signature.as_str())];
    tracing::debug!("Workflows Payload: {}", payload);

    let client = build_client(self.verify_certificate)?;
    throttle().await;
    let resp = match client
      .post(self.notify_url())
      .query(&params)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .body(payload.to_string())
      .send()
      .await
    {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Workflows notification.");
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return Ok(false);
      }
    };

    let status = resp.status();
    if matches!(status.as_u16(), 200 | 202) {
      tracing::info!("Sent Workflows notification.");
      return Ok(true);
    }
    let reason = status.canonical_reason().unwrap_or("");
    tracing::warn!("Failed to send Workflows notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
    let details = resp.text().await.unwrap_or_default();
    tracing::debug!("Response Details:\r\n{}", details.chars().take(2000).collect::<String>());
    Ok(false)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::notify::with_test_base;
  use wiremock::matchers::method;
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  const IMAGE: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-32x32.png";
  const INVOKE: &str = "/workflows/WORKFLOWID/triggers/manual/paths/invoke";

  fn workflows(url: &str) -> Workflows {
    Workflows::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    server
  }

  async fn invokes(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap().into_iter().filter(|r| r.url.path().ends_with("/triggers/manual/paths/invoke")).collect()
  }

  fn card(body: Value, msteams: Value) -> Value {
    json!({
      "type": "message",
      "attachments": [{
        "contentType": "application/vnd.microsoft.card.adaptive",
        "contentUrl": null,
        "content": {
          "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
          "type": "AdaptiveCard",
          "version": "1.4",
          "body": body,
          "msteams": msteams,
        },
      }],
    })
  }

  #[test]
  fn test_storage_id_matches_upstream() {
    // uid: lines of upstream's apprise --dry-run; a missing value hashes as "None"
    crate::notify::custom_json::assert_upstream_uids(&[
      ("workflow://host:443/workflow1a/signature/", "9ebd09f7"),
      ("workflows://host/workflow1a/signature/?id=abc", "54fd36dd"),
      ("workflow://host/3XXX5/iXXXU/?pa=yes&routeid=123456789", "602e0a16"),
      ("workflows://host:8443/3XXX5/iXXXU/?route=123456789", "67048b98"),
    ]);
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      "workflow://host:443/workflow1a/signature/?image=no",
      "workflows://host:443/workflow1b/signature/",
      "workflows://host:443/signature/?id=workflow1c",
      "workflows://host:443/signature/?workflow=workflow1d&wrap=yes",
      "workflows://host:443/signature/?workflow=workflow1d&wrap=no",
      "workflows://host:443/workflow1e/signature/?api-version=2024-01-01",
      "workflows://host:443/workflow1b/signature/?ver=2016-06-01",
      "workflows://host:443/?id=workflow1b&signature=signature",
      "workflows://host:443/workflow1e/signature/?powerautomate=yes",
      "workflows://host:443/workflow1e/signature/?pa=yes&ver=1995-01-01",
      "workflows://host:443/workflow1e/signature/?pa=yes",
      "https://server.azure.com:443/workflows/643e69f83c8944/triggers/manual/paths/invoke?api-version=2016-06-01&sp=%2Ftriggers%2Fmanual%2Frun&sv=1.0&sig=KODuebWbDGYFr0z0eu",
      "https://server.azure.com:443/powerautomate/automations/direct/workflows/643e69f83c8944/triggers/manual/paths/invoke?api-version=2022-03-01-preview&sp=%2Ftriggers%2Fmanual%2Frun&sv=1.0&sig=KODuebWbDGYFr0z0eu",
      "workflow://host/3XXX5/iXXXU/?pa=yes&route=123456789",
      "workflow://host/3XXX5/iXXXU/?pa=yes&routeid=123456789",
      "workflow://host:443/workflow4/signature/",
    ];
    for url in urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "workflow://",
      "workflow://:@/",
      "workflow://host/workflow",
      "workflow://host:443/^(/signature",
      "workflow://host:443/workflow/sig@",
      "workflow://host:443/%20%20/signature",
      "workflow://host/3XXX5/iXXXU/?pa=yes&route=1/../evil",
      "workflow://host/3XXX5/iXXXU/?route=abc",
      "workflow://host/3XXX5/iXXXU/?template=invalid://template",
      "workflow://-invalid-host-/3XXX5/iXXXU/",
    ];
    for url in urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parse_options() {
    let w = workflows("workflows://host:443/signature/?id=workflow1c");
    assert_eq!((w.workflow.as_str(), w.signature.as_str()), ("workflow1c", "signature"));
    let w = workflows("workflows://host:443/?id=workflow1b&sig=signature&workflow=wins");
    assert_eq!((w.workflow.as_str(), w.signature.as_str()), ("wins", "signature"));
    assert_eq!(w.api_version, API_VERSION_WORKFLOW);
    assert!(w.include_image && w.wrap && !w.power_automate);

    let w = workflows("workflows://host/a/b/?pa=yes&wrap=no&image=no");
    assert!(w.power_automate && !w.wrap && !w.include_image);
    assert_eq!(w.api_version, API_VERSION_POWER_AUTOMATE);
    // ?powerautomate= wins over ?pa=
    assert!(!workflows("workflows://host/a/b/?pa=yes&powerautomate=no").power_automate);
    assert_eq!(workflows("workflows://host/a/b/?pa=yes&ver=1995-01-01").api_version, "1995-01-01");
    assert_eq!(workflows("workflows://host/a/b/?ver=1&api-version=2").api_version, "2");
    assert_eq!(workflows("workflows://host/a/b/?route=&routeid=7").routing_id.as_deref(), Some("7"));

    // The native Azure URL keeps its workflow, signature and version
    let w = Workflows::from_url(
      &crate::notify::registry::parse_service_url(
        "https://prod-15.uksouth.logic.azure.com:443/workflows/3XXX5/triggers/manual/paths/invoke?api-version=2016-06-01&sp=%2Ftriggers%2Fmanual%2Frun&sv=1.0&sig=iXXXU",
      )
      .unwrap(),
    )
    .unwrap();
    assert_eq!((w.workflow.as_str(), w.signature.as_str(), w.api_version.as_str()), ("3XXX5", "iXXXU", "2016-06-01"));
    assert_eq!(w.notify_url(), "https://prod-15.uksouth.logic.azure.com:443/workflows/3XXX5/triggers/manual/paths/invoke");

    // So does a Power Automate one routed through a CU
    let w = Workflows::from_url(
      &crate::notify::registry::parse_service_url(
        "https://default.environment.api.powerplatform.com:443/powerautomate/automations/direct/cu/123456789/workflows/3XXX5/triggers/manual/paths/invoke?api-version=2022-03-01-preview&sp=%2Ftriggers%2Fmanual%2Frun&sv=1.0&sig=iXXXU",
      )
      .unwrap(),
    )
    .unwrap();
    assert_eq!(w.routing_id.as_deref(), Some("123456789"));
    assert_eq!(
      w.notify_url(),
      "https://default.environment.api.powerplatform.com:443/powerautomate/automations/direct/cu/123456789/workflows/3XXX5/triggers/manual/paths/invoke"
    );
    // A route without Power Automate is not part of the URL
    assert_eq!(workflows("workflow://host/3XXX5/iXXXU/?route=5").notify_url(), "https://host/workflows/3XXX5/triggers/manual/paths/invoke");
  }

  /// The default Adaptive Card and query (pycapture).
  #[tokio::test]
  async fn test_send_card() {
    let server = mock(200).await;
    let w = workflows("workflow://example.com/WORKFLOWID/SIGNATURE?wrap=no");
    assert!(with_test_base(server.uri(), w.send(&ctx("title", "body <at>a@b.c</at>"))).await.unwrap());

    let requests = invokes(&server).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), INVOKE);
    assert_eq!(requests[0].url.query(), Some("api-version=2016-06-01&sp=%2Ftriggers%2Fmanual%2Frun&sv=1.0&sig=SIGNATURE"));
    assert_eq!(requests[0].headers.get("Content-Type").unwrap(), "application/json");
    assert_eq!(requests[0].headers.get("User-Agent").unwrap(), APP_ID);
    let payload: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
      payload,
      card(
        json!([
          {"type": "Image", "url": IMAGE, "height": "32px", "altText": "info"},
          {"type": "TextBlock", "text": "title", "style": "heading", "weight": "Bolder", "size": "Large", "id": "title"},
          {"type": "TextBlock", "text": "body <at>a@b.c</at>", "style": "default", "wrap": false, "id": "body"},
        ]),
        json!({"width": "full", "entities": [{"type": "mention", "text": "<at>a@b.c</at>", "mentioned": {"id": "a@b.c", "name": "a@b.c"}}]}),
      )
    );
  }

  /// Power Automate with a CU route and no image or title (pycapture).
  #[tokio::test]
  async fn test_send_power_automate() {
    let server = mock(202).await;
    let w = workflows("workflows://example.com:8443/WORKFLOWID/SIGNATURE?pa=yes&route=19&image=no");
    let mut c = ctx("", "message");
    c.notify_type = crate::types::NotifyType::Warning;
    assert!(with_test_base(server.uri(), w.send(&c)).await.unwrap());

    let requests = invokes(&server).await;
    assert_eq!(requests[0].url.path(), format!("/powerautomate/automations/direct/cu/19{INVOKE}"));
    assert_eq!(requests[0].url.query(), Some("api-version=2022-03-01-preview&sp=%2Ftriggers%2Fmanual%2Frun&sv=1.0&sig=SIGNATURE"));
    let payload: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(payload, card(json!([{"type": "TextBlock", "text": "message", "style": "default", "wrap": true, "id": "body"}]), json!({"width": "full"})));
    assert_eq!(w.notify_url(), format!("https://example.com:8443/powerautomate/automations/direct/cu/19{INVOKE}"));
  }

  #[tokio::test]
  async fn test_mention_entities() {
    let cases: [(&str, Vec<(&str, &str)>); 9] = [
      ("Hello world", vec![]),
      ("Hello <at>foo@example.com</at>!", vec![("<at>foo@example.com</at>", "foo@example.com")]),
      ("Hi <at>alice</at> and <at>bob</at>!", vec![("<at>alice</at>", "alice"), ("<at>bob</at>", "bob")]),
      ("<at>dup</at> and <at>dup</at> again", vec![("<at>dup</at>", "dup")]),
      ("Hi <at><at>user</at>!", vec![("<at>user</at>", "user")]),
      ("<at>garbage<at>inner</at>", vec![("<at>inner</at>", "inner")]),
      ("<at></at> <at>   </at> <at>unclosed", vec![]),
      ("Hi <AT>user</AT>!", vec![("<AT>user</AT>", "user")]),
      ("Hi <at>  padded  </at>!", vec![("<at>  padded  </at>", "padded")]),
    ];
    for (body, expected) in cases {
      let server = mock(200).await;
      let w = workflows("workflow://hostname/workflow1/signature1/?image=no");
      assert!(with_test_base(server.uri(), w.send(&ctx("T", body))).await.unwrap());
      let payload: Value = serde_json::from_slice(&invokes(&server).await[0].body).unwrap();
      let msteams = &payload["attachments"][0]["content"]["msteams"];
      if expected.is_empty() {
        assert_eq!(msteams, &json!({"width": "full"}), "{body}");
      } else {
        let entities: Vec<Value> = expected.iter().map(|(text, id)| json!({"type": "mention", "text": text, "mentioned": {"id": id, "name": id}})).collect();
        assert_eq!(msteams, &json!({"width": "full", "entities": entities}), "{body}");
      }
    }
  }

  /// A template replaces the card, with tokens JSON-escaped (pycapture).
  #[tokio::test]
  async fn test_template() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("wf.json");
    std::fs::write(
      &file,
      r#"{"type": "message", "attachments": [{"contentType": "application/vnd.microsoft.card.adaptive", "content": {"text": "{{app_title}} {{app_body}} {{app_image_url}} {{tok}} {{app_color}}"}}]}"#,
    )
    .unwrap();
    let server = mock(200).await;
    let w = workflows(&format!("workflow://example.com/WORKFLOWID/SIGNATURE?template={}&:tok=v", file.display()));
    assert!(with_test_base(server.uri(), w.send(&ctx("T \"q\"", "b\nb"))).await.unwrap());
    let payload: Value = serde_json::from_slice(&invokes(&server).await[0].body).unwrap();
    assert_eq!(
      payload,
      json!({"type": "message", "attachments": [{"contentType": "application/vnd.microsoft.card.adaptive", "content": {"text": format!("T \"q\" b\nb {IMAGE} v #3AA3E3")}}]})
    );

    // Without an image the token is empty
    let w = workflows(&format!("workflow://example.com/WORKFLOWID/SIGNATURE?template={}&image=no", file.display()));
    assert!(with_test_base(server.uri(), w.send(&ctx("T", "b"))).await.unwrap());
    let payload: Value = serde_json::from_slice(&invokes(&server).await[1].body).unwrap();
    assert_eq!(payload["attachments"][0]["content"]["text"], "T b  {{tok}} #3AA3E3");

    let bad = [
      ("invalid.json", "{"),
      ("list.json", "[]"),
      ("type.json", r#"{"type": "invalid", "attachments": [{"contentType": "x"}]}"#),
      ("missing.json", r#"{"type": "message"}"#),
      ("empty.json", r#"{"type": "message", "attachments": []}"#),
      ("not-dict.json", r#"{"type": "message", "attachments": ["x"]}"#),
      ("content-type.json", r#"{"type": "message", "attachments": [{"contentType": 1}]}"#),
    ];
    for (name, content) in bad {
      let file = dir.path().join(name);
      std::fs::write(&file, content).unwrap();
      let w = workflows(&format!("workflow://example.com/WORKFLOWID/SIGNATURE?template={}", file.display()));
      assert!(!with_test_base(server.uri(), w.send(&ctx("T", "b"))).await.unwrap(), "{name}");
    }
    let w = workflows(&format!("workflow://example.com/WORKFLOWID/SIGNATURE?template={}", dir.path().join("nothing.json").display()));
    assert!(!with_test_base(server.uri(), w.send(&ctx("T", "b"))).await.unwrap());
    assert_eq!(invokes(&server).await.len(), 2);
  }

  #[tokio::test]
  async fn test_failures() {
    for status in [500, 999, 204] {
      let server = mock(status).await;
      let w = workflows("workflow://example.com/WORKFLOWID/SIGNATURE");
      assert!(!with_test_base(server.uri(), w.send(&ctx("t", "b"))).await.unwrap(), "{status}");
    }
    let w = workflows("workflow://example.com/WORKFLOWID/SIGNATURE");
    assert!(!with_test_base("http://127.0.0.1:1", w.send(&ctx("t", "b"))).await.unwrap());
  }

  /// HTML input reaches the card as Markdown.
  #[tokio::test]
  async fn test_html_to_markdown() {
    let server = mock(200).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("workflow://hostname/workflow1/signature1/?image=no"));
    let c = NotifyContext { body: "<b>hello</b> <i>world</i>".into(), body_format: NotifyFormat::Html, ..Default::default() };
    assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success());
    let payload: Value = serde_json::from_slice(&invokes(&server).await[0].body).unwrap();
    let body = &payload["attachments"][0]["content"]["body"];
    assert_eq!(body.as_array().unwrap().iter().find(|b| b["id"] == "body").unwrap()["text"], "**hello** *world*");
  }
}
