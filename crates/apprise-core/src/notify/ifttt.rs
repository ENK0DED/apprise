use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_list};

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
    // Native https://maker.ifttt.com/use/{webhook_id}/{events} URLs arrive
    // here already rewritten by the registry as ifttt://{webhook_id}@/{events}
    let host = url.host.clone().unwrap_or_default();
    let (webhook_id, mut raw_events) = match url.user.clone() {
      // ifttt://{webhook_id}@{event}/{events}: the host is an event too
      Some(user) => {
        let mut events: Vec<String> = Some(host).filter(|h| !h.is_empty()).into_iter().collect();
        events.extend(url.path_parts.iter().cloned());
        (user, events)
      }
      // ifttt://{webhook_id}/{events}
      None => (host, url.path_parts.clone()),
    };

    // A YAML `webhook_id:` replaces the URL's (upstream passes it to the
    // constructor); the events stay as the URL gives them
    let webhook_id = url.kwarg("webhook_id").map_or(webhook_id, str::to_string);
    let webhook_id = webhook_id.trim().to_string();
    if webhook_id.is_empty() {
      tracing::warn!("An invalid IFTTT Webhook ID was specified.");
      return None;
    }

    if let Some(to) = url.get("to") {
      raw_events.push(to.to_string());
    }
    let events = parse_list(&raw_events.join(" "));
    if events.is_empty() {
      tracing::warn!("You must specify at least one event you wish to trigger on.");
      return None;
    }

    // Upstream `qsd+` / `qsd-`, in URL order
    let add_tokens: Vec<(String, String)> = url.qsd_add.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let del_tokens: Vec<String> = url.qsd_del.keys().cloned().collect();

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
      // Skip an event that already accepted this message so a retry does
      // not deliver it twice
      if crate::delivery::is_delivered(event, false) {
        continue;
      }
      let url = format!("{}/trigger/{}/with/key/{}", service_base(IFTTT_API), event, self.webhook_id);
      tracing::debug!("IFTTT POST URL to event {} (cert_verify={})", event, self.verify_certificate);
      crate::notify::throttle().await;
      let resp = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(payload.clone()).send().await;
      match resp {
        Ok(r) if r.status().as_u16() == 200 => {
          tracing::info!("Sent IFTTT notification to {}.", event);
          crate::delivery::mark_delivered(event, false);
        }
        Ok(r) => {
          let status = r.status();
          let reason = status.canonical_reason().unwrap_or("");
          tracing::warn!("Failed to send IFTTT notification to {}: {}{}error={}.", event, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending IFTTT:{} notification.", event);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
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

    // What the registry makes of https://maker.ifttt.com/use/WebHookID/EventID/Second
    let i = parse_ifttt("ifttt://WebHookID@/EventID/Second");
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

    let reqs: Vec<_> = server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == "POST").collect();
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

  #[tokio::test]
  async fn test_retry_skips_events_that_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/trigger/a/with/key/k")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    // The second event fails once, then accepts the retry
    let b = || Mock::given(method("POST")).and(path("/trigger/b/with/key/k"));
    b().respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    b().respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("ifttt://k@a/b?retry=1&wait=0"));
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx)).await.is_success());
  }

  #[test]
  fn test_service_details() {
    let details = Ifttt::static_details();
    assert_eq!(details.service_name, "IFTTT");
    assert_eq!(details.protocols, vec!["ifttt"]);
  }

  /// A YAML `webhook_id:` replaces the URL's, as upstream's constructor
  /// argument does; the host written as `{webhook_id}@{event}` stays an
  /// event (upstream: EventOne and EventTwo, both with key WebHookB).
  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_yaml_webhook_id_replaces_the_urls() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let doc = "urls:\n  - ifttt://WebHookA@EventOne/EventTwo:\n      webhook_id: WebHookB\n";
    let (a, _) = crate::config::yaml::load_for_test(doc).await;
    let ctx = NotifyContext { title: "title".into(), body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
    let mut paths: Vec<String> =
      server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST").map(|r| r.url.path().to_string()).collect();
    paths.sort();
    assert_eq!(paths, ["/trigger/EventOne/with/key/WebHookB", "/trigger/EventTwo/with/key/WebHookB"]);
  }
}
