use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;

/// Gorush reports each device it could not reach with this log type
const FAILED_PUSH: &str = "failed-push";

/// The device platforms Gorush can deliver to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
  /// Apple Push Notification service
  Ios,
  /// Firebase Cloud Messaging
  Android,
  /// Huawei Mobile Services
  Huawei,
}

impl Platform {
  /// Accept the platform names, their service names and Gorush's own numbers
  fn parse(value: &str) -> Option<Self> {
    match value.trim().to_lowercase().as_str() {
      "ios" | "apns" | "1" => Some(Self::Ios),
      "android" | "fcm" | "2" => Some(Self::Android),
      "huawei" | "hms" | "3" => Some(Self::Huawei),
      _ => None,
    }
  }

  /// The platform number Gorush expects
  fn number(self) -> u8 {
    match self {
      Self::Ios => 1,
      Self::Android => 2,
      Self::Huawei => 3,
    }
  }
}

/// Upstream `parse_list()`: split on whitespace, commas, semicolons and
/// brackets, then de-duplicate and sort.
fn parse_list<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
  let mut items: Vec<String> = values
    .into_iter()
    .flat_map(|v| v.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '[' | ']')))
    .filter(|s| !s.is_empty())
    .map(String::from)
    .collect();
  items.sort();
  items.dedup();
  items
}

/// Self-hosted Gorush push server (APNs, FCM and HMS).
///
/// `gorush://[user[:password]@]host[:port]/token1/token2?platform=ios`;
/// `gorushs://` uses HTTPS.
pub struct Gorush {
  host: String,
  port: Option<u16>,
  user: Option<String>,
  password: Option<String>,
  secure: bool,
  targets: Vec<String>,
  platform: Platform,
  priority: Option<&'static str>,
  topic: Option<String>,
  sound: Option<String>,
  badge: Option<u64>,
  data: BTreeMap<String, String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Gorush {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let get = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty());

    // Every path entry is a device token; ?to= adds more
    let path_entries: Vec<String> = url
      .path
      .split([' ', '\t', '\r', '\n', ',', '\\', '/'])
      .filter(|s| !s.is_empty())
      .map(|s| urlencoding::decode(s).map(|d| d.into_owned()).unwrap_or_else(|_| s.to_string()))
      .collect();
    let targets = parse_list(path_entries.iter().map(String::as_str).chain(get("to")));

    // Every token in this URL goes to the same platform; platform= wins
    // over its mode= alias
    let platform = match get("platform").or_else(|| get("mode")) {
      Some(p) => match Platform::parse(p) {
        Some(platform) => platform,
        None => {
          tracing::warn!("The Gorush platform specified ({}) is invalid.", p);
          return None;
        }
      },
      None => Platform::Android,
    };

    // Gorush accepts "normal" or "high"; unset leaves it to Gorush
    let priority = match get("priority") {
      Some(p) => {
        let p = p.to_lowercase();
        match ["normal", "high"].into_iter().find(|candidate| candidate.starts_with(p.as_str())) {
          Some(priority) => Some(priority),
          None => {
            tracing::warn!("The Gorush priority specified ({}) is invalid.", p);
            return None;
          }
        }
      }
      None => None,
    };

    // The badge count shown on the app icon (iOS only)
    let badge = match get("badge") {
      Some(b) => match b.parse::<i64>() {
        Ok(n) if n >= 0 => Some(n as u64),
        _ => {
          tracing::warn!("The Gorush badge specified ({}) is invalid.", b);
          return None;
        }
      },
      None => None,
    };

    // Extra key/value pairs handed to the mobile app (?+key=value; a raw
    // '+' arrives decoded as a space)
    let data =
      url.qsd.iter().filter_map(|(k, v)| k.strip_prefix('+').or_else(|| k.strip_prefix(' ')).map(|key| (key.to_string(), v.trim().to_string()))).collect();

