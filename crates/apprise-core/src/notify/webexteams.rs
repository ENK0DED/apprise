use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

/// How messages are delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  /// Incoming webhook: an 80 to 160 character alphanumeric token
  Webhook,
  /// Bot API access token posting to room IDs (supports attachments)
  Bot,
}

/// Cisco Webex Teams: `wxteams://{webhook_token}` or
/// `wxteams://{bot_access_token}/{room_id}/...`.
pub struct WebexTeams {
  mode: Mode,
  /// Webhook token or bot access token, depending on the mode
  token: String,
  /// Room IDs (bot mode)
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

fn is_webhook_token(s: &str) -> bool {
  (80..=160).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Upstream `parse_list()`: split on `[]`, `;`, `,` and whitespace, then
/// drop empty entries, de-duplicate and sort.
fn parse_list<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<String> {
  let mut out: Vec<String> = items
    .into_iter()
    .flat_map(|s| s.split(|c: char| c == '[' || c == ']' || c == ';' || c == ',' || c.is_whitespace()))
    .filter(|s| !s.is_empty())
    .map(String::from)
    .collect();
  out.sort();
  out.dedup();
  out
}

/// Strip `prefix` from the start of `s`, ignoring ASCII case.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
  s.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| &s[prefix.len()..])
}

/// Upstream `parse_native_url`:
/// `https?://(api.ciscospark.com|webexapis.com)/v{N}/webhooks/incoming/{token}/?{params}`.
/// Returns the equivalent `wxteams://{token}/{params}` URL.
fn native_to_apprise(raw: &str) -> Option<String> {
  let rest = strip_prefix_ci(raw, "https://").or_else(|| strip_prefix_ci(raw, "http://"))?;
  let rest = strip_prefix_ci(rest, "api.ciscospark.com/").or_else(|| strip_prefix_ci(rest, "webexapis.com/"))?;
  // v[1-9][0-9]*
  let rest = strip_prefix_ci(rest, "v")?;
  let digits = rest.chars().take_while(char::is_ascii_digit).count();
  if digits == 0 || rest.starts_with('0') {
    return None;
  }
  let rest = strip_prefix_ci(&rest[digits..], "/webhooks/incoming/")?;
  let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(rest.len());
  let (token, rest) = rest.split_at(end);
  let params = rest.strip_prefix('/').unwrap_or(rest);
  if token.is_empty() || !(params.is_empty() || (params.starts_with('?') && params.len() > 1)) {
    return None;
  }
  Some(format!("wxteams://{}/{}", token, params))
}

impl WebexTeams {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    if url.schema == "https" || url.schema == "http" {
      return Self::from_url(&ParsedUrl::parse(&native_to_apprise(&url.raw)?)?);
    }

