use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::email::parse_list;
use crate::utils::parse::{ParsedUrl, unquote};
use async_trait::async_trait;
use serde_json::json;

/// Longest sender name Viber accepts.
const SENDER_NAME_LIMIT: usize = 28;

/// Viber REST Bot API: `viber://{token}/{receiver_id}/...`.
pub struct Viber {
  token: String,
  targets: Vec<String>,
  /// Bot name (`?from=`)
  source: Option<String>,
  /// Bot avatar URL (`?avatar=`)
  avatar: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Viber {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().unwrap_or_default();
    let mut entries: Vec<String> = std::iter::once(host).chain(url.path_parts.iter().cloned()).collect();
    // Upstream unquotes these query values once more
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => unquote(t),
      // Without ?token= the first entry is the token, the rest receiver IDs
      None => entries.remove(0),
    };
    let token = token.trim().to_string();
    if token.is_empty() {
      tracing::warn!("An invalid Viber authentication token was specified.");
      return None;
    }
    // Upstream appends the ?to= string to its target list one character at
    // a time; apprise-go splits it into IDs instead, which is followed here.
    if let Some(to) = url.get("to") {
      entries.push(to.to_string());
    }
    let targets = parse_list(&entries);

    let source = url.get("from").map(unquote).filter(|s| !s.is_empty()).map(|s| {
      if s.chars().count() > SENDER_NAME_LIMIT {
        tracing::warn!("Viber sender name exceeds {} characters, truncating.", SENDER_NAME_LIMIT);
      }
      s.chars().take(SENDER_NAME_LIMIT).collect()
    });
    let avatar = url.get("avatar").map(unquote).filter(|s| !s.is_empty());

