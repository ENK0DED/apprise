//! Whapi.Cloud, WhatsApp messages from your own number linked the way
//! WhatsApp Web is (upstream `NotifyWhapi`): `whapi://{token}/{targets}`.
//!
//! A target is a phone number in international format, a group ID (the bare
//! number or the full ID ending in `@g.us`) or any other full chat ID (such
//! as one ending in `@newsletter`), which is passed along as it is.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::evolution::commonmark_to_whatsapp;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::email::is_space;
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::plugin_args::unquote;

/// Upstream `WHAPI_HTTP_ERROR_MAP`, on top of the shared `HTML_LOOKUP`.
fn status_text(status: u16) -> &'static str {
  match status {
    401 => "Unauthorized - Invalid Token or Channel not linked.",
    402 => "Trial limit exceeded.",
    403 => "Sending to this recipient is forbidden.",
    413 => "Request too large.",
    429 => "Too many requests were made.",
    400 => "Bad Request - Unsupported Parameters.",
    404 => "Page not found.",
    405 => "Method not allowed.",
    500 => "Internal server error.",
    503 => "Servers are overloaded.",
    _ => "",
  }
}

/// The token (`^[a-z0-9_-]+$`, any case), trimmed as upstream's
/// `validate_regex` returns it.
fn validate_token(token: &str) -> Option<String> {
  // Python's `$` also matches before one final line break
  let body = token.strip_suffix('\n').unwrap_or(token);
  (!body.is_empty() && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then(|| body.to_string())
}

/// Upstream `IS_CHAT_ID` (`^[^\s@/]+@[a-z0-9.]+$`, any case): a full chat
/// ID such as `120363...@g.us` or `120363...@newsletter`.
fn is_chat_id(target: &str) -> bool {
  let Some((user, domain)) = target.split_once('@') else { return false };
  !user.is_empty() && !user.chars().any(|c| is_space(c) || c == '/') && !domain.is_empty() && domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
}

/// Upstream `IS_GROUP_ID`: 16 or more digits (past the 15 digit phone
/// number maximum), or the older `{creator_phone}-{timestamp}` form.
fn is_group_id(target: &str) -> bool {
  let digits = |s: &str, min: usize| s.len() >= min && s.bytes().all(|b| b.is_ascii_digit());
  digits(target, 16) || target.split_once('-').is_some_and(|(phone, stamp)| digits(phone, 10) && digits(stamp, 10))
}

/// How WhatsApp should present a file of this MIME type (upstream
/// `WHAPI_MEDIA_TYPES`); anything else is a document.
fn media_kind(mime_type: &str) -> &'static str {
  match mime_type {
    "image/jpeg" | "image/png" => "image",
    "video/mp4" | "video/3gpp" => "video",
    m if m.starts_with("audio/") => "audio",
    _ => "document",
  }
}

pub struct Whapi {
  token: String,
  /// Show a preview of the first link in the message.
  preview: bool,
  /// The chat IDs to notify.
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Whapi {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The host is the Channel token, which ?token= replaces
    let mut token = url.host.as_deref().map(unquote);
    if let Some(t) = url.get("token").filter(|t| !t.is_empty()) {
      token = Some(unquote(t));
    }
    let Some(token) = token.as_deref().and_then(validate_token) else {
      tracing::warn!("An invalid Whapi.Cloud Token was specified.");
      return None;
    };

    let preview = url.get("preview").is_none_or(parse_bool);

    // The chat IDs are in the path and in ?to=
    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.extend(parse_phone_no(&[to]));
    }

    let mut targets = Vec::new();
    for target in parse_phone_no(&entries) {
      if is_chat_id(&target) {
        // A full chat ID is passed along as it is
        targets.push(target);
      } else if is_group_id(&target) {
        // A bare group ID needs its @g.us suffix
        targets.push(format!("{target}@g.us"));
      } else if let Some(phone) = is_phone_no(&target, 10) {
        // Whapi.Cloud wants the digits only
        targets.push(phone.full);
      } else {
        tracing::warn!("Dropped invalid Whapi.Cloud target ({}) specified.", target);
      }
    }

