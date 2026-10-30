use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;

/// GroupMe Bot notifications.
///
/// `groupme://{bot_id}` posts text; `groupme://{bot_id}/{access_token}` (or
/// `?token=`) also uploads image attachments through GroupMe's image service.
pub struct GroupMe {
  bot_id: String,
  token: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl GroupMe {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // bot_id is in the host position; ?bot_id= overrides it
    let mut bot_id = url.host.clone().unwrap_or_default();
    if let Some(b) = url.get("bot_id").filter(|b| !b.is_empty()) {
      bot_id = b.to_string();
    }
    if bot_id.is_empty() || !bot_id.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("A valid GroupMe bot_id must be specified.");
      return None;
    }

    // The access token may be the first path segment; ?token= overrides it
    let mut token = url.path_parts.first().cloned();
    if let Some(t) = url.get("token").filter(|t| !t.is_empty()) {
      token = Some(t.to_string());
    }
    // validate_regex(): an unusable token is dropped rather than refused
    let token = token.filter(|t| t.chars().next().is_some_and(|c| !c.is_whitespace())).map(|t| t.trim().to_string());

    Some(Self { bot_id, token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "GroupMe",
      service_url: Some("https://groupme.com/"),
      setup_url: Some("https://appriseit.com/services/groupme/"),
      protocols: vec!["groupme"],
      description: "Post messages and images to a GroupMe group through a bot.",
      attachment_support: true,
    }
  }

  fn status_str(status: u16) -> &'static str {
    match status {
      401 => "Unauthorized - Invalid or expired access token.",
      400 => "Bad Request - Malformed payload or missing required fields.",
      404 => "Not Found - The specified bot_id does not exist.",
      429 => "Too many requests; rate-limit exceeded.",
      _ => "",
    }
  }

  /// Upload an image attachment to GroupMe's image service and return the
  /// hosted image URL, or `None` on failure.
  async fn upload_image(&self, client: &reqwest::Client, token: &str, attachment: &Attachment) -> Option<String> {
    let url = format!("{}/pictures", service_base("https://image.groupme.com"));
    tracing::debug!("GroupMe image upload URL: {} (cert_verify={})", url, self.verify_certificate);
    let resp = match client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", &attachment.mime_type)
      .header("X-Access-Token", token)
      .body(attachment.data.clone())
      .send()
      .await
    {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred uploading GroupMe image.");
        tracing::debug!("Socket Exception: {}", e);
        return None;
      }
    };

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status != 200 {
      let status_str = Self::status_str(status);
      tracing::warn!("Failed to upload GroupMe image: {}{}error={}.", status_str, if status_str.is_empty() { "" } else { ", " }, status);
      return None;
    }

    // The image URL is nested under payload.url
    let content: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match content.get("payload").and_then(|p| p.get("url")).and_then(Value::as_str).filter(|u| !u.is_empty()) {
      Some(image_url) => Some(image_url.to_string()),
      None => {
        tracing::warn!("GroupMe image service returned no URL for {}.", if attachment.name.is_empty() { "attachment" } else { &attachment.name });
        None
      }
    }
  }
}

#[async_trait]
impl Notify for GroupMe {
  fn schemas(&self) -> &[&str] {
    &["groupme"]
  }
  fn service_name(&self) -> &str {
    "GroupMe"
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
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Text
  }
  /// GroupMe bot messages are limited to 1000 characters
  fn body_maxlen(&self) -> usize {
    1000
  }
  /// GroupMe does not have a native title field
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let mut payload = json!({ "bot_id": self.bot_id, "text": ctx.body });

    // Handle image attachments
    let mut images = Vec::new();
    if !ctx.attachments.is_empty() {
      match &self.token {
        // Warn but do not abort; the text still gets sent
        None => tracing::warn!("GroupMe image attachments require an access token; skipping attachments."),
        Some(token) => {
          for attachment in &ctx.attachments {
            // Only image MIME types are accepted by GroupMe's image service
            if !attachment.mime_type.is_empty() && !attachment.mime_type.starts_with("image/") {
              tracing::warn!("GroupMe image service only supports images; skipping {} ({}).", attachment.name, attachment.mime_type);
              continue;
            }
            // A failed upload aborts the entire send
            let Some(image_url) = self.upload_image(&client, token, attachment).await else {
              return Ok(false);
            };
            images.push(json!({ "type": "image", "url": image_url }));
          }
        }
      }
    }
    if !images.is_empty() {
      payload["attachments"] = Value::Array(images);
    }

    let url = format!("{}/v3/bots/post", service_base("https://api.groupme.com"));
    tracing::debug!("GroupMe POST URL: {} (cert_verify={})", url, self.verify_certificate);
    tracing::debug!("GroupMe Payload: {}", payload);
    let resp =
      client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json; charset=utf-8").body(payload.to_string()).send().await?;