    Some(Self { token, targets, source, avatar, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Viber",
      service_url: Some("https://www.viber.com/"),
      setup_url: Some("https://appriseit.com/services/viber/"),
      protocols: vec!["viber"],
      description: "Send messages via Viber Bot API.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Viber {
  fn schemas(&self) -> &[&str] {
    &["viber"]
  }
  fn service_name(&self) -> &str {
    "Viber"
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
    30000
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There were no Viber targets to notify");
      return Ok(false);
    }
    let name = self.source.clone().unwrap_or_else(|| ctx.asset.app_desc.chars().take(SENDER_NAME_LIMIT).collect());
    let mut sender = json!({ "name": name });
    if let Some(ref avatar) = self.avatar {
      sender["avatar"] = json!(avatar);
    }

    let url = format!("{}/pa/send_message", service_base("https://chatapi.viber.com"));
    let client = build_client(self.verify_certificate)?;
    let mut all_ok = true;
    for dest in &self.targets {
      // A retry skips receivers that already accepted the message
      if crate::delivery::is_delivered(dest, false) {
        continue;
      }
      let payload = json!({ "type": "text", "text": ctx.body, "sender": sender, "receiver": dest });
      crate::notify::throttle().await;
      let resp = match client.post(&url).header("User-Agent", APP_ID).header("X-Viber-Auth-Token", &self.token).json(&payload).send().await {
        Ok(resp) => resp,
        Err(e) => {
          tracing::warn!("A Connection error occured sending Viber notification to {}: {}", dest, crate::error::loggable_http_error(&e));
          all_ok = false;
          continue;
        }
      };
      let status = resp.status();
      let Ok(content) = resp.json::<serde_json::Value>().await else {
        tracing::warn!("Invalid JSON response from Viber sending notification to {}", dest);
        all_ok = false;
        continue;
      };
      if status != reqwest::StatusCode::OK {
        tracing::warn!("Failed to send Viber notification to {} - {} error={}.", dest, content["status_message"].as_str().unwrap_or_default(), status.as_u16());
        all_ok = false;
        continue;
      }
      // Viber reports its own errors in a 200 response
      let code = content.get("status").and_then(|s| s.as_i64().or_else(|| s.as_str().and_then(|s| s.trim().parse().ok()))).unwrap_or(-1);
      if code != 0 {
        tracing::warn!(
          "Failed to send Viber notification to {} - Viber Error {{{}}} (status={})",
          dest,
          content["status_message"].as_str().unwrap_or("unknown"),
          code
        );
        all_ok = false;
        continue;
      }
      tracing::info!("Sent Viber notification to {}.", dest);
      crate::delivery::mark_delivered(dest, false);
    }
    Ok(all_ok)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, body_partial_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Viber {
    Viber::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "viber://tokena",
      "viber://?token=tokenb",
      "viber://token/targetx",
      "viber://token/t1/t2?from=Viber%20Bot",
      "viber://t1/t2?token=token",
      "viber://?token=token&to=t5",
      "viber://token/t3?avatar=value",
      "viber://token/?to=abc,def",
      "viber://token/t10",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["viber://", "viber:///", "viber://%20/target"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let v = parse("viber://mytoken/target1");
    assert_eq!(v.token, "mytoken");
    assert_eq!(v.targets, vec!["target1"]);
    assert!(v.source.is_none() && v.avatar.is_none());

    // With ?token= the host is a receiver too
    let v = parse("viber://t2/t1?token=token");
    assert_eq!(v.token, "token");
    assert_eq!(v.targets, vec!["t1", "t2"]);

    let v = parse("viber://token/t1?from=%20Viber%20Bot%20&avatar=%20http://a/b.png");
    assert_eq!(v.source.as_deref(), Some("Viber Bot"));
    assert_eq!(v.avatar.as_deref(), Some("http://a/b.png"));

    // Sender names are cut to 28 characters
    let v = parse(&format!("viber://token/t1?from={}", "x".repeat(30)));
    assert_eq!(v.source.as_deref(), Some("x".repeat(28).as_str()));
  }

  #[test]
  fn test_to_is_split_into_ids() {
    // Upstream would yield the receivers a, b, c, d, e and f here
    let v = parse("viber://?token=token&to=def,abc");
    assert_eq!(v.targets, vec!["abc", "def"]);
  }

  #[test]
  fn test_service_details() {
    let details = Viber::static_details();
    assert_eq!(details.service_name, "Viber");
    assert_eq!(details.service_url, Some("https://www.viber.com/"));
    let svc = from_url("viber://token/t1").unwrap();
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.body_maxlen(), 30000);
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    let sender = json!({ "name": "x".repeat(28), "avatar": "http://a/b.png" });
    Mock::given(method("POST"))
      .and(path("/pa/send_message"))
      .and(header("X-Viber-Auth-Token", "tok"))
      .and(header("Content-Type", "application/json"))
      .and(body_json(json!({ "type": "text", "text": "T\r\nB", "sender": sender, "receiver": "t1" })))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": 0, "status_message": "ok" })))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("viber://tok/t1?from={}&avatar=http://a/b.png", "x".repeat(30))).unwrap();
    let ctx = NotifyContext { body: "T\r\nB".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_default_sender_per_target() {
    let server = MockServer::start().await;
    for receiver in ["abc", "def"] {
      Mock::given(method("POST"))
        .and(path("/pa/send_message"))
        .and(body_json(json!({ "type": "text", "text": "B", "sender": { "name": "Apprise Notifications" }, "receiver": receiver })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": 0 })))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("viber://token/?to=abc,def").unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  async fn send_with(status: u16, response: &str) -> bool {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/pa/send_message"))
      .respond_with(ResponseTemplate::new(status).set_body_string(response))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("viber://token/t1").unwrap();
    crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.unwrap()
  }

  #[tokio::test]
  async fn test_send_failures() {
    // No receivers: nothing is sent
    assert!(!from_url("viber://tokena").unwrap().send(&NotifyContext::default()).await.unwrap());
    assert!(!send_with(200, r#"{"status": 1, "status_message": "bad"}"#).await);
    assert!(!send_with(200, "{").await);
    assert!(!send_with(200, "{}").await);
    assert!(!send_with(500, r#"{"status": 0}"#).await);
    assert!(!send_with(999, r#"{"status": 0}"#).await);
    assert!(send_with(200, r#"{"status": "0"}"#).await);
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second receiver fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/pa/send_message"))
      .and(body_partial_json(json!({"receiver": "def"})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/pa/send_message")).respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status": 0}"#)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("viber://token/abc/def?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&NotifyContext { body: "b".into(), ..Default::default() })).await.is_success());
    let sent: Vec<serde_json::Value> = crate::notify::custom_json::requests_to(&server, "POST", "/pa/send_message")
      .await
      .iter()
      .map(|r| r.body_json::<serde_json::Value>().unwrap()["receiver"].clone())
      .collect();
    assert_eq!(sent, vec![json!("abc"), json!("def"), json!("def")]);
  }
}
