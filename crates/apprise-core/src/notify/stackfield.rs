use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;

/// Stackfield room notifications through an incoming chat-message webhook.
pub struct Stackfield {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Stackfield {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // https://www.stackfield.com/apiwh/{token} is accepted as-is
    if url.schema == "http" || url.schema == "https" {
      return Self::from_url(&ParsedUrl::parse(&native_url(&url.raw)?)?);
    }

    // ?token= wins over the host, which is handy in config files
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => t.to_string(),
      None => unquote(url.host.as_deref()?),
    };
    let token = token.trim();
    if !is_uuid(token) {
      tracing::warn!("An invalid Stackfield webhook token was specified.");
      return None;
    }
    Some(Self { token: token.to_string(), verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Stackfield",
      service_url: Some("https://www.stackfield.com/"),
      setup_url: Some("https://appriseit.com/services/stackfield/"),
      protocols: vec!["stackfield"],
      description: "Send chat messages to a Stackfield room through an incoming webhook.",
      attachment_support: false,
    }
  }
}

/// Upstream `parse_native_url`: `https://(www.)stackfield.com/apiwh/{uuid}[/][?params]`
/// becomes `stackfield://{uuid}/[?params]`.
fn native_url(raw: &str) -> Option<String> {
  let rest = strip_prefix_ci(raw, "https://").or_else(|| strip_prefix_ci(raw, "http://"))?;
  let (before, params) = match rest.find('?') {
    Some(i) if i + 1 < rest.len() => (&rest[..i], &rest[i..]),
    Some(_) => return None,
    None => (rest, ""),
  };
  let before = before.strip_suffix('/').unwrap_or(before);
  let token = strip_prefix_ci(before, "www.stackfield.com/apiwh/").or_else(|| strip_prefix_ci(before, "stackfield.com/apiwh/"))?;
  if !is_uuid(token) {
    return None;
  }
  Some(format!("stackfield://{}/{}", token, params))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
  if s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix) { Some(&s[prefix.len()..]) } else { None }
}

fn unquote(s: &str) -> String {
  urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}

/// Stackfield webhook tokens are standard UUIDs (either case).
fn is_uuid(s: &str) -> bool {
  let b = s.as_bytes();
  b.len() == 36 && b.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

#[async_trait]
impl Notify for Stackfield {
  fn schemas(&self) -> &[&str] {
    &["stackfield"]
  }
  fn service_name(&self) -> &str {
    "Stackfield"
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
  // No title field; the title is folded into the body
  fn title_maxlen(&self) -> usize {
    0
  }
  // Stackfield allows at most one request per second
  fn request_rate_per_sec(&self) -> f64 {
    1.0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // "Title" is the chat message content
    let payload = json!({ "Title": ctx.body });
    let url = format!("{}/apiwh/{}", service_base("https://www.stackfield.com"), self.token);

    let client = build_client(self.verify_certificate)?;
    let resp = match client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Stackfield notification: {}", e.without_url());
        return Ok(false);
      }
    };

    let status = resp.status().as_u16();
    if status != 200 {
      tracing::warn!("Failed to send Stackfield notification: error={}.", status);
      tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
      return Ok(false);
    }
    tracing::info!("Sent Stackfield notification.");
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const TOKEN: &str = "11111111-2222-3333-4444-555555555555";

  fn parse(url: &str) -> Option<Stackfield> {
    Stackfield::from_url(&ParsedUrl::parse(url)?)
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("stackfield://{}", TOKEN),
      format!("stackfield://?token={}", TOKEN),
      format!("stackfield://{}", TOKEN.to_uppercase()),
      format!("https://www.stackfield.com/apiwh/{}", TOKEN),
      format!("https://stackfield.com/apiwh/{}", TOKEN),
      format!("http://www.stackfield.com/apiwh/{}/", TOKEN),
      format!("https://www.stackfield.com/apiwh/{}?tag=a", TOKEN),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "stackfield://",
      "stackfield://not-a-valid-uuid",
      "stackfield://11111111-2222-3333-4444-55555555555",
      "stackfield://11111111-2222-3333-4444-5555555555555",
      "stackfield://g1111111-2222-3333-4444-555555555555",
      "https://www.stackfield.com/apiwh/not-a-valid-uuid",
      "https://example.com/webhook/12345",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_token_sources() {
    assert_eq!(parse(&format!("stackfield://{}", TOKEN)).unwrap().token, TOKEN);
    assert_eq!(parse(&format!("stackfield://?token={}", TOKEN)).unwrap().token, TOKEN);
    // ?token= wins over the host
    let other = "abcdef01-2345-6789-abcd-ef0123456789";
    assert_eq!(parse(&format!("stackfield://{}?token={}", TOKEN, other)).unwrap().token, other);
    // Native URLs keep the token and their query arguments
    let obj = parse(&format!("https://www.stackfield.com/apiwh/{}?tag=x", TOKEN)).unwrap();
    assert_eq!(obj.token, TOKEN);
    assert_eq!(obj.tags, vec!["x"]);
  }

  #[test]
  fn test_service_attributes() {
    let svc = from_url(&format!("stackfield://{}", TOKEN)).unwrap();
    assert_eq!(svc.body_maxlen(), 4000);
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.request_rate_per_sec(), 1.0);
    assert!(!svc.attachment_support());
    assert_eq!(Stackfield::static_details().protocols, vec!["stackfield"]);
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/apiwh/{}", TOKEN)))
      .and(header("User-Agent", APP_ID))
      .and(header("Content-Type", "application/json"))
      .and(body_json(json!({ "Title": "T\r\nB" })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("stackfield://{}", TOKEN)).unwrap();
    let ctx = NotifyContext { body: "T\r\nB".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [500u16, 999, 204] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("stackfield://{}", TOKEN)).unwrap();
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "status {}", status);
    }
  }

  #[tokio::test]
  async fn test_send_connection_error() {
    let svc = from_url(&format!("stackfield://{}", TOKEN)).unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }
}