    // Room IDs come from the path and ?to=
    let mut entries: Vec<String> = url.path_parts.clone();
    if let Some(to) = url.get("to") {
      entries.extend(to.split(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n' | ',' | '\\' | '/')).filter(|s| !s.is_empty()).map(String::from));
    }
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => t.to_string(),
      None => urlencoding::decode(url.host.as_deref().unwrap_or_default()).ok()?.into_owned(),
    };

    // An explicit ?mode= may be abbreviated (b, w, ...)
    let explicit = url.get("mode").filter(|m| !m.is_empty());
    let mode = match explicit {
      Some(m) if "webhook".starts_with(m) => Mode::Webhook,
      Some(m) if "bot".starts_with(m) => Mode::Bot,
      Some(m) => {
        tracing::warn!("The Webex Teams mode specified ({}) is invalid.", m);
        return None;
      }
      // Room IDs mean a bot; otherwise a webhook token decides
      None if !entries.is_empty() => Mode::Bot,
      None if is_webhook_token(token.trim()) || token.is_empty() => Mode::Webhook,
      None => Mode::Bot,
    };

    match mode {
      Mode::Webhook => {
        let token = token.trim().to_string();
        if !is_webhook_token(&token) {
          tracing::warn!("The Webex Teams webhook token specified is invalid.");
          return None;
        }
        Some(Self { mode, token, targets: Vec::new(), verify_certificate: url.verify_certificate(), tags: url.tags() })
      }
      Mode::Bot => {
        if token.is_empty() {
          tracing::warn!("A Webex Teams bot access token must be specified.");
          return None;
        }
        let targets = parse_list(entries.iter().map(String::as_str));
        Some(Self { mode, token, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
      }
    }
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Cisco Webex Teams",
      service_url: Some("https://webex.teams.com/"),
      setup_url: Some("https://appriseit.com/services/wxteams/"),
      protocols: vec!["wxteams", "webex"],
      description: "Send via Cisco Webex Teams incoming webhooks or the Bot API.",
      attachment_support: true,
    }
  }

  async fn send_webhook(&self, client: &reqwest::Client, ctx: &NotifyContext, key: &str) -> Result<bool, NotifyError> {
    if !ctx.attachments.is_empty() {
      tracing::warn!("Webex Teams Webhooks do not support attachments; use bot mode.");
    }
    // Upstream joins its slash-terminated base and the token with another '/'
    let url = format!("{}/v1/webhooks/incoming//{}", service_base("https://api.ciscospark.com"), self.token);
    let resp = client.post(&url).header("User-Agent", APP_ID).json(&json!({ key: ctx.body })).send().await?;
    let status = resp.status().as_u16();
    if status == 200 || status == 204 {
      tracing::info!("Sent Webex Teams notification.");
      Ok(true)
    } else {
      tracing::warn!("Failed to send Webex Teams notification: error={}.", status);
      Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() })
    }
  }

  /// Post the message to one room: as JSON, or as one multipart post per
  /// attachment with the text carried by the first.
  async fn post_to_room(&self, client: &reqwest::Client, ctx: &NotifyContext, key: &str, room: &str) -> bool {
    let url = format!("{}/v1/messages", service_base("https://webexapis.com"));
    let requests: Vec<reqwest::RequestBuilder> = if ctx.attachments.is_empty() {
      vec![client.post(&url).json(&json!({ "roomId": room, key: ctx.body }))]
    } else {
      ctx
        .attachments
        .iter()
        .enumerate()
        .map(|(i, attach)| {
          let name = if attach.name.is_empty() { format!("file{:03}.dat", i + 1) } else { attach.name.clone() };
          let part = reqwest::multipart::Part::bytes(attach.data.clone()).file_name(name.clone());
          let part = part.mime_str(&attach.mime_type).unwrap_or_else(|_| reqwest::multipart::Part::bytes(attach.data.clone()).file_name(name));
          let mut form = reqwest::multipart::Form::new().text("roomId", room.to_string());
          if i == 0 {
            form = form.text(key.to_string(), ctx.body.clone());
          }
          client.post(&url).multipart(form.part("files", part))
        })
        .collect()
    };
    for request in requests {
      match request.header("User-Agent", APP_ID).bearer_auth(&self.token).send().await {
        Ok(resp) if resp.status() == reqwest::StatusCode::OK => {}
        Ok(resp) => {
          tracing::warn!("Failed to send Webex Teams Bot notification to room {}: error={}.", room, resp.status().as_u16());
          return false;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Webex Teams Bot notification: {}", e);
          return false;
        }
      }
    }
    tracing::info!("Sent Webex Teams Bot notification to room {}.", room);
    true
  }
}

