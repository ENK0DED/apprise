//! Gupshup WhatsApp notifications.
//!
//! ```text
//! gupshup://{app}:{apikey}@{source}/{to_phone}/{to_phone}
//! ```
//!
//! WhatsApp only delivers free-form text to someone who messaged the source
//! number within the last 24 hours. Outside of that window an approved
//! template is needed: pick one with `?template={id}` and fill its
//! placeholders with `:1=value`, `:body=1` (the message body) or `:type=2`
//! (the notification type).

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::phone::{is_phone_no, parse_phone_no};

/// What fills one template placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fill {
  /// A fixed value
  Text(String),
  /// The message body
  Body,
  /// The notification type
  Type,
}

pub struct Gupshup {
  app: String,
  apikey: String,
  source: String,
  template: Option<String>,
  /// Each placeholder position and what fills it
  params: BTreeMap<u32, Fill>,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Gupshup {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream strips query values and ignores empty ones
    let qsd = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty());

    let app = qsd("app").map(str::to_string).or_else(|| url.user.clone()).unwrap_or_default();
    let Some(app) = validate(&app) else {
      tracing::warn!("A Gupshup App Name must be specified.");
      return None;
    };
    let apikey = qsd("key").map(str::to_string).or_else(|| url.password.clone()).unwrap_or_default();
    let Some(apikey) = validate(&apikey) else {
      tracing::warn!("A Gupshup API Key must be specified.");
      return None;
    };

    // ?source= wins over ?from=, which wins over the host
    let source = qsd("source").or_else(|| qsd("from")).map(str::to_string).unwrap_or_else(|| url.host.clone().unwrap_or_default());
    let Some(source) = is_phone_no(&source, 10) else {
      tracing::warn!("An invalid Gupshup Source Phone No ({}) was specified.", source);
      return None;
    };

    let template = match qsd("template") {
      Some(t) => Some(validate(t)?),
      None => None,
    };

    // Template placeholders arrive as :key=value
    let mut params = BTreeMap::new();
    for (key, value) in url.qsd_colon.iter().map(|(k, v)| (k.as_str(), v.as_str())) {
      let (index, fill) = match template_key(key) {
        Some(TemplateKey::Index(index)) => (index, Fill::Text(value.to_string())),
        Some(TemplateKey::Map(fill)) => match template_key(value) {
          // "body" and "type" name the position they fill
          Some(TemplateKey::Index(index)) => (index, fill),
          _ => {
            tracing::warn!("An invalid Gupshup template placeholder (:{}={}) was specified.", key, value);
            return None;
          }
        },
        None => {
          tracing::warn!("An invalid Gupshup template placeholder (:{}) was specified.", key);
          return None;
        }
      };
      if params.insert(index, fill).is_some() {
        tracing::warn!("The Gupshup template placeholder ({}) was assigned more than once.", index);
        return None;
      }
    }

    let mut entries = url.path_parts.clone();
    if let Some(to) = qsd("to") {
      entries.extend(parse_phone_no(&[to]));
    }
    let mut targets = Vec::new();
    for entry in parse_phone_no(&entries) {
      match is_phone_no(&entry, 10) {
        Some(no) => targets.push(no.full),
        None => tracing::warn!("Dropped invalid Gupshup phone # ({}) specified.", entry),
      }
    }