    // No targets still loads; the send then fails, as upstream's does
    Some(Self { token, preview, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Whapi.Cloud",
      service_url: Some("https://whapi.cloud/"),
      setup_url: Some("https://appriseit.com/services/whapi/"),
      protocols: vec!["whapi"],
      description: "Send WhatsApp messages from your own number via Whapi.Cloud.",
      attachment_support: true,
    }
  }

  /// Post one message to Whapi.Cloud; whether it was sent.
  async fn post(&self, client: &reqwest::Client, url: &str, target: &str, content_type: &str, data: Vec<u8>, params: &[(&str, &str)]) -> bool {
    tracing::debug!("Whapi.Cloud POST URL: {} (cert_verify={})", url, self.verify_certificate);
    if params.is_empty() {
      tracing::debug!("Whapi.Cloud Payload: {}", String::from_utf8_lossy(&data));
    } else {
      tracing::debug!("Whapi.Cloud Payload: {:?}", params);
    }

    throttle().await;
    let resp = client
      .post(url)
      .header("User-Agent", APP_ID)
      .header("Accept", "application/json")
      .header("Authorization", format!("Bearer {}", self.token))
      .header("Content-Type", content_type)
      .query(params)
      .body(data)
      .send()
      .await;
    let resp = match resp {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Whapi.Cloud notification to {}.", target);
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return false;
      }
    };
    let status = resp.status().as_u16();
    let content = resp.bytes().await.unwrap_or_default();

    // A successful response looks like {"sent": true, "message": {...}},
    // an error like {"error": {"code": 401, "message": "..."}}
    let reply: Value = serde_json::from_slice::<Value>(&content).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));

    // Only an explicit "sent": true confirms the delivery
    if !(200..300).contains(&status) || reply.get("sent") != Some(&Value::Bool(true)) {
      // Prefer Whapi.Cloud's own explanation when it gave one
      let message = reply.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).filter(|m| !m.is_empty());
      let reason = message.unwrap_or_else(|| status_text(status));
      tracing::warn!("Failed to send Whapi.Cloud notification to {}: {}{}error={}.", target, reason, if reason.is_empty() { "" } else { ", " }, status);
      tracing::debug!("Response Details:\r\n{}", String::from_utf8_lossy(&content[..content.len().min(2000)]));
      return false;
    }
    tracing::info!("Sent Whapi.Cloud notification to {}.", target);
    true
  }
}

