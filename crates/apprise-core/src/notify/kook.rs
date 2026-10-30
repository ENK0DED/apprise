//! Kook (KOOK, formerly Kaihei) bots and incoming webhooks (upstream
//! `NotifyKook`).
//!
//! Bot mode posts to channels and direct messages:
//! `kook://{bot_token}/{channel_id}/@{user_id}`. Webhook mode posts to one
//! incoming webhook: `kook://{webhook_key}?mode=webhook`, also written
//! `https://www.kookapp.cn/api/v3/incoming/{webhook_key}`.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::conversion::commonmark_headings_to_bold;
use crate::delivery::{is_delivered, mark_delivered, recall, remember};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_list, unquote};

/// Kook message and asset types.
const TYPE_TEXT: u8 = 1;
const TYPE_IMAGE: u8 = 2;
const TYPE_FILE: u8 = 4;
const TYPE_KMARKDOWN: u8 = 9;

/// The default API host (upstream `KOOK_API_URL` is this plus `/api/v3`).
const API_BASE: &str = "https://www.kookapp.cn";

/// Upstream `KookMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  /// The Bot API: channels and direct messages, with attachments.
  Bot,
  /// One incoming webhook, without attachments.
  Webhook,
}

impl Mode {
  fn as_str(self) -> &'static str {
    match self {
      Self::Bot => "bot",
      Self::Webhook => "webhook",
    }
  }
}

pub struct Kook {
  token: String,
  mode: Mode,
  channels: Vec<String>,
  dm_users: Vec<String>,
  /// KMarkdown unless `?format=` asks for something else.
  markdown: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// A channel or user ID (upstream `IS_TARGET_ID`, `^\d{1,20}$`).
fn is_target_id(value: &str) -> bool {
  (1..=20).contains(&value.len()) && value.chars().all(|c| c.is_ascii_digit())
}

/// Upstream `KOOK_HTTP_ERROR_MAP`, else the standard reason.
fn status_reason(status: u16) -> &'static str {
  match status {
    400 => "Bad Request - Malformed or missing parameters.",
    401 => "Unauthorized - Invalid token.",
    403 => "Forbidden - Insufficient permissions.",
    404 => "Not Found - Invalid channel or endpoint.",
    429 => "Too many consecutive requests were made.",
    500 => "Internal Server Error.",
    _ => reqwest::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or(""),
  }
}

/// The JSON object of a response body; anything else reads as empty.
fn response_json(body: &[u8]) -> serde_json::Map<String, Value> {
  match serde_json::from_slice(body) {
    Ok(Value::Object(map)) => map,
    _ => serde_json::Map::new(),
  }
}

/// Upstream `content.get("code", 0) != 0`: the API error code of a reply,
/// if it reports one.
fn api_error(content: &serde_json::Map<String, Value>) -> Option<String> {
  match content.get("code") {
    None | Some(Value::Bool(false)) => None,
    Some(Value::Number(n)) if n.as_f64() == Some(0.0) => None,
    Some(code) => Some(code.to_string()),
  }
}