    Some(Self { app, apikey, source: source.full, template, params, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Gupshup",
      service_url: Some("https://www.gupshup.io/"),
      setup_url: Some("https://appriseit.com/services/gupshup/"),
      protocols: vec!["gupshup"],
      description: "Send WhatsApp messages via Gupshup.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Gupshup {
  fn schemas(&self) -> &[&str] {
    &["gupshup"]
  }
  fn service_name(&self) -> &str {
    "Gupshup"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // The maximum length of a WhatsApp text message
  fn body_maxlen(&self) -> usize {
    4096
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Gupshup targets to notify.");
      return Ok(false);
    }

    // Upstream sends these as JSON encoded by Python's json.dumps()
    let (endpoint, fields) = match &self.template {
      Some(template) => {
        let params: Vec<String> = self
          .params
          .values()
          .map(|fill| match fill {
            Fill::Text(value) => py_json_str(value),
            Fill::Body => py_json_str(&ctx.body),
            Fill::Type => py_json_str(ctx.notify_type.as_str()),
          })
          .collect();
        ("/wa/api/v1/template/msg", vec![("template", format!("{{\"id\": {}, \"params\": [{}]}}", py_json_str(template), params.join(", ")))])
      }
      None => {
        ("/wa/api/v1/msg", vec![("channel", "whatsapp".to_string()), ("message", format!("{{\"type\": \"text\", \"text\": {}}}", py_json_str(&ctx.body)))])
      }
    };
    let url = format!("{}{}", service_base("https://api.gupshup.io"), endpoint);
    let client = build_client(self.verify_certificate)?;

    let mut ok = true;
    for target in &self.targets {
      // Skip a target that already accepted this message so a retry does
      // not deliver it twice
      if crate::delivery::is_delivered(target, false) {
        continue;
      }

      let mut form: Vec<(&str, &str)> = vec![("source", &self.source), ("src.name", &self.app), ("destination", target)];
      form.extend(fields.iter().map(|(k, v)| (*k, v.as_str())));

      tracing::debug!("Gupshup POST URL: {} (cert_verify={})", url, self.verify_certificate);
      crate::notify::throttle().await;
      let resp = match client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .header("apikey", self.apikey.as_str())
        .form(&form)
        .send()
        .await
      {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Gupshup notification to {}.", target);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          ok = false;
          continue;
        }
      };
      let status = resp.status().as_u16();
      let text = resp.text().await.unwrap_or_default();
      let content = serde_json::from_str::<Value>(&text).ok().filter(Value::is_object).unwrap_or(Value::Null);

      // A 2xx answer can still carry {"status": "error", "message": ...}
      if !(200..300).contains(&status) || content.get("status").and_then(Value::as_str) == Some("error") {
        let reason = content.get("message").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or_else(|| status_reason(status));
        tracing::warn!("Failed to send Gupshup notification to {}: {}, error={}.", target, reason, status);
        tracing::debug!("Response Details: {}", truncate(&text));
        ok = false;
        continue;
      }
      tracing::info!("Sent Gupshup notification to {}.", target);
      crate::delivery::mark_delivered(target, false);
    }
    Ok(ok)
  }
}

enum TemplateKey {
  Index(u32),
  Map(Fill),
}

/// Upstream IS_TEMPLATE_KEY: `^\s*((?P<index>[1-9][0-9]*)|(?P<map>body|type))\s*$`
fn template_key(key: &str) -> Option<TemplateKey> {
  let key = key.trim();
  if key.eq_ignore_ascii_case("body") {
    return Some(TemplateKey::Map(Fill::Body));
  }
  if key.eq_ignore_ascii_case("type") {
    return Some(TemplateKey::Map(Fill::Type));
  }
  if key.starts_with(|c: char| ('1'..='9').contains(&c)) && key.chars().all(|c| c.is_ascii_digit()) {
    return key.parse().ok().map(TemplateKey::Index);
  }
  None
}

/// A JSON string exactly as Python's `json.dumps()` writes it (non-ASCII
/// escaped as `\uXXXX`).
fn py_json_str(s: &str) -> String {
  let mut out = String::with_capacity(s.len() + 2);
  out.push('"');
  for c in s.chars() {
    match c {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      '\u{8}' => out.push_str("\\b"),
      '\u{c}' => out.push_str("\\f"),
      ' '..='~' => out.push(c),
      _ => {
        for unit in c.encode_utf16(&mut [0; 2]) {
          out.push_str(&format!("\\u{:04x}", unit));
        }
      }
    }
  }
  out.push('"');
  out
}

/// Upstream `validate_regex()` with its default pattern: the value must not
/// start with whitespace, and is returned stripped.
fn validate(value: &str) -> Option<String> {
  if value.is_empty() || value.starts_with(char::is_whitespace) { None } else { Some(value.trim().to_string()) }
}

fn status_reason(status: u16) -> &'static str {
  match status {
    400 => "Bad Request - Check the phone numbers and message.",
    401 => "Unauthorized - Invalid API Key.",
    429 => "Too many requests were made.",
    _ => reqwest::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or(""),
  }
}

