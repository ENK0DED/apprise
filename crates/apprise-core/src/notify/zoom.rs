use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;

/// How the message is delivered to the Zoom Team Chat webhook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZoomMode {
  /// Plain text; the title is prepended to the body
  Simple,
  /// Structured message with head (title) and body sections
  Full,
}

impl ZoomMode {
  /// Resolve a mode, accepting any prefix (`sim` for `simple`).
  fn parse(value: &str) -> Option<Self> {
    let v = value.trim().to_lowercase();
    if v.is_empty() {
      None
    } else if "simple".starts_with(&v) {
      Some(Self::Simple)
    } else if "full".starts_with(&v) {
      Some(Self::Full)
    } else {
      None
    }
  }
}

/// Zoom Team Chat notifications through an incoming webhook.
pub struct Zoom {
  webhook_id: String,
  token: String,
  mode: ZoomMode,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Zoom {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // https://inbots.zoom.us/incoming/hook/{webhook_id}?token={token}
    if url.schema == "http" || url.schema == "https" {
      return Self::from_url(&ParsedUrl::parse(&native_url(&url.raw)?)?);
    }

    let webhook_id = unquote(url.host.as_deref()?).trim().to_string();
    if webhook_id.is_empty() || !webhook_id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
      tracing::warn!("A Zoom webhook ID must be specified and contain only alphanumeric, hyphen, or underscore characters.");
      return None;
    }

    // The first path entry is the verification token; ?token= overrides it
    let mut token = split_path(&url.path).into_iter().next();
    if let Some(t) = url.get("token").filter(|t| !t.is_empty()) {
      token = Some(t.to_string());
    }
    let token = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    let Some(token) = token else {
      tracing::warn!("A Zoom verification token must be specified.");
      return None;
    };

    let mode = match url.get("mode").filter(|m| !m.is_empty()) {
      None => ZoomMode::Full,
      Some(m) => match ZoomMode::parse(m) {
        Some(mode) => mode,
        None => {
          tracing::warn!("The Zoom mode ({}) is invalid.", m);
          return None;
        }
      },
    };

    Some(Self { webhook_id, token, mode, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Zoom",
      service_url: Some("https://zoom.us/"),
      setup_url: Some("https://appriseit.com/services/zoom/"),
      protocols: vec!["zoom"],
      description: "Send Zoom Team Chat messages through an incoming webhook.",
      attachment_support: false,
    }
  }
}

/// Upstream `parse_native_url`: `https://inbots.zoom.us/incoming/hook/{id}[/][?params]`
/// becomes `zoom://{id}/[?params]`; the token then comes from `?token=`.
fn native_url(raw: &str) -> Option<String> {
  let rest = strip_prefix_ci(raw, "https://").or_else(|| strip_prefix_ci(raw, "http://"))?;
  let (before, params) = match rest.find('?') {
    Some(i) if i + 1 < rest.len() => (&rest[..i], &rest[i..]),
    Some(_) => return None,
    None => (rest, ""),
  };
  let before = before.strip_suffix('/').unwrap_or(before);
  let webhook_id = strip_prefix_ci(before, "inbots.zoom.us/incoming/hook/")?;
  if webhook_id.is_empty() || !webhook_id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
    return None;
  }
  Some(format!("zoom://{}/{}", webhook_id, params))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
  if s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix) { Some(&s[prefix.len()..]) } else { None }
}

fn unquote(s: &str) -> String {
  urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}

/// Upstream `URLBase.split_path`: split on whitespace, commas and slashes,
/// then unquote each entry.
fn split_path(path: &str) -> Vec<String> {
  path.split([' ', '\t', '\r', '\n', ',', '\\', '/']).filter(|s| !s.is_empty()).map(unquote).collect()
}

#[async_trait]
impl Notify for Zoom {
  fn schemas(&self) -> &[&str] {
    &["zoom"]
  }
  fn service_name(&self) -> &str {
    "Zoom"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    4000
  }
  fn title_maxlen(&self) -> usize {
    250
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let base = format!("{}/incoming/hook/{}", service_base("https://inbots.zoom.us"), self.webhook_id);
    let client = build_client(self.verify_certificate)?;
    let req = client.post(match self.mode {
      ZoomMode::Full => format!("{}?format=full", base),
      ZoomMode::Simple => base,
    });
    let req = req.header("User-Agent", APP_ID).header("Authorization", &self.token);

    let req = match self.mode {
      ZoomMode::Full => {
        let mut content = serde_json::Map::new();
        // The head section is only included when there is a title
        if !ctx.title.is_empty() {
          content.insert("head".into(), json!({ "text": ctx.title }));
        }
        content.insert("body".into(), json!([{ "type": "message", "text": ctx.body }]));
        req.json(&json!({ "content": Value::Object(content) }))
      }
      ZoomMode::Simple => {
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        req.body(text)
      }
    };

    let resp = match req.send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Zoom notification: {}", e.without_url());
        return Ok(false);
      }
    };
    let status = resp.status().as_u16();
    if status != 200 && status != 204 {
      tracing::warn!("Failed to send Zoom notification: {}error={}.", error_hint(status), status);
      tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
      return Ok(false);
    }
    tracing::info!("Sent Zoom notification.");
    Ok(true)
  }
}

