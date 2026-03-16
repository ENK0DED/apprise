use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;

#[allow(dead_code)]
pub struct Telegram {
  bot_token: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
  parse_mode: String,
  silent: bool,
  /// Override the API base URL (for testing). When `None`, uses the default
  /// Telegram API endpoint.
  api_base_override: Option<String>,
}

impl Telegram {
  const API_BASE: &'static str = "https://api.telegram.org/bot";

  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Bot token can be in the host (123456789:abcdefg_hijklmnop) when
    // the fallback parser handles the colon, OR split as user:password
    // by the url crate when there's an @ sign (e.g., botname@token).
    let bot_token = if let Some(ref h) = url.host {
      if h.contains(':') {
        // Fallback parser kept the full "id:token" as host
        h.clone()
      } else if let Some(ref user) = url.user {
        if user.chars().all(|c| c.is_ascii_digit()) {
          // user = bot_id (numeric), password = bot_token part
          // This shouldn't normally happen, but handle it
          if let Some(ref pass) = url.password { format!("{}:{}", user, pass) } else { h.clone() }
        } else {
          // user is a bot name (like "bottest"), host is the token
          h.clone()
        }
      } else {
        h.clone()
      }
    } else {
      return None;
    };

    if bot_token.is_empty() {
      return None;
    }

    // Validate bot token format: should be digits:alphanumeric
    if bot_token.contains(':') {
      let parts: Vec<&str> = bot_token.splitn(2, ':').collect();
      if parts.len() != 2 {
        return None;
      }
      // First part should be numeric (bot ID)
      if !parts[0].chars().all(|c| c.is_ascii_digit()) {
        return None;
      }
      // Second part should be non-empty
      if parts[1].is_empty() {
        return None;
      }
    }

    let mut targets: Vec<String> = url.path_parts.clone();
    // Support ?to= query param
    if let Some(to) = url.get("to") {
      targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }

    // Validate topic/thread if provided
    if let Some(topic) = url.get("topic").or_else(|| url.get("thread")) {
      if topic.parse::<i64>().is_err() {
        return None;
      }
    }

    // Validate content param if provided
    if let Some(content) = url.get("content") {
      match content.to_lowercase().as_str() {
        "before" | "after" | "" => {}
        _ => return None,
      }
    }

    let parse_mode = url.get("format").unwrap_or("html").to_string();
    let silent = url.get("silent").map(crate::utils::parse::parse_bool).unwrap_or(false);
    Some(Self { bot_token, targets, verify_certificate: url.verify_certificate(), tags: url.tags(), parse_mode, silent, api_base_override: None })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Telegram",
      service_url: Some("https://telegram.org"),
      setup_url: Some("https://core.telegram.org/bots"),
      protocols: vec!["tgram"],
      description: "Send Telegram messages via bot API.",
      attachment_support: true,
    }
  }

  /// Return the effective API base URL (with trailing `/bot{token}`).
  fn api_base(&self) -> String {
    match &self.api_base_override {
      Some(base) => format!("{}/bot{}", base.trim_end_matches('/'), self.bot_token),
      None => format!("{}{}", Self::API_BASE, self.bot_token),
    }
  }

  /// Determine the Telegram API method and form field name for a given MIME type
  fn endpoint_for_mime(mime: &str) -> (&'static str, &'static str) {
    let m = mime.to_lowercase();
    if m == "image/gif" || m.starts_with("video/h264") {
      ("sendAnimation", "animation")
    } else if m.starts_with("image/") {
      ("sendPhoto", "photo")
    } else if m == "video/mp4" {
      ("sendVideo", "video")
    } else if m == "audio/ogg" || m == "application/ogg" {
      ("sendVoice", "voice")
    } else if m.starts_with("audio/") {
      ("sendAudio", "audio")
    } else {
      ("sendDocument", "document")
    }
  }
}