    Some(Self {
      host,
      port: url.port,
      user: url.user.clone().filter(|u| !u.is_empty()),
      password: url.password.clone(),
      secure: url.schema == "gorushs",
      targets,
      platform,
      priority,
      topic: get("topic").map(String::from),
      sound: get("sound").map(String::from),
      badge,
      data,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Gorush",
      service_url: Some("https://github.com/appleboy/gorush"),
      setup_url: Some("https://appriseit.com/services/gorush/"),
      protocols: vec!["gorush", "gorushs"],
      description: "Send push notifications to iOS, Android and Huawei devices through a Gorush server.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    let mut notification = Map::new();
    notification.insert("tokens".into(), json!(self.targets));
    notification.insert("platform".into(), json!(self.platform.number()));
    notification.insert("message".into(), json!(ctx.body));
    if !ctx.title.is_empty() {
      notification.insert("title".into(), json!(ctx.title));
    }
    // Only send the optional fields that were set
    if let Some(priority) = self.priority {
      notification.insert("priority".into(), json!(priority));
    }
    if let Some(topic) = &self.topic {
      notification.insert("topic".into(), json!(topic));
    }
    if let Some(sound) = &self.sound {
      notification.insert("sound".into(), json!(sound));
    }
    if let Some(badge) = self.badge {
      notification.insert("badge".into(), json!(badge));
    }
    if !self.data.is_empty() {
      notification.insert("data".into(), json!(self.data));
    }
    json!({ "notifications": [notification] })
  }
}

#[async_trait]
impl Notify for Gorush {
  fn schemas(&self) -> &[&str] {
    &["gorush", "gorushs"]
  }
  fn service_name(&self) -> &str {
    "Gorush"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Text
  }
  /// APNs, FCM and HMS each cap a push at roughly 4KB in total
  fn body_maxlen(&self) -> usize {
    4000
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Gorush devices to notify.");
      return Ok(false);
    }

    let schema = if self.secure { "https" } else { "http" };
    let port = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}/api/push", schema, self.host, port);
    let payload = self.payload(ctx);

    tracing::debug!("Gorush POST URL: {} (cert_verify={})", url, self.verify_certificate);
    tracing::debug!("Gorush Payload: {}", payload);

    let client = build_client(self.verify_certificate)?;
    let mut req = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(payload.to_string());
    // Basic auth is only needed when a reverse proxy asks for it. Upstream
    // hands requests a None password, which it sends as the text "None".
    if let Some(user) = &self.user {
      req = req.basic_auth(user, Some(self.password.as_deref().unwrap_or("None")));
    }
    let resp = req.send().await?;

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status != 200 {
      let status_str = if status == 400 { "Bad request, or too many notifications." } else { "" };
      tracing::warn!("Failed to send Gorush notification: {}{}error={}.", status_str, if status_str.is_empty() { "" } else { ", " }, status);
      return Err(NotifyError::ServiceError { status, body: text });
    }

    // Gorush only lists failed devices, and only in its sync mode; an empty
    // or unreadable reply still means Gorush accepted it
    let content: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let failed: Vec<&Map<String, Value>> = content
      .get("logs")
      .and_then(Value::as_array)
      .into_iter()
      .flatten()
      .filter_map(Value::as_object)
      .filter(|entry| entry.get("type").and_then(Value::as_str) == Some(FAILED_PUSH))
      .collect();

    if failed.is_empty() {
      tracing::info!("Sent Gorush notification to {} device(s).", self.targets.len());
      return Ok(true);
    }

    for entry in failed {
      let error = entry.get("error").filter(|e| !e.is_null() && e.as_str() != Some("")).map(|e| e.as_str().map(String::from).unwrap_or_else(|| e.to_string()));
      tracing::warn!("Gorush could not notify a device: {}", error.as_deref().unwrap_or("unknown error"));
    }
    Ok(false)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Option<Gorush> {
    Gorush::from_url(&ParsedUrl::parse(url).unwrap())
  }