impl Kook {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The host holds the bot token or webhook key; ?token= overrides it
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => unquote(t),
      None => url.host.as_deref().map(unquote).unwrap_or_default(),
    };
    if token.is_empty() {
      tracing::warn!("A Kook token must be specified.");
      return None;
    }

    // A prefix of a mode is enough ("web" is webhook)
    let mode = match url.get("mode").filter(|m| !m.is_empty()) {
      Some(m) => {
        let lower = unquote(m).to_lowercase();
        match [Mode::Bot, Mode::Webhook].into_iter().find(|mode| mode.as_str().starts_with(&lower)) {
          Some(mode) => mode,
          None => {
            tracing::warn!("The Kook mode specified ({}) is not valid.", m);
            return None;
          }
        }
      }
      None => Mode::Bot,
    };

    let mut targets = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.extend(parse_list([to], true, true));
    }

    let mut channels = Vec::new();
    let mut dm_users = Vec::new();
    for target in parse_list(targets, true, true) {
      if let Some(user_id) = target.strip_prefix('@') {
        if is_target_id(user_id) {
          dm_users.push(user_id.to_string());
        } else {
          tracing::warn!("Dropping invalid Kook DM user ID: {}", target);
        }
      } else {
        // The # of a channel is optional
        let channel_id = target.strip_prefix('#').unwrap_or(&target);
        if is_target_id(channel_id) {
          channels.push(channel_id.to_string());
        } else {
          tracing::warn!("Dropping invalid Kook channel ID: {}", target);
        }
      }
    }

    let markdown = url.get("format").and_then(|f| f.parse::<NotifyFormat>().ok()).is_none_or(|f| f == NotifyFormat::Markdown);

    Some(Self { token, mode, channels, dm_users, markdown, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Kook",
      service_url: Some("https://www.kookapp.cn/"),
      setup_url: Some("https://appriseit.com/services/kook/"),
      protocols: vec!["kook"],
      description: "Send messages to Kook channels and users through a bot or an incoming webhook.",
      attachment_support: true,
    }
  }

  /// The message type: KMarkdown for Markdown, plain text otherwise.
  fn message_type(&self) -> u8 {
    if self.markdown { TYPE_KMARKDOWN } else { TYPE_TEXT }
  }

  fn api_url(path: &str) -> String {
    format!("{}/api/v3{}", service_base(API_BASE), path)
  }

  /// Post one JSON message; `what` names it in the logs.
  async fn post(&self, client: &reqwest::Client, endpoint: &str, payload: &Value, bot: bool, what: &str) -> bool {
    tracing::debug!("Kook POST URL: {} (cert_verify={})", if bot { endpoint } else { "webhook" }, self.verify_certificate);
    tracing::debug!("Kook Payload: {}", payload);

    throttle().await;
    let mut req = client.post(endpoint).header("User-Agent", APP_ID).header("Content-Type", "application/json");
    if bot {
      req = req.header("Authorization", format!("Bot {}", self.token));
    }
    let resp = match req.body(payload.to_string()).send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending {}.", what);
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return false;
      }
    };

    let status = resp.status().as_u16();
    let body = resp.bytes().await.unwrap_or_default();
    if status != 200 {
      let reason = status_reason(status);
      tracing::warn!("Failed to send {}: {}{}error={}.", what, reason, if reason.is_empty() { "" } else { ", " }, status);
      tracing::debug!("Response Details:\r\n{}", String::from_utf8_lossy(&body).chars().take(2000).collect::<String>());
      return false;
    }
    let content = response_json(&body);
    if let Some(code) = api_error(&content) {
      let message = content.get("message").and_then(Value::as_str).unwrap_or("Unknown");
      tracing::warn!("Failed to send {}: code={}, message={}.", what, code, message);
      return false;
    }
    true
  }

  /// Upload an attachment to the Kook CDN (upstream `_upload()`) and
  /// return its URL.
  async fn upload(&self, client: &reqwest::Client, attachment: &Attachment) -> Option<String> {
    let url = Self::api_url("/asset/create");
    tracing::debug!("Kook CDN Upload URL: {} (cert_verify={})", url, self.verify_certificate);

    let name = if attachment.name.is_empty() { "attachment.dat".to_string() } else { attachment.name.clone() };
    let part = || reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(name.clone());
    let part = part().mime_str(&attachment.mime_type).unwrap_or_else(|_| part());

    throttle().await;
    let resp = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Authorization", format!("Bot {}", self.token))
      .multipart(reqwest::multipart::Form::new().part("file", part))
      .send()
      .await;
    let resp = match resp {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred uploading Kook attachment.");
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return None;
      }
    };

    let status = resp.status().as_u16();
    let body = resp.bytes().await.unwrap_or_default();
    if status != 200 {
      let reason = status_reason(status);
      tracing::warn!("Failed to upload Kook attachment: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status);
      tracing::debug!("Response Details:\r\n{}", String::from_utf8_lossy(&body).chars().take(2000).collect::<String>());
      return None;
    }
    let cdn_url = response_json(&body).get("data").and_then(|d| d.get("url")).and_then(Value::as_str).filter(|u| !u.is_empty()).map(str::to_string);
    if cdn_url.is_none() {
      tracing::warn!("Kook CDN upload returned no URL.");
    }
    cdn_url
  }

  async fn send_bot(&self, client: &reqwest::Client, ctx: &NotifyContext) -> bool {
    if self.channels.is_empty() && self.dm_users.is_empty() {
      tracing::warn!("Kook: no targets specified; nothing to notify.");
      return false;
    }

    // Channels and direct messages use different endpoints
    let message_url = Self::api_url("/message/create");
    let dm_url = Self::api_url("/direct-message/create");
    let targets: Vec<(&str, &str, &str)> = self
      .channels
      .iter()
      .map(|c| ("channel", message_url.as_str(), c.as_str()))
      .chain(self.dm_users.iter().map(|u| ("dm", dm_url.as_str(), u.as_str())))
      .collect();

    let mut has_error = false;
    for (kind, endpoint, target_id) in &targets {
      // Skip a target that already accepted this message
      let key = format!("body:{kind}:{target_id}");
      if is_delivered(&key, false) {
        continue;
      }
      let payload = json!({ "type": self.message_type(), "target_id": target_id, "content": ctx.body });
      if !self.post(client, endpoint, &payload, true, &format!("Kook Bot notification to {target_id}")).await {
        has_error = true;
        continue;
      }
      tracing::info!("Sent Kook Bot notification to {}.", target_id);
      mark_delivered(&key, false);
    }

    // Attachments follow the text, once every target has it
    if has_error {
      return false;
    }
    for (attachment_no, attachment) in ctx.attachments.iter().enumerate() {
      // Reuse the CDN URL from an earlier attempt
      let cdn_key = format!("cdn:{attachment_no}");
      let cdn_url = match recall(&cdn_key, false).and_then(|v| v.as_str().map(str::to_string)) {
        Some(url) => url,
        None => {
          let Some(url) = self.upload(client, attachment).await else { return false };
          remember(&cdn_key, json!(url), false);
          url
        }
      };

      let attach_type = if attachment.mime_type.starts_with("image/") { TYPE_IMAGE } else { TYPE_FILE };
      for (kind, endpoint, target_id) in &targets {
        let key = format!("attachment:{attachment_no}:{kind}:{target_id}");
        if is_delivered(&key, false) {
          continue;
        }
        let payload = json!({ "type": attach_type, "target_id": target_id, "content": cdn_url });
        if !self.post(client, endpoint, &payload, true, &format!("Kook attachment to {target_id}")).await {
          has_error = true;
          continue;
        }
        mark_delivered(&key, false);
      }
    }
    !has_error
  }
}