#[async_trait]
impl Notify for Telegram {
  fn schemas(&self) -> &[&str] {
    &["tgram"]
  }
  fn service_name(&self) -> &str {
    "Telegram"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn attachment_support(&self) -> bool {
    true
  }
  fn notify_format(&self) -> crate::types::NotifyFormat {
    crate::types::NotifyFormat::Html
  }
  fn body_maxlen(&self) -> usize {
    4096
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      return Err(NotifyError::MissingParam("chat_id".into()));
    }
    let client = build_client(self.verify_certificate)?;
    let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("<b>{}</b>\n{}", ctx.title, ctx.body) };

    let mut all_ok = true;
    for target in &self.targets {
      // Always send text message first
      let url = format!("{}/sendMessage", self.api_base());
      let payload = json!({
          "chat_id": target,
          "text": text,
          "parse_mode": "HTML",
          "disable_notification": self.silent,
      });
      let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
      if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("Telegram send to {} failed: {}", target, body);
        all_ok = false;
      }

      // Upload attachments using the appropriate endpoint for the MIME type
      for attach in &ctx.attachments {
        let (method, field_name) = Self::endpoint_for_mime(&attach.mime_type);
        let attach_url = format!("{}/{}", self.api_base(), method);
        let part = reqwest::multipart::Part::bytes(attach.data.clone())
          .file_name(attach.name.clone())
          .mime_str(&attach.mime_type)
          .unwrap_or_else(|_| reqwest::multipart::Part::bytes(attach.data.clone()).file_name(attach.name.clone()));
        let form = reqwest::multipart::Form::new().text("chat_id", target.clone()).part(field_name, part);
        let resp = client.post(&attach_url).multipart(form).send().await;
        if let Ok(r) = resp {
          if !r.status().is_success() {
            all_ok = false;
          }
        } else {
          all_ok = false;
        }
      }
    }
    Ok(all_ok)
  }
}

#[cfg(test)]
mod tests {
  use crate::notify::registry::from_url;

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/",
      "tgram://123456789:abcdefg_hijklmnop/id1/id2/",
      "tgram://123456789:abcdefg_hijklmnop/?to=id1,id2",
      "tgram://123456789:abcdefg_hijklmnop/id1/id2/23423/-30/",
      "tgram://bottest@123456789:abcdefg_hijklmnop/lead2gold/",
      "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?topic=12345",
      "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?thread=12345",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?image=Yes",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=invalid",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=markdown",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=markdown&mdv=v1",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=markdown&mdv=v2",
      "tgram://123456789:abcdefg_hijklmnop/l2g/?format=markdown&mdv=bad",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=html",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=text",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?silent=yes",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?silent=no",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?preview=yes",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?preview=no",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "tgram://",
      "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?topic=invalid",
      "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?content=invalid",
      "tgram://alpha:abcdefg_hijklmnop/lead2gold/",
      "tgram://:@/",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  // ── Behavioral tests using wiremock ──────────────────────────────────

  use super::*;
  use crate::asset::AppriseAsset;
  use crate::notify::{Attachment, Notify, NotifyContext};
  use crate::types::{NotifyFormat, NotifyType};
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const TEST_BOT_TOKEN: &str = "123456789:abcdefg_hijklmnop";