  fn ok_reply() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"counts": 1, "logs": [], "success": "ok"}))
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      // No device tokens; loads but has nothing to notify
      "gorush://localhost",
      "gorush://localhost:8088/abcdef1234567890",
      "gorushs://user:pass@localhost/token1/token2?platform=ios",
      "gorush://user@localhost/?to=token1,token2&mode=huawei",
      "gorush://localhost/token?priority=high&topic=com.example.app&sound=default&badge=3&+key=value",
      "gorush://localhost/token?badge=0&priority=n",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "gorush://",
      "gorush://localhost/token?platform=invalid",
      "gorush://localhost/token?priority=invalid",
      "gorush://localhost/token?priority=highest",
      "gorush://localhost/token?badge=abc",
      "gorush://localhost/token?badge=-1",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let g = parse("gorush://localhost/token").unwrap();
    assert_eq!(g.platform, Platform::Android);
    assert_eq!(g.priority, None);
    assert_eq!(g.badge, None);
    assert!(g.data.is_empty());
    assert!(!g.secure);

    // The platform= value wins over mode=
    let g = parse("gorush://localhost/token?mode=ios&platform=3").unwrap();
    assert_eq!(g.platform, Platform::Huawei);
    let g = parse("gorush://localhost/token?mode=APNS").unwrap();
    assert_eq!(g.platform, Platform::Ios);

    // Tokens are de-duplicated and sorted, as upstream's parse_list() does
    let g = parse("gorushs://example.com/zeta/alpha?to=beta,alpha&platform=fcm").unwrap();
    assert_eq!(g.targets, vec!["alpha", "beta", "zeta"]);
    assert!(g.secure);

    let g = parse("gorush://localhost/t?priority=H&badge=%2B3&+Key=value&%2BOther=x").unwrap();
    assert_eq!(g.priority, Some("high"));
    assert_eq!(g.badge, Some(3));
    assert_eq!(g.data.get("Key").map(String::as_str), Some("value"));
    assert_eq!(g.data.get("Other").map(String::as_str), Some("x"));
  }

  #[tokio::test]
  async fn test_send_full_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/push"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .and(body_json(json!({
        "notifications": [{
          "tokens": ["tok1", "tok2"],
          "platform": 1,
          "title": "Title & <b>",
          "message": "body _ . ! * # <",
          "priority": "high",
          "topic": "com.example.app",
          "sound": "ping",
          "badge": 0,
          "data": {"key": "value"},
        }]
      })))
      .respond_with(ok_reply())
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let svc = from_url(&format!(
      "gorush://user:pass@{}:{}/tok2/tok1?platform=apns&priority=h&topic=com.example.app&sound=ping&badge=0&+key=value",
      addr.ip(),
      addr.port()
    ))
    .unwrap();
    let ctx = NotifyContext { title: "Title & <b>".into(), body: "body _ . ! * # <".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_defaults() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/push"))
      .and(body_json(json!({"notifications": [{"tokens": ["token"], "platform": 2, "message": "body"}]})))
      .respond_with(ok_reply())
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let svc = from_url(&format!("gorush://{}:{}/token", addr.ip(), addr.port())).unwrap();
    assert!(svc.send(&NotifyContext { body: "body".into(), ..Default::default() }).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("Authorization").is_none());
  }

  #[tokio::test]
  async fn test_send_user_without_password() {
    let server = MockServer::start().await;
    // Upstream sends the literal password "None" (user:None)
    Mock::given(method("POST")).and(header("Authorization", "Basic dXNlcjpOb25l")).respond_with(ok_reply()).expect(1).mount(&server).await;
    let addr = server.address();
    let svc = from_url(&format!("gorush://user@{}:{}/token?mode=3", addr.ip(), addr.port())).unwrap();
    assert!(svc.send(&NotifyContext { body: "Body".into(), ..Default::default() }).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url("gorush://localhost").unwrap();
    assert!(!svc.send(&NotifyContext { body: "body".into(), ..Default::default() }).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_odd_replies_succeed() {
    // An HTTP 200 without failed devices is a success
    for reply in [r#"["unexpected"]"#, r#"{"logs": "unexpected"}"#, r#"{"logs": ["text", {"type": "succeeded-push"}]}"#, "<html/>", ""] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string(reply)).expect(1).mount(&server).await;
      let addr = server.address();
      let svc = from_url(&format!("gorush://{}:{}/token", addr.ip(), addr.port())).unwrap();
      assert!(svc.send(&NotifyContext { body: "body".into(), ..Default::default() }).await.unwrap(), "reply {}", reply);
    }
  }

  #[tokio::test]
  async fn test_send_failed_devices() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "counts": 3,
        "logs": [{"type": "failed-push", "token": "tok2", "error": "invalid token"}],
        "success": "ok",
      })))
      .expect(1)
      .mount(&server)
      .await;
    let addr = server.address();
    let svc = from_url(&format!("gorush://{}:{}/tok1/tok2/tok3", addr.ip(), addr.port())).unwrap();
    assert!(!svc.send(&NotifyContext { body: "body".into(), ..Default::default() }).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_http_errors() {
    for code in [400u16, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let addr = server.address();
      let svc = from_url(&format!("gorush://{}:{}/token", addr.ip(), addr.port())).unwrap();
      assert!(svc.send(&NotifyContext { body: "body".into(), ..Default::default() }).await.is_err(), "code {}", code);
    }
  }

  #[test]
  fn test_service_details() {
    let details = Gorush::static_details();
    assert_eq!(details.service_name, "Gorush");
    assert_eq!(details.protocols, vec!["gorush", "gorushs"]);
    assert!(!details.attachment_support);
    let svc = parse("gorush://localhost/token").unwrap();
    assert_eq!(svc.body_maxlen(), 4000);
  }
}
