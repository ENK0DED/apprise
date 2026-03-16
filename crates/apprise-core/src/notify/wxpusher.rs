use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct WxPusher {
  token: String,
  uids: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
  #[cfg(test)]
  api_url_override: Option<String>,
}
impl WxPusher {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Token can be in host (if starts with AT_) or ?token= param
    let token = if let Some(ref h) = url.host {
      if h.starts_with("AT_") {
        h.clone()
      } else {
        // Host is not a token; try ?token= param
        url.get("token").map(|s| s.to_string())?
      }
    } else {
      url.get("token").map(|s| s.to_string())?
    };
    if !token.starts_with("AT_") {
      return None;
    }

    // UIDs from host (if not token), path_parts, and ?to= param
    let mut uids: Vec<String> = Vec::new();
    if let Some(ref h) = url.host {
      if !h.starts_with("AT_") && !h.is_empty() {
        uids.push(h.clone());
      }
    }
    uids.extend(url.path_parts.clone());
    if let Some(to) = url.get("to") {
      uids.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    if uids.is_empty() {
      return None;
    }
    Some(Self {
      token,
      uids,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      #[cfg(test)]
      api_url_override: None,
    })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "WxPusher",
      service_url: Some("https://wxpusher.zjiecode.com"),
      setup_url: None,
      protocols: vec!["wxpusher"],
      description: "Send messages via WxPusher WeChat service.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for WxPusher {
  fn schemas(&self) -> &[&str] {
    &["wxpusher"]
  }
  fn service_name(&self) -> &str {
    "WxPusher"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let payload = json!({ "appToken": self.token, "content": format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}\n", ctx.title) }, ctx.body), "contentType": 1, "uids": self.uids });
    let api_url = {
      #[cfg(test)]
      {
        self.api_url_override.as_deref().unwrap_or("https://wxpusher.zjiecode.com/api/send/message")
      }
      #[cfg(not(test))]
      {
        "https://wxpusher.zjiecode.com/api/send/message"
      }
    };
    let resp = client.post(api_url).header("User-Agent", APP_ID).json(&payload).send().await?;
    Ok(resp.status().is_success())
  }
}

#[cfg(test)]
mod tests {
  use crate::notify::registry::from_url;

  #[test]
  fn test_valid_urls() {
    let urls = vec!["wxpusher://AT_appid/123/", "wxpusher://123?token=AT_abc1234", "wxpusher://?token=AT_abc1234&to=UID_abc", "wxpusher://AT_appid/UID_abcd/"];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["wxpusher://", "wxpusher://:@/", "wxpusher://invalid"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  // ── Behavioral tests using wiremock ──────────────────────────────────

  use super::*;
  use crate::asset::AppriseAsset;
  use crate::notify::{Notify, NotifyContext};
  use crate::types::{NotifyFormat, NotifyType};
  use crate::utils::parse::ParsedUrl;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext {
      body: body.to_string(),
      title: title.to_string(),
      notify_type: NotifyType::Info,
      body_format: NotifyFormat::Text,
      attachments: vec![],
      interpret_escapes: false,
      interpret_emojis: false,
      tags: vec![],
      asset: AppriseAsset::default(),
    }
  }

  fn make_wxpusher(server: &MockServer, uids: Vec<&str>) -> WxPusher {
    let addr = server.address();
    let base = format!("http://127.0.0.1:{}/api/send/message", addr.port());
    WxPusher {
      token: "AT_appid".to_string(),
      uids: uids.iter().map(|s| s.to_string()).collect(),
      verify_certificate: false,
      tags: vec![],
      api_url_override: Some(base),
    }
  }

  #[tokio::test]
  async fn test_wxpusher_basic_send_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/send/message"))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"code": 1000})))
      .expect(1)
      .mount(&server)
      .await;

    let wp = make_wxpusher(&server, vec!["UID_abcd"]);
    let result = wp.send(&ctx("My Title", "test body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap(), "WxPusher POST should succeed");
  }

  #[tokio::test]
  async fn test_wxpusher_payload_correctness() {
    use wiremock::matchers::body_json;
    let server = MockServer::start().await;
    let expected = serde_json::json!({
        "appToken": "AT_appid",
        "content": "My Title\ntest body",
        "contentType": 1,
        "uids": ["UID_abcd"],
    });
    Mock::given(method("POST"))
      .and(path("/api/send/message"))
      .and(body_json(&expected))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"code": 1000})))
      .expect(1)
      .mount(&server)
      .await;

    let wp = make_wxpusher(&server, vec!["UID_abcd"]);
    let result = wp.send(&ctx("My Title", "test body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_wxpusher_http_500_returns_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/send/message")).respond_with(ResponseTemplate::new(500).set_body_string("error")).expect(1).mount(&server).await;

    let wp = make_wxpusher(&server, vec!["UID_abcd"]);
    let result = wp.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "HTTP 500 should return false");
  }

  #[tokio::test]
  async fn test_wxpusher_connection_refused_returns_error() {
    let wp = WxPusher {
      token: "AT_appid".to_string(),
      uids: vec!["UID_abcd".to_string()],
      verify_certificate: false,
      tags: vec![],
      api_url_override: Some("http://127.0.0.1:19999/api/send/message".to_string()),
    };
    let result = wp.send(&ctx("title", "body")).await;
    assert!(result.is_err(), "Connection refused should return Err");
  }

  #[test]
  fn test_wxpusher_from_url_struct_fields() {
    let parsed = ParsedUrl::parse("wxpusher://AT_appid/UID_abcd/").unwrap();
    let wp = WxPusher::from_url(&parsed).unwrap();
    assert_eq!(wp.token, "AT_appid");
    assert!(wp.uids.contains(&"UID_abcd".to_string()));
  }

  #[test]
  fn test_wxpusher_token_from_query_param() {
    let parsed = ParsedUrl::parse("wxpusher://123?token=AT_abc1234").unwrap();
    let wp = WxPusher::from_url(&parsed).unwrap();
    assert_eq!(wp.token, "AT_abc1234");
    assert!(wp.uids.contains(&"123".to_string()));
  }

  #[test]
  fn test_wxpusher_static_details() {
    let details = WxPusher::static_details();
    assert_eq!(details.service_name, "WxPusher");
    assert_eq!(details.protocols, vec!["wxpusher"]);
    assert!(!details.attachment_support);
  }
}
