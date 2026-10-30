use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::parse_list;
use crate::utils::parse::{ParsedUrl, unquote};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Delivery channels (`PUSHPLUS_CHANNELS`); the first is the default.
const PUSHPLUS_CHANNELS: &[&str] = &["wechat", "webhook", "cp", "mail", "sms"];

/// Service-level result codes returned in the JSON body.
fn response_code_str(code: i64) -> Option<&'static str> {
  Some(match code {
    200 => "Request succeeded.",
    900 => "System exception.",
    903 => "Sending failed.",
    905 => "Request parameter error.",
    907 => "Token does not exist.",
    908 => "User is blocked.",
    909 => "Content requires review before sending.",
    912 => "No available service package.",
    _ => return None,
  })
}

/// Tokens are 32 to 64 characters of `[a-z0-9_-]`.
fn is_token(token: &str) -> bool {
  (32..=64).contains(&token.len()) && token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// Topics (group codes) are 1 to 50 characters of `[a-z0-9_-]`.
fn is_topic(topic: &str) -> bool {
  (1..=50).contains(&topic.len()) && topic.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

pub struct PushPlus {
  token: String,
  channel: &'static str,
  topics: Vec<String>,
  webhook: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl PushPlus {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The registry turns https://www.pushplus.plus/send?token=... into
    // pushplus://{token}/?... Upstream unquotes the query values once more.
    let qsd = |k: &str| url.get(k).filter(|v| !v.is_empty()).map(unquote);

    let token = qsd("token").or_else(|| url.host.clone()).unwrap_or_default();
    if !is_token(&token) {
      tracing::warn!("The Pushplus token is invalid.");
      return None;
    }

    let mut targets = url.path_parts.clone();
    for key in ["to", "topic"] {
      // Upstream's URLBase.parse_list() unquotes each entry
      if let Some(value) = url.get(key).filter(|v| !v.is_empty()) {
        targets.extend(parse_list(&[value]).iter().map(|t| unquote(t)));
      }
    }

    // A user part names a webhook and selects the webhook channel
    let user = url.user.clone();
    let webhook = qsd("name").or_else(|| user.clone());
    let channel = qsd("channel").or_else(|| qsd("mode")).or_else(|| user.map(|_| "webhook".to_string()));

    let channel = match channel {
      Some(channel) => {
        let lower = channel.to_lowercase();
        let lower = if lower == "wecom" { "cp".to_string() } else { lower };
        let Some(found) = PUSHPLUS_CHANNELS.iter().copied().find(|c| *c == lower) else {
          tracing::warn!("The Pushplus channel ({}) is not valid.", channel);
          return None;
        };
        found
      }
      // The wecom:// schema selects the WeCom channel
      None if url.schema == "wecom" => "cp",
      None => PUSHPLUS_CHANNELS[0],
    };

    let mut topics = Vec::new();
    for target in parse_list(&targets) {
      if is_topic(&target) {
        topics.push(target);
      } else {
        tracing::warn!("Dropped invalid Pushplus topic: {}", target);
      }
    }

    Some(Self { token, channel, topics, webhook, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pushplus",
      service_url: Some("https://www.pushplus.plus/"),
      setup_url: Some("https://appriseit.com/services/pushplus/"),
      protocols: vec!["pushplus", "wecom"],
      description: "Send notifications via PushPlus.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for PushPlus {
  fn schemas(&self) -> &[&str] {
    &["pushplus", "wecom"]
  }
  fn service_name(&self) -> &str {
    "Pushplus"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Html
  }
  fn body_maxlen(&self) -> usize {
    20000
  }
  fn title_maxlen(&self) -> usize {
    200
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let template = match ctx.body_format {
      NotifyFormat::Html => "html",
      NotifyFormat::Markdown => "markdown",
      NotifyFormat::Text => "txt",
    };
    let url = format!("{}/send", service_base("https://www.pushplus.plus"));
    let client = build_client(self.verify_certificate)?;
    let topics: Vec<Option<&str>> = if self.topics.is_empty() { vec![None] } else { self.topics.iter().map(|t| Some(t.as_str())).collect() };

    let mut has_error = false;
    for topic in topics {
      // A retry skips topics that already accepted the message
      let key = topic.unwrap_or_default();
      if crate::delivery::is_delivered(key, false) {
        continue;
      }
      let mut payload = json!({
        "token": self.token,
        "title": if ctx.title.is_empty() { &ctx.body } else { &ctx.title },
        "content": ctx.body,
        "template": template,
        "channel": self.channel,
      });
      if let Some(topic) = topic {
        payload["topic"] = json!(topic);
      }
      if let Some(webhook) = self.webhook.as_ref().filter(|_| self.channel == "webhook") {
        payload["webhook"] = json!(webhook);
      }

      tracing::debug!("PushPlus POST URL: {} (cert_verify={})", url, self.verify_certificate);
      crate::notify::throttle().await;
      let resp =
        match client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(serde_json::to_vec(&payload)?).send().await {
          Ok(resp) => resp,
          Err(e) => {
            tracing::warn!("A Connection error occurred sending PushPlus notification.");
            tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
            has_error = true;
            continue;
          }
        };

      let status = resp.status();
      if status.as_u16() != 200 {
        let reason = status.canonical_reason().unwrap_or("");
        tracing::warn!("Failed to send PushPlus notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
        has_error = true;
        continue;
      }

      // PushPlus answers 200; the outcome is the code in the body
      let content: Option<Value> = resp.json().await.ok().filter(Value::is_object);
      let code = content.as_ref().and_then(|c| c.get("code")).and_then(Value::as_i64);
      if code != Some(200) {
        let msg = content.as_ref().and_then(|c| c.get("msg")).and_then(Value::as_str).unwrap_or("Unknown error");
        let error = code.and_then(response_code_str).unwrap_or(msg);
        tracing::warn!("Failed to send PushPlus notification: code={}: {}.", code.map(|c| c.to_string()).unwrap_or_else(|| "None".into()), error);
        has_error = true;
        continue;
      }

      tracing::info!("Sent PushPlus notification{}.", topic.map(|t| format!(" to topic {}", t)).unwrap_or_default());
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

  const GOOD_TOKEN: &str = "abc123def456ghi789jkl012mno345pq";

  /// Parse `url`, converting a native URL the way the registry does.
  fn pushplus(url: &str) -> PushPlus {
    let native = crate::notify::registry::NATIVE_URLS.iter().find_map(|n| (n.convert)(url));
    PushPlus::from_url(&ParsedUrl::parse(native.as_deref().unwrap_or(url)).unwrap()).unwrap()
  }

  fn good() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"code": 200, "msg": "ok", "data": "msgid"}))
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      format!("pushplus://{}", GOOD_TOKEN),
      format!("pushplus://?token={}", GOOD_TOKEN),
      format!("pushplus://{}", "a".repeat(64)),
      format!("pushplus://{}/mygroup", GOOD_TOKEN),
      format!("pushplus://{}/group1/group2", GOOD_TOKEN),
      format!("pushplus://{}/?topic=mygroup", GOOD_TOKEN),
      format!("pushplus://{}/?to=mygroup", GOOD_TOKEN),
      format!("pushplus://{}/bad!topic", GOOD_TOKEN),
      format!("pushplus://{}?channel=mail", GOOD_TOKEN),
      format!("pushplus://{}?channel=sms", GOOD_TOKEN),
      format!("pushplus://{}?channel=cp", GOOD_TOKEN),
      format!("pushplus://{}?channel=wecom", GOOD_TOKEN),
      format!("pushplus://{}?mode=mail", GOOD_TOKEN),
      format!("wecom://{}", GOOD_TOKEN),
      format!("pushplus://{}?channel=webhook&name=myhook", GOOD_TOKEN),
      format!("pushplus://myhook@{}", GOOD_TOKEN),
      format!("https://www.pushplus.plus/send?token={}", GOOD_TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "pushplus://".to_string(),
      "pushplus://short".to_string(),
      "pushplus://invalid!chars00000000000000000000000000000".to_string(),
      format!("pushplus://{}", "a".repeat(65)),
      format!("pushplus://{}?channel=invalid", GOOD_TOKEN),
      format!("https://www.pushplus.plus/other?token={}", GOOD_TOKEN),
      format!("https://pushplus.plus/send?token={}", GOOD_TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parsing() {
    let p = pushplus(&format!("pushplus://{}", GOOD_TOKEN));
    assert_eq!((p.channel, p.topics.len(), p.webhook.as_deref()), ("wechat", 0, None));
    assert_eq!(pushplus(&format!("wecom://{}", GOOD_TOKEN)).channel, "cp");
    assert_eq!(pushplus(&format!("wecom://{}?channel=mail", GOOD_TOKEN)).channel, "mail");
    assert_eq!(pushplus(&format!("pushplus://{}?channel=WeCom", GOOD_TOKEN)).channel, "cp");
    assert_eq!(pushplus(&format!("pushplus://{}?mode=sms&channel=mail", GOOD_TOKEN)).channel, "mail");

    let p = pushplus(&format!("pushplus://myhook@{}/g2/bad!t?to=g1,g2", GOOD_TOKEN));
    assert_eq!(p.channel, "webhook");
    assert_eq!(p.webhook.as_deref(), Some("myhook"));
    assert_eq!(p.topics, vec!["g1", "g2"]);

    // A user part does not override an explicit channel or name
    let p = pushplus(&format!("pushplus://user@{}?channel=mail&name=hook", GOOD_TOKEN));
    assert_eq!((p.channel, p.webhook.as_deref()), ("mail", Some("hook")));

    let p = pushplus(&format!("https://www.pushplus.plus/send?token={}&topic=grp", GOOD_TOKEN));
    assert_eq!(p.token, GOOD_TOKEN);
    assert_eq!(p.topics, vec!["grp"]);
  }

  #[tokio::test]
  async fn test_send_webhook_topics() {
    // Matches pycapture.py for pushplus://myhook@<token>/g1/bad!t?to=g0
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/send")).and(header("content-type", "application/json")).respond_with(good()).expect(2).mount(&server).await;
    let token = "a".repeat(32);
    let svc = from_url(&format!("pushplus://myhook@{}/g1/bad!t?to=g0", token)).unwrap();
    let ctx = NotifyContext { body: "B ü".into(), body_format: NotifyFormat::Html, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = crate::notify::custom_json::requests_to(&server, "POST", "/send").await;
    for (request, topic) in requests.iter().zip(["g0", "g1"]) {
      let body: Value = serde_json::from_slice(&request.body).unwrap();
      assert_eq!(
        body,
        json!({"token": token, "title": "B ü", "content": "B ü", "template": "html", "channel": "webhook", "topic": topic, "webhook": "myhook"})
      );
    }
  }

  #[tokio::test]
  async fn test_send_templates() {
    for (format, template) in [(NotifyFormat::Text, "txt"), (NotifyFormat::Markdown, "markdown"), (NotifyFormat::Html, "html")] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/send")).respond_with(good()).expect(1).mount(&server).await;
      let svc = from_url(&format!("pushplus://{}", GOOD_TOKEN)).unwrap();
      let ctx = NotifyContext { title: "T".into(), body: "B".into(), body_format: format, ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
      let body: Value = serde_json::from_slice(&crate::notify::custom_json::requests_to(&server, "POST", "/send").await[0].body).unwrap();
      assert_eq!(body, json!({"token": GOOD_TOKEN, "title": "T", "content": "B", "template": template, "channel": "wechat"}));
    }
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    for response in [
      ResponseTemplate::new(200).set_body_json(json!({"code": 907, "msg": "Token does not exist."})),
      ResponseTemplate::new(200).set_body_json(json!({"code": 999, "msg": "Other"})),
      ResponseTemplate::new(200).set_body_string("{bad json"),
      ResponseTemplate::new(200),
      ResponseTemplate::new(500),
      ResponseTemplate::new(999),
    ] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/send")).respond_with(response).expect(1).mount(&server).await;
      let svc = from_url(&format!("pushplus://{}", GOOD_TOKEN)).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }
  }

  #[test]
  fn test_static_details() {
    let details = PushPlus::static_details();
    assert_eq!(details.service_name, "Pushplus");
    assert_eq!(details.protocols, vec!["pushplus", "wecom"]);
    let svc = from_url(&format!("pushplus://{}", GOOD_TOKEN)).unwrap();
    assert_eq!(svc.notify_format(), NotifyFormat::Html);
    assert_eq!((svc.body_maxlen(), svc.title_maxlen()), (20000, 200));
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second topic fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/send"))
      .and(body_partial_json(json!({"topic": "g2"})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/send")).respond_with(good()).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("pushplus://{}/g1/g2?retry=1&wait=0", GOOD_TOKEN)));
    assert!(
      crate::notify::with_test_base(server.uri(), a.notify(&NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() })).await.is_success()
    );
    let sent: Vec<Value> =
      crate::notify::custom_json::requests_to(&server, "POST", "/send").await.iter().map(|r| r.body_json::<Value>().unwrap()["topic"].clone()).collect();
    assert_eq!(sent, vec![json!("g1"), json!("g2"), json!("g2")]);
  }
}
