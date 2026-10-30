use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::parse_list;
use crate::utils::parse::{ParsedUrl, unquote};
use async_trait::async_trait;
use serde::Serialize;

/// WxPusher: `wxpusher://{app_token}/{UID_user | topic_id}/...`.
pub struct WxPusher {
  token: String,
  /// `UID_` user IDs, as given
  users: Vec<String>,
  /// Numeric topic IDs (up to 21 digits)
  topics: Vec<u128>,
  verify_certificate: bool,
  tags: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Payload<'a> {
  app_token: &'a str,
  content: &'a str,
  summary: &'a str,
  content_type: u8,
  topic_ids: &'a [u128],
  uids: &'a [String],
  url: Option<&'a str>,
}

/// Upstream `IS_TOPIC`: 1 to 21 digits without a leading zero.
fn topic_id(target: &str) -> Option<u128> {
  let valid = (1..=21).contains(&target.len()) && target.chars().all(|c| c.is_ascii_digit()) && !target.starts_with('0');
  valid.then(|| target.parse().ok()).flatten()
}

impl WxPusher {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().unwrap_or_default();
    let mut raw: Vec<String> = url.path_parts.clone();
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => {
        // With ?token= the host is a target too
        raw.push(host);
        // Upstream unquotes ?token= once more
        unquote(t)
      }
      None => host,
    };
    let valid = token.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("AT_")) && token.len() > 3 && !token.chars().any(char::is_whitespace);
    if !valid {
      tracing::warn!("An invalid WxPusher App Token was specified.");
      return None;
    }

    if let Some(to) = url.get("to") {
      // Upstream's URLBase.parse_list() also unquotes each entry
      raw.extend(parse_list(&[to]).iter().map(|t| unquote(t)));
    }
    let mut users = Vec::new();
    let mut topics = Vec::new();
    for target in parse_list(&raw) {
      if target.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("UID_")) && target.len() > 4 {
        users.push(target);
      } else if let Some(topic) = topic_id(&target) {
        topics.push(topic);
      } else {
        tracing::warn!("Dropped invalid WxPusher user/topic ({}) specified.", target);
      }
    }

    Some(Self { token, users, topics, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "WxPusher",
      service_url: Some("https://wxpusher.zjiecode.com/"),
      setup_url: Some("https://appriseit.com/services/wxpusher/"),
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
    if self.users.is_empty() && self.topics.is_empty() {
      tracing::warn!("There were no WxPusher targets to notify");
      return Ok(false);
    }
    let payload = Payload {
      app_token: &self.token,
      content: &ctx.body,
      summary: &ctx.title,
      content_type: match ctx.body_format {
        NotifyFormat::Text => 1,
        NotifyFormat::Html => 2,
        NotifyFormat::Markdown => 3,
      },
      topic_ids: &self.topics,
      uids: &self.users,
      url: None,
    };
    let url = format!("{}/api/send/message", service_base("https://wxpusher.zjiecode.com"));
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .header("Accept", "application/json")
      .body(serde_json::to_vec(&payload)?)
      .send()
      .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let code = serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v.get("code").and_then(|c| c.as_i64()));
    if status == reqwest::StatusCode::OK && code == Some(1000) {
      tracing::info!("Sent WxPusher notification to {} targets.", self.users.len() + self.topics.len());
      Ok(true)
    } else {
      tracing::warn!("Failed to send WxPusher notification, code={}/{}", status.as_u16(), code.map(|c| c.to_string()).unwrap_or_else(|| "unk".into()));
      Err(NotifyError::ServiceError { status: status.as_u16(), body })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use serde_json::json;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> WxPusher {
    WxPusher::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "wxpusher://AT_appid/123/".to_string(),
      "wxpusher://AT_appid/%20/%20/".into(),
      "wxpusher://123?token=AT_abc1234".into(),
      "wxpusher://?token=AT_abc1234".into(),
      "wxpusher://?token=AT_abc1234&to=UID_abc".into(),
      "wxpusher://AT_appid/UID_abcd/".into(),
      "wxpusher://AT_appid/@/#/,/".into(),
      "wxpusher://at_lower/UID_abcd/".into(),
      format!("wxpusher://AT_appid/?to={},{}", "2".repeat(11), "3".repeat(11)),
      format!("wxpusher://AT_appid/{}/{}/", "2".repeat(11), "3".repeat(11)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["wxpusher://", "wxpusher://:@/", "wxpusher://invalid", "wxpusher://AT_", "wxpusher://123?token=AT_a%20b"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_targets() {
    let w = parse("wxpusher://AT_appid/123/abc/UID_456");
    assert_eq!(w.topics, vec![123]);
    assert_eq!(w.users, vec!["UID_456"]);

    // ?token= moves the host into the targets; everything is sorted the
    // way upstream parse_list() does, and 0-prefixed topics are dropped
    let w = parse("wxpusher://uid_b/45/123/bad?token=at_tok&to=UID_a,0");
    assert_eq!(w.token, "at_tok");
    assert_eq!(w.topics, vec![123, 45]);
    assert_eq!(w.users, vec!["UID_a", "uid_b"]);

    // Topic IDs can be longer than a u64
    let w = parse(&format!("wxpusher://AT_appid/{}/{}", "9".repeat(21), "9".repeat(22)));
    assert_eq!(w.topics, vec!["9".repeat(21).parse::<u128>().unwrap()]);
  }

  #[test]
  fn test_static_details() {
    let details = WxPusher::static_details();
    assert_eq!(details.service_name, "WxPusher");
    assert_eq!(details.protocols, vec!["wxpusher"]);
    assert!(!details.attachment_support);
  }

  async fn send(url: &str, ctx: &NotifyContext, status: u16, response: &str) -> Result<bool, NotifyError> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/send/message"))
      .respond_with(ResponseTemplate::new(status).set_body_string(response))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(url).unwrap();
    crate::notify::with_test_base(server.uri(), svc.send(ctx)).await
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/send/message"))
      .and(header("Content-Type", "application/json"))
      .and(header("Accept", "application/json"))
      .and(body_json(json!({
        "appToken": "at_tok",
        "content": "B",
        "summary": "T",
        "contentType": 1,
        "topicIds": [123, 45],
        "uids": ["UID_a", "uid_b"],
        "url": null,
      })))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "code": 1000 })))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("wxpusher://uid_b/45/123/bad?token=at_tok&to=UID_a,0").unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_content_type_follows_format() {
    for (format, content_type) in [(NotifyFormat::Html, 2), (NotifyFormat::Markdown, 3)] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path("/api/send/message"))
        .and(body_json(json!({
          "appToken": "AT_tok", "content": "B", "summary": "", "contentType": content_type,
          "topicIds": [], "uids": ["UID_a"], "url": null,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "code": 1000 })))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url("wxpusher://AT_tok/UID_a").unwrap();
      let ctx = NotifyContext { body: "B".into(), body_format: format, ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    // No valid targets: nothing is sent
    assert!(!from_url("wxpusher://AT_appid/%20/").unwrap().send(&ctx).await.unwrap());
    // Service error code, bad JSON, empty body and HTTP errors all fail
    assert!(send("wxpusher://AT_appid/123", &ctx, 200, r#"{"code": 99}"#).await.is_err());
    assert!(send("wxpusher://AT_appid/UID_345", &ctx, 200, "{").await.is_err());
    assert!(send("wxpusher://AT_appid/UID_345", &ctx, 200, "").await.is_err());
    assert!(send("wxpusher://AT_appid/123", &ctx, 999, r#"{"code": 1000}"#).await.is_err());
    assert!(send("wxpusher://AT_appid/123", &ctx, 200, r#"{"code": 1000}"#).await.unwrap());
  }
}
