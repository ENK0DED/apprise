use async_trait::async_trait;
use serde::Serialize;
use serde::ser::Serializer;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, unquote};

/// Pinglet renders at most 3 badges (pills) per notification; longer keys
/// and values are rejected by the server, so they are truncated instead
const MAX_BADGE_COUNT: usize = 3;
const MAX_BADGE_KEY_LEN: usize = 24;
const MAX_BADGE_VALUE_LEN: usize = 32;

/// Metadata (data) key/value length limits
const MAX_DATA_KEY_LEN: usize = 64;
const MAX_DATA_VALUE_LEN: usize = 256;

/// Insert `key` keeping its first position, or update its value
fn upsert(entries: &mut Vec<(String, String)>, key: String, value: String) {
  match entries.iter_mut().find(|(k, _)| *k == key) {
    Some(entry) => entry.1 = value,
    None => entries.push((key, value)),
  }
}

/// Serialize key/value pairs as a JSON object in their own order (upstream
/// sends badges in the order the URL lists them)
fn ordered_map<S: Serializer>(entries: &[(String, String)], serializer: S) -> Result<S::Ok, S::Error> {
  serializer.collect_map(entries.iter().map(|(k, v)| (k, v)))
}

/// The JSON body Pinglet receives
#[derive(Serialize)]
struct Payload<'a> {
  message: &'a str,
  priority: &'a str,
  level: &'a str,
  /// Only included when a title was actually provided
  #[serde(skip_serializing_if = "str::is_empty")]
  title: &'a str,
  #[serde(skip_serializing_if = "<[_]>::is_empty", serialize_with = "ordered_map")]
  badges: &'a [(String, String)],
  #[serde(skip_serializing_if = "<[_]>::is_empty", serialize_with = "ordered_map")]
  data: &'a [(String, String)],
}

/// `validate_regex()`: must start with a non-space; surrounding whitespace is
/// stripped
fn validate(value: &str) -> Option<String> {
  value.chars().next().filter(|c| !c.is_whitespace())?;
  Some(value.trim().to_string())
}

/// Pinglet topic feeds.
///
/// `pinglets://{apikey}@{host}[:port][/prefix]/{namespace}/{topic}` with
/// optional `?priority=`, `:key=value` badges and `+key=value` metadata;
/// `pinglet://` uses plain HTTP.
pub struct Pinglet {
  token: String,
  host: String,
  port: Option<u16>,
  secure: bool,
  fullpath: String,
  namespace: String,
  topic: String,
  priority: &'static str,
  badges: Vec<(String, String)>,
  data: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Pinglet {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;

    // The last two path entries are the namespace and topic; anything before
    // them is a path prefix such as a reverse-proxy mount point
    let mut entries: Vec<String> = url.path_parts.clone();
    let topic = entries.pop();
    let namespace = entries.pop();
    let fullpath = if entries.is_empty() { "/".to_string() } else { format!("/{}/", entries.join("/")) };

    // The API key is the user component, or ?token=
    // (upstream unquotes ?token= and ?priority= once more)
    let token = url.get("token").filter(|t| !t.is_empty()).map(unquote).or_else(|| url.user.clone());
    let Some(token) = token.as_deref().and_then(validate) else {
      tracing::warn!("An invalid Pinglet API Key was specified.");
      return None;
    };
    let Some(namespace) = namespace.as_deref().and_then(validate) else {
      tracing::warn!("An invalid Pinglet Namespace ({}) was specified.", namespace.unwrap_or_default());
      return None;
    };
    let Some(topic) = topic.as_deref().and_then(validate) else {
      tracing::warn!("An invalid Pinglet Topic ({}) was specified.", topic.unwrap_or_default());
      return None;
    };

    // Priority by first letter; anything else is normal
    let priority = match url.get("priority").map(|p| unquote(p).to_lowercase()) {
      Some(p) if p.starts_with('s') => "silent",
      Some(p) if p.starts_with('u') => "urgent",
      _ => "normal",
    };

    // Badges (:key=value) and metadata (+key=value); upstream unquotes them
    // a second time
    let mut badges = Vec::new();
    for (key, value) in &url.qsd_colon {
      if badges.len() >= MAX_BADGE_COUNT {
        tracing::warn!("Pinglet renders at most {} badges; additional entries were ignored.", MAX_BADGE_COUNT);
        break;
      }
      let (key, value) = (unquote(key), unquote(value));
      if key.chars().count() > MAX_BADGE_KEY_LEN || value.chars().count() > MAX_BADGE_VALUE_LEN {
        tracing::warn!("Pinglet badge {} was truncated.", key);
      }
      upsert(&mut badges, key.chars().take(MAX_BADGE_KEY_LEN).collect(), value.chars().take(MAX_BADGE_VALUE_LEN).collect());
    }
    let mut data = Vec::new();
    for (key, value) in &url.qsd_add {
      let (key, value) = (unquote(key), unquote(value));
      if key.chars().count() > MAX_DATA_KEY_LEN || value.chars().count() > MAX_DATA_VALUE_LEN {
        tracing::warn!("Pinglet metadata {} was truncated.", key);
      }
      upsert(&mut data, key.chars().take(MAX_DATA_KEY_LEN).collect(), value.chars().take(MAX_DATA_VALUE_LEN).collect());
    }

    Some(Self {
      token,
      host,
      port: url.port,
      secure: url.schema == "pinglets",
      fullpath,
      namespace,
      topic,
      priority,
      badges,
      data,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pinglet",
      service_url: Some("https://pinglet.co.uk/"),
      setup_url: Some("https://appriseit.com/services/pinglet/"),
      protocols: vec!["pinglet", "pinglets"],
      description: "Publish push notifications to Pinglet topic feeds.",
      attachment_support: false,
    }
  }

  /// Pinglet's display-only level for each notification type
  fn level(notify_type: &NotifyType) -> &'static str {
    match notify_type {
      NotifyType::Info => "info",
      NotifyType::Success => "success",
      NotifyType::Warning => "warning",
      NotifyType::Failure => "error",
    }
  }
}

