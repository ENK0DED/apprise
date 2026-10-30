use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, unquote};
use async_trait::async_trait;
use serde_json::json;

/// WeCom group robot webhook: `wecombot://{key}`.
pub struct WeComBot {
  key: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

fn is_key(s: &str) -> bool {
  !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl WeComBot {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The registry turns the native webhook URL into wecombot://{key}
    let mut key = url.host.clone().unwrap_or_default();
    // Upstream unquotes ?key= once more
    if let Some(k) = url.get("key").filter(|k| !k.is_empty()) {
      key = unquote(k);
    }
    if !is_key(&key) {
      tracing::warn!("An invalid WeCom Bot Webhook Key was specified.");
      return None;
    }
    Some(Self { key, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "WeCom Bot",
      service_url: Some("https://weixin.qq.com/"),
      setup_url: Some("https://appriseit.com/services/wecombot/"),
      protocols: vec!["wecombot"],
      description: "Send messages via WeCom group robot.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for WeComBot {
  fn schemas(&self) -> &[&str] {
    &["wecombot"]
  }
  fn service_name(&self) -> &str {
    "WeCom Bot"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    2048
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let payload = json!({ "msgtype": "text", "text": { "content": ctx.body } });
    let url = format!("{}/cgi-bin/webhook/send", service_base("https://qyapi.weixin.qq.com"));
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client
      .post(&url)
      .query(&[("key", &self.key)])
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json; charset=utf-8")
      .body(serde_json::to_vec(&payload)?)
      .send()
      .await?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
      tracing::info!("Sent WeCom Bot notification.");
      Ok(true)
    } else {
      tracing::warn!("Failed to send WeCom Bot notification: error={}", status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  /// Parse `url`, converting a native URL the way the registry does.
  fn parse(url: &str) -> Option<WeComBot> {
    let native = crate::notify::registry::NATIVE_URLS.iter().find_map(|n| (n.convert)(url));
    WeComBot::from_url(&ParsedUrl::parse(native.as_deref().unwrap_or(url))?)
  }

  fn key_of(url: &str) -> Option<String> {
    parse(url).map(|w| w.key)
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "wecombot://botkey",
      "wecombot://?key=botkey",
      "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=BOTKEY",
      "https://qyapi.weixin.qq.com/cgi-bin/webhook/send/?key=BOTKEY&data=123",
      "http://QYAPI.weixin.qq.com/cgi-bin/webhook/send?KEY=ab_c",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "wecombot://",
      "wecombot://:@/",
      "wecombot://a.b",
      "https://qyapi.weixin.qq.com/cgi-bin/webhook/send",
      "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_key() {
    assert_eq!(key_of("wecombot://mykey").as_deref(), Some("mykey"));
    // ?key= takes priority over the host
    assert_eq!(key_of("wecombot://host?key=k-1").as_deref(), Some("k-1"));
    assert_eq!(key_of("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=BOTKEY").as_deref(), Some("BOTKEY"));
    // The rest of a native query string is kept, so a later key= wins
    assert_eq!(key_of("https://qyapi.weixin.qq.com/cgi-bin/webhook/send/?key=BOTKEY/&key=OTHER").as_deref(), Some("OTHER"));
  }

  #[test]
  fn test_native_options_are_kept() {
    let w = parse("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=BOTKEY&verify=no").unwrap();
    assert!(!w.verify_certificate);
  }

  #[test]
  fn test_service_details() {
    let details = WeComBot::static_details();
    assert_eq!(details.service_name, "WeCom Bot");
    assert_eq!(details.service_url, Some("https://weixin.qq.com/"));
    assert!(details.protocols.contains(&"wecombot"));
    let svc = from_url("wecombot://botkey").unwrap();
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.body_maxlen(), 2048);
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/cgi-bin/webhook/send"))
      .and(query_param("key", "botkey"))
      .and(header("Content-Type", "application/json; charset=utf-8"))
      .and(body_json(json!({ "msgtype": "text", "text": { "content": "T\r\nB" } })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("wecombot://botkey").unwrap();
    let ctx = NotifyContext { body: "T\r\nB".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for code in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/cgi-bin/webhook/send")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url("wecombot://botkey").unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.is_err());
    }
  }
}