#[async_trait]
impl Notify for Kook {
  fn schemas(&self) -> &[&str] {
    &["kook"]
  }
  fn service_name(&self) -> &str {
    "Kook"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn attachment_support(&self) -> bool {
    // Uploading needs the bot's authorization
    self.mode == Mode::Bot
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn title_maxlen(&self) -> usize {
    // The title is placed in the body
    0
  }
  fn body_maxlen(&self) -> usize {
    5000
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    // KMarkdown has no headings, so they are shown in bold
    if *body_format != NotifyFormat::Markdown {
      return body.to_string();
    }
    commonmark_headings_to_bold(body)
  }
  fn has_dialect(&self) -> bool {
    true
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["kook".into(), self.mode.as_str().into(), self.token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let ok = match self.mode {
      Mode::Webhook => {
        let url = Self::api_url(&format!("/incoming/{}", self.token));
        let payload = json!({ "type": self.message_type(), "content": ctx.body });
        let ok = self.post(&client, &url, &payload, false, "Kook webhook notification").await;
        if ok {
          tracing::info!("Sent Kook webhook notification.");
        }
        ok
      }
      Mode::Bot => self.send_bot(&client, ctx).await,
    };
    Ok(ok)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Apprise;
  use crate::notify::registry::{from_url, parse_service_url};
  use crate::notify::with_test_base;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  const BOT_TOKEN: &str = "MTQ3OTE5MTA3MzQ1ODE4OTQ0ABC123XYZ";
  const CHANNEL_ID: &str = "1234567890";
  const CHANNEL_ID2: &str = "9876543210";
  const USER_ID: &str = "5555555555";
  const WEBHOOK_KEY: &str = "abc123webhookkey";
  const MESSAGE: &str = "/api/v3/message/create";
  const DM: &str = "/api/v3/direct-message/create";
  const ASSET: &str = "/api/v3/asset/create";
  const CDN: &str = "https://img.kookapp.cn/assets/test.png";

  fn kook(url: &str) -> Kook {
    Kook::from_url(&parse_service_url(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn png() -> Attachment {
    Attachment { name: "apprise-test.png".into(), data: b"PNG".to_vec(), mime_type: "image/png".into() }
  }

  /// Answer every POST to `p` with `status` and `body`.
  async fn reply(server: &MockServer, p: &str, status: u16, body: &str) {
    Mock::given(method("POST")).and(path(p)).respond_with(ResponseTemplate::new(status).set_body_string(body)).mount(server).await;
  }

  /// The POST requests made, in order, as (path, JSON body or null).
  async fn posts(server: &MockServer) -> Vec<(String, Value)> {
    server
      .received_requests()
      .await
      .unwrap_or_default()
      .into_iter()
      .filter(|r: &Request| r.method.as_str() == "POST" && r.url.path().starts_with("/api/v3/"))
      .map(|r| (r.url.path().to_string(), serde_json::from_slice(&r.body).unwrap_or(Value::Null)))
      .collect()
  }

  async fn send(svc: &Kook, c: &NotifyContext, server: &MockServer) -> bool {
    with_test_base(server.uri(), svc.send(c)).await.unwrap()
  }

  #[test]
  fn test_urls() {
    for url in ["kook://".to_string(), format!("kook://{BOT_TOKEN}/?mode=invalid"), "kook://?token=".to_string()] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
    for url in [
      format!("kook://{WEBHOOK_KEY}/?mode=webhook"),
      format!("kook://ignored/?token={WEBHOOK_KEY}&mode=webhook"),
      format!("kook://{BOT_TOKEN}/{CHANNEL_ID}"),
      format!("kook://{BOT_TOKEN}/{CHANNEL_ID}/{CHANNEL_ID2}"),
      format!("kook://{BOT_TOKEN}/@{USER_ID}"),
      format!("kook://{BOT_TOKEN}/{CHANNEL_ID}/@{USER_ID}"),
      format!("kook://{BOT_TOKEN}"),
      format!("kook://{BOT_TOKEN}/?to={CHANNEL_ID}"),
      format!("kook://{BOT_TOKEN}/{CHANNEL_ID}/?format=text"),
      "https://www.kookapp.cn/api/v3/incoming/nativekey123".to_string(),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }

    let k = kook(&format!("kook://ignored/?token={WEBHOOK_KEY}&mode=web"));
    assert_eq!((k.token.as_str(), k.mode), (WEBHOOK_KEY, Mode::Webhook));
    for native in ["https://www.kookapp.cn/api/v3/incoming/mywebhookkey", "https://www.kookapp.cn/api/v3/incoming/mywebhookkey/"] {
      let k = kook(native);
      assert_eq!((k.token.as_str(), k.mode), ("mywebhookkey", Mode::Webhook));
    }
    assert_eq!(kook("https://www.kookapp.cn/api/v3/incoming/abc=+-_123").token, "abc=+-_123");
  }

  #[test]
  fn test_targets() {
    let k = kook(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}/@{USER_ID}/#{CHANNEL_ID2}/notanumber/@notanumber"));
    assert_eq!(k.channels, vec![CHANNEL_ID2, CHANNEL_ID]);
    assert_eq!(k.dm_users, vec![USER_ID]);
    // Targets are sorted and made unique before the # is removed, as
    // upstream does, so "#1" and "1" are both kept
    let k = kook("kook://bottoken123/#1111111111?to=2222222222,@3333333333,1111111111,2222222222");
    assert_eq!(k.channels, vec!["1111111111", "1111111111", "2222222222"]);
    assert_eq!(k.dm_users, vec!["3333333333"]);
  }

  #[test]
  fn test_details() {
    let k = kook(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}"));
    assert_eq!(k.url_identifier(), Some(vec!["kook".into(), "bot".into(), BOT_TOKEN.into()]));
    assert!(k.attachment_support());
    assert_eq!(k.message_type(), TYPE_KMARKDOWN);
    assert_eq!((k.body_maxlen(), k.title_maxlen(), k.request_rate_per_sec()), (5000, 0, 5.5));
    assert_eq!(k.dialect_convert("# Heading\ntext", &NotifyFormat::Markdown), "**Heading**\ntext");
    assert_eq!(k.dialect_convert("# Heading", &NotifyFormat::Text), "# Heading");
    let w = kook(&format!("kook://{WEBHOOK_KEY}/?mode=webhook"));
    assert_eq!(w.url_identifier(), Some(vec!["kook".into(), "webhook".into(), WEBHOOK_KEY.into()]));
    assert!(!w.attachment_support());
    assert_eq!(kook(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}/?format=text")).message_type(), TYPE_TEXT);
  }

  #[tokio::test]
  async fn test_webhook_send() {
    // Matches pycapture.py and the apprise-go kook golden
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v3/incoming/webhookkey456"))
      .and(header("content-type", "application/json"))
      .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"code": 0, "message": "ok"}"#))
      .expect(1)
      .mount(&server)
      .await;
    let svc = kook("kook://webhookkey456?mode=webhook");
    assert!(send(&svc, &ctx("hook title\r\nhook body"), &server).await);
    let reqs = server.received_requests().await.unwrap();
    let r = reqs.iter().find(|r| r.url.path() == "/api/v3/incoming/webhookkey456").unwrap();
    assert!(r.headers.get("authorization").is_none());
    assert_eq!(serde_json::from_slice::<Value>(&r.body).unwrap(), json!({ "type": 9, "content": "hook title\r\nhook body" }));

    // HTTP and API errors fail; unparseable JSON counts as success
    for (status, body, ok) in
      [(500, "", false), (999, "", false), (200, r#"{"code": 40001}"#, false), (200, r#"{"code": null}"#, false), (200, "not-json", true)]
    {
      let server = MockServer::start().await;
      reply(&server, "/api/v3/incoming/webhookkey456", status, body).await;
      assert_eq!(send(&svc, &ctx("hello"), &server).await, ok, "{} {}", status, body);
    }
    assert!(!with_test_base("http://127.0.0.1:1", svc.send(&ctx("hello"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_bot_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(header("authorization", "Bot bottoken123"))
      .and(header("content-type", "application/json"))
      .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"code":0}"#))
      .mount(&server)
      .await;
    let svc = kook("kook://bottoken123/#1111111111?to=2222222222,@3333333333");
    assert!(send(&svc, &ctx("b"), &server).await);
    assert_eq!(
      posts(&server).await,
      vec![
        (MESSAGE.to_string(), json!({ "type": 9, "target_id": "1111111111", "content": "b" })),
        (MESSAGE.to_string(), json!({ "type": 9, "target_id": "2222222222", "content": "b" })),
        (DM.to_string(), json!({ "type": 9, "target_id": "3333333333", "content": "b" })),
      ]
    );

    // ?format=text sends plain text
    let server = MockServer::start().await;
    reply(&server, MESSAGE, 200, r#"{"code":0}"#).await;
    assert!(send(&kook(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}/?format=text")), &ctx("hello"), &server).await);
    assert_eq!(posts(&server).await[0].1["type"], 1);

    // No targets fails without a request
    let server = MockServer::start().await;
    reply(&server, MESSAGE, 200, r#"{"code":0}"#).await;
    assert!(!send(&kook(&format!("kook://{BOT_TOKEN}")), &ctx("hello"), &server).await);
    assert!(posts(&server).await.is_empty());

    // One failing target fails the send, but the others are still tried
    for (status, body) in [(500, ""), (999, ""), (200, r#"{"code": 40001, "message": "error"}"#)] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path(MESSAGE))
        .and(|r: &Request| String::from_utf8_lossy(&r.body).contains(CHANNEL_ID))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
      reply(&server, MESSAGE, 200, r#"{"code":0}"#).await;
      assert!(!send(&kook(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}/{CHANNEL_ID2}")), &ctx("hello"), &server).await);
      assert_eq!(posts(&server).await.len(), 2);
    }
    let svc = kook(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}"));
    assert!(!with_test_base("http://127.0.0.1:1", svc.send(&ctx("hello"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_attachments() {
    let ok_cdn = format!(r#"{{"code": 0, "data": {{"url": "{CDN}"}}}}"#);
    for (attachment, attach_type) in [(png(), 2), (Attachment { name: "a.mp4".into(), data: b"MP4".to_vec(), mime_type: "video/mp4".into() }, 4)] {
      let server = MockServer::start().await;
      reply(&server, MESSAGE, 200, r#"{"code":0}"#).await;
      reply(&server, DM, 200, r#"{"code":0}"#).await;
      reply(&server, ASSET, 200, &ok_cdn).await;
      let svc = kook(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}/@{USER_ID}"));
      let mut c = ctx("hello");
      c.attachments = vec![attachment.clone()];
      assert!(send(&svc, &c, &server).await);
      let reqs = posts(&server).await;
      let paths: Vec<&str> = reqs.iter().map(|(p, _)| p.as_str()).collect();
      assert_eq!(paths, vec![MESSAGE, DM, ASSET, MESSAGE, DM]);
      assert_eq!(reqs[3].1, json!({ "type": attach_type, "target_id": CHANNEL_ID, "content": CDN }));
      assert_eq!(reqs[4].1, json!({ "type": attach_type, "target_id": USER_ID, "content": CDN }));

      // The upload is multipart with a `file` part and the bot's authorization
      let all = server.received_requests().await.unwrap();
      let upload = all.iter().find(|r| r.url.path() == ASSET).unwrap();
      assert_eq!(upload.headers.get("authorization").unwrap(), &format!("Bot {BOT_TOKEN}"));
      let text = String::from_utf8_lossy(&upload.body);
      assert!(text.contains(&format!("name=\"file\"; filename=\"{}\"\r\nContent-Type: {}", attachment.name, attachment.mime_type)), "{}", text);
    }

    // Upload failures: HTTP error, no URL, invalid JSON
    for (status, body) in [(500, ""), (200, r#"{"code": 0, "data": {}}"#), (200, "not-json")] {
      let server = MockServer::start().await;
      reply(&server, MESSAGE, 200, r#"{"code":0}"#).await;
      reply(&server, ASSET, status, body).await;
      let mut c = ctx("hello");
      c.attachments = vec![png()];
      assert!(!send(&kook(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}")), &c, &server).await, "{} {}", status, body);
      assert_eq!(posts(&server).await.len(), 2);
    }

    // Posting the attachment message fails
    for (status, body) in [(500, ""), (200, r#"{"code": 40001, "message": "forbidden"}"#)] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path(MESSAGE))
        .and(|r: &Request| String::from_utf8_lossy(&r.body).contains(CDN))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
      reply(&server, MESSAGE, 200, r#"{"code":0}"#).await;
      reply(&server, ASSET, 200, &ok_cdn).await;
      let mut c = ctx("hello");
      c.attachments = vec![png()];
      assert!(!send(&kook(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}")), &c, &server).await);
    }

    // A failed text message skips the attachments
    let server = MockServer::start().await;
    reply(&server, MESSAGE, 500, "").await;
    reply(&server, ASSET, 200, &ok_cdn).await;
    let mut c = ctx("hello");
    c.attachments = vec![png()];
    assert!(!send(&kook(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}")), &c, &server).await);
    assert_eq!(posts(&server).await.len(), 1);
  }

  #[tokio::test]
  async fn test_retry_reuses_upload() {
    // The attachment message fails once; the retry neither re-sends the
    // text nor uploads the file again
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(MESSAGE))
      .and(|r: &Request| String::from_utf8_lossy(&r.body).contains(CDN))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    reply(&server, MESSAGE, 200, r#"{"code":0}"#).await;
    reply(&server, ASSET, 200, &format!(r#"{{"code": 0, "data": {{"url": "{CDN}"}}}}"#)).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("kook://{BOT_TOKEN}/{CHANNEL_ID}?retry=1&wait=0")));
    let mut c = ctx("hello");
    c.attachments = vec![png()];
    assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success());
    let paths: Vec<String> = posts(&server).await.into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths, vec![MESSAGE, ASSET, MESSAGE, MESSAGE]);
  }

  #[tokio::test]
  async fn test_formats() {
    let notify = |url: String, title: &'static str, body: &'static str, format: NotifyFormat| async move {
      let server = MockServer::start().await;
      reply(&server, MESSAGE, 200, r#"{"code":0}"#).await;
      let mut a = Apprise::new();
      assert!(a.add(&url));
      let c = NotifyContext { title: title.into(), body: body.into(), body_format: format, ..Default::default() };
      assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success());
      posts(&server).await.into_iter().map(|(_, v)| v["content"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    };
    // pycapture.py and the apprise-go kook goldens
    let url = format!("kook://{BOT_TOKEN}/{CHANNEL_ID}");
    assert_eq!(notify(url.clone(), "Title", "# Heading\n**Bold**", NotifyFormat::Markdown).await, vec!["**Title**\n**Heading**\n**Bold**"]);
    assert_eq!(
      notify(url.clone(), "over", "<b>bold</b> and <a href='https://example.com'>link</a> plus <code>code</code>.", NotifyFormat::Html).await,
      vec!["**over**\n**bold** and [link](<https://example.com>) plus `code`."]
    );
    assert_eq!(notify(format!("{url}?format=text"), "t", "plain body", NotifyFormat::Text).await, vec!["t\r\nplain body"]);
  }
}
