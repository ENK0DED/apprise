use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;

const IFTTT_API: &str = "https://maker.ifttt.com";

pub struct Ifttt {
  webhook_id: String,
  events: Vec<String>,
  /// `+Key=Value` entries added to (or overriding) the payload.
  add_tokens: Vec<(String, String)>,
  /// `-Key` entries removed from the payload (matched without case).
  del_tokens: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Ifttt {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (webhook_id, raw_events) = if url.schema == "https" || url.schema == "http" {
      // Native URL: https://maker.ifttt.com/use/{webhook_id}/{events}
      if !url.host.as_deref().is_some_and(|h| h.eq_ignore_ascii_case("maker.ifttt.com")) {
        return None;
      }
      let valid = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
      match url.path_parts.split_first() {
        Some((use_, rest)) if use_ == "use" && !rest.is_empty() && rest.iter().all(|p| valid(p)) => (rest[0].clone(), rest[1..].to_vec()),
        _ => return None,
      }
    } else {
      let host = url.host.as_deref().map(unquote).unwrap_or_default();
      match url.user.as_deref().filter(|u| !u.is_empty()) {
        // ifttt://{webhook_id}@{event}/{events}, or ifttt://{webhook_id}@/{events}
        // (the form upstream builds from a native URL)
        Some(user) => {
          let mut events: Vec<String> = Some(host).filter(|h| !h.is_empty()).into_iter().collect();
          events.extend(url.path_parts.iter().cloned());
          (unquote(user), events)
        }
        // ifttt://{webhook_id}/{events}
        None => (host, url.path_parts.clone()),
      }
    };

    let webhook_id = webhook_id.trim().to_string();
    if webhook_id.is_empty() {
      tracing::warn!("An invalid IFTTT Webhook ID was specified.");
      return None;
    }

    let mut raw_events = raw_events;
    if let Some(to) = url.get("to") {
      raw_events.push(to.to_string());
    }
    let events = parse_list(&raw_events);
    if events.is_empty() {
      tracing::warn!("You must specify at least one event you wish to trigger on.");
      return None;
    }

    // Upstream `qsd+` / `qsd-`. A literal `+` in a query key may arrive
    // decoded as a space, which upstream treats the same way.
    let mut add_tokens: Vec<(String, String)> = Vec::new();
    let mut del_tokens: Vec<String> = Vec::new();
    for (key, value) in &url.qsd {
      if let Some(k) = key.strip_prefix('+').or_else(|| key.strip_prefix(' ')) {
        add_tokens.push((k.to_string(), value.trim().to_string()));
      } else if let Some(k) = key.strip_prefix('-') {
        del_tokens.push(k.to_string());
      }
    }
    add_tokens.sort();
    del_tokens.sort();

    Some(Self { webhook_id, events, add_tokens, del_tokens, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "IFTTT",
      service_url: Some("https://ifttt.com/"),
      setup_url: Some("https://appriseit.com/services/ifttt/"),
      protocols: vec!["ifttt"],
      description: "Trigger IFTTT webhooks.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    // value1/value2/value3, then the +tokens; all keys are sent lowercase,
    // so a +Value1 overrides value1. -tokens are removed without case.
    let mut entries: Vec<(String, Value)> =
      vec![("value1".into(), json!(ctx.title)), ("value2".into(), json!(ctx.body)), ("value3".into(), json!(ctx.notify_type.as_str()))];
    entries.extend(self.add_tokens.iter().map(|(k, v)| (k.clone(), json!(v))));

    let mut payload = Map::new();
    for (k, v) in entries {
      let k = k.to_lowercase();
      if !self.del_tokens.iter().any(|d| d.to_lowercase() == k) {
        payload.insert(k, v);
      }
    }
    Value::Object(payload)
  }
}

#[async_trait]
impl Notify for Ifttt {
  fn schemas(&self) -> &[&str] {
    &["ifttt"]
  }
  fn service_name(&self) -> &str {
    "IFTTT"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let payload = self.payload(ctx).to_string();
    let mut has_error = false;
    for event in &self.events {
      let url = format!("{}/trigger/{}/with/key/{}", service_base(IFTTT_API), event, self.webhook_id);
      tracing::debug!("IFTTT POST URL to event {} (cert_verify={})", event, self.verify_certificate);
      let resp = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(payload.clone()).send().await;
      match resp {
        Ok(r) if r.status().as_u16() == 200 => tracing::info!("Sent IFTTT notification to {}.", event),
        Ok(r) => {
          let status = r.status();
          let reason = status.canonical_reason().unwrap_or("");
          tracing::warn!("Failed to send IFTTT notification to {}: {}{}error={}.", event, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending IFTTT:{} notification.", event);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

fn unquote(s: &str) -> String {
  urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}

/// Upstream `parse_list()`: split on `[]`, `;`, `,` and whitespace, then
/// de-duplicate and sort.
fn parse_list(items: &[String]) -> Vec<String> {
  let mut out: Vec<String> = items
    .iter()
    .flat_map(|i| i.split(|c: char| c == '[' || c == ']' || c == ';' || c == ',' || c.is_whitespace()))
    .filter(|t| !t.is_empty())
    .map(String::from)
    .collect();
  out.sort();
  out.dedup();
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse_ifttt(url: &str) -> Ifttt {
    Ifttt::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "ifttt://WebHookID@EventID/?+TemplateKey=TemplateVal",
      "ifttt://WebHookID?to=EventID,EventID2",
      "ifttt://WebHookID@EventID/?-Value1=&-Value2",
      "ifttt://WebHookID@EventID/EventID2/",
      "ifttt://webhook123/event1",
      "https://maker.ifttt.com/use/WebHookID/EventID/",
      "https://maker.ifttt.com/use/WebHookID/EventID/?-Value1=",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["ifttt://", "ifttt://:@/", "ifttt://EventID/", "https://maker.ifttt.com/use/WebHookID/"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let i = parse_ifttt("ifttt://WebHookID@EventB/EventA/EventB?to=EventC");
    assert_eq!(i.webhook_id, "WebHookID");
    assert_eq!(i.events, vec!["EventA", "EventB", "EventC"]);

    let i = parse_ifttt("ifttt://WebHookID?to=EventID,EventID2");
    assert_eq!(i.webhook_id, "WebHookID");
    assert_eq!(i.events, vec!["EventID", "EventID2"]);

    let i = parse_ifttt("https://maker.ifttt.com/use/WebHookID/EventID/Second");
    assert_eq!(i.webhook_id, "WebHookID");
    assert_eq!(i.events, vec!["EventID", "Second"]);

    let i = parse_ifttt("ifttt://WebHookID@EventID/?%2Bswitch=on&-value1&-value2");
    assert_eq!(i.add_tokens, vec![("switch".to_string(), "on".to_string())]);
    assert_eq!(i.del_tokens, vec!["value1", "value2"]);
  }

  #[test]
  fn test_payload_tokens() {
    let ctx = NotifyContext { title: "title".into(), body: "body".into(), ..Default::default() };
    let i = parse_ifttt("ifttt://WebHookID@EventID/?%2Bswitch=on&-value1&-value2");
    assert_eq!(i.payload(&ctx), json!({"value3": "info", "switch": "on"}));

    // Removal ignores case
    let i = parse_ifttt("ifttt://WebHookID@EventID/?-Value1&-VALUE2");
    assert_eq!(i.payload(&ctx), json!({"value3": "info"}));

    // An added token overrides a default one, and keys go out lowercase
    let i = parse_ifttt("ifttt://WebHookID@EventID/?%2BValue1=over&-VALUE2&%2BExtra=x");
    assert_eq!(i.payload(&ctx), json!({"value1": "over", "value3": "info", "extra": "x"}));
  }

  #[tokio::test]
  async fn test_send_each_event() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/trigger/event1/with/key/webhook123"))
      .and(header("Content-Type", "application/json"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/trigger/event2/with/key/webhook123")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let svc = from_url("ifttt://webhook123@event2/event1").unwrap();
    let ctx = NotifyContext { title: "title".into(), body: "hello".into(), notify_type: NotifyType::Success, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    // Events are sorted, as upstream's parse_list() does
    assert_eq!(reqs[0].url.path(), "/trigger/event1/with/key/webhook123");
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body, json!({"value1": "title", "value2": "hello", "value3": "success"}));
  }

  #[tokio::test]
  async fn test_send_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/trigger/a/with/key/k")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/trigger/b/with/key/k")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("ifttt://k@a/b").unwrap();
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    // A failed event does not stop the remaining ones
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[test]
  fn test_service_details() {
    let details = Ifttt::static_details();
    assert_eq!(details.service_name, "IFTTT");
    assert_eq!(details.protocols, vec!["ifttt"]);
  }
}