#[async_trait]
impl Notify for Pinglet {
  fn schemas(&self) -> &[&str] {
    &["pinglet", "pinglets"]
  }
  fn service_name(&self) -> &str {
    "Pinglet"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  /// Pinglet rejects payloads over 4KB in total; leave room for everything
  /// that is not the message body
  fn body_maxlen(&self) -> usize {
    3000
  }
  /// Pinglet API keys are limited to 120 requests/min, so upstream waits
  /// 0.5s between requests
  fn request_rate_per_sec(&self) -> f64 {
    0.5
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let schema = if self.secure { "https" } else { "http" };
    let port = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}{}{}/{}", schema, self.host, port, self.fullpath, self.namespace, self.topic);

    let payload = serde_json::to_string(&Payload {
      message: &ctx.body,
      priority: self.priority,
      level: Self::level(&ctx.notify_type),
      title: &ctx.title,
      badges: &self.badges,
      data: &self.data,
    })?;

    tracing::debug!("Pinglet POST URL: {} (cert_verify={})", url, self.verify_certificate);
    tracing::debug!("Pinglet Payload: {}", payload);
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .header("Authorization", format!("Bearer {}", self.token))
      .body(payload)
      .send()
      .await?;

    let status = resp.status().as_u16();
    if status == 200 {
      tracing::info!("Sent Pinglet notification.");
      Ok(true)
    } else {
      tracing::warn!("Failed to send Pinglet notification: error={}.", status);
      Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() })
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

