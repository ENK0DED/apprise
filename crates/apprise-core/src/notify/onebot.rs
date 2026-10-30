use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;

/// Upstream `parse_list()`: split on whitespace, commas, semicolons and
/// brackets, then de-duplicate and sort.
fn parse_list<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
  let mut items: Vec<String> = values
    .into_iter()
    .flat_map(|v| v.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '[' | ']')))
    .filter(|s| !s.is_empty())
    .map(String::from)
    .collect();
  items.sort();
  items.dedup();
  items
}

/// QQ notifications through a OneBot 11 HTTP server (NapCat, LLOneBot,
/// Lagrange.OneBot, go-cqhttp).
///
/// `onebot://[token@]host[:port]/@user_id/#group_id`; `onebots://` uses
/// HTTPS. A bare number or `@` prefix is a QQ user, `#` a QQ group.
pub struct OneBot {
  host: String,
  port: Option<u16>,
  secure: bool,
  token: Option<String>,
  users: Vec<u64>,
  groups: Vec<u64>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl OneBot {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;

    // Read the token from the query string or URL user field
    let token = url.get("token").filter(|t| !t.is_empty()).map(String::from).or_else(|| url.user.clone()).filter(|t| !t.is_empty());

    // Treat every path entry as a target; ?to= adds more
    let path_entries: Vec<String> = url
      .path
      .split([' ', '\t', '\r', '\n', ',', '\\', '/'])
      .filter(|s| !s.is_empty())
      .map(|s| urlencoding::decode(s).map(|d| d.into_owned()).unwrap_or_else(|_| s.to_string()))
      .collect();
    let to = url.get("to").filter(|t| !t.is_empty());

    // Users may start with '@', groups with '#'; both hold 1 to 19 digits
    let mut users = Vec::new();
    let mut groups = Vec::new();
    for target in parse_list(path_entries.iter().map(String::as_str).chain(to)) {
      let (is_group, id) = match target.strip_prefix('#') {
        Some(id) => (true, id),
        None => (false, target.strip_prefix('@').unwrap_or(&target)),
      };
      if id.is_empty() || id.len() > 19 || !id.bytes().all(|b| b.is_ascii_digit()) {
        tracing::warn!("Dropped invalid OneBot target ({}) specified.", target);
        continue;
      }
      // 19 digits always fit in a u64
      let id: u64 = id.parse().ok()?;
      if is_group { groups.push(id) } else { users.push(id) }
    }

    Some(Self { host, port: url.port, secure: url.schema == "onebots", token, users, groups, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "OneBot",
      service_url: Some("https://github.com/botuniverse/onebot-11"),
      setup_url: Some("https://appriseit.com/services/onebot/"),
      protocols: vec!["onebot", "onebots"],
      description: "Send QQ messages through a OneBot 11 HTTP server.",
      attachment_support: true,
    }
  }

  fn status_str(status: u16) -> &'static str {
    match status {
      400 => "Malformed request body.",
      401 => "An access token is required.",
      403 => "The access token is invalid.",
      404 => "The action is not supported.",
      406 => "Unsupported Content-Type.",
      _ => "",
    }
  }

  /// A `retcode` of 0 or 1 (or no retcode at all) is a success
  fn retcode_ok(retcode: Option<&Value>) -> bool {
    match retcode {
      None => true,
      Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f == 0.0 || f == 1.0),
      Some(Value::Bool(_)) => true,
      Some(_) => false,
    }
  }

  /// Run one OneBot action and report whether it succeeded.
  async fn post(&self, client: &reqwest::Client, action: &str, payload: &Value) -> bool {
    let schema = if self.secure { "https" } else { "http" };
    let port = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}/{}", schema, self.host, port, action);
    tracing::debug!("OneBot POST URL: {} (cert_verify={})", url, self.verify_certificate);

    let mut req = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(payload.to_string());
    if let Some(token) = &self.token {
      req = req.header("Authorization", format!("Bearer {}", token));
    }
    let resp = match req.send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending OneBot {}.", action);
        tracing::debug!("Socket Exception: {}", e);
        return false;
      }
    };

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status != 200 {
      let status_str = Self::status_str(status);
      tracing::warn!("Failed to send OneBot {}: {}{}error={}.", action, status_str, if status_str.is_empty() { "" } else { ", " }, status);
      tracing::debug!("Response Details:\r\n{}", text);
      return false;
    }

    // OneBot can report an action failure inside an HTTP 200 response; an
    // empty or unreadable reply is accepted
    if let Ok(Value::Object(content)) = serde_json::from_str::<Value>(&text)
      && (content.get("status").and_then(Value::as_str) == Some("failed") || !Self::retcode_ok(content.get("retcode")))
    {
      let reason = ["wording", "message"]
        .iter()
        .filter_map(|k| content.get(*k))
        .find(|v| !v.is_null() && v.as_str() != Some(""))
        .map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
      let retcode = content.get("retcode").cloned().unwrap_or(Value::Null);
      tracing::warn!("Failed to send OneBot {}: retcode={}, {}", action, retcode, reason.unwrap_or_default());
      return false;
    }
    true
  }
}

