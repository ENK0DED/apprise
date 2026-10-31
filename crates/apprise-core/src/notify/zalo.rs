use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::parse_list;
use crate::utils::parse::{ParsedUrl, unquote};

/// Zalo Bot messages (`zalo://{bot_token}/{chat_ids}`).
pub struct Zalo {
  /// Bot Token (`{bot_id}:{secret}`)
  token: String,
  /// Chat IDs to notify
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// `^[0-9]+:[A-Z0-9_-]+$` (case-insensitive)
fn is_bot_token(token: &str) -> bool {
  token.split_once(':').is_some_and(|(id, secret)| {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) && !secret.is_empty() && secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
  })
}

/// `^[A-Z0-9._-]+$` (case-insensitive)
fn is_chat_id(target: &str) -> bool {
  !target.is_empty() && target.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Upstream's rewrite of `{schema}://{bot_id}:` (or `%3A`) to
/// `{schema}://{bot_id}/` before parsing. The colon inside the Bot Token
/// looks like a port to a URL parser, so the token is split across the
/// host and the first path entry instead. `None` when the URL does not
/// start that way.
pub(crate) fn token_colon_to_slash(url: &str) -> Option<String> {
  let (schema, rest) = url.split_once("://")?;
  if schema.is_empty() || !schema.chars().all(|c| c.is_ascii_alphanumeric()) {
    return None;
  }
  let id = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
  if id == 0 {
    return None;
  }
  let after = &rest[id..];
  let secret = after.strip_prefix(':').or_else(|| after.get(..3).filter(|p| p.eq_ignore_ascii_case("%3A")).map(|_| &after[3..]))?;
  Some(format!("{}://{}/{}", schema, &rest[..id], secret))
}

impl Zalo {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The registry applies token_colon_to_slash() before parsing. A URL
    // parsed without it has the secret as a port (leading zeros lost) or
    // left in the host, so parse it again the upstream way.
    let reparsed;
    let url = match token_colon_to_slash(&url.raw) {
      Some(raw) if url.port.is_some() || url.host.as_deref().is_some_and(|h| h.contains(':')) => {
        let mut p = ParsedUrl::parse(&raw)?;
        p.assigned_tags = url.assigned_tags.clone();
        reparsed = p;
        &reparsed
      }
      _ => url,
    };
    let host = url.host.clone().unwrap_or_default();
    let mut entries = url.path_parts.clone();

    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(token) => {
        // The whole Bot Token was given as ?token=, so the host and path
        // are all Chat IDs
        if !host.is_empty() {
          entries.insert(0, host);
        }
        // Upstream unquotes ?token= once more
        unquote(token)
      }
      None => {
        let secret = if entries.is_empty() { String::new() } else { entries.remove(0) };
        format!("{}:{}", host, secret)
      }
    };

    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      // Upstream's URLBase.parse_list() also unquotes each entry
      entries.extend(parse_list(&[to]).iter().map(|t| unquote(t)));
    }

    if !is_bot_token(&token) {
      tracing::warn!("An invalid Zalo Bot Token was specified.");
      return None;
    }

    let mut targets = Vec::new();
    for target in parse_list(&entries) {
      if is_chat_id(&target) {
        targets.push(target);
      } else {
        tracing::warn!("Dropped invalid Zalo Chat ID ({}) specified.", target);
      }
    }

    Some(Self { token, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Zalo",
      service_url: Some("https://zalo.me/"),
      setup_url: Some("https://appriseit.com/services/zalo/"),
      protocols: vec!["zalo"],
      description: "Send messages to Zalo chats through a Zalo Bot.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Zalo {
  fn schemas(&self) -> &[&str] {
    &["zalo"]
  }
  fn service_name(&self) -> &str {
    "Zalo"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  /// Upstream accepts Markdown or plain text; HTML (with no line break tag
  /// on Zalo) is converted to Markdown.
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![NotifyFormat::Markdown, NotifyFormat::Text]
  }
  fn body_maxlen(&self) -> usize {
    2000
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Zalo Chat IDs to notify.");
      return Ok(false);
    }

    let url = format!("{}/bot{}/sendMessage", service_base("https://bot-api.zaloplatforms.com"), self.token);
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;

    let mut payload = json!({ "text": ctx.body });
    // Ask Zalo to style the text when it is Markdown
    if ctx.body_format == NotifyFormat::Markdown {
      payload["parse_mode"] = json!("markdown");
    }

    for target in &self.targets {
      // A retry skips targets that already accepted the message

      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      payload["chat_id"] = json!(target);

      crate::notify::throttle().await;
      let result =
        client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json; charset=utf-8").body(payload.to_string()).send().await;

      match result {
        Ok(resp) => {
          let status = resp.status();
          let content: Value = resp.json().await.unwrap_or(Value::Null);
          // Zalo reports a refused message with "ok" set to false
          if status != reqwest::StatusCode::OK || content.get("ok") == Some(&Value::Bool(false)) {
            tracing::warn!("Failed to send Zalo notification to {}: error={}.", target, status.as_u16());
            tracing::debug!("Response Details: {}", content);
            has_error = true;
          } else {
            tracing::info!("Sent Zalo notification to {}.", target);
            crate::delivery::mark_delivered(target, false);
          }
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Zalo notification to {}.", target);
          // The request URL holds the Bot Token, so never log it
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
  use wiremock::matchers::{body_json, body_partial_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const TOKEN: &str = "123456789:abcdefghijKLMNOPqrst";

  fn parse(url: &str) -> Option<Zalo> {
    Zalo::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str, body_format: NotifyFormat) -> NotifyContext {
    NotifyContext { body: body.into(), body_format, ..Default::default() }
  }

  fn ok_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {"message_id": "82599fa32f", "date": 1749632637}}))
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "zalo://",
      // Only half of a token
      "zalo://123456789",
      // The bot id must be numeric
      "zalo://abc:def/chat1",
      // A secret with a space
      "zalo://?token=123:bad%20token",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    // No Chat IDs: loads, but has nothing to notify
    assert!(parse(&format!("zalo://{}", TOKEN)).unwrap().targets.is_empty());

    let obj = parse(&format!("zalo://{}/chat1", TOKEN)).unwrap();
    assert_eq!(obj.token, TOKEN);
    assert_eq!(obj.targets, vec!["chat1"]);

    // An encoded colon is accepted too
    let obj = parse("zalo://123456789%3Aabcdefghij/chat1/chat.2/").unwrap();
    assert_eq!(obj.token, "123456789:abcdefghij");
    assert_eq!(obj.targets, vec!["chat.2", "chat1"]);

    // A slash in place of the colon is accepted too
    let obj = parse("zalo://123456789/abcdefghij/chat1/").unwrap();
    assert_eq!(obj.token, "123456789:abcdefghij");
    assert_eq!(obj.targets, vec!["chat1"]);

    // A numeric secret is read as a port by the URL parser
    let obj = parse("zalo://12345:6789/chat1").unwrap();
    assert_eq!(obj.token, "12345:6789");
    assert_eq!(obj.targets, vec!["chat1"]);

    // Token and Chat IDs given as arguments
    let obj = parse(&format!("zalo://chat1/?token={}&to=chat2,chat3", TOKEN)).unwrap();
    assert_eq!(obj.token, TOKEN);
    assert_eq!(obj.targets, vec!["chat1", "chat2", "chat3"]);

    // No host at all
    let obj = parse(&format!("zalo://?token={}&to=chat1", TOKEN)).unwrap();
    assert_eq!(obj.targets, vec!["chat1"]);

    // Bad Chat IDs are dropped
    let obj = parse(&format!("zalo://{}/chat1/invalid!id", TOKEN)).unwrap();
    assert_eq!(obj.targets, vec!["chat1"]);
    assert!(parse(&format!("zalo://{}/invalid!id", TOKEN)).unwrap().targets.is_empty());

    // Chat IDs are de-duplicated and sorted, as upstream's parse_list() does
    let obj = parse(&format!("zalo://{}/chat1/a.b-c_d/chat1", TOKEN)).unwrap();
    assert_eq!(obj.targets, vec!["a.b-c_d", "chat1"]);
  }

  #[test]
  fn test_attributes() {
    let svc = from_url(&format!("zalo://{}/chat1", TOKEN)).unwrap();
    assert_eq!(svc.notify_format(), NotifyFormat::Markdown);
    assert_eq!(svc.notify_formats(), vec![NotifyFormat::Markdown, NotifyFormat::Text]);
    assert_eq!(svc.body_maxlen(), 2000);
    assert_eq!(svc.title_maxlen(), 0);
    assert!(!svc.attachment_support());
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    for chat in ["chatA", "chatB"] {
      Mock::given(method("POST"))
        .and(path("/bot12345:secret-key/sendMessage"))
        .and(header("User-Agent", APP_ID))
        .and(header("Content-Type", "application/json; charset=utf-8"))
        .and(body_json(json!({"text": "# Title\n**Body**", "parse_mode": "markdown", "chat_id": chat})))
        .respond_with(ok_response())
        .expect(1)
        .mount(&server)
        .await;
    }

    let svc = from_url("zalo://12345:secret-key/chatA/chatB").unwrap();
    let ok = crate::notify::with_test_base(server.uri(), svc.send(&ctx("# Title\n**Body**", NotifyFormat::Markdown))).await.unwrap();
    assert!(ok);
  }

  #[test]
  fn test_numeric_secret_is_not_a_port() {
    assert_eq!(token_colon_to_slash("zalo://123:00123/chat").as_deref(), Some("zalo://123/00123/chat"));
    assert_eq!(token_colon_to_slash("ZALO://123%3a456").as_deref(), Some("ZALO://123/456"));
    for url in ["zalo://abc:123/chat", "zalo://123/456", "zalo://?token=1:2", "za-lo://123:456", "zalo:123:456"] {
      assert_eq!(token_colon_to_slash(url), None, "{url}");
    }

    let token = |url: &str| Zalo::from_url(&crate::notify::registry::parse_service_url(url).unwrap()).unwrap().token;
    for (url, expected) in [
      // pycapture.py sends these to /bot{expected}/sendMessage
      ("zalo://123456789:00123/chat", "123456789:00123"),
      ("zalo://123456789:65536/chat", "123456789:65536"),
      ("zalo://123456789%3a00123/chat", "123456789:00123"),
      ("zalo://123456789:00123/chat?port=7", "123456789:00123"),
      ("zalo://123456789/abcdef/chat?port=5", "123456789:abcdef"),
    ] {
      assert_eq!(token(url), expected, "{url}");
      // Parsed without the registry's rewrite, the plugin applies it itself
      if let Some(parsed) = ParsedUrl::parse(url) {
        assert_eq!(Zalo::from_url(&parsed).unwrap().token, expected, "{url}");
      }
    }

    // Re-parsing keeps the tags whatever loaded the URL assigned
    let mut url = ParsedUrl::parse("zalo://123456789:00123/chat").unwrap();
    url.assigned_tags = Some(vec!["mytag".to_string()]);
    assert_eq!(Zalo::from_url(&url).unwrap().tags(), vec!["mytag".to_string()]);
  }

  #[tokio::test]
  async fn test_send_leading_zero_secret() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/bot123456789:00123/sendMessage")).respond_with(ok_response()).expect(1).mount(&server).await;
    let svc = from_url("zalo://123456789:00123/chat").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("hi", NotifyFormat::Text))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_text_has_no_parse_mode() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/bot12345:secret-key/sendMessage"))
      .and(body_json(json!({"text": "Title\r\nBody *literal*", "chat_id": "chatA"})))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("zalo://12345%3Asecret-key/chatA").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Title\r\nBody *literal*", NotifyFormat::Text))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    // No Chat IDs
    let svc = from_url(&format!("zalo://{}", TOKEN)).unwrap();
    assert!(!svc.send(&ctx("body", NotifyFormat::Markdown)).await.unwrap());

    for response in [
      // Zalo refused the message
      ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error_code": 400, "description": "Bad Request"})),
      ResponseTemplate::new(500),
      ResponseTemplate::new(999),
      ResponseTemplate::new(401),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(format!("/bot{}/sendMessage", TOKEN))).respond_with(response).expect(1).mount(&server).await;
      let svc = from_url(&format!("zalo://{}/chat1", TOKEN)).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body", NotifyFormat::Markdown))).await.unwrap());
    }

    // An empty or non-JSON body with a 200 is still a success
    for response in [ResponseTemplate::new(200), ResponseTemplate::new(200).set_body_string("not json")] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(format!("/bot{}/sendMessage", TOKEN))).respond_with(response).expect(1).mount(&server).await;
      let svc = from_url(&format!("zalo://{}/chat1", TOKEN)).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body", NotifyFormat::Markdown))).await.unwrap());
    }

    // One refused Chat ID fails the send, but the other is still notified
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TOKEN)))
      .and(body_json(json!({"text": "body", "parse_mode": "markdown", "chat_id": "chat1"})))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TOKEN)))
      .and(body_json(json!({"text": "body", "parse_mode": "markdown", "chat_id": "chat2"})))
      .respond_with(ResponseTemplate::new(401))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("zalo://{}/chat1/chat2", TOKEN)).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body", NotifyFormat::Markdown))).await.unwrap());

    let svc = from_url(&format!("zalo://{}/chat1", TOKEN)).unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("body", NotifyFormat::Markdown))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second chat fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path(format!("/bot{}/sendMessage", TOKEN)))
      .and(body_partial_json(json!({"chat_id": "chat2"})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(format!("/bot{}/sendMessage", TOKEN))).respond_with(ok_response()).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("zalo://{}/chat1/chat2?retry=1&wait=0", TOKEN)));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("body", NotifyFormat::Markdown))).await.is_success());
    let sent: Vec<Value> = crate::notify::custom_json::requests_to(&server, "POST", &format!("/bot{}/sendMessage", TOKEN))
      .await
      .iter()
      .map(|r| r.body_json::<Value>().unwrap()["chat_id"].clone())
      .collect();
    assert_eq!(sent, vec![json!("chat1"), json!("chat2"), json!("chat2")]);
  }
}
