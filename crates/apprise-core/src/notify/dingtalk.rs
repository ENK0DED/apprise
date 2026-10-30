use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

/// DingTalk custom robot webhook.
///
/// `dingtalk://{token}/`, `dingtalk://{secret}@{token}/{phone}/...`
pub struct DingTalk {
  token: String,
  secret: Option<String>,
  /// Phone numbers to @mention (digits only, 11 to 14 of them)
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

fn is_token(s: &str) -> bool {
  !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric())
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

/// Upstream `IS_PHONE_NO`: an optional `+`, then digits, spaces, `(`, `)`,
/// `+` and `-`. Returns the digits when there are 11 to 14 of them.
fn phone_digits(target: &str) -> Option<String> {
  let rest = target.strip_prefix('+').unwrap_or(target).trim_end();
  if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '(' | ')' | '+' | '-')) {
    return None;
  }
  let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
  (11..=14).contains(&digits.len()).then_some(digits)
}

/// Request signature for robots with signing enabled: the timestamp and the
/// percent-encoded base64 HMAC-SHA256 of `"{timestamp}\n{secret}"`.
fn signature(secret: &str, timestamp: &str) -> String {
  use base64::Engine;
  use hmac::{Hmac, KeyInit, Mac};
  let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
  mac.update(format!("{}\n{}", timestamp, secret).as_bytes());
  let digest = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
  urlencoding::encode(&digest).into_owned()
}

impl DingTalk {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let mut token = url.host.as_deref().map(|h| urlencoding::decode(h).map(|s| s.into_owned()).unwrap_or_default()).unwrap_or_default();
    if let Some(t) = url.get("token").filter(|t| !t.is_empty()) {
      token = t.to_string();
    }
    if !is_token(&token) {
      tracing::warn!("An invalid DingTalk API Token was specified.");
      return None;
    }

    // The user field (or ?secret=) holds the signing secret
    let mut secret = url.user.clone().filter(|s| !s.is_empty());
    if let Some(s) = url.get("secret").filter(|s| !s.is_empty()) {
      secret = Some(s.to_string());
    }
    if let Some(ref s) = secret
      && !is_token(s)
    {
      tracing::warn!("An invalid DingTalk Secret was specified.");
      return None;
    }