#[async_trait]
impl Notify for OneBot {
  fn schemas(&self) -> &[&str] {
    &["onebot", "onebots"]
  }
  fn service_name(&self) -> &str {
    "OneBot"
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
  /// Limit each QQ text message to this many characters
  fn body_maxlen(&self) -> usize {
    4500
  }
  /// QQ has no title field, so Apprise adds the title to the body
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.users.is_empty() && self.groups.is_empty() {
      tracing::warn!("There are no OneBot targets to notify.");
      return Ok(false);
    }

    // Users and groups use different actions and ID fields
    let targets: Vec<(&str, &str, u64)> =
      self.users.iter().map(|id| ("send_private_msg", "user_id", *id)).chain(self.groups.iter().map(|id| ("send_group_msg", "group_id", *id))).collect();

    let client = build_client(self.verify_certificate)?;
    // Targets that failed are left alone for the rest of this notification
    let mut failed = vec![false; targets.len()];

    if !ctx.body.is_empty() {
      for (i, (action, field, id)) in targets.iter().enumerate() {
        let payload = json!({ *field: id, "message": [{ "type": "text", "data": { "text": ctx.body } }] });
        if self.post(&client, action, &payload).await {
          tracing::info!("Sent OneBot notification to {} {}.", field, id);
        } else {
          failed[i] = true;
        }
      }
    }

    // Send each attachment as its own message
    for attachment in &ctx.attachments {
      if failed.iter().all(|f| *f) {
        break;
      }

      // Base64 avoids requiring the server to access a local path
      let mut data = json!({ "file": format!("base64://{}", base64::engine::general_purpose::STANDARD.encode(&attachment.data)) });

      // Select the matching OneBot media type; other formats are sent as
      // named files
      let mime = attachment.mime_type.as_str();
      let kind = if mime.starts_with("image/") {
        "image"
      } else if mime.starts_with("audio/") {
        "record"
      } else if mime.starts_with("video/") {
        "video"
      } else {
        data["name"] = json!(if attachment.name.is_empty() { "attachment.dat" } else { attachment.name.as_str() });
        "file"
      };
      let segment = json!({ "type": kind, "data": data });

      for (i, (action, field, id)) in targets.iter().enumerate() {
        if failed[i] {
          continue;
        }
        let payload = json!({ *field: id, "message": [segment] });
        if !self.post(&client, action, &payload).await {
          failed[i] = true;
        }
      }
    }

    Ok(!failed.iter().any(|f| *f))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> OneBot {
    OneBot::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"status": "ok", "retcode": 0}))
  }

  fn addr(server: &MockServer) -> String {
    let a = server.address();
    format!("{}:{}", a.ip(), a.port())
  }

  fn text(field: &str, id: u64, body: &str) -> Value {
    json!({ field: id, "message": [{"type": "text", "data": {"text": body}}] })
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      // No targets; loads but has nothing to notify
      "onebot://localhost",
      "onebot://localhost/abc",
      "onebot://localhost/12345",
      "onebot://mytoken@localhost:3000/@12345/%2367890",
      "onebot://mytoken@localhost:3000/@12345/#67890",
      "onebots://localhost:5700/?to=12345,%2367890&token=mytoken",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["onebot://", "onebot://:@/"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_targets() {
    let o = parse("onebots://secret@bot.example:3000/@111/#222/bad");
    assert_eq!(o.users, vec![111]);
    assert_eq!(o.groups, vec![222]);
    assert_eq!(o.token.as_deref(), Some("secret"));
    assert!(o.secure);

    // Targets follow upstream's sorted parse_list() order
    let o = parse("onebot://tok@localhost:3000/@99/123/#7/x");
    assert_eq!(o.users, vec![123, 99]);
    assert_eq!(o.groups, vec![7]);

    // ?token= wins over the user field; 19 digits is the longest ID
    let o = parse(&format!("onebot://user@localhost/{}/%23{}/{}?token=abc%2Fdef", "1".repeat(19), "2".repeat(19), "3".repeat(20)));
    assert_eq!(o.token.as_deref(), Some("abc/def"));
    assert_eq!(o.users, vec![1111111111111111111]);
    assert_eq!(o.groups, vec![2222222222222222222]);
  }

  #[tokio::test]
  async fn test_send_users_and_groups() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/send_private_msg"))
      .and(header("Authorization", "Bearer secret"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(text("user_id", 111, "A & <b>\r\nx _ . ! * # <")))
      .respond_with(ok())
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/send_group_msg"))
      .and(body_json(text("group_id", 222, "A & <b>\r\nx _ . ! * # <")))
      .respond_with(ok())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("onebot://secret@{}/@111/%23222/bad", addr(&server))).unwrap();
    let ctx = NotifyContext { body: "A & <b>\r\nx _ . ! * # <".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_without_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/send_private_msg")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url(&format!("onebot://{}/111", addr(&server))).unwrap();
    assert!(svc.send(&NotifyContext { body: "hello".into(), ..Default::default() }).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().find(|r| r.url.path() == "/send_private_msg").unwrap().headers.get("Authorization").is_none());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url("onebot://localhost/abc").unwrap();
    assert!(!svc.send(&NotifyContext { body: "hello".into(), ..Default::default() }).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_service_errors() {
    // Failures reported inside an HTTP 200 reply
    for reply in [json!({"status": "failed", "retcode": 100}), json!({"status": "ok", "retcode": 1404}), json!({"retcode": null}), json!({"retcode": "0"})] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(reply.clone())).expect(1).mount(&server).await;
      let svc = from_url(&format!("onebot://{}/@111", addr(&server))).unwrap();
      assert!(!svc.send(&NotifyContext { body: "hello".into(), ..Default::default() }).await.unwrap(), "reply {}", reply);
    }

    // Async replies (retcode 1) and unreadable replies are successes
    for reply in [r#"{"status": "async", "retcode": 1}"#, "", "not json", "[]"] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string(reply)).expect(1).mount(&server).await;
      let svc = from_url(&format!("onebot://{}/@111", addr(&server))).unwrap();
      assert!(svc.send(&NotifyContext { body: "hello".into(), ..Default::default() }).await.unwrap(), "reply {}", reply);
    }

    for code in [400u16, 401, 403, 404, 406, 500] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url(&format!("onebot://{}/@111", addr(&server))).unwrap();
      assert!(!svc.send(&NotifyContext { body: "hello".into(), ..Default::default() }).await.unwrap(), "code {}", code);
    }
  }

  #[tokio::test]
  async fn test_send_attachments() {
    let server = MockServer::start().await;
    let b64 = |d: &[u8]| format!("base64://{}", base64::engine::general_purpose::STANDARD.encode(d));
    Mock::given(method("POST")).and(body_json(text("user_id", 111, "hello"))).respond_with(ok()).expect(1).mount(&server).await;
    for segment in [
      json!({"type": "image", "data": {"file": b64(b"PNG")}}),
      json!({"type": "record", "data": {"file": b64(b"MP3")}}),
      json!({"type": "video", "data": {"file": b64(b"MP4")}}),
      json!({"type": "file", "data": {"file": b64(b"ZIP"), "name": "apprise-archive.zip"}}),
      json!({"type": "file", "data": {"file": b64(b"abc"), "name": "attachment.dat"}}),
    ] {
      Mock::given(method("POST"))
        .and(path("/send_private_msg"))
        .and(body_json(json!({"user_id": 111, "message": [segment]})))
        .respond_with(ok())
        .expect(1)
        .mount(&server)
        .await;
    }

    let svc = from_url(&format!("onebot://{}/@111", addr(&server))).unwrap();
    let mut ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    for (name, data, mime) in [
      ("a.png", "PNG", "image/png"),
      ("a.mp3", "MP3", "audio/mpeg"),
      ("a.mp4", "MP4", "video/mp4"),
      ("apprise-archive.zip", "ZIP", "application/zip"),
      ("", "abc", "application/pdf"),
    ] {
      ctx.attachments.push(Attachment { name: name.into(), data: data.as_bytes().to_vec(), mime_type: mime.into() });
    }
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_attachment_only_and_failed_targets() {
    // An empty body sends only the attachment
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ok()).expect(1).mount(&server).await;
    let svc = from_url(&format!("onebot://{}/@111", addr(&server))).unwrap();
    let mut ctx = NotifyContext::default();
    ctx.attachments.push(Attachment { name: "a.png".into(), data: b"PNG".to_vec(), mime_type: "image/png".into() });
    assert!(svc.send(&ctx).await.unwrap());

    // A target whose text failed gets no attachments
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/send_private_msg")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/send_group_msg")).respond_with(ok()).expect(2).mount(&server).await;
    let svc = from_url(&format!("onebot://{}/@111/%23222", addr(&server))).unwrap();
    ctx.body = "hello".into();
    assert!(!svc.send(&ctx).await.unwrap());
  }

  #[test]
  fn test_service_details() {
    let details = OneBot::static_details();
    assert_eq!(details.service_name, "OneBot");
    assert_eq!(details.protocols, vec!["onebot", "onebots"]);
    assert!(details.attachment_support);
    let svc = parse("onebot://localhost/1");
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.body_maxlen(), 4500);
  }
}