fn truncate(text: &str) -> &str {
  match text.char_indices().nth(2000) {
    Some((i, _)) => &text[..i],
    None => text,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use serde_json::json;
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const APP: &str = "DemoAPI";
  const APIKEY: &str = "abcdef0123456789abcdef0123456789";
  const SOURCE: &str = "917834811114";
  const TEMPLATE: &str = "c6aecef6-bcb0-4fb1-8100-28c094e3bc6b";

  fn base() -> String {
    format!("gupshup://{APP}:{APIKEY}@{SOURCE}")
  }

  fn parse(url: &str) -> Option<Gupshup> {
    Gupshup::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn ok_response() -> ResponseTemplate {
    ResponseTemplate::new(202).set_body_json(json!({"status": "submitted", "messageId": "ee4a68a0-1203-4c85-8dc3-49d0b3226a35"}))
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("{}/", base()),
      format!("{}/15551234567", base()),
      format!("{}/15551234567/15551234568?template={TEMPLATE}&:body=1", base()),
      format!("gupshup://?app={APP}&key={APIKEY}&from={SOURCE}&to=15551234567,15551234568"),
      format!("gupshup://{APP}:{APIKEY}@/15551234567?source={SOURCE}"),
      format!("{}/15551234567/123", base()),
      format!("{}/123", base()),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "gupshup://".to_string(),
      format!("gupshup://{SOURCE}/15551234567"),
      format!("gupshup://{APP}@{SOURCE}/15551234567"),
      format!("gupshup://{APP}:{APIKEY}@abcd/15551234567"),
      format!("{}/15551234567?template={TEMPLATE}&:abc=1", base()),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_template_mapping() {
    // Values checked against upstream
    let g = parse(&format!("{}/15551234567?template=T&:1=a&: 2 =b", base())).unwrap();
    assert_eq!(g.template.as_deref(), Some("T"));
    assert_eq!(g.params, BTreeMap::from([(1, Fill::Text("a".into())), (2, Fill::Text("b".into()))]));

    let g = parse(&format!("{}/15551234567?template=T&:BODY=2&:type=1", base())).unwrap();
    assert_eq!(g.params, BTreeMap::from([(1, Fill::Type), (2, Fill::Body)]));

    // body/type need a position, positions start at 1 and are used once
    for bad in [":body=x", ":1=a&:body=1", ":BODY=01", ":0=a", ":abc=1", ":type="] {
      assert!(parse(&format!("{}/15551234567?template=T&{}", base(), bad)).is_none(), "{}", bad);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let g = parse(&format!("gupshup://?app=A&key=K&from={SOURCE}&source=15551234567&to=15551234568")).unwrap();
    assert_eq!((g.app.as_str(), g.apikey.as_str(), g.source.as_str()), ("A", "K", "15551234567"));
    assert_eq!(g.targets, vec!["15551234568"]);
    assert_eq!(g.template, None);

    let g = parse(&format!("{}/+1 555 123 4567/123/abcd", base())).unwrap();
    assert_eq!(g.source, SOURCE);
    assert_eq!(g.targets, vec!["15551234567"]);
  }

  #[test]
  fn test_service_details() {
    let g = parse(&format!("{}/15551234567", base())).unwrap();
    assert_eq!(g.body_maxlen(), 4096);
    assert_eq!(g.title_maxlen(), 0);
    assert!(!g.attachment_support());
  }

  #[test]
  fn test_py_json_str() {
    assert_eq!(py_json_str("Héllo \"q\"\nx"), r#""H\u00e9llo \"q\"\nx""#);
    assert_eq!(py_json_str("😀\u{7f}\\"), r#""\ud83d\ude00\u007f\\""#);
  }

  #[tokio::test]
  async fn test_send_text() {
    // Verified against upstream with pycapture.py
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/wa/api/v1/msg"))
      .and(header("apikey", APIKEY))
      .and(header("Accept", "application/json"))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(header("User-Agent", APP_ID))
      .and(body_string(format!(
        "source={SOURCE}&src.name={APP}&destination=15551234567&channel=whatsapp&message={}",
        urlencoding::encode(r#"{"type": "text", "text": "H\u00e9llo \"q\"\nx"}"#).replace("%20", "+")
      )))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("{}/15551234567", base())).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Héllo \"q\"\nx"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_template() {
    let server = MockServer::start().await;
    for target in ["15551234567", "15551234568"] {
      Mock::given(method("POST"))
        .and(path("/wa/api/v1/template/msg"))
        .and(body_string(format!(
          "source={SOURCE}&src.name={APP}&destination={target}&template={}",
          urlencoding::encode(r#"{"id": "c6aecef6", "params": ["Fixed \u00e9", "B", "warning"]}"#).replace("%20", "+")
        )))
        .respond_with(ok_response())
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&format!("{}/15551234567/15551234568?template=c6aecef6&:body=2&:1=Fixed%20é&:type=3", base())).unwrap();
    let ctx = NotifyContext { body: "B".into(), notify_type: NotifyType::Warning, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_accepted_targets() {
    use wiremock::matchers::body_string_contains;
    let server = MockServer::start().await;
    let send = || Mock::given(method("POST")).and(path("/wa/api/v1/msg"));
    send().and(body_string_contains("destination=15551234567")).respond_with(ok_response()).expect(1).mount(&server).await;
    // The second number fails once, then accepts the retry
    send().and(body_string_contains("destination=15551234568")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    send().and(body_string_contains("destination=15551234568")).respond_with(ok_response()).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("{}/15551234567/15551234568?retry=1&wait=0", base())));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("B"))).await.is_success());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let svc = from_url(&format!("{}/15551234567", base())).unwrap();
    for (status, body) in [(200, r#"{"status": "error", "message": "Authentication Failed"}"#), (401, ""), (500, "not json"), (999, "")] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/wa/api/v1/msg")).respond_with(ResponseTemplate::new(status).set_body_string(body)).expect(1).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap(), "status {}", status);
    }

    // A 2xx answer that is not JSON relies on the status code
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/wa/api/v1/msg")).respond_with(ResponseTemplate::new(200).set_body_string("garbage")).mount(&server).await;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    // One failed target fails the send, but the rest are still tried
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/wa/api/v1/msg"))
      .and(wiremock::matchers::body_string_contains("destination=15551234567"))
      .respond_with(ResponseTemplate::new(400))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/wa/api/v1/msg")).respond_with(ok_response()).expect(1).mount(&server).await;
    let svc = from_url(&format!("{}/15551234567/15551234568", base())).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    // Nothing to notify
    let svc = from_url(&format!("{}/123", base())).unwrap();
    assert!(!svc.send(&ctx("B")).await.unwrap());

    // Connection errors fail the send
    let svc = from_url(&format!("{}/15551234567", base())).unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("B"))).await.unwrap());
  }
}