    let status = resp.status().as_u16();
    if matches!(status, 200 | 202) {
      tracing::info!("Sent GroupMe notification.");
      Ok(true)
    } else {
      let status_str = Self::status_str(status);
      tracing::warn!("Failed to send GroupMe notification: {}{}error={}.", status_str, if status_str.is_empty() { "" } else { ", " }, status);
      Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_bytes, body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const BOT_ID: &str = "68ca900a7d17f9b9891a73af2a";
  const TOKEN: &str = "abc123def456gh789ijklmn0op";
  const IMAGE_URL: &str = "https://i.groupme.com/somethingsomething.large";

  fn parse(url: &str) -> GroupMe {
    GroupMe::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn image(name: &str) -> Attachment {
    Attachment { name: name.into(), data: b"PNGDATA".to_vec(), mime_type: "image/png".into() }
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      format!("groupme://{}", BOT_ID),
      format!("groupme://?bot_id={}", BOT_ID),
      format!("groupme://{}/{}", BOT_ID, TOKEN),
      format!("groupme://{}?token={}", BOT_ID, TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = ["groupme://", "groupme://:@/", "groupme://not-a-valid-bot-id!"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let g = parse(&format!("groupme://{}", BOT_ID));
    assert_eq!(g.bot_id, BOT_ID);
    assert_eq!(g.token, None);

    let g = parse(&format!("groupme://{}/{}", BOT_ID, TOKEN));
    assert_eq!(g.token.as_deref(), Some(TOKEN));

    // ?token= and ?bot_id= override the path and host
    let g = parse(&format!("groupme://abc/pathtoken?token={}&bot_id={}", TOKEN, BOT_ID));
    assert_eq!(g.bot_id, BOT_ID);
    assert_eq!(g.token.as_deref(), Some(TOKEN));
  }

  #[tokio::test]
  async fn test_send_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v3/bots/post"))
      .and(header("Content-Type", "application/json; charset=utf-8"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({"bot_id": BOT_ID, "text": "T\r\nB"})))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("groupme://{}", BOT_ID)).unwrap();
    let ctx = NotifyContext { body: "T\r\nB".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_http_errors() {
    for code in [400u16, 401, 404, 429, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url(&format!("groupme://{}", BOT_ID)).unwrap();
      let ctx = NotifyContext { body: "Test".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.is_err(), "code {}", code);
    }
    // 200 is accepted as well as 202
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url(&format!("groupme://{}", BOT_ID)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_attachments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/pictures"))
      .and(header("Content-Type", "image/png"))
      .and(header("X-Access-Token", TOKEN))
      .and(body_bytes(b"PNGDATA".to_vec()))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"payload": {"url": IMAGE_URL}})))
      .expect(2)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/v3/bots/post"))
      .and(body_json(json!({
        "bot_id": BOT_ID,
        "text": "Test",
        "attachments": [{"type": "image", "url": IMAGE_URL}, {"type": "image", "url": IMAGE_URL}],
      })))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("groupme://{}/{}", BOT_ID, TOKEN)).unwrap();
    let mut ctx = NotifyContext { body: "Test".into(), ..Default::default() };
    ctx.attachments.push(image("a.png"));
    // Non-image attachments are skipped
    ctx.attachments.push(Attachment { name: "a.zip".into(), data: b"ZIP".to_vec(), mime_type: "application/zip".into() });
    ctx.attachments.push(image("b.png"));
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_attachments_without_token() {
    // The text is still sent without uploading anything
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v3/bots/post"))
      .and(body_json(json!({"bot_id": BOT_ID, "text": "Test"})))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("groupme://{}", BOT_ID)).unwrap();
    let mut ctx = NotifyContext { body: "Test".into(), ..Default::default() };
    ctx.attachments.push(image("a.png"));
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_upload_failures() {
    // An upload rejected, or answered without an image URL, aborts the send
    for reply in [ResponseTemplate::new(500), ResponseTemplate::new(200).set_body_string("{}"), ResponseTemplate::new(200).set_body_string("not-json")] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/pictures")).respond_with(reply).expect(1).mount(&server).await;
      Mock::given(method("POST")).and(path("/v3/bots/post")).respond_with(ResponseTemplate::new(202)).expect(0).mount(&server).await;
      let svc = from_url(&format!("groupme://{}/{}", BOT_ID, TOKEN)).unwrap();
      let mut ctx = NotifyContext { body: "Test".into(), ..Default::default() };
      ctx.attachments.push(image("a.png"));
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }
  }

  #[test]
  fn test_service_details() {
    let details = GroupMe::static_details();
    assert_eq!(details.service_name, "GroupMe");
    assert_eq!(details.protocols, vec!["groupme"]);
    assert!(details.attachment_support);
    let svc = parse(&format!("groupme://{}", BOT_ID));
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.body_maxlen(), 1000);
  }
}
