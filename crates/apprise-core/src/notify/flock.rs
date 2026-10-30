use async_trait::async_trait;
use serde_json::{Value, json};

use crate::asset::AppriseAsset;
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::{NotifyFormat, NotifyType};
use crate::utils::parse::{ParsedUrl, parse_bool};

/// Webhook endpoint; the token is appended as the last path segment.
const NOTIFY_URL: &str = "https://api.flock.com";
/// Bot API used when one or more users or channels are targeted.
const NOTIFY_API: &str = "https://api.flock.co";

/// Upstream `AppriseAsset.image_url_mask` default.
const DEFAULT_IMAGE_URL_MASK: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/{THEME}/apprise-{TYPE}-{XY}{EXTENSION}";

pub struct Flock {
  token: String,
  botname: Option<String>,
  /// Normalised `u:<id>` / `g:<id>` targets; empty means webhook mode.
  targets: Vec<String>,
  include_image: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Flock {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (token, mut raw_targets) = if url.schema == "https" || url.schema == "http" {
      // Native URL: https://api.flock.com/hooks/sendMessage/{token}/
      if !url.host.as_deref().is_some_and(|h| h.eq_ignore_ascii_case("api.flock.com")) {
        return None;
      }
      match url.path_parts.as_slice() {
        [hooks, send, token] if hooks.eq_ignore_ascii_case("hooks") && send.eq_ignore_ascii_case("sendMessage") && token.len() == 24 => {
          (token.clone(), Vec::new())
        }
        _ => return None,
      }
    } else {
      (unquote(url.host.as_deref().unwrap_or_default()), url.path_parts.clone())
    };
    if let Some(to) = url.get("to") {
      raw_targets.extend(parse_list(to));
    }

    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
      tracing::warn!("An invalid Flock Access Key was specified.");
      return None;
    }

    let mut targets = Vec::new();
    let mut has_error = false;
    for target in parse_list_of(&raw_targets) {
      if let Some(id) = match_target(&target, &["@", "u:"], true) {
        targets.push(format!("u:{}", id));
      } else if let Some(id) = match_target(&target, &["#", "g:"], false) {
        targets.push(format!("g:{}", id));
      } else {
        has_error = true;
        tracing::warn!("Ignoring invalid Flock target ({}) specified.", target);
      }
    }
    if has_error && targets.is_empty() {
      tracing::warn!("No Flock targets to notify.");
      return None;
    }

    Some(Self {
      token,
      botname: url.user.clone().filter(|u| !u.is_empty()),
      targets,
      include_image: url.get("image").map(parse_bool).unwrap_or(true),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Flock",
      service_url: Some("https://flock.com/"),
      setup_url: Some("https://appriseit.com/services/flock/"),
      protocols: vec!["flock"],
      description: "Send via Flock webhooks or the Flock bot API.",
      attachment_support: false,
    }
  }

