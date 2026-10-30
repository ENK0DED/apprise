use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, service_base};
use crate::utils::email::parse_list;
use crate::utils::parse::{ParsedUrl, unquote};

/// Delivery channels WPUSH accepts; one request may use several.
const CHANNELS: [&str; 10] = ["wechat", "app", "sms", "mail", "webhook", "dingtalk", "feishu", "wechat_work", "clawbot", "qqbot"];

/// WPUSH delivers to WeChat unless told otherwise.
const DEFAULT_CHANNEL: &str = "wechat";

/// WPUSH notifications (WeChat, its App, SMS, email, DingTalk, Feishu, WeCom,
/// webhooks, WeChat ClawBot and QQ Robot).
pub struct WPush {
  apikey: String,
  /// Sorted, without duplicates
  channels: Vec<String>,
  /// One request per topic; none sends a single personal notification
  topics: Vec<String>,
  /// QQ group code or bound channel instance (API `option`)
  group: Option<String>,
  click_url: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl WPush {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The registry turns https://api.wpush.cn/api/v1/send?apikey={apikey}
    // into wpush://{apikey}/?... Upstream unquotes the query values once
    // more.
    let qsd = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(unquote);
    // ?apikey= wins over the host
    let apikey = match qsd("apikey") {
      Some(k) => k,
      None => url.host.clone().unwrap_or_default(),
    };
    let apikey = apikey.trim().to_string();
    if !is_apikey(&apikey) {
      tracing::warn!("The WPUSH API Key is invalid.");
      return None;
    }

    let mut channels: Vec<String> = Vec::new();
    match qsd("channel") {
      Some(value) => {
        for entry in parse_list(&[value]) {
          let entry = entry.to_lowercase();
          if !CHANNELS.contains(&entry.as_str()) {
            tracing::warn!("The WPUSH channel ({}) is not valid.", entry);
            return None;
          }
          if !channels.contains(&entry) {
            channels.push(entry);
          }
        }
        channels.sort();
      }
      None => channels.push(DEFAULT_CHANNEL.to_string()),
    }

    // Topics come from the path, ?to=, ?topic_code= and ?topic=
    let mut entries = url.path_parts.clone();
    for key in ["to", "topic_code", "topic"] {
      // Upstream's URLBase.parse_list() also unquotes each entry
      if let Some(v) = url.get(key).filter(|v| !v.is_empty()) {
        entries.extend(parse_list(&[v]).iter().map(|t| unquote(t)));
      }
    }
    let topics = parse_list(&entries);

    // ?group= wins over ?option=
    let group = qsd("group").or_else(|| qsd("option")).filter(|v| !v.trim().is_empty());
    if group.is_some() && !topics.is_empty() {
      tracing::warn!("The WPUSH option/group cannot be combined with topic targets.");
      return None;
    }

    Some(Self {
      apikey,
      channels,
      topics,
      group,
      click_url: qsd("url").filter(|v| !v.trim().is_empty()),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "WPUSH",
      service_url: Some("https://wpush.cn/"),
      setup_url: Some("https://appriseit.com/services/wpush/"),
      protocols: vec!["wpush"],
      description: "Send notifications through WPUSH to WeChat, SMS, email, DingTalk, Feishu, WeCom and more.",
      attachment_support: false,
    }
  }

  /// The JSON body for one request; `topic` is `None` for a direct send.
  fn payload(&self, ctx: &NotifyContext, topic: Option<&str>) -> Value {
    let mut payload = json!({
      "apikey": self.apikey,
      // The title is required; fall back to the body
      "title": if ctx.title.is_empty() { &ctx.body } else { &ctx.title },
      "content": ctx.body,
      "channel": self.channels.join(","),
    });
    if let Some(url) = &self.click_url {
      payload["url"] = json!(url);
    }
    if let Some(topic) = topic {
      payload["topic_code"] = json!(topic);
    } else if let Some(group) = &self.group {
      payload["option"] = json!(group);
    }
    payload
  }
}

/// `WPUSH` followed by 27 alphanumerics (either case).
fn is_apikey(s: &str) -> bool {
  s.len() == 32 && s.as_bytes()[..5].eq_ignore_ascii_case(b"WPUSH") && s.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// A random (version 4) UUID for the `X-Idempotency-Key` header.
fn uuid4() -> String {
  let mut b: [u8; 16] = rand::random();
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  let h = hex::encode(b);
  format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// WPUSH reports success as JSON `code` 0. Only an explicit integer zero
/// counts: a missing or null code, `false` (equal to 0 in Python) or `0.0`
/// is a failure.
fn is_success_code(content: &Value) -> bool {
  content.get("code").is_some_and(|c| c.as_i64() == Some(0))
}

#[async_trait]
impl Notify for WPush {
  fn schemas(&self) -> &[&str] {
    &["wpush"]
  }
  fn service_name(&self) -> &str {
    "WPUSH"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    10000
  }
  fn title_maxlen(&self) -> usize {
    255
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // The API key travels in a header and in the body, so a redirect must
    // never replay the request to another host (apprise-go hardening;
    // upstream follows redirects).
    let client = crate::notify::client_builder(self.verify_certificate, Default::default())
      .redirect(reqwest::redirect::Policy::none())
      .build()
      .map_err(NotifyError::Http)?;
    let url = format!("{}/api/v1/send", service_base("https://api.wpush.cn"));

    let topics: Vec<Option<&str>> = if self.topics.is_empty() { vec![None] } else { self.topics.iter().map(|t| Some(t.as_str())).collect() };
    let mut has_error = false;
    for topic in topics {
      // A retry skips topics that already accepted the message
      let key = topic.unwrap_or_default();
      if crate::delivery::is_delivered(key, false) {
        continue;
      }
      let label = topic.map(|t| format!(" to topic {}", t)).unwrap_or_default();
      // serde_json keeps non-ASCII text (Chinese) as UTF-8, like ensure_ascii=False
      let body = serde_json::to_vec(&self.payload(ctx, topic))?;
      crate::notify::throttle().await;
      let resp = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("X-API-Key", &self.apikey)
        // A unique key per request prevents duplicate delivery
        .header("X-Idempotency-Key", uuid4())
        .body(body)
        .send()
        .await;
      let resp = match resp {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending WPUSH notification{}: {}", label, crate::error::loggable_http_error(&e));
          has_error = true;
          continue;
        }
      };

      let status = resp.status().as_u16();
      let text = resp.text().await.unwrap_or_default();
      if status != 200 {
        tracing::warn!("Failed to send WPUSH notification{}: error={}.", label, status);
        tracing::debug!("Response Details: {}", text);
        has_error = true;
        continue;
      }
      let content: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
      if !is_success_code(&content) {
        let code = content.get("code").cloned().unwrap_or_default();
        let message = content.get("message").and_then(Value::as_str).unwrap_or("Unknown error");
        tracing::warn!("Failed to send WPUSH notification{}: code={}: {}.", label, code, message);
        has_error = true;
        continue;
      }
      tracing::info!("Sent WPUSH notification{}.", label);
      crate::delivery::mark_delivered(key, false);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_partial_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  /// The POSTs this test sent; other processes probing localhost ports can
  /// reach a mock server too, so anything else is ignored.
  async fn received(server: &MockServer) -> Vec<wiremock::Request> {
    crate::notify::custom_json::requests_to(server, "POST", "/api/v1/send").await
  }

  const KEY: &str = "WPUSHaaaaaaaaaaaaaaaaaaaaaaaaaaa";
  const GOOD_RESPONSE: &str = r#"{"code": 0, "message": "success", "data": "1126950958891274240"}"#;
  const BAD_RESPONSE: &str = r#"{"code": 401, "message": "API Key error"}"#;

  /// Parse `url`, converting a native URL the way the registry does.
  fn parse(url: &str) -> Option<WPush> {
    let native = crate::notify::registry::NATIVE_URLS.iter().find_map(|n| (n.convert)(url));
    WPush::from_url(&ParsedUrl::parse(native.as_deref().unwrap_or(url))?)
  }

  fn bodies(requests: &[wiremock::Request]) -> Vec<Value> {
    requests.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  async fn send_with(url: &str, ctx: &NotifyContext, status: u16, response: &str) -> (bool, Vec<wiremock::Request>) {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/v1/send")).respond_with(ResponseTemplate::new(status).set_body_string(response)).mount(&server).await;
    let svc = from_url(url).unwrap();
    let ok = crate::notify::with_test_base(server.uri(), svc.send(ctx)).await.unwrap();
    (ok, received(&server).await)
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("wpush://{}", KEY),
      format!("wpush://?apikey={}", KEY),
      format!("wpush://{}?channel=feishu", KEY),
      format!("wpush://{}?channel=feishu,dingtalk,qqbot", KEY),
      format!("wpush://{}/topic1/topic2", KEY),
      format!("wpush://{}?to=topic1,topic2", KEY),
      format!("wpush://{}?topic_code=mytopic", KEY),
      format!("wpush://{}?topic=mytopic", KEY),
      format!("wpush://{}?channel=qqbot&group=123456", KEY),
      format!("wpush://{}?channel=qqbot&option=123456", KEY),
      format!("wpush://{}?url=https://example.com/", KEY),
      format!("https://api.wpush.cn/api/v1/send?apikey={}", KEY),
      format!("https://api.wpush.cn/api/v1/send?channel=mail&apikey={}", KEY),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "wpush://".to_string(),
      "wpush://short".to_string(),
      format!("wpush://{}a", KEY),
      format!("wpush://WPUSH{}", "-".repeat(27)),
      format!("wpush://{}?channel=nope", KEY),
      format!("wpush://{}?channel=feishu,nope", KEY),
      format!("wpush://{}?group=123456&topic_code=mytopic", KEY),
      format!("wpush://{}/topic?option=123456", KEY),
      "https://api.wpush.cn/api/v1/send?channel=mail".to_string(),
      format!("https://other.example.com/api/v1/send?apikey={}", KEY),
      "https://api.wpush.cn/api/v1/send".to_string(),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parse_fields() {
    let obj = parse(&format!("wpush://{}", KEY)).unwrap();
    assert_eq!(obj.apikey, KEY);
    assert_eq!(obj.channels, vec![DEFAULT_CHANNEL]);
    assert!(obj.topics.is_empty());
    assert!(obj.group.is_none());
    assert!(obj.click_url.is_none());

    // Channels ignore case and duplicates and are stored sorted
    assert_eq!(parse(&format!("wpush://{}?channel=Feishu,dingtalk,feishu", KEY)).unwrap().channels, vec!["dingtalk", "feishu"]);
    assert_eq!(parse(&format!("wpush://{}?channel=QQBOT,app,APP,feishu", KEY)).unwrap().channels, vec!["app", "feishu", "qqbot"]);

    // Topics from every source, sorted and de-duplicated
    let obj = parse(&format!("wpush://{}/zeta/alpha/?to=beta,alpha&topic_code=gamma&topic=delta", KEY)).unwrap();
    assert_eq!(obj.topics, vec!["alpha", "beta", "delta", "gamma", "zeta"]);

    // ?group= wins over ?option=; a blank group is ignored
    assert_eq!(parse(&format!("wpush://{}?group=group1&option=other", KEY)).unwrap().group.as_deref(), Some("group1"));
    assert_eq!(parse(&format!("wpush://{}?option=ops", KEY)).unwrap().group.as_deref(), Some("ops"));
    assert!(parse(&format!("wpush://{}/topic?group=%20", KEY)).unwrap().group.is_none());

    // ?apikey= wins over the host
    let other = "WPUSHabcdefghijklmnopqrstuvwxyz0";
    assert_eq!(parse(&format!("wpush://{}?apikey={}", KEY, other)).unwrap().apikey, other);

    // Native URLs keep their other arguments
    let obj = parse(&format!("https://api.wpush.cn/api/v1/send?apikey={}&channel=mail", KEY)).unwrap();
    assert_eq!(obj.apikey, KEY);
    assert_eq!(obj.channels, vec!["mail"]);
  }

  #[test]
  fn test_success_code() {
    assert!(is_success_code(&json!({ "code": 0 })));
    for content in [
      json!({}),
      json!({ "code": null }),
      json!({ "code": false }),
      json!({ "code": 0.0 }),
      json!({ "code": "0" }),
      json!({ "code": 401 }),
      json!([0]),
      Value::Null,
    ] {
      assert!(!is_success_code(&content), "{}", content);
    }
  }

  #[test]
  fn test_uuid4() {
    let id = uuid4();
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "4");
    assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
    assert_ne!(id, uuid4());
  }

  #[test]
  fn test_service_attributes() {
    let svc = from_url(&format!("wpush://{}", KEY)).unwrap();
    assert_eq!(svc.body_maxlen(), 10000);
    assert_eq!(svc.title_maxlen(), 255);
    assert!(!svc.attachment_support());
  }

  #[tokio::test]
  async fn test_send_direct() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/send"))
      .and(header("User-Agent", APP_ID))
      .and(header("Content-Type", "application/json"))
      .and(header("X-API-Key", KEY))
      .respond_with(ResponseTemplate::new(200).set_body_string(GOOD_RESPONSE))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("wpush://{}", KEY)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Title", "Body"))).await.unwrap());
    let requests = received(&server).await;
    assert_eq!(bodies(&requests)[0], json!({ "apikey": KEY, "title": "Title", "content": "Body", "channel": "wechat" }));
    assert_eq!(requests[0].headers.get("X-Idempotency-Key").unwrap().len(), 36);
  }

  #[tokio::test]
  async fn test_send_unicode_body_is_utf8() {
    let (ok, requests) = send_with(&format!("wpush://{}", KEY), &ctx("你好", "中文 🦄"), 200, GOOD_RESPONSE).await;
    assert!(ok);
    // Non-ASCII text is sent as UTF-8, not \u escapes
    assert!(String::from_utf8(requests[0].body.clone()).unwrap().contains("你好"));
    assert_eq!(bodies(&requests)[0]["content"], "中文 🦄");
  }

  #[tokio::test]
  async fn test_send_title_falls_back_to_body() {
    let (_, requests) = send_with(&format!("wpush://{}", KEY), &ctx("", "Body only"), 200, GOOD_RESPONSE).await;
    assert_eq!(bodies(&requests)[0]["title"], "Body only");
  }

  #[tokio::test]
  async fn test_send_channels_group_and_url() {
    // Matches pycapture for the same URL
    let url = format!("wpush://{}?channel=QQBOT,app,APP,feishu&group=group1&option=other&url=https://example.com", KEY);
    let (ok, requests) = send_with(&url, &ctx("Title", "Body"), 200, GOOD_RESPONSE).await;
    assert!(ok);
    assert_eq!(
      bodies(&requests)[0],
      json!({ "apikey": KEY, "title": "Title", "content": "Body", "channel": "app,feishu,qqbot", "url": "https://example.com", "option": "group1" })
    );
  }

  #[tokio::test]
  async fn test_send_multiple_topics() {
    let (ok, requests) = send_with(&format!("wpush://{}/zeta/alpha/?to=beta,alpha", KEY), &ctx("", "B"), 200, GOOD_RESPONSE).await;
    assert!(ok);
    let payloads = bodies(&requests);
    let topics: Vec<&str> = payloads.iter().map(|p| p["topic_code"].as_str().unwrap()).collect();
    assert_eq!(topics, vec!["alpha", "beta", "zeta"]);
    assert!(payloads.iter().all(|p| p.get("option").is_none()));
    // Each request has its own idempotency key
    let keys: Vec<_> = requests.iter().map(|r| r.headers.get("X-Idempotency-Key").unwrap().clone()).collect();
    assert_ne!(keys[0], keys[1]);
  }

  #[tokio::test]
  async fn test_send_instance_options() {
    for channel in ["feishu", "dingtalk", "webhook", "wechat_work"] {
      let (ok, requests) = send_with(&format!("wpush://{}?channel={}&option=ops", KEY, channel), &ctx("t", "b"), 200, GOOD_RESPONSE).await;
      assert!(ok);
      let payload = &bodies(&requests)[0];
      assert_eq!(payload["option"], "ops");
      assert_eq!(payload["channel"], channel);
    }
  }

  #[tokio::test]
  async fn test_send_rejections() {
    let url = format!("wpush://{}", KEY);
    for (status, response) in [
      (200u16, BAD_RESPONSE),
      (200, "not-json"),
      (200, ""),
      (200, "{}"),
      (200, r#"{"code": null}"#),
      (200, r#"{"code": false, "message": "nope"}"#),
      (200, r#"{"code": 0.0}"#),
      (200, "[0]"),
      // An HTTP failure is a failure even when the body says code 0
      (500, GOOD_RESPONSE),
      (201, GOOD_RESPONSE),
    ] {
      let (ok, _) = send_with(&url, &ctx("t", "b"), status, response).await;
      assert!(!ok, "{} {}", status, response);
    }
    let svc = from_url(&url).unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("t", "b"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_redirects_not_followed() {
    for status in [301u16, 302, 303, 307, 308] {
      let target = MockServer::start().await;
      Mock::given(method("POST")).and(path("/api/v1/send")).respond_with(ResponseTemplate::new(200).set_body_string(GOOD_RESPONSE)).mount(&target).await;
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path("/api/v1/send"))
        .respond_with(ResponseTemplate::new(status).insert_header("Location", format!("{}/api/v1/send", target.uri()).as_str()))
        .expect(1)
        .mount(&server)
        .await;

      let svc = from_url(&format!("wpush://{}", KEY)).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap(), "status {}", status);
      // The credentials never reach the redirect target
      assert!(received(&target).await.is_empty(), "status {}", status);
    }
  }

  #[tokio::test]
  async fn test_send_partial_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/send"))
      .respond_with(ResponseTemplate::new(200).set_body_string(BAD_RESPONSE))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/api/v1/send")).respond_with(ResponseTemplate::new(200).set_body_string(GOOD_RESPONSE)).mount(&server).await;
    let svc = from_url(&format!("wpush://{}/one/two", KEY)).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());
    // The second topic is still tried
    assert_eq!(received(&server).await.len(), 2);
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second topic fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/api/v1/send"))
      .and(body_partial_json(json!({"topic_code": "two"})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/api/v1/send")).respond_with(ResponseTemplate::new(200).set_body_string(GOOD_RESPONSE)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("wpush://{}/one/two?retry=1&wait=0", KEY)));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("t", "b"))).await.is_success());
    let sent: Vec<Value> = crate::notify::custom_json::requests_to(&server, "POST", "/api/v1/send")
      .await
      .iter()
      .map(|r| r.body_json::<Value>().unwrap()["topic_code"].clone())
      .collect();
    assert_eq!(sent, vec![json!("one"), json!("two"), json!("two")]);
  }
}
