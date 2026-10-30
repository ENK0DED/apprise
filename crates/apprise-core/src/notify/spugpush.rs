use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

/// Native webhook URL prefix (upstream `parse_native_url`).
const NATIVE_PREFIX: &str = "https://push.spug.dev/send/";

/// SpugPush: `spugpush://{token}`.
pub struct SpugPush {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

fn is_token_char(c: char) -> bool {
  c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

impl SpugPush {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let token = if url.schema == "https" || url.schema == "http" {
      // https://push.spug.dev/send/{token}, nothing after
      let prefix = url.raw.get(..NATIVE_PREFIX.len())?;
      if !prefix.eq_ignore_ascii_case(NATIVE_PREFIX) {
        return None;
      }
      url.raw[NATIVE_PREFIX.len()..].to_string()
    } else {
      match url.get("token").filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => urlencoding::decode(url.host.as_deref().unwrap_or_default()).ok()?.into_owned(),
      }
    };
    if !(32..=64).contains(&token.len()) || !token.chars().all(is_token_char) {
      tracing::warn!("The SpugPush token is invalid.");
      return None;
    }
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SpugPush",
      service_url: Some("https://docs.spug.dev/push/"),
      setup_url: Some("https://appriseit.com/services/spugpush/"),
      protocols: vec!["spugpush"],
      description: "Send push via Spug.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for SpugPush {
  fn schemas(&self) -> &[&str] {
    &["spugpush"]
  }
  fn service_name(&self) -> &str {
    "SpugPush"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // The body stands in for a missing title
    let title = if ctx.title.is_empty() { &ctx.body } else { &ctx.title };
    let payload = json!({ "title": title, "content": ctx.body });
    let url = format!("{}/send/{}", service_base("https://push.spug.dev"), self.token);
    let client = build_client(self.verify_certificate)?;
    let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
      tracing::info!("SpugPush notification sent successfully.");
      Ok(true)
    } else {
      tracing::warn!("SpugPush notification failed: {}", status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const TOKEN: &str = "abc123def456ghi789jkl012mno345pq";

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      format!("spugpush://{}", TOKEN),
      format!("spugpush://?token={}", TOKEN),
      format!("https://push.spug.dev/send/{}", TOKEN),
      format!("spugpush://{}", "f".repeat(64)),
      format!("spugpush://{}_-", TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "spugpush://".to_string(),
      "spugpush://invalid!".to_string(),
      format!("spugpush://{}", "a".repeat(31)),
      format!("spugpush://{}", "a".repeat(65)),
      // Only push.spug.dev native URLs, ending with the token
      format!("https://push.spug.cc/send/{}", TOKEN),
      format!("https://push.spug.dev/send/{}/", TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let sp = SpugPush::from_url(&ParsedUrl::parse(&format!("spugpush://{}", TOKEN)).unwrap()).unwrap();
    assert_eq!(sp.token, TOKEN);
    let sp = SpugPush::from_url(&ParsedUrl::parse(&format!("https://push.spug.dev/send/{}", TOKEN)).unwrap()).unwrap();
    assert_eq!(sp.token, TOKEN);
  }

  #[test]
  fn test_static_details() {
    let details = SpugPush::static_details();
    assert_eq!(details.service_name, "SpugPush");
    assert_eq!(details.service_url, Some("https://docs.spug.dev/push/"));
    assert!(details.protocols.contains(&"spugpush"));
    assert!(!details.attachment_support);
  }

  async fn send_expecting(ctx: &NotifyContext, expected: serde_json::Value) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/send/{}", TOKEN)))
      .and(header("Content-Type", "application/json"))
      .and(body_json(expected))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("spugpush://{}", TOKEN)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send() {
    send_expecting(&NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() }, json!({ "title": "T", "content": "B" })).await;
    send_expecting(&NotifyContext { body: "B".into(), ..Default::default() }, json!({ "title": "B", "content": "B" })).await;
  }

  #[tokio::test]
  async fn test_send_failure() {
    for code in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url(&format!("spugpush://{}", TOKEN)).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.is_err());
    }
  }
}