  async fn post(&self, client: &reqwest::Client, url: &str, payload: &Value) -> bool {
    tracing::debug!("Flock POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let resp = client.post(url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(payload.to_string()).send().await;
    match resp {
      Ok(r) if r.status().as_u16() == 200 => {
        tracing::info!("Sent Flock notification.");
        true
      }
      Ok(r) => {
        let status = r.status().as_u16();
        let reason = if status == 401 { "Unauthorized - Invalid Token." } else { r.status().canonical_reason().unwrap_or("") };
        tracing::warn!("Failed to send Flock notification : {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status);
        false
      }
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Flock notification.");
        tracing::debug!("Socket Exception: {}", e);
        false
      }
    }
  }
}

#[async_trait]
impl Notify for Flock {
  fn schemas(&self) -> &[&str] {
    &["flock"]
  }
  fn service_name(&self) -> &str {
    "Flock"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // The title always arrives as plain text, so escape it for FlockML
    let title = escape_html(&ctx.title);
    // An HTML body is already FlockML-ready; anything else is escaped
    let body = if ctx.body_format == NotifyFormat::Html { ctx.body.clone() } else { escape_html(&ctx.body) };
    let flockml = format!("<flockml>{}{}</flockml>", if title.is_empty() { String::new() } else { format!("<b>{}</b><br/>", title) }, body);

    let profile_image = if self.include_image { image_url(&ctx.asset, &ctx.notify_type).map(Value::String).unwrap_or(Value::Null) } else { Value::Null };
    let payload = json!({
      "token": self.token,
      "flockml": flockml,
      "sendAs": {
        "name": self.botname.as_deref().unwrap_or(&ctx.asset.app_id),
        "profileImage": profile_image,
      },
    });

    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    if self.targets.is_empty() {
      let url = format!("{}/hooks/sendMessage/{}", service_base(NOTIFY_URL), self.token);
      has_error = !self.post(&client, &url, &payload).await;
    } else {
      let url = format!("{}/v1/chat.sendMessage", service_base(NOTIFY_API));
      for target in &self.targets {
        let mut p = payload.clone();
        p["to"] = json!(target);
        if !self.post(&client, &url, &p).await {
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

/// Match `^(prefix)?(?P<id>[A-Z0-9_]+)$` (case-insensitive); the prefix is
/// mandatory unless `optional` is set.
fn match_target<'a>(target: &'a str, prefixes: &[&str], optional: bool) -> Option<&'a str> {
  let lower = target.to_ascii_lowercase();
  let stripped = prefixes.iter().find(|p| lower.starts_with(*p)).map(|p| &target[p.len()..]);
  let id = match stripped {
    Some(rest) => rest,
    None if optional => target,
    None => return None,
  };
  (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then_some(id)
}

/// Upstream `NotifyBase.escape_html(..., whitespace=False)`.
fn escape_html(s: &str) -> String {
  s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\'', "&apos;").replace('"', "&quot;")
}

/// Upstream `AppriseAsset.image_url()` at the 72x72 size Flock uses.
fn image_url(asset: &AppriseAsset, notify_type: &NotifyType) -> Option<String> {
  let mask = asset.image_url_mask.as_deref().unwrap_or(DEFAULT_IMAGE_URL_MASK);
  if mask.is_empty() {
    return None;
  }
  Some(mask.replace("{THEME}", &asset.theme).replace("{TYPE}", notify_type.as_str()).replace("{XY}", "72x72").replace("{EXTENSION}", ".png"))
}

fn unquote(s: &str) -> String {
  urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}

/// Upstream `parse_list()`: split on `[]`, `;`, `,` and whitespace.
fn parse_list(s: &str) -> Vec<String> {
  s.split(|c: char| c == '[' || c == ']' || c == ';' || c == ',' || c.is_whitespace()).filter(|t| !t.is_empty()).map(String::from).collect()
}

/// Upstream `parse_list()` over a list: split, de-duplicate and sort.
fn parse_list_of(items: &[String]) -> Vec<String> {
  let mut out: Vec<String> = items.iter().flat_map(|i| parse_list(i)).collect();
  out.sort();
  out.dedup();
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const LOGO: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-72x72.png";

  fn parse_flock(url: &str) -> Flock {
    Flock::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let (t, i, u, g) = ("t".repeat(24), "i".repeat(24), "u".repeat(12), "g".repeat(12));
    let urls = vec![
      format!("flock://{}", t),
      format!("flock://{}?image=True", t),
      format!("flock://{}?image=False", t),
      format!("flock://{}?to=u:{}&format=markdown", i, u),
      format!("flock://{}?format=markdown", i),
      format!("flock://{}?format=text", i),
      format!("https://api.flock.com/hooks/sendMessage/{}/", i),
      format!("https://api.flock.com/hooks/sendMessage/{}/?format=markdown", i),
      format!("flock://{}/u:{}?format=markdown", i, u),
      format!("flock://{}/u:{}?format=html", i, u),
      format!("flock://{}/{}?format=text", i, u),
      format!("flock://{}/g:{}/u:{}?format=text", i, g, u),
      format!("flock://{}/#{}/@{}?format=text", i, g, u),
      // Lengths of the user/group fields are not validated
      format!("flock://{}/g:{}/u:{}?format=text", i, "g".repeat(14), "u".repeat(10)),
      format!("flock://{}/", t),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let i = "i".repeat(24);
    let urls = vec![
      "flock://".to_string(),
      "flock://:@/".to_string(),
      // g: and u: with no content after the prefix
      format!("flock://{}/g:/u:?format=text", i),
      // Invalid token characters
      "flock://bad_token".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    // Native URL with a token of the wrong length
    assert!(Flock::from_url(&ParsedUrl::parse("https://api.flock.com/hooks/sendMessage/abc/").unwrap()).is_none());
  }

  #[test]
  fn test_targets_are_normalised_and_sorted() {
    // Raw targets are sorted first: "!bad", "#team", "@def", "abc", "g:abc", "u:def"
    let f = parse_flock("flock://bot@token123/g:abc/@def/!bad/u:def?to=%23team,abc&image=no");
    assert_eq!(f.targets, vec!["g:team", "u:def", "u:abc", "g:abc", "u:def"]);
    assert_eq!(f.botname.as_deref(), Some("bot"));
    assert!(!f.include_image);
  }

  #[test]
  fn test_native_url() {
    let f = parse_flock(&format!("https://api.flock.com/hooks/sendMessage/{}/", "a".repeat(24)));
    assert_eq!(f.token, "a".repeat(24));
    assert!(f.targets.is_empty());
  }

  #[test]
  fn test_escape_and_image() {
    assert_eq!(escape_html("T<a>&'\" x"), "T&lt;a&gt;&amp;&apos;&quot; x");
    assert_eq!(image_url(&AppriseAsset::default(), &NotifyType::Info).as_deref(), Some(LOGO));
  }

  #[tokio::test]
  async fn test_send_webhook() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/hooks/sendMessage/token123"))
      .and(header("Content-Type", "application/json"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("flock://token123").unwrap();
    let ctx = NotifyContext { title: "T<a>".into(), body: "B&<b>".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(
      body,
      json!({
        "token": "token123",
        "flockml": "<flockml><b>T&lt;a&gt;</b><br/>B&amp;&lt;b&gt;</flockml>",
        "sendAs": {"name": "Apprise", "profileImage": LOGO},
      })
    );
  }

  #[tokio::test]
  async fn test_send_html_body_untouched_and_no_title() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

    let svc = from_url("flock://token123").unwrap();
    let ctx = NotifyContext { title: "Title & more".into(), body: "<i>Body</i>".into(), body_format: NotifyFormat::Html, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let ctx = NotifyContext { body: "Body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    let first: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(first["flockml"], "<flockml><b>Title &amp; more</b><br/><i>Body</i></flockml>");
    let second: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert_eq!(second["flockml"], "<flockml>Body</flockml>");
  }

  #[tokio::test]
  async fn test_send_bot_targets() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/chat.sendMessage")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;

    let svc = from_url("flock://bot@token123/#TEAM/@USER?image=no").unwrap();
    let ctx = NotifyContext { title: "alert".into(), body: "hello".into(), notify_type: NotifyType::Warning, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    let bodies: Vec<Value> = reqs.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
    assert_eq!(
      bodies[0],
      json!({
        "token": "token123",
        "flockml": "<flockml><b>alert</b><br/>hello</flockml>",
        "sendAs": {"name": "bot", "profileImage": null},
        "to": "g:TEAM",
      })
    );
    assert_eq!(bodies[1]["to"], "u:USER");
  }

  #[tokio::test]
  async fn test_send_failure_continues_targets() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).expect(2).mount(&server).await;

    let svc = from_url("flock://token123/g:abc/u:def").unwrap();
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_webhook_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(999)).mount(&server).await;
    let svc = from_url(&format!("flock://{}/", "t".repeat(24))).unwrap();
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[test]
  fn test_service_details() {
    let details = Flock::static_details();
    assert_eq!(details.service_name, "Flock");
    assert_eq!(details.protocols, vec!["flock"]);
    assert!(!details.attachment_support);
  }
}