    let mut raw: Vec<&str> = url.path_parts.iter().map(String::as_str).collect();
    if let Some(to) = url.get("to") {
      raw.push(to);
    }
    let mut targets = Vec::new();
    for target in parse_list(raw) {
      match phone_digits(&target) {
        Some(digits) => targets.push(digits),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self { token, secret, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "DingTalk",
      service_url: Some("https://www.dingtalk.com/"),
      setup_url: Some("https://appriseit.com/services/dingtalk/"),
      protocols: vec!["dingtalk"],
      description: "Send via DingTalk robot webhook.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> serde_json::Value {
    let mut payload = json!({
      "msgtype": "text",
      "at": { "atMobiles": self.targets, "isAtAll": false },
    });
    if ctx.body_format == NotifyFormat::Markdown {
      // Keep the title on one line and drop any leading heading or list
      // markers. `markdown.title` is only a preview, so the title is also
      // repeated as a heading in `markdown.text`.
      let title = ctx.title.split_whitespace().collect::<Vec<_>>().join(" ");
      let title = title.trim_start_matches(['#', '-', ' ']);
      payload["msgtype"] = json!("markdown");
      payload["markdown"] = if title.is_empty() {
        // A title is mandatory for markdown messages
        json!({ "title": ctx.asset.app_desc, "text": ctx.body })
      } else {
        json!({ "title": title, "text": format!("# {}\n{}", title, ctx.body) })
      };
    } else {
      // Plain text has no title field, so the title goes on the first line
      let content = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\r\n{}", ctx.title, ctx.body) };
      payload["text"] = json!({ "content": content });
    }
    payload
  }
}

#[async_trait]
impl Notify for DingTalk {
  fn schemas(&self) -> &[&str] {
    &["dingtalk"]
  }
  fn service_name(&self) -> &str {
    "DingTalk"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    // DingTalk rejects requests over 20000 bytes; leave room for the rest
    // of the payload
    19985
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = format!("{}/robot/send", service_base("https://oapi.dingtalk.com"));
    let mut query = vec![("access_token", self.token.clone())];
    if let Some(ref secret) = self.secret {
      let timestamp = chrono::Utc::now().timestamp_millis().to_string();
      let sign = signature(secret, &timestamp);
      query.push(("timestamp", timestamp));
      query.push(("sign", sign));
    }

    let client = build_client(self.verify_certificate)?;
    let resp = client.post(&url).query(&query).header("User-Agent", APP_ID).json(&self.payload(ctx)).send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
      tracing::info!("Sent DingTalk notification.");
      Ok(true)
    } else {
      tracing::warn!("Failed to send DingTalk notification: error={}", status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path, query_param};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  fn parse(url: &str) -> DingTalk {
    DingTalk::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "dingtalk://12345678".to_string(),
      format!("dingtalk://{}/{}", "a".repeat(8), "1".repeat(14)),
      format!("dingtalk://{}/{}/invalid", "a".repeat(8), "1".repeat(3)),
      format!("dingtalk://{}/?to={}", "a".repeat(8), "1".repeat(14)),
      format!("dingtalk://secret@{}/?to={}", "a".repeat(8), "1".repeat(14)),
      format!("dingtalk://?token={}&to={}&secret={}", "b".repeat(8), "1".repeat(14), "a".repeat(15)),
      format!("dingtalk://{}?format=markdown", "a".repeat(8)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "dingtalk://".to_string(),
      "dingtalk://a_bd_/".to_string(),
      // Invalid secret
      format!("dingtalk://{}/?to={}&secret=_", "a".repeat(8), "1".repeat(14)),
      // Invalid secret via the user field
      format!("dingtalk://a_b@{}", "a".repeat(8)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let d = parse("dingtalk://12345678");
    assert_eq!(d.token, "12345678");
    assert!(d.secret.is_none());
    assert!(d.targets.is_empty());

    // ?token= and ?secret= override the host and user fields
    let d = parse("dingtalk://usersecret@hosttoken/?token=bbbbbbbb&secret=qsdsecret");
    assert_eq!(d.token, "bbbbbbbb");
    assert_eq!(d.secret.as_deref(), Some("qsdsecret"));

    let d = parse("dingtalk://usersecret@hosttoken");
    assert_eq!(d.secret.as_deref(), Some("usersecret"));
  }

  #[test]
  fn test_targets_are_validated_and_sorted() {
    // Invalid phone numbers are dropped; the rest keep only their digits,
    // ordered like upstream parse_list() (sorted on the raw value)
    let d = parse("dingtalk://aaaaaaaa/11111111111/+1(234)567-8901/bad/123?to=22222222222222,333");
    assert_eq!(d.targets, vec!["12345678901", "11111111111", "22222222222222"]);
    // More than 14 digits is too long
    let d = parse(&format!("dingtalk://aaaaaaaa/{}", "1".repeat(15)));
    assert!(d.targets.is_empty());
  }

  #[test]
  fn test_signature() {
    // Reference value from upstream get_signature() with a fixed timestamp
    assert_eq!(signature("secret", "1793334887411"), "e7Iidh6qWsXfRnw29hcHd1T5ActDzfZT3%2F18BEDo87M%3D");
  }

  #[test]
  fn test_service_details() {
    let details = DingTalk::static_details();
    assert_eq!(details.service_name, "DingTalk");
    assert_eq!(details.protocols, vec!["dingtalk"]);
    assert!(!details.attachment_support);
    assert_eq!(parse("dingtalk://aaaaaaaa").body_maxlen(), 19985);
    assert!(parse("dingtalk://aaaaaaaa").title_maxlen() > 0);
  }

  fn ctx(title: &str, body: &str, format: NotifyFormat) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), body_format: format, ..Default::default() }
  }

  async fn send_expecting(url: &str, ctx: &NotifyContext, expected: serde_json::Value) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/robot/send"))
      .and(query_param("access_token", "aaaaaaaa"))
      .and(header("Content-Type", "application/json"))
      .and(body_json(&expected))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(url).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_text() {
    // Matches pycapture: dingtalk://aaaaaaaa/11111111111/+1(234)567-8901/bad
    let expected = json!({
      "msgtype": "text",
      "at": { "atMobiles": ["12345678901", "11111111111"], "isAtAll": false },
      "text": { "content": "T\r\nB" },
    });
    send_expecting("dingtalk://aaaaaaaa/11111111111/+1(234)567-8901/bad", &ctx("T", "B", NotifyFormat::Text), expected).await;

    let expected = json!({ "msgtype": "text", "at": { "atMobiles": [], "isAtAll": false }, "text": { "content": "hello world" } });
    send_expecting("dingtalk://aaaaaaaa", &ctx("", "hello world", NotifyFormat::Text), expected).await;
  }

  #[tokio::test]
  async fn test_send_markdown() {
    let expected = json!({
      "msgtype": "markdown",
      "at": { "atMobiles": [], "isAtAll": false },
      "markdown": { "title": "My Title", "text": "# My Title\n**B**" },
    });
    send_expecting("dingtalk://aaaaaaaa", &ctx("## My   Title", "**B**", NotifyFormat::Markdown), expected).await;

    // A multi-line title is kept on a single heading line
    let expected = json!({
      "msgtype": "markdown",
      "at": { "atMobiles": [], "isAtAll": false },
      "markdown": { "title": "Build failed", "text": "# Build failed\nbody" },
    });
    send_expecting("dingtalk://aaaaaaaa", &ctx("## Build\nfailed", "body", NotifyFormat::Markdown), expected).await;

    // No title: the application description stands in, and no heading is added
    let expected = json!({
      "msgtype": "markdown",
      "at": { "atMobiles": [], "isAtAll": false },
      "markdown": { "title": "Apprise Notifications", "text": "body" },
    });
    send_expecting("dingtalk://aaaaaaaa", &ctx("", "body", NotifyFormat::Markdown), expected).await;
  }

  #[tokio::test]
  async fn test_send_signed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/robot/send"))
      .and(query_param("access_token", "aaaaaaaa"))
      .respond_with(|req: &Request| {
        let q: std::collections::HashMap<String, String> = req.url.query_pairs().into_owned().collect();
        // The sign value is percent-encoded once more by the query string,
        // exactly as upstream passes its already-quoted signature to requests
        let ok = q.get("sign").map(|s| s.as_str()) == Some(signature("secret", &q["timestamp"]).as_str());
        ResponseTemplate::new(if ok { 200 } else { 400 })
      })
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("dingtalk://secret@aaaaaaaa").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyFormat::Text))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(999)).mount(&server).await;
    let svc = from_url("dingtalk://aaaaaaaa").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyFormat::Text))).await.is_err());

    // Only HTTP 200 counts as delivered
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(204)).mount(&server).await;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyFormat::Text))).await.is_err());
  }
}