#[async_trait]
impl Notify for Whapi {
  fn schemas(&self) -> &[&str] {
    &["whapi"]
  }
  fn service_name(&self) -> &str {
    "Whapi.Cloud"
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
    // WhatsApp has its own Markdown-like formatting
    NotifyFormat::Markdown
  }
  fn body_maxlen(&self) -> usize {
    4096
  }
  fn title_maxlen(&self) -> usize {
    // WhatsApp messages have no title; it is placed in the body
    0
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    if *body_format != NotifyFormat::Markdown {
      return body.to_string();
    }
    commonmark_to_whatsapp(body)
  }
  fn has_dialect(&self) -> bool {
    true
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    // The Channel token alone identifies the linked WhatsApp number
    Some(vec!["whapi".into(), self.token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Whapi.Cloud targets to notify.");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let base = service_base("https://gate.whapi.cloud");
    let mut has_error = false;

    for target in &self.targets {
      // A message made of attachments alone has no text to send. Skip a
      // target that already accepted the text so a retry does not repeat it.
      if !ctx.body.is_empty() && !is_delivered(target, false) {
        let mut payload = json!({ "to": target, "body": ctx.body });
        if !self.preview {
          payload["no_link_preview"] = json!(true);
        }
        let url = format!("{base}/messages/text");
        if !self.post(&client, &url, target, "application/json", payload.to_string().into_bytes(), &[]).await {
          // No files follow a failed message
          has_error = true;
          continue;
        }
        mark_delivered(target, false);
      }

      for (no, attachment) in ctx.attachments.iter().enumerate() {
        // Each file is tracked on its own so a retry only sends the ones
        // that did not make it
        let key = format!("attachment:{no}:{target}");
        if is_delivered(&key, false) {
          continue;
        }
        let kind = media_kind(&attachment.mime_type);
        // Who it goes to, and the name shown for a document
        let mut params = vec![("to", target.as_str())];
        if kind == "document" {
          params.push(("filename", if attachment.name.is_empty() { "attachment.dat" } else { attachment.name.as_str() }));
        }
        let url = format!("{base}/messages/media/{kind}");
        if !self.post(&client, &url, target, &attachment.mime_type, attachment.data.clone(), &params).await {
          // Later files wait for a retry
          has_error = true;
          break;
        }
        mark_delivered(&key, false);
      }
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Apprise;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use crate::notify::with_test_base;
  use wiremock::matchers::{method, path, path_regex};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  /// A realistic Channel token.
  const TOKEN: &str = "AbCdEf0123456789AbCdEf0123456789";
  /// A group ID as Whapi.Cloud reports it.
  const GROUP: &str = "120363042829563251";
  /// A successful Whapi.Cloud response.
  const OK: &str = r#"{"sent": true, "message": {"id": "PsobWy36679._7w-wKmB9tMeGQ"}}"#;

  fn whapi(url: &str) -> Whapi {
    Whapi::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn f(body: &str) -> String {
    whapi(&format!("whapi://{TOKEN}/15551234567")).dialect_convert(body, &NotifyFormat::Markdown)
  }

  fn ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(OK)
  }

  /// A server answering every message and media call with `response`.
  async fn server_with(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path_regex(r"^/messages/(text|media/[a-z]+)$")).respond_with(response).mount(&server).await;
    server
  }

  /// The message and media calls the server received, in order.
  async fn calls(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default().into_iter().filter(|r| r.method.as_str() == "POST" && r.url.path().starts_with("/messages/")).collect()
  }

  fn paths(calls: &[Request]) -> Vec<String> {
    calls.iter().map(|r| r.url.path().to_string()).collect()
  }

  fn json_body(r: &Request) -> Value {
    serde_json::from_slice(&r.body).unwrap()
  }

  fn query(r: &Request) -> Vec<(String, String)> {
    r.url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
  }

  fn header<'a>(r: &'a Request, name: &str) -> &'a str {
    r.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
  }

  fn ctx(title: &str, body: &str, body_format: NotifyFormat) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), body_format, ..Default::default() }
  }

  fn attachment(name: &str, mime_type: &str) -> Attachment {
    Attachment { name: name.into(), data: format!("data:{name}").into_bytes(), mime_type: mime_type.into() }
  }

  /// Send `ctx` through the whole pipeline to `url`.
  async fn notify(server: &MockServer, url: &str, ctx: &NotifyContext) -> bool {
    let mut a = Apprise::new();
    assert!(a.add(url), "{url}");
    with_test_base(server.uri(), a.notify(ctx)).await.is_success()
  }

  #[test]
  fn test_urls() {
    for url in [
      // Nothing specified
      "whapi://".to_string(),
      // An invalid token
      "whapi://%20/15551234567".to_string(),
      format!("whapi://{TOKEN}!/15551234567"),
      "whapi://?token=bad!token&to=15551234567".to_string(),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {url}");
    }

    // No targets still loads; there is just nothing to notify
    assert!(whapi(&format!("whapi://{TOKEN}/")).targets.is_empty());

    let w = whapi(&format!("whapi://{TOKEN}/15551234567"));
    assert_eq!((w.token.as_str(), w.targets.as_slice(), w.preview), (TOKEN, &["15551234567".to_string()][..], true));

    // A phone, a group and a WhatsApp Channel
    let w = whapi(&format!("whapi://{TOKEN}/15551234567/{GROUP}/abc@newsletter?preview=no"));
    assert_eq!(w.targets, vec!["15551234567".to_string(), format!("{GROUP}@g.us"), "abc@newsletter".into()]);
    assert!(!w.preview);

    // Everything given as arguments
    let w = whapi(&format!("whapi://?token={TOKEN}&to=15551234567,{GROUP}"));
    assert_eq!(w.token, TOKEN);
    assert_eq!(w.targets, vec!["15551234567".to_string(), format!("{GROUP}@g.us")]);

    // ?token= wins over the host
    assert_eq!(whapi(&format!("whapi://other/15551234567?token={TOKEN}")).token, TOKEN);
    // An empty ?preview= is false, as upstream's parse_bool() makes it
    assert!(!whapi(&format!("whapi://{TOKEN}/15551234567?preview=")).preview);
    assert!(whapi(&format!("whapi://{TOKEN}/15551234567?preview=yes")).preview);
  }

  #[test]
  fn test_targets() {
    // Each kind of target becomes the chat ID Whapi.Cloud wants
    let path = ["+1 (555) 123-4567", GROUP, &format!("%23{GROUP}"), "1234567890-1234567890", &format!("{GROUP}@g.us"), "123456789@newsletter", "abc"];
    let w = whapi(&format!("whapi://{TOKEN}/{}", path.join("/").replace(' ', "%20")));
    assert_eq!(
      w.targets,
      vec![
        "15551234567".to_string(),
        format!("{GROUP}@g.us"),
        format!("{GROUP}@g.us"),
        "1234567890-1234567890@g.us".into(),
        format!("{GROUP}@g.us"),
        "123456789@newsletter".into(),
      ]
    );

    // A group given with a leading # in ?to= loses its prefix cleanly
    assert_eq!(whapi(&format!("whapi://{TOKEN}?to=%23{GROUP}")).targets, vec![format!("{GROUP}@g.us")]);

    assert!(is_chat_id("120363@g.us") && is_chat_id("a.b@NewsLetter"));
    assert!(!is_chat_id("@g.us") && !is_chat_id("a@") && !is_chat_id("a/b@g.us") && !is_chat_id("a b@g.us") && !is_chat_id("a@b@c"));
    assert!(is_group_id(&"1".repeat(16)) && !is_group_id(&"1".repeat(15)));
    assert!(is_group_id("1234567890-1234567890") && !is_group_id("123456789-1234567890") && !is_group_id("1234567890-"));
    assert_eq!(validate_token("abc\n"), Some("abc".into()));
    assert_eq!(validate_token(" abc"), None);
  }

  #[test]
  fn test_details() {
    let w = whapi(&format!("whapi://{TOKEN}/15551234567"));
    assert_eq!((w.title_maxlen(), w.body_maxlen(), w.request_rate_per_sec(), w.notify_format()), (0, 4096, 5.5, NotifyFormat::Markdown));
    assert!(w.attachment_support() && w.has_dialect());
    assert_eq!(w.schemas(), &["whapi"]);
    // Formats WhatsApp does not translate are left alone
    assert_eq!(w.dialect_convert("*bold*", &NotifyFormat::Html), "*bold*");

    // The Channel token alone identifies the connection, as upstream
    let id = w.url_identifier();
    assert_eq!(id, Some(vec!["whapi".to_string(), TOKEN.to_string()]));
    assert_eq!(whapi(&format!("whapi://?token={TOKEN}&to={GROUP}&preview=no")).url_identifier(), id);
    assert_ne!(whapi("whapi://other/15551234567").url_identifier(), id);
  }

  #[tokio::test]
  async fn test_url_tests() {
    // Upstream's AppriseURLTester cases that send
    for url in [
      format!("whapi://{TOKEN}/15551234567"),
      format!("whapi://{TOKEN}/15551234567/{GROUP}/abc@newsletter?preview=no"),
      format!("whapi://?token={TOKEN}&to=15551234567,{GROUP}"),
    ] {
      let server = server_with(ok()).await;
      assert!(notify(&server, &url, &ctx("title", "body", NotifyFormat::Text)).await, "{url}");
    }

    // No targets: nothing is sent and the notification fails
    let server = server_with(ok()).await;
    assert!(!notify(&server, &format!("whapi://{TOKEN}/"), &ctx("", "body", NotifyFormat::Text)).await);
    assert!(calls(&server).await.is_empty());

    for status in [999, 500] {
      let server = server_with(ResponseTemplate::new(status)).await;
      assert!(!notify(&server, &format!("whapi://{TOKEN}/15551234567"), &ctx("", "body", NotifyFormat::Text)).await, "{status}");
    }

    // A connection error
    let svc = whapi(&format!("whapi://{TOKEN}/15551234567"));
    assert!(!with_test_base("http://127.0.0.1:1", svc.send(&ctx("", "body", NotifyFormat::Text))).await.unwrap());
  }

  #[tokio::test]
  async fn test_text_payload() {
    // Verified against upstream with pycapture.py
    let server = server_with(ok()).await;
    let url = format!("whapi://{TOKEN}/15551234567?preview=no");
    assert!(notify(&server, &url, &ctx("A & B <c>", "**bold** & <tag> _x_", NotifyFormat::Markdown)).await);
    let sent = calls(&server).await;
    assert_eq!(paths(&sent), vec!["/messages/text"]);
    let r = &sent[0];
    assert_eq!(header(r, "authorization"), format!("Bearer {TOKEN}"));
    assert_eq!(header(r, "content-type"), "application/json");
    assert_eq!(header(r, "accept"), "application/json");
    assert_eq!(header(r, "user-agent"), APP_ID);
    assert!(query(r).is_empty());
    // The title becomes a bold first line and Markdown becomes WhatsApp's
    assert_eq!(json_body(r), json!({ "to": "15551234567", "body": "*A & B <c>*\n*bold* & <tag> _x_", "no_link_preview": true }));

    // Plain text keeps every character exactly as typed
    let server = server_with(ok()).await;
    assert!(notify(&server, &url, &ctx("", "1 * 2 _ 3 . 4 ! # <", NotifyFormat::Text)).await);
    assert_eq!(json_body(&calls(&server).await[0])["body"], "1 * 2 _ 3 . 4 ! # <");

    // Undeclared content is passed along untouched
    let server = server_with(ok()).await;
    let passthrough = NotifyContext { body_passthrough: true, ..ctx("", "**as is**", NotifyFormat::Text) };
    assert!(notify(&server, &format!("whapi://{TOKEN}/15551234567"), &passthrough).await);
    assert_eq!(json_body(&calls(&server).await[0]), json!({ "to": "15551234567", "body": "**as is**" }));
  }

  #[tokio::test]
  async fn test_responses() {
    let svc = whapi(&format!("whapi://{TOKEN}/15551234567"));
    let send = |response: ResponseTemplate| {
      let svc = &svc;
      async move {
        let server = server_with(response).await;
        with_test_base(server.uri(), svc.send(&ctx("", "test", NotifyFormat::Text))).await.unwrap()
      }
    };

    // A 200 that says nothing was sent
    assert!(!send(ResponseTemplate::new(200).set_body_string(r#"{"sent": false}"#)).await);
    // An error with Whapi.Cloud's own explanation
    assert!(!send(ResponseTemplate::new(401).set_body_string(r#"{"error": {"code": 401, "message": "Unauthorized"}}"#)).await);
    // An error with an unexpected shape
    assert!(!send(ResponseTemplate::new(402).set_body_string(r#"{"error": "nope"}"#)).await);
    // A 2xx answer only counts when it confirms the message was sent
    for content in ["", "<html></html>", "[]", "{}", r#"{"sent": 1}"#, r#"{"sent": "true"}"#] {
      assert!(!send(ResponseTemplate::new(200).set_body_string(content)).await, "{content:?}");
    }
    // Any 2xx code is accepted when the message was sent
    assert!(send(ResponseTemplate::new(201).set_body_string(OK)).await);
    assert!(send(ok()).await);
  }

  #[tokio::test]
  async fn test_partial_failure() {
    // One failing target does not stop the others
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages/text")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/messages/text")).respond_with(ok()).mount(&server).await;
    assert!(!notify(&server, &format!("whapi://{TOKEN}/15551234567/{GROUP}"), &ctx("", "test", NotifyFormat::Text)).await);
    let sent = calls(&server).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(json_body(&sent[1])["to"], format!("{GROUP}@g.us"));
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    // Upstream test_delivery_tracking_fleet: only the failing target is
    // contacted again
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/messages/text"))
      .and(|r: &Request| String::from_utf8_lossy(&r.body).contains("12125559999"))
      .respond_with(ResponseTemplate::new(500))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/messages/text")).respond_with(ok()).mount(&server).await;
    let url = format!("whapi://{}/12125550001/12125559999/?retry=2&wait=0", "a".repeat(32));
    assert!(!notify(&server, &url, &ctx("", "test", NotifyFormat::Text)).await);
    let to: Vec<Value> = calls(&server).await.iter().map(|r| json_body(r)["to"].clone()).collect();
    assert_eq!(to.iter().filter(|t| *t == "12125550001").count(), 1);
    assert_eq!(to.iter().filter(|t| *t == "12125559999").count(), 3);
  }

  #[tokio::test]
  async fn test_attachments() {
    // Files are posted raw to the media call matching their type, with
    // their type detected from the file name
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<String> = ["apprise-test.png", "apprise-test.mp4", "apprise-test.gif"]
      .iter()
      .map(|name| {
        let p = dir.path().join(name);
        std::fs::write(&p, format!("data:{name}")).unwrap();
        p.to_string_lossy().into_owned()
      })
      .collect();
    let server = server_with(ok()).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("whapi://{TOKEN}/15551234567")));
    assert!(with_test_base(server.uri(), a.notify_with_attachments(&ctx("", "test", NotifyFormat::Text), &files)).await.is_success());

    // Our text, then one call per file
    let sent = calls(&server).await;
    assert_eq!(paths(&sent), vec!["/messages/text", "/messages/media/image", "/messages/media/video", "/messages/media/document"]);
    // Who the file goes to travels in the query string
    assert_eq!(query(&sent[1]), vec![("to".to_string(), "15551234567".to_string())]);
    assert_eq!(header(&sent[1], "content-type"), "image/png");
    assert_eq!(header(&sent[1], "authorization"), format!("Bearer {TOKEN}"));
    assert_eq!(header(&sent[1], "accept"), "application/json");
    assert_eq!(sent[1].body, b"data:apprise-test.png");
    assert_eq!(header(&sent[2], "content-type"), "video/mp4");
    // A document keeps its file name
    assert_eq!(query(&sent[3]), vec![("to".to_string(), "15551234567".to_string()), ("filename".to_string(), "apprise-test.gif".to_string())]);
    assert_eq!(header(&sent[3], "content-type"), "image/gif");

    // An audio file is sent as audio
    let server = server_with(ok()).await;
    let audio = NotifyContext { attachments: vec![attachment("a.mp3", "audio/mpeg")], ..ctx("", "test", NotifyFormat::Text) };
    assert!(notify(&server, &format!("whapi://{TOKEN}/15551234567"), &audio).await);
    assert_eq!(paths(&calls(&server).await), vec!["/messages/text", "/messages/media/audio"]);

    // Attachments alone send no text
    let server = server_with(ok()).await;
    let alone = NotifyContext { attachments: vec![attachment("a.png", "image/png")], ..ctx("", "", NotifyFormat::Text) };
    assert!(notify(&server, &format!("whapi://{TOKEN}/15551234567"), &alone).await);
    assert_eq!(paths(&calls(&server).await), vec!["/messages/media/image"]);

    // A document without a name gets a default one
    assert_eq!((media_kind("image/jpeg"), media_kind("video/3gpp"), media_kind("application/pdf")), ("image", "video", "document"));
    let server = server_with(ok()).await;
    let svc = whapi(&format!("whapi://{TOKEN}/15551234567"));
    let unnamed = NotifyContext { attachments: vec![attachment("", "application/pdf")], ..ctx("", "", NotifyFormat::Text) };
    assert!(with_test_base(server.uri(), svc.send(&unnamed)).await.unwrap());
    assert_eq!(query(&calls(&server).await[0])[1], ("filename".to_string(), "attachment.dat".to_string()));
  }

  #[tokio::test]
  async fn test_attach_errors() {
    let url = format!("whapi://{TOKEN}/15551234567");
    let png = || NotifyContext { attachments: vec![attachment("apprise-test.png", "image/png")], ..ctx("", "", NotifyFormat::Text) };

    // An inaccessible file is never posted
    let server = server_with(ok()).await;
    let mut a = Apprise::new();
    assert!(a.add(&url));
    let missing = vec!["/nonexistent/whapi/apprise-test.png".to_string()];
    assert!(!with_test_base(server.uri(), a.notify_with_attachments(&ctx("", "", NotifyFormat::Text), &missing)).await.is_success());
    assert!(calls(&server).await.is_empty());

    // The media call is refused
    let server = server_with(ResponseTemplate::new(500)).await;
    assert!(!notify(&server, &url, &png()).await);
    assert_eq!(calls(&server).await.len(), 1);

    // The media call raises a connection error
    let svc = whapi(&url);
    assert!(!with_test_base("http://127.0.0.1:1", svc.send(&png())).await.unwrap());

    // A refused text message is not followed by its files
    let server = server_with(ResponseTemplate::new(500)).await;
    let with_text = NotifyContext { body: "test".into(), ..png() };
    assert!(!notify(&server, &url, &with_text).await);
    assert_eq!(paths(&calls(&server).await), vec!["/messages/text"]);

    // A refused file holds back the ones after it
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages/media/image")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    Mock::given(method("POST")).and(path_regex(r"^/messages/")).respond_with(ok()).mount(&server).await;
    let two = NotifyContext { attachments: vec![attachment("a.png", "image/png"), attachment("b.gif", "image/gif")], ..png() };
    assert!(!with_test_base(server.uri(), svc.send(&two)).await.unwrap());
    assert_eq!(paths(&calls(&server).await), vec!["/messages/media/image"]);
  }

  #[tokio::test]
  async fn test_attach_retry() {
    // A retry sends only the file that failed, not the text again
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages/media/document")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).and(path_regex(r"^/messages/")).respond_with(ok()).mount(&server).await;
    let c = NotifyContext {
      attachments: vec![attachment("apprise-test.png", "image/png"), attachment("apprise-test.gif", "image/gif")],
      ..ctx("", "test", NotifyFormat::Text)
    };
    assert!(notify(&server, &format!("whapi://{TOKEN}/15551234567?retry=1&wait=0"), &c).await);
    assert_eq!(paths(&calls(&server).await), vec!["/messages/text", "/messages/media/image", "/messages/media/document", "/messages/media/document"]);
  }

  #[tokio::test]
  async fn test_html_to_whatsapp() {
    // HTML is delivered in WhatsApp's own formatting
    for (body, expected) in [
      ("<b>hello</b> <i>world</i>", "*hello* _world_"),
      ("<code>inline</code>", "```inline```"),
      ("<a href=\"https://example.com/x\">click here</a>", "click here (https://example.com/x)"),
      ("<p>literal * asterisk</p>", "literal * asterisk"),
    ] {
      let server = server_with(ok()).await;
      assert!(notify(&server, &format!("whapi://{TOKEN}/15551234567"), &ctx("", body, NotifyFormat::Html)).await);
      assert_eq!(json_body(&calls(&server).await[0])["body"], expected, "{body}");
    }
  }

  #[tokio::test]
  async fn test_formats_match_upstream() {
    // Captured from upstream through the library (ptcap.py), including a
    // call that declares no format
    let (title, body) = ("T <b>x</b>", "**as is** <i>y</i> 1 * 2 _ #");
    for (c, expected) in [
      (NotifyContext { body_passthrough: true, ..ctx(title, body, NotifyFormat::Text) }, "T <b>x</b>\r\n**as is** <i>y</i> 1 * 2 _ #"),
      (ctx(title, body, NotifyFormat::Text), "*T <b>x</b>*\n**as is** <i>y</i> 1 * 2 _ #"),
      (ctx(title, body, NotifyFormat::Html), "*T *x**\n**as is** _y_ 1 * 2 _ #"),
      (ctx(title, body, NotifyFormat::Markdown), "*T <b>x</b>*\n*as is* <i>y</i> 1 * 2 _ #"),
    ] {
      let server = server_with(ok()).await;
      assert!(notify(&server, &format!("whapi://{TOKEN}/15551234567"), &c).await);
      assert_eq!(json_body(&calls(&server).await[0]), json!({ "to": "15551234567", "body": expected }), "{:?}", c.body_format);
    }
  }

  #[test]
  fn test_markdown_dialect() {
    // CommonMark edge cases convert safely to WhatsApp formatting
    for (input, expected) in [
      // Headings become bold
      ("# Heading\nbody", "*Heading*\nbody"),
      // Unmatched and oversized backtick runs
      ("text ``unterminated", "text ``unterminated"),
      ("`` code```here ``", "``` code``here ```"),
      // Unmatched emphasis stays literal
      ("**unterminated", "**unterminated"),
      // Angle links, including escapes, empty labels and broken ones
      ("[click](<https://e/x\\>y>)", "click (https://e/x>y)"),
      ("[](<https://example.com/x>)", "https://example.com/x"),
      ("[text](<https://e/unterminated", "[text](<https://e/unterminated"),
      // Bare links, including broken ones
      ("[click here](https://a*b)", "click here (https://a*b)"),
      ("[label](has space)", "[label](has space)"),
      // A nested invalid link keeps the outer one working
      ("[OUTER [INNER](<bad)](https://e.com)", "OUTER [INNER](<bad) (https://e.com)"),
      // A stray "]" does not pair with a later, unrelated link
      ("[label] and ](https://e.com) end", "[label] and ](https://e.com) end"),
      // Autolinks lose their brackets; anything else stays literal
      ("see <https://a*b> now", "see https://a*b now"),
      ("a < b", "a < b"),
      ("see <https://a*b end", "see <https://a*b end"),
    ] {
      assert_eq!(f(input), expected, "input: {input:?}");
    }
  }

  #[test]
  fn test_deeply_nested_links() {
    // Deeply nested link labels convert quickly and keep their text
    let depth = 20000;
    let body = format!("{}x{}", "[".repeat(depth), "](u)".repeat(depth));
    let start = std::time::Instant::now();
    let result = f(&body);
    assert!(start.elapsed().as_secs_f64() < 15.0, "took {:?}", start.elapsed());
    // The innermost links still convert; the extra brackets stay as text
    assert!(result.starts_with(&"[".repeat(depth - crate::conversion::MAX_LINK_DEPTH)));
    assert!(result.contains("x (u)"));
  }
}
