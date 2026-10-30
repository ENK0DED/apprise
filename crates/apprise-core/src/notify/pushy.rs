use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_list, split_path, unquote};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct Pushy {
  apikey: String,
  topics: Vec<String>,
  devices: Vec<String>,
  sound: Option<String>,
  badge: Option<u64>,
  verify_certificate: bool,
  tags: Vec<String>,
}

fn is_name(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric())
}

impl Pushy {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // ?key= wins over the host
    let apikey = url.get("key").filter(|k| !k.is_empty()).map(str::to_string).or_else(|| url.host.as_deref().map(unquote)).unwrap_or_default();
    let Some(apikey) = apikey.split_whitespace().next().map(str::to_string) else {
      tracing::warn!("An invalid Pushy Secret API Key was specified.");
      return None;
    };

    let mut entries = split_path(url);
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.push(to.to_string());
    }
    let (mut topics, mut devices) = (Vec::new(), Vec::new());
    for target in parse_list(entries, true, true) {
      if let Some(topic) = Some(target.strip_prefix('#').unwrap_or(&target)).filter(|t| is_name(t)) {
        topics.push(topic.to_string());
      } else if let Some(device) = target.strip_prefix('@').filter(|d| is_name(d)) {
        devices.push(device.to_string());
      } else {
        tracing::warn!("Dropped invalid topic/device ({}) specified.", target);
      }
    }

    let badge = url.get("badge").map(str::trim).filter(|b| !b.is_empty()).and_then(|b| match b.parse::<i64>() {
      Ok(n) if n >= 0 => Some(n as u64),
      _ => {
        tracing::warn!("The specified Pushy badge ({}) is not valid", b);
        None
      }
    });

    Some(Self {
      apikey,
      topics,
      devices,
      sound: url.get("sound").filter(|s| !s.is_empty()).map(str::to_string),
      badge,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pushy",
      service_url: Some("https://pushy.me/"),
      setup_url: Some("https://appriseit.com/services/pushy/"),
      protocols: vec!["pushy"],
      description: "Send push notifications via Pushy.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Pushy {
  fn schemas(&self) -> &[&str] {
    &["pushy"]
  }
  fn service_name(&self) -> &str {
    "Pushy"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    4096
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.topics.is_empty() && self.devices.is_empty() {
      tracing::warn!("There were no Pushy targets to notify.");
      return Ok(false);
    }

    let url = format!("{}/push", service_base("https://api.pushy.me"));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;

    // Topics first, then devices
    for target in self.topics.iter().chain(&self.devices) {
      let mut payload = json!({ "to": target, "data": { "message": ctx.body }, "notification": { "body": ctx.body } });
      if !ctx.title.is_empty() {
        payload["notification"]["title"] = json!(ctx.title);
      }
      if let Some(sound) = &self.sound {
        payload["notification"]["sound"] = json!(sound);
      }
      if let Some(badge) = self.badge {
        payload["notification"]["badge"] = json!(badge);
      }

      let result = client
        .post(&url)
        .query(&[("api_key", self.apikey.as_str())])
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Accepts", "application/json")
        .body(serde_json::to_vec(&payload)?)
        .send()
        .await;
      let resp = match result {
        Ok(resp) => resp,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Pushy:{} notification", target);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
          continue;
        }
      };

      // A 200 must also carry "success": true
      let status = resp.status();
      let success = resp.json::<Value>().await.ok().and_then(|c| c.get("success").map(|s| s.as_bool().unwrap_or(!s.is_null()))).unwrap_or(false);
      if status.as_u16() != 200 || !success {
        let reason = if status.as_u16() == 401 { "Unauthorized - Invalid Token." } else { status.canonical_reason().unwrap_or("") };
        tracing::warn!("Failed to send Pushy notification to {}: {}{}error={}.", target, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
        has_error = true;
        continue;
      }
      tracing::info!("Sent Pushy notification to {}.", target);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn good() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"success": true, "id": "5ea9b214b47cad768a35f13a", "info": {"devices": 1}}))
  }

  #[test]
  fn test_urls() {
    for url in [
      "pushy://apikey",
      "pushy://apikey/topic",
      "pushy://apikey/%20(",
      "pushy://apikey/@device",
      "pushy://apikey/device/?sound=alarm.aiff",
      "pushy://apikey/device/?badge=100",
      "pushy://apikey/device/?badge=invalid",
      "pushy://apikey/device/?badge=-12",
      "pushy://?key=apikey&to=device",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
    for url in ["pushy://", "pushy://:@/"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    let obj = Pushy::from_url(&ParsedUrl::parse("pushy://apikey/@dev1/%23t2/t1/bad-one?badge=3").unwrap()).unwrap();
    assert_eq!(obj.topics, vec!["t2", "t1"]);
    assert_eq!(obj.devices, vec!["dev1"]);
    assert_eq!(obj.badge, Some(3));
    assert_eq!(Pushy::from_url(&ParsedUrl::parse("pushy://apikey/d?badge=-12").unwrap()).unwrap().badge, None);
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/push"))
      .and(query_param("api_key", "apikey"))
      .and(header("accepts", "application/json"))
      .and(body_json(json!({"to": "topic", "data": {"message": "B"}, "notification": {"body": "B", "title": "T", "sound": "alarm.aiff", "badge": 100}})))
      .respond_with(good())
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(body_json(json!({"to": "device", "data": {"message": "B"}, "notification": {"body": "B", "title": "T", "sound": "alarm.aiff", "badge": 100}})))
      .respond_with(good())
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("pushy://apikey/@device/topic?sound=alarm.aiff&badge=100").unwrap();
    assert_eq!(svc.body_maxlen(), 4096);
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&requests[0].body).unwrap()["to"], "topic");
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    // No (valid) targets
    assert!(!from_url("pushy://apikey/%20(").unwrap().send(&ctx).await.unwrap());
    for response in [
      ResponseTemplate::new(200).set_body_json(json!({"success": false})),
      ResponseTemplate::new(200).set_body_string("garbage"),
      ResponseTemplate::new(200),
      ResponseTemplate::new(401).set_body_json(json!({"success": true})),
      ResponseTemplate::new(999),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(response).expect(1).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), from_url("pushy://apikey/topic").unwrap().send(&ctx)).await.unwrap());
    }
  }

  #[test]
  fn test_service_details() {
    let details = Pushy::static_details();
    assert_eq!(details.service_name, "Pushy");
    assert_eq!(details.protocols, vec!["pushy"]);
  }
}
