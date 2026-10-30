use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_list, split_path, unquote};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct Pushed {
  app_key: String,
  app_secret: String,
  channels: Vec<String>,
  users: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

fn is_name(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric())
}

impl Pushed {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // pushed://{app_key}/{app_secret}/{targets}
    let token = |v: &str| v.split_whitespace().next().map(str::to_string);
    let mut entries = split_path(url);
    let app_secret = if entries.is_empty() { None } else { Some(entries.remove(0)) };
    let Some(app_key) = app_secret.as_ref().and(url.host.as_deref()).map(unquote).as_deref().and_then(token) else {
      tracing::warn!("An invalid Pushed Application Key was specified.");
      return None;
    };
    let Some(app_secret) = app_secret.as_deref().and_then(token) else {
      tracing::warn!("An invalid Pushed Application Secret was specified.");
      return None;
    };

    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.push(to.to_string());
    }
    let targets = parse_list(entries, true, true);
    let (mut channels, mut users) = (Vec::new(), Vec::new());
    for target in &targets {
      if let Some(user) = target.strip_prefix('@').filter(|u| is_name(u)) {
        users.push(user.to_string());
      } else if let Some(channel) = Some(target.strip_prefix('#').unwrap_or(target)).filter(|c| is_name(c)) {
        channels.push(channel.to_string());
      } else {
        tracing::warn!("Dropped invalid channel/userid ({}) specified.", target);
      }
    }
    if !targets.is_empty() && channels.is_empty() && users.is_empty() {
      tracing::warn!("No Pushed targets to notify.");
      return None;
    }

    Some(Self { app_key, app_secret, channels, users, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pushed",
      service_url: Some("https://pushed.co/"),
      setup_url: Some("https://appriseit.com/services/pushed/"),
      protocols: vec!["pushed"],
      description: "Send push notifications via Pushed.",
      attachment_support: false,
    }
  }

  async fn post(&self, client: &reqwest::Client, payload: &Value) -> bool {
    let url = format!("{}/1/push", service_base("https://api.pushed.co"));
    let Ok(body) = serde_json::to_vec(payload) else { return false };
    match client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(body).send().await {
      Ok(resp) if resp.status().as_u16() == 200 => {
        tracing::info!("Sent Pushed notification.");
        true
      }
      Ok(resp) => {
        let reason = resp.status().canonical_reason().unwrap_or("");
        tracing::warn!("Failed to send Pushed notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, resp.status().as_u16());
        false
      }
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Pushed notification.");
        tracing::debug!("Socket Exception: {}", e);
        false
      }
    }
  }
}

#[async_trait]
impl Notify for Pushed {
  fn schemas(&self) -> &[&str] {
    &["pushed"]
  }
  fn service_name(&self) -> &str {
    "Pushed"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // The title is merged into the body
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    160
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let payload = json!({ "app_key": self.app_key, "app_secret": self.app_secret, "target_type": "app", "content": ctx.body });

    // Without targets the whole app is notified
    if self.channels.is_empty() && self.users.is_empty() {
      return Ok(self.post(&client, &payload).await);
    }

    let mut has_error = false;
    for channel in &self.channels {
      let mut data = payload.clone();
      data["target_type"] = json!("channel");
      data["target_alias"] = json!(channel);
      has_error |= !self.post(&client, &data).await;
    }
    for user in &self.users {
      let mut data = payload.clone();
      data["target_type"] = json!("pushed_id");
      data["pushed_id"] = json!(user);
      has_error |= !self.post(&client, &data).await;
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn keys() -> (String, String) {
    ("a".repeat(32), "a".repeat(64))
  }

  #[test]
  fn test_urls() {
    let (k, s) = keys();
    for rest in ["", "/#channel/", "?to=channel", "/#channel1/#channel2", "/@ABCD/", "/@ABCD/@DEFG/", "/@ABCD/#channel"] {
      let url = format!("pushed://{}/{}{}", k, s, rest);
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
    for url in ["pushed://".to_string(), format!("pushed://{}", k), "pushed://:@/".to_string(), format!("pushed://{}/{}/dropped_value/", k, s)] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
    let obj = Pushed::from_url(&ParsedUrl::parse(&format!("pushed://{}/{}/@user/%23chan/bad_one?to=plain", k, s)).unwrap()).unwrap();
    assert_eq!(obj.channels, vec!["chan", "plain"]);
    assert_eq!(obj.users, vec!["user"]);
  }

  #[tokio::test]
  async fn test_send_app() {
    let (k, s) = keys();
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/1/push"))
      .and(header("content-type", "application/json"))
      .and(wiremock::matchers::body_json(json!({"app_key": k, "app_secret": s, "target_type": "app", "content": "B"})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("pushed://{}/{}", k, s)).unwrap();
    assert_eq!((svc.title_maxlen(), svc.body_maxlen()), (0, 160));
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_targets() {
    let (k, s) = keys();
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(3).mount(&server).await;
    let svc = from_url(&format!("pushed://{}/{}/@ABCD/%23c2/%23c1", k, s)).unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let bodies: Vec<Value> = server.received_requests().await.unwrap().iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
    let base = |extra: Value| {
      let mut v = json!({"app_key": k, "app_secret": s, "content": "B"});
      for (key, value) in extra.as_object().unwrap() {
        v[key] = value.clone();
      }
      v
    };
    assert_eq!(bodies[0], base(json!({"target_type": "channel", "target_alias": "c1"})));
    assert_eq!(bodies[1], base(json!({"target_type": "channel", "target_alias": "c2"})));
    assert_eq!(bodies[2], base(json!({"target_type": "pushed_id", "pushed_id": "ABCD"})));
  }

  #[tokio::test]
  async fn test_send_failure() {
    let (k, s) = keys();
    for status in [500, 999] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(2).mount(&server).await;
      let svc = from_url(&format!("pushed://{}/{}/%23c/@u", k, s)).unwrap();
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }
  }

  #[test]
  fn test_service_details() {
    let details = Pushed::static_details();
    assert_eq!(details.service_name, "Pushed");
    assert_eq!(details.protocols, vec!["pushed"]);
  }
}