  fn parse(url: &str) -> Pinglet {
    Pinglet::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn addr(server: &MockServer) -> String {
    let a = server.address();
    format!("{}:{}", a.ip(), a.port())
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      "pinglet://abc123@hostname/acme/deploys",
      "pinglets://abc123@hostname/acme/deploys",
      "pinglets://hostname/acme/deploys?token=abc123",
      "pinglet://abc123@hostname/prefix/acme/deploys",
      "pinglet://abc123@hostname:8080/acme/deploys",
      "pinglet://abc123@hostname/acme/deploys?priority=urgent",
      "pinglet://abc123@hostname/acme/deploys?priority=s",
      "pinglet://abc123@hostname/acme/deploys?priority=invalid",
      "pinglet://abc123@hostname/acme/deploys?:CPU=95%25&+region=eu-west",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "pinglet://",
      "pinglet://:@/",
      // No API key
      "pinglet://hostname/acme/deploys",
      // No namespace or topic
      "pinglet://token@hostname",
      "pinglet://token@hostname/deploys",
      // A namespace starting with whitespace
      "pinglet://token@hostname/%20acme/deploys",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let p = parse("pinglet://abc123@hostname/acme/deploys");
    assert_eq!(p.fullpath, "/");
    assert_eq!(p.namespace, "acme");
    assert_eq!(p.topic, "deploys");
    assert_eq!(p.priority, "normal");

    let p = parse("pinglets://other@hostname/a/b%20c/acme/deploys?token=abc123");
    assert_eq!(p.token, "abc123");
    assert_eq!(p.fullpath, "/a/b c/");

    for (priority, expected) in [("silent", "silent"), ("S", "silent"), ("normal", "normal"), ("urgent", "urgent"), ("u", "urgent"), ("bogus", "normal")] {
      assert_eq!(parse(&format!("pinglet://abc123@hostname/acme/deploys?priority={}", priority)).priority, expected, "priority={}", priority);
    }
  }

  #[test]
  fn test_badges_and_data() {
    // At most 3 badges, in URL order: a repeated key keeps its first
    // position and its last value; '+' stays literal; values are unquoted
    // twice like upstream
    let p = parse("pinglets://k@host/ns/topic?:Z=1&:A=a+b&:Z=2&:M=3&:N=4&+r=%2541&%2Bs=x");
    let pairs = |v: &[(&str, &str)]| v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
    assert_eq!(p.badges, pairs(&[("Z", "2"), ("A", "a+b"), ("M", "3")]));
    assert_eq!(p.data, pairs(&[("r", "A"), ("s", "x")]));

    // Badges are truncated to 24/32 characters
    let p = parse(&format!("pinglet://abc123@hostname/acme/deploys?:{}={}&:b=2&:c=3&:d=4&:e=5", "k".repeat(30), "v".repeat(40)));
    assert_eq!(p.badges, vec![("k".repeat(24), "v".repeat(32)), ("b".into(), "2".into()), ("c".into(), "3".into())]);

    // Metadata is truncated to 64/256 characters
    let p = parse(&format!("pinglet://abc123@hostname/acme/deploys?+{}={}", "k".repeat(70), "v".repeat(300)));
    assert_eq!(p.data, vec![("k".repeat(64), "v".repeat(256))]);
  }

  #[tokio::test]
  async fn test_send_full_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/acme/deploys"))
      .and(header("Authorization", "Bearer mykey"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({
        "message": "It works",
        "priority": "urgent",
        "level": "error",
        "title": "Hello",
        "badges": {"CPU": "95%", "Host": "web-1"},
        "data": {"region": "eu-west"},
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("pinglet://mykey@{}/acme/deploys?priority=urgent&:Host=web-1&:CPU=95%25&+region=eu-west", addr(&server))).unwrap();
    let ctx = NotifyContext { title: "Hello".into(), body: "It works".into(), notify_type: NotifyType::Failure, ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());

    // Badges keep the order the URL lists them in
    let requests = crate::notify::custom_json::requests_to(&server, "POST", "/acme/deploys").await;
    let publish = &requests[0];
    let body = String::from_utf8_lossy(&publish.body);
    assert!(body.contains(r#""badges":{"Host":"web-1","CPU":"95%"}"#), "{}", body);
  }

  #[tokio::test]
  async fn test_send_minimal_with_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/prefix/acme/deploys"))
      .and(body_json(json!({"message": "It works", "priority": "normal", "level": "info"})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("pinglet://mykey@{}/prefix/acme/deploys", addr(&server))).unwrap();
    assert!(svc.send(&NotifyContext { body: "It works".into(), ..Default::default() }).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for code in [500u16, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/acme/deploys")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url(&format!("pinglet://abc123@{}/acme/deploys", addr(&server))).unwrap();
      assert!(svc.send(&NotifyContext::default()).await.is_err());
    }
  }

  #[test]
  fn test_service_details() {
    let details = Pinglet::static_details();
    assert_eq!(details.service_name, "Pinglet");
    assert_eq!(details.protocols, vec!["pinglet", "pinglets"]);
    let svc = parse("pinglet://abc123@hostname/acme/deploys");
    assert_eq!(svc.body_maxlen(), 3000);
    assert_eq!(svc.request_rate_per_sec(), 0.5);
  }
}
