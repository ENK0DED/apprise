use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

pub struct Zulip {
  user: String,
  token: String,
  org_url: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Zulip {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let user = url.user.clone()?;
    if user.is_empty() {
      return None;
    }
    // Validate user (bot name) - must contain at least one alphanumeric
    if !user.chars().any(|c| c.is_ascii_alphanumeric()) {
      return None;
    }
    let host = url.host.clone()?;
    let org_url = format!("https://{}", host);
    // Token from password, first path part, or ?token= query
    let token = url.password.clone().or_else(|| url.get("token").map(|s| s.to_string())).or_else(|| url.path_parts.first().cloned())?;
    if token.is_empty() {
      return None;
    }
    // Token must be at least 32 chars
    if token.len() < 32 {
      return None;
    }
    // Targets are remaining path parts (after token) + ?to=
    let path_targets: Vec<String> =
      if url.password.is_some() || url.get("token").is_some() { url.path_parts.clone() } else { url.path_parts.get(1..).unwrap_or(&[]).to_vec() };
    let mut targets = path_targets;
    if let Some(to) = url.get("to") {
      targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    Some(Self { user, token, org_url, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Zulip",
      service_url: Some("https://zulip.com"),
      setup_url: None,
      protocols: vec!["zulip"],
      description: "Send messages via Zulip.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Zulip {
  fn schemas(&self) -> &[&str] {
    &["zulip"]
  }
  fn service_name(&self) -> &str {
    "Zulip"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/api/v1/messages", self.org_url);
    let mut all_ok = true;

    for target in &self.targets {
      // Detect target type: emails → private, else → stream
      let (msg_type, to_field) = if target.contains('@') { ("private", target.as_str()) } else { ("stream", target.as_str()) };
      let params = [
        ("type", msg_type),
        ("to", to_field),
        ("topic", if ctx.title.is_empty() { "Notification" } else { ctx.title.as_str() }),
        ("content", ctx.body.as_str()),
      ];
      let resp = client.post(&url).header("User-Agent", APP_ID).basic_auth(&self.user, Some(&self.token)).form(&params).send().await?;
      if !resp.status().is_success() {
        all_ok = false;
      }
    }
    Ok(all_ok)
  }
}

#[cfg(test)]
mod tests {
  use crate::notify::registry::from_url;

  #[test]
  fn test_valid_urls() {
    let token = "a".repeat(32);
    let urls = vec![
      format!("zulip://bot-name@apprise/{}", token),
      format!("zulip://botname@apprise/{}", token),
      format!("zulip://botname@apprise.zulipchat.com/{}", token),
      format!("zulip://botname@apprise/{}/channel1/channel2", token),
      format!("zulip://botname@apprise/{}/?to=channel1/channel2", token),
      format!("zulip://botname@apprise/?token={}&to=channel1", token),
      format!("zulip://botname@apprise/{}/user@example.com/user2@example.com", token),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let short_tok = format!("zulip://botname@apprise/{}", "a".repeat(24));
    let bad_bot = format!("zulip://....@apprise/{}", "a".repeat(32));
    let urls: Vec<&str> = vec![
      "zulip://",
      "zulip://:@/",
      "zulip://apprise",
      "zulip://botname@apprise",
      // Token too short (24 chars, need >= 32)
      &short_tok,
      // Invalid botname (no alphanumeric)
      &bad_bot,
    ];
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

  fn make_zulip(server: &MockServer, targets: Vec<&str>) -> Zulip {
    let addr = server.address();
    let base = format!("http://127.0.0.1:{}", addr.port());
    Zulip {
      user: "botname".to_string(),
      token: "a".repeat(32),
      org_url: base,
      targets: targets.iter().map(|s| s.to_string()).collect(),
      verify_certificate: false,
      tags: vec![],
    }
  }

  #[tokio::test]
  async fn test_zulip_basic_send_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/messages"))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": "success"})))
      .expect(1)
      .mount(&server)
      .await;

    let z = make_zulip(&server, vec!["general"]);
    let result = z.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap(), "Zulip POST should succeed");
  }

  #[tokio::test]
  async fn test_zulip_multiple_targets() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/messages"))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": "success"})))
      .expect(2)
      .mount(&server)
      .await;

    let z = make_zulip(&server, vec!["channel1", "channel2"]);
    let result = z.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_zulip_email_target_uses_private_type() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/messages"))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": "success"})))
      .expect(1)
      .mount(&server)
      .await;

    let z = make_zulip(&server, vec!["user@example.com"]);
    let result = z.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_zulip_http_500_returns_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/v1/messages")).respond_with(ResponseTemplate::new(500).set_body_string("error")).expect(1).mount(&server).await;

    let z = make_zulip(&server, vec!["general"]);
    let result = z.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "HTTP 500 should return false");
  }

  #[tokio::test]
  async fn test_zulip_connection_refused_returns_error() {
    let z = Zulip {
      user: "botname".to_string(),
      token: "a".repeat(32),
      org_url: "http://127.0.0.1:19999".to_string(),
      targets: vec!["general".to_string()],
      verify_certificate: false,
      tags: vec![],
    };
    let result = z.send(&ctx("title", "body")).await;
    assert!(result.is_err(), "Connection refused should return Err");
  }

  #[test]
  fn test_zulip_from_url_struct_fields() {
    let token = "a".repeat(32);
    let url = format!("zulip://botname@apprise/{}/channel1", token);
    let parsed = ParsedUrl::parse(&url).unwrap();
    let z = Zulip::from_url(&parsed).unwrap();
    assert_eq!(z.user, "botname");
    assert_eq!(z.token, token);
    assert_eq!(z.org_url, "https://apprise");
    assert!(z.targets.contains(&"channel1".to_string()));
  }

  #[test]
  fn test_zulip_token_from_query_param() {
    let token = "a".repeat(32);
    let url = format!("zulip://botname@apprise/?token={}&to=channel1", token);
    let parsed = ParsedUrl::parse(&url).unwrap();
    let z = Zulip::from_url(&parsed).unwrap();
    assert_eq!(z.token, token);
    assert!(z.targets.contains(&"channel1".to_string()));
  }

  #[test]
  fn test_zulip_static_details() {
    let details = Zulip::static_details();
    assert_eq!(details.service_name, "Zulip");
    assert_eq!(details.protocols, vec!["zulip"]);
    assert!(!details.attachment_support);
  }
}