/// Zoom's own explanation of an HTTP status, for the log.
fn error_hint(status: u16) -> &'static str {
  match status {
    401 => "Unauthorized - Invalid verification token., ",
    404 => "Webhook not found; check the Webhook ID., ",
    429 => "Rate limit exceeded; too many consecutive requests., ",
    500 => "Zoom internal server error., ",
    503 => "Service unavailable; try again later., ",
    _ => "",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, body_string, header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  /// The POSTs this test sent; other processes probing localhost ports can
  /// reach a mock server too, so anything else is ignored.
  async fn received(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == "POST").collect()
  }

  const WEBHOOK_ID: &str = "abcHook123";
  const TOKEN: &str = "xyzVerify456";

  fn parse(url: &str) -> Option<Zoom> {
    Zoom::from_url(&ParsedUrl::parse(url)?)
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("zoom://{}/{}", WEBHOOK_ID, TOKEN),
      format!("zoom://{}/{}?mode=full", WEBHOOK_ID, TOKEN),
      format!("zoom://{}/{}?mode=simple", WEBHOOK_ID, TOKEN),
      format!("zoom://{}/{}?mode=", WEBHOOK_ID, TOKEN),
      format!("zoom://{}?token={}", WEBHOOK_ID, TOKEN),
      format!("https://inbots.zoom.us/incoming/hook/{}?token={}", WEBHOOK_ID, TOKEN),
      format!("https://inbots.zoom.us/incoming/hook/{}/?token={}&mode=simple", WEBHOOK_ID, TOKEN),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "zoom://".to_string(),
      "zoom://:@/".to_string(),
      format!("zoom://{}", WEBHOOK_ID),
      format!("zoom://{}/{}?mode=invalid", WEBHOOK_ID, TOKEN),
      format!("zoom://{}/{}?mode=%20", WEBHOOK_ID, TOKEN),
      format!("zoom://bad!id/{}", TOKEN),
      // A native URL needs the token in ?token=
      format!("https://inbots.zoom.us/incoming/hook/{}", WEBHOOK_ID),
      format!("https://example.com/incoming/hook/{}?token={}", WEBHOOK_ID, TOKEN),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parse_fields() {
    let obj = parse(&format!("zoom://{}/{}", WEBHOOK_ID, TOKEN)).unwrap();
    assert_eq!(obj.webhook_id, WEBHOOK_ID);
    assert_eq!(obj.token, TOKEN);
    assert_eq!(obj.mode, ZoomMode::Full);

    // ?token= overrides the path token; modes match on any prefix
    let obj = parse(&format!("zoom://{}/old?token={}&mode=SIM", WEBHOOK_ID, TOKEN)).unwrap();
    assert_eq!(obj.token, TOKEN);
    assert_eq!(obj.mode, ZoomMode::Simple);
    assert_eq!(parse(&format!("zoom://{}/{}?mode=f", WEBHOOK_ID, TOKEN)).unwrap().mode, ZoomMode::Full);

    let obj = parse(&format!("https://inbots.zoom.us/incoming/hook/{}?token={}&mode=simple", WEBHOOK_ID, TOKEN)).unwrap();
    assert_eq!(obj.webhook_id, WEBHOOK_ID);
    assert_eq!(obj.token, TOKEN);
    assert_eq!(obj.mode, ZoomMode::Simple);
  }

  #[test]
  fn test_service_attributes() {
    let svc = from_url(&format!("zoom://{}/{}", WEBHOOK_ID, TOKEN)).unwrap();
    assert_eq!(svc.body_maxlen(), 4000);
    assert_eq!(svc.title_maxlen(), 250);
    assert!(!svc.attachment_support());
  }

  #[tokio::test]
  async fn test_send_full() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/incoming/hook/{}", WEBHOOK_ID)))
      .and(query_param("format", "full"))
      .and(header("User-Agent", APP_ID))
      .and(header("Authorization", TOKEN))
      .and(header("Content-Type", "application/json"))
      .and(body_json(json!({ "content": { "head": { "text": "T" }, "body": [{ "type": "message", "text": "B" }] } })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("zoom://{}/{}", WEBHOOK_ID, TOKEN)).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_full_without_title() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(json!({ "content": { "body": [{ "type": "message", "text": "B" }] } })))
      .respond_with(ResponseTemplate::new(204))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("zoom://{}/{}", WEBHOOK_ID, TOKEN)).unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_simple() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/incoming/hook/{}", WEBHOOK_ID)))
      .and(header("Authorization", TOKEN))
      .and(body_string("T: B"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("zoom://{}/{}?mode=simple", WEBHOOK_ID, TOKEN)).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = received(&server).await;
    // Plain text goes out without a format argument or content type
    assert!(requests[0].url.query().is_none());
    assert!(requests[0].headers.get("Content-Type").is_none());
  }

  #[tokio::test]
  async fn test_send_simple_without_title() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(body_string("B")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let svc = from_url(&format!("zoom://{}/{}?mode=simple", WEBHOOK_ID, TOKEN)).unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    for mode in ["full", "simple"] {
      for status in [401u16, 500, 999] {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
        let svc = from_url(&format!("zoom://{}/{}?mode={}", WEBHOOK_ID, TOKEN, mode)).unwrap();
        let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
        assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "{} {}", mode, status);
      }
      let svc = from_url(&format!("zoom://{}/{}?mode={}", WEBHOOK_ID, TOKEN, mode)).unwrap();
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
    }
  }
}