#[async_trait]
impl Notify for WebexTeams {
  fn schemas(&self) -> &[&str] {
    &["wxteams", "webex"]
  }
  fn service_name(&self) -> &str {
    "Cisco Webex Teams"
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
    NotifyFormat::Markdown
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    match self.mode {
      Mode::Webhook => 1000,
      Mode::Bot => 7439,
    }
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let key = if ctx.body_format == NotifyFormat::Markdown { "markdown" } else { "text" };
    let client = build_client(self.verify_certificate)?;
    if self.mode == Mode::Webhook {
      return self.send_webhook(&client, ctx, key).await;
    }
    if self.targets.is_empty() {
      tracing::warn!("Webex Teams Bot mode has no room IDs to notify, aborting.");
      return Ok(false);
    }
    let mut all_ok = true;
    for room in &self.targets {
      if !self.post_to_room(&client, ctx, key, room).await {
        all_ok = false;
      }
    }
    Ok(all_ok)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  const ROOM_ID: &str = "Y2lzY29zcGFyazovL3VzL1JPTU9NLzEyMzQ1Njc4OTAxMjM0";
  const ROOM_ID2: &str = "Y2lzY29zcGFyazovL3VzL1JPTU9NL2FiY2RlZjEyMzQ1Njc4";

  fn bot_token() -> String {
    "Bc10".repeat(50)
  }

  fn parse(url: &str) -> WebexTeams {
    WebexTeams::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_invalid_urls() {
    let a80 = "a".repeat(80);
    let urls: Vec<String> = vec![
      "wxteams://".into(),
      "wxteams://:@/".into(),
      format!("wxteams://{}?mode=invalid", a80),
      // Modes are matched case-sensitively, as upstream does
      format!("wxteams://{}?mode=BOT", a80),
      // A forced webhook needs a webhook token
      "wxteams://abc?mode=webhook".into(),
      format!("https://webexapis.com/v0/webhooks/incoming/{}", a80),
      format!("https://webexapis.com/v1/webhooks/incoming/{}/x", a80),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let a80 = "a".repeat(80);
    let urls: Vec<String> = vec![
      format!("wxteams://{}", a80),
      format!("wxteams://?token={}", a80),
      format!("webex://{}", "a".repeat(140)),
      format!("https://api.ciscospark.com/v1/webhooks/incoming/{}", a80),
      format!("https://webexapis.com/v1/webhooks/incoming/{}", "a".repeat(100)),
      format!("https://api.ciscospark.com/v1/webhooks/incoming/{}?format=text", a80),
      format!("wxteams://{}/{}", bot_token(), ROOM_ID),
      format!("wxteams://{}?mode=bot", a80),
      format!("wxteams://{}?mode=webhook", a80),
      // A token that is not a webhook token is a bot without rooms
      "wxteams://abc".into(),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_mode_detection() {
    assert_eq!(parse(&format!("wxteams://{}", "a".repeat(80))).mode, Mode::Webhook);
    assert_eq!(parse(&format!("wxteams://{}", "A".repeat(80))).mode, Mode::Webhook);
    assert_eq!(parse(&format!("wxteams://?token={}", "a".repeat(80))).mode, Mode::Webhook);
    assert_eq!(parse(&format!("wxteams://{}/{}", "a".repeat(200), ROOM_ID)).mode, Mode::Bot);
    // Room IDs make a webhook-looking token a bot token too
    assert_eq!(parse(&format!("wxteams://{}/{}", "a".repeat(80), ROOM_ID)).mode, Mode::Bot);
    // An explicit mode wins, and may be abbreviated
    assert_eq!(parse(&format!("wxteams://{}?mode=b", "a".repeat(80))).mode, Mode::Bot);
    let w = parse(&format!("wxteams://{}/{}?mode=w", "a".repeat(80), ROOM_ID));
    assert_eq!(w.mode, Mode::Webhook);
    assert!(w.targets.is_empty());
  }

  #[test]
  fn test_bot_rooms() {
    let w = parse(&format!("wxteams://{}/{}/{}", bot_token(), ROOM_ID2, ROOM_ID));
    assert_eq!(w.token, bot_token());
    // Sorted like upstream parse_list()
    assert_eq!(w.targets, vec![ROOM_ID2, ROOM_ID]);
    let w = parse(&format!("wxteams://{}?to={},{}&mode=bot", bot_token(), ROOM_ID, ROOM_ID));
    assert_eq!(w.targets, vec![ROOM_ID]);
  }

  #[test]
  fn test_native_urls() {
    let w = parse("https://WEBEXAPIS.com/v2/webhooks/incoming/abc_DEF-1/?mode=b");
    assert_eq!(w.token, "abc_DEF-1");
    assert_eq!(w.mode, Mode::Bot);
    let w = parse(&format!("https://api.ciscospark.com/v1/webhooks/incoming/{}", "a".repeat(80)));
    assert_eq!(w.mode, Mode::Webhook);
  }

  #[test]
  fn test_service_details() {
    let details = WebexTeams::static_details();
    assert_eq!(details.service_name, "Cisco Webex Teams");
    assert!(details.protocols.contains(&"wxteams") && details.protocols.contains(&"webex"));
    assert!(details.attachment_support);
    let webhook = from_url(&format!("wxteams://{}", "a".repeat(80))).unwrap();
    assert_eq!(webhook.body_maxlen(), 1000);
    assert_eq!(webhook.title_maxlen(), 0);
    assert_eq!(webhook.notify_format(), NotifyFormat::Markdown);
    assert_eq!(from_url(&format!("wxteams://{}/{}", bot_token(), ROOM_ID)).unwrap().body_maxlen(), 7439);
  }

  fn ctx(body: &str, format: NotifyFormat) -> NotifyContext {
    NotifyContext { body: body.into(), body_format: format, ..Default::default() }
  }

  #[tokio::test]
  async fn test_webhook_send() {
    let a80 = "a".repeat(80);
    for (format, expected) in [(NotifyFormat::Markdown, json!({ "markdown": "T\r\nB" })), (NotifyFormat::Text, json!({ "text": "T\r\nB" }))] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path(format!("/v1/webhooks/incoming//{}", a80)))
        .and(header("Content-Type", "application/json"))
        .and(body_json(expected))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url(&format!("wxteams://{}", a80)).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T\r\nB", format))).await.unwrap());
    }

    for code in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url(&format!("wxteams://{}", a80)).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B", NotifyFormat::Markdown))).await.is_err());
    }
  }

  #[tokio::test]
  async fn test_bot_send_per_room() {
    let server = MockServer::start().await;
    for room in [ROOM_ID, ROOM_ID2] {
      Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("Authorization", format!("Bearer {}", bot_token()).as_str()))
        .and(header("Content-Type", "application/json"))
        .and(body_json(json!({ "roomId": room, "markdown": "# T\n**B**" })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&format!("wxteams://{}/{}/{}", bot_token(), ROOM_ID, ROOM_ID2)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("# T\n**B**", NotifyFormat::Markdown))).await.unwrap());
  }

  #[tokio::test]
  async fn test_bot_send_failures() {
    // No rooms: nothing is sent
    let svc = from_url(&format!("wxteams://{}?mode=bot", "a".repeat(80))).unwrap();
    assert!(!svc.send(&ctx("B", NotifyFormat::Markdown)).await.unwrap());

    // A failing room does not stop the next one
    let server = MockServer::start().await;
    let first_room = |req: &Request| serde_json::from_slice::<serde_json::Value>(&req.body).map(|v| v["roomId"] == ROOM_ID).unwrap_or(false);
    Mock::given(method("POST")).and(first_room).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url(&format!("wxteams://{}/{}/{}", bot_token(), ROOM_ID, ROOM_ID2)).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B", NotifyFormat::Markdown))).await.unwrap());

    // Only HTTP 200 counts in bot mode
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    let svc = from_url(&format!("wxteams://{}/{}", bot_token(), ROOM_ID)).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B", NotifyFormat::Markdown))).await.unwrap());
  }

  #[tokio::test]
  async fn test_bot_attachments() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;
    let svc = from_url(&format!("wxteams://{}/{}", bot_token(), ROOM_ID)).unwrap();
    let mut c = ctx("see attached", NotifyFormat::Text);
    c.attachments = vec![
      Attachment { name: "a.txt".into(), data: b"hello".to_vec(), mime_type: "text/plain".into() },
      Attachment { name: String::new(), data: b"\x00\x01".to_vec(), mime_type: "application/octet-stream".into() },
    ];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());

    // One multipart post per attachment; only the first carries the text
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let first = String::from_utf8_lossy(&requests[0].body).into_owned();
    let second = String::from_utf8_lossy(&requests[1].body).into_owned();
    assert!(requests[0].headers.get("content-type").unwrap().to_str().unwrap().starts_with("multipart/form-data"));
    assert!(first.contains("name=\"roomId\"") && first.contains(ROOM_ID));
    assert!(first.contains("name=\"text\"") && first.contains("see attached"));
    assert!(first.contains("name=\"files\"; filename=\"a.txt\"") && first.contains("Content-Type: text/plain"));
    assert!(!second.contains("name=\"text\""));
    assert!(second.contains("filename=\"file002.dat\""));

    // A failed upload fails the room
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());
  }
}