  /// Helper: build a NotifyContext with sensible defaults.
  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext {
      body: body.to_string(),
      title: title.to_string(),
      notify_type: NotifyType::Info,
      body_format: NotifyFormat::Text,
      attachments: vec![],
      interpret_escapes: false,
      interpret_emojis: false,
      tags: vec![],
      asset: AppriseAsset::default(),
    }
  }

  /// Helper: create a Telegram instance pointing at the mock server.
  fn tg_for_mock(server: &MockServer, targets: &[&str], silent: bool) -> Telegram {
    let base = format!("http://{}", server.address());
    Telegram {
      bot_token: TEST_BOT_TOKEN.to_string(),
      targets: targets.iter().map(|s| s.to_string()).collect(),
      verify_certificate: false,
      tags: vec![],
      parse_mode: "html".to_string(),
      silent,
      api_base_override: Some(base),
    }
  }

  // ── 1. sendMessage payload correctness ───────────────────────────────

  #[tokio::test]
  async fn test_send_message_correct_chat_id_and_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let result = tg.send(&ctx("hello", "world")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    // Verify the request payload
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["chat_id"], "lead2gold");
    assert_eq!(body["text"], "<b>hello</b>\nworld");
    assert_eq!(body["parse_mode"], "HTML");
    assert_eq!(body["disable_notification"], false);
  }

  #[tokio::test]
  async fn test_send_message_body_only_no_title() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let result = tg.send(&ctx("", "body only")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    // When title is empty, text should be the body only (no <b> wrapper)
    assert_eq!(body["text"], "body only");
  }

  // ── 2. Bot token in URL path ─────────────────────────────────────────

  #[tokio::test]
  async fn test_bot_token_in_url_path() {
    let server = MockServer::start().await;
    // The mock expects the bot token to appear in the URL path
    let expected_path = format!("/bot{}/sendMessage", TEST_BOT_TOKEN);
    Mock::given(method("POST")).and(path(&expected_path)).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let tg = tg_for_mock(&server, &["12345"], false);
    let result = tg.send(&ctx("", "test")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 3. Silent mode (disable_notification) ────────────────────────────

  #[tokio::test]
  async fn test_silent_mode_sends_disable_notification_true() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], true);
    let result = tg.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["disable_notification"], true);
  }

  #[tokio::test]
  async fn test_non_silent_mode_sends_disable_notification_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let result = tg.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["disable_notification"], false);
  }

  #[tokio::test]
  async fn test_silent_mode_from_url_parse() {
    let parsed = ParsedUrl::parse("tgram://123456789:abcdefg_hijklmnop/lead2gold/?silent=yes").unwrap();
    let tg = Telegram::from_url(&parsed).unwrap();
    assert!(tg.silent);

    let parsed = ParsedUrl::parse("tgram://123456789:abcdefg_hijklmnop/lead2gold/?silent=no").unwrap();
    let tg = Telegram::from_url(&parsed).unwrap();
    assert!(!tg.silent);
  }

  // ── 4. Multiple chat_ids → multiple API calls ────────────────────────

  #[tokio::test]
  async fn test_multiple_chat_ids_sends_multiple_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(3)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["id1", "id2", "id3"], false);
    let result = tg.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    // Verify each request went to a different chat_id
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let chat_ids: Vec<String> = requests
      .iter()
      .map(|r| {
        let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        body["chat_id"].as_str().unwrap().to_string()
      })
      .collect();
    assert_eq!(chat_ids, vec!["id1", "id2", "id3"]);
  }

  #[tokio::test]
  async fn test_to_query_param_multiple_targets() {
    let parsed = ParsedUrl::parse("tgram://123456789:abcdefg_hijklmnop/?to=id1,id2").unwrap();
    let tg = Telegram::from_url(&parsed).unwrap();
    assert_eq!(tg.targets, vec!["id1", "id2"]);
  }

  #[tokio::test]
  async fn test_multiple_path_targets() {
    let parsed = ParsedUrl::parse("tgram://123456789:abcdefg_hijklmnop/id1/id2/23423/-30/").unwrap();
    let tg = Telegram::from_url(&parsed).unwrap();
    assert_eq!(tg.targets, vec!["id1", "id2", "23423", "-30"]);
  }

  // ── 5. Error handling ────────────────────────────────────────────────

  #[tokio::test]
  async fn test_http_500_returns_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({"description": "Internal Server Error"})))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let result = tg.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(!result.unwrap()); // send returns Ok(false)
  }

  #[tokio::test]
  async fn test_http_401_bad_token_returns_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({"ok": false, "description": "Unauthorized"})))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let result = tg.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(!result.unwrap());
  }

  #[tokio::test]
  async fn test_http_500_multiple_targets_all_fail() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(500))
      .expect(2)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["id1", "id2"], false);
    let result = tg.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(!result.unwrap());
  }

  #[tokio::test]
  async fn test_empty_targets_returns_error() {
    let server = MockServer::start().await;
    let tg = tg_for_mock(&server, &[], false);
    let result = tg.send(&ctx("title", "body")).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn test_bizarre_status_code_returns_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(999))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let result = tg.send(&ctx("title", "body")).await;
    // Non-success status codes should result in Ok(false)
    assert!(result.is_ok());
    assert!(!result.unwrap());
  }

  // ── 6. Attachment upload via sendDocument ─────────────────────────────

  #[tokio::test]
  async fn test_attachment_sends_document() {
    let server = MockServer::start().await;
    // Text message
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    // Document upload
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendDocument", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let mut context = ctx("title", "body");
    context.attachments.push(Attachment { name: "test.pdf".to_string(), data: b"fake-pdf-data".to_vec(), mime_type: "application/pdf".to_string() });
    let result = tg.send(&context).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    // Verify 2 requests: sendMessage + sendDocument
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
  }

  #[tokio::test]
  async fn test_multiple_attachments_send_multiple_documents() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendDocument", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(2)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let mut context = ctx("title", "body");
    context.attachments.push(Attachment { name: "file1.txt".to_string(), data: b"data1".to_vec(), mime_type: "text/plain".to_string() });
    context.attachments.push(Attachment { name: "file2.txt".to_string(), data: b"data2".to_vec(), mime_type: "text/plain".to_string() });
    let result = tg.send(&context).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 7. Image sending (sendPhoto for image attachments) ───────────────

  #[tokio::test]
  async fn test_image_attachment_uses_send_photo() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    // Image should go to sendPhoto, not sendDocument
    Mock::given(method("POST")).and(path(format!("/bot{}/sendPhoto", TEST_BOT_TOKEN))).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let mut context = ctx("title", "body");
    context.attachments.push(Attachment { name: "photo.jpg".to_string(), data: b"fake-jpg-data".to_vec(), mime_type: "image/jpeg".to_string() });
    let result = tg.send(&context).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_gif_attachment_uses_send_animation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendAnimation", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let mut context = ctx("title", "body");
    context.attachments.push(Attachment { name: "animation.gif".to_string(), data: b"fake-gif-data".to_vec(), mime_type: "image/gif".to_string() });
    let result = tg.send(&context).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_video_attachment_uses_send_video() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(format!("/bot{}/sendVideo", TEST_BOT_TOKEN))).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let mut context = ctx("title", "body");
    context.attachments.push(Attachment { name: "clip.mp4".to_string(), data: b"fake-mp4-data".to_vec(), mime_type: "video/mp4".to_string() });
    let result = tg.send(&context).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_audio_attachment_uses_send_audio() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(format!("/bot{}/sendAudio", TEST_BOT_TOKEN))).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let mut context = ctx("title", "body");
    context.attachments.push(Attachment { name: "song.mp3".to_string(), data: b"fake-mp3-data".to_vec(), mime_type: "audio/mpeg".to_string() });
    let result = tg.send(&context).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_voice_attachment_uses_send_voice() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(format!("/bot{}/sendVoice", TEST_BOT_TOKEN))).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let mut context = ctx("title", "body");
    context.attachments.push(Attachment { name: "voice.ogg".to_string(), data: b"fake-ogg-data".to_vec(), mime_type: "audio/ogg".to_string() });
    let result = tg.send(&context).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 8. Attachment with multiple targets ──────────────────────────────

  #[tokio::test]
  async fn test_attachment_with_multiple_targets() {
    let server = MockServer::start().await;
    // 2 sendMessage calls (one per target)
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(2)
      .mount(&server)
      .await;
    // 2 sendDocument calls (one per target)
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendDocument", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(2)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["id1", "id2"], false);
    let mut context = ctx("title", "body");
    context.attachments.push(Attachment { name: "file.pdf".to_string(), data: b"pdf-data".to_vec(), mime_type: "application/pdf".to_string() });
    let result = tg.send(&context).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    // Total: 2 sendMessage + 2 sendDocument = 4 requests
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
  }

  // ── 9. Attachment upload failure returns false ────────────────────────

  #[tokio::test]
  async fn test_attachment_upload_failure_returns_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    // Document upload fails with 500
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendDocument", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(500))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["lead2gold"], false);
    let mut context = ctx("title", "body");
    context.attachments.push(Attachment { name: "test.pdf".to_string(), data: b"fake-data".to_vec(), mime_type: "application/pdf".to_string() });
    let result = tg.send(&context).await;
    assert!(result.is_ok());
    assert!(!result.unwrap());
  }

  // ── 10. endpoint_for_mime unit tests ─────────────────────────────────

  #[test]
  fn test_endpoint_for_mime_mapping() {
    assert_eq!(Telegram::endpoint_for_mime("image/gif"), ("sendAnimation", "animation"));
    assert_eq!(Telegram::endpoint_for_mime("image/jpeg"), ("sendPhoto", "photo"));
    assert_eq!(Telegram::endpoint_for_mime("image/png"), ("sendPhoto", "photo"));
    assert_eq!(Telegram::endpoint_for_mime("video/mp4"), ("sendVideo", "video"));
    assert_eq!(Telegram::endpoint_for_mime("video/h264"), ("sendAnimation", "animation"));
    assert_eq!(Telegram::endpoint_for_mime("audio/ogg"), ("sendVoice", "voice"));
    assert_eq!(Telegram::endpoint_for_mime("application/ogg"), ("sendVoice", "voice"));
    assert_eq!(Telegram::endpoint_for_mime("audio/mpeg"), ("sendAudio", "audio"));
    assert_eq!(Telegram::endpoint_for_mime("application/pdf"), ("sendDocument", "document"));
    assert_eq!(Telegram::endpoint_for_mime("text/plain"), ("sendDocument", "document"));
  }

  // ── 11. Title + body formatting ──────────────────────────────────────

  #[tokio::test]
  async fn test_title_and_body_formatting() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TEST_BOT_TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let tg = tg_for_mock(&server, &["12345"], false);
    let result = tg.send(&ctx("special characters", "test body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["text"], "<b>special characters</b>\ntest body");
    assert_eq!(body["parse_mode"], "HTML");
  }

  // ── 12. Bot token parsing from URL ───────────────────────────────────

  #[test]
  fn test_bot_token_parsing_with_bot_prefix() {
    let parsed = ParsedUrl::parse("tgram://bottest@123456789:abcdefg_hijklmnop/lead2gold/").unwrap();
    let tg = Telegram::from_url(&parsed).unwrap();
    assert_eq!(tg.bot_token, "123456789:abcdefg_hijklmnop");
    assert_eq!(tg.targets, vec!["lead2gold"]);
  }

  #[test]
  fn test_invalid_bot_token_alpha_id_rejected() {
    let parsed = ParsedUrl::parse("tgram://alpha:abcdefg_hijklmnop/lead2gold/").unwrap();
    assert!(Telegram::from_url(&parsed).is_none());
  }

  #[test]
  fn test_negative_chat_id_target() {
    // Negative IDs are valid (group chats in Telegram)
    let parsed = ParsedUrl::parse("tgram://123456789:ABCdefghijkl123456789opqyz/-123456789525/").unwrap();
    let tg = Telegram::from_url(&parsed).unwrap();
    assert!(tg.targets.contains(&"-123456789525".to_string()));
  }
}
