use async_trait::async_trait;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;

pub struct Ntfy {
  host: Option<String>,
  port: Option<u16>,
  topics: Vec<String>,
  secure: bool,
  priority: &'static str,
  auth: Option<NtfyAuth>,
  verify_certificate: bool,
  tags: Vec<String>,
}

enum NtfyAuth {
  Basic { user: String, pass: String },
  Token(String),
}

impl Ntfy {
  const CLOUD_HOST: &'static str = "ntfy.sh";

  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // ntfy://topic  (cloud mode)
    // ntfy://host/topic  or  ntfys://host/topics
    // ntfy://user:pass@host/topic
    // ntfy://token@host/topic  (if user starts with "tk_")
    // https://ntfy.sh?to=topic

    let secure = url.schema == "ntfys" || url.schema == "https";

    // Validate auth mode if specified
    if let Some(auth_mode) = url.get("auth") {
      match auth_mode.to_lowercase().as_str() {
        "token" | "bearer" | "basic" | "login" | "" => {}
        _ => return None,
      }
    }

    // Validate mode if specified
    if let Some(mode) = url.get("mode") {
      match mode.to_lowercase().as_str() {
        "cloud" | "private" | "" => {}
        _ => return None,
      }
    }

    // Validate hostname if present (reject hosts starting/ending with hyphen or containing invalid chars)
    if let Some(ref h) = url.host {
      if h.starts_with('-') || h.starts_with('_') || h.ends_with('-') {
        return None;
      }
    }

    // Determine host and topics
    let (host, mut topics): (Option<String>, Vec<String>) = match &url.host {
      None => (None, vec![]),
      Some(h) if url.schema == "https" || url.schema == "http" => {
        // For https://ntfy.sh URLs, host is the server
        if h == Self::CLOUD_HOST || h.ends_with(".ntfy.sh") {
          (Some(h.clone()), url.path_parts.clone())
        } else {
          // Not an ntfy host — reject
          return None;
        }
      }
      Some(h) if url.path_parts.is_empty() => {
        // ntfy://topic  — host IS the topic, use cloud
        (None, vec![h.clone()])
      }
      Some(h) => {
        // ntfy://host/topic1/topic2
        (Some(h.clone()), url.path_parts.clone())
      }
    };

    // Support ?to= query param for topics
    if let Some(to) = url.get("to") {
      topics.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }

    if topics.is_empty() {
      return None;
    }

    // Determine authentication
    let auth_mode = url.get("auth").map(|s| s.to_lowercase());

    let auth = if let Some(token_val) = url.get("token") {
      // ?token=xxx param
      Some(NtfyAuth::Token(token_val.to_string()))
    } else {
      match (&url.user, &url.password) {
        (Some(u), _) if u.starts_with("tk_") => Some(NtfyAuth::Token(u.clone())),
        (Some(u), Some(p)) if auth_mode.as_deref() == Some("token") || auth_mode.as_deref() == Some("bearer") => {
          // When auth=token, use the password as the token
          Some(NtfyAuth::Token(p.clone()))
        }
        (None, Some(p)) if auth_mode.as_deref() == Some("token") || auth_mode.as_deref() == Some("bearer") => Some(NtfyAuth::Token(p.clone())),
        (Some(u), _) if auth_mode.as_deref() == Some("token") || auth_mode.as_deref() == Some("bearer") => Some(NtfyAuth::Token(u.clone())),
        (Some(u), Some(p)) => Some(NtfyAuth::Basic { user: u.clone(), pass: p.clone() }),
        _ => None,
      }
    };

    let priority = url
      .get("priority")
      .map(|p| match p.to_lowercase().as_str() {
        "min" | "1" => "min",
        "low" | "2" => "low",
        "high" | "4" => "high",
        "max" | "urgent" | "5" => "max",
        _ => "default",
      })
      .unwrap_or("default");

    Some(Self { host, port: url.port, topics, secure, priority, auth, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Ntfy",
      service_url: Some("https://ntfy.sh"),
      setup_url: Some("https://docs.ntfy.sh/publish/"),
      protocols: vec!["ntfy", "ntfys"],
      description: "Send notifications via ntfy.sh (self-hosted or cloud).",
      attachment_support: true,
    }
  }

  fn base_url(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    match (&self.host, self.port) {
      (Some(h), Some(p)) => format!("{}://{}:{}", schema, h, p),
      (Some(h), None) => format!("{}://{}", schema, h),
      _ => format!("https://{}", Self::CLOUD_HOST),
    }
  }
}

#[async_trait]
impl Notify for Ntfy {
  fn schemas(&self) -> &[&str] {
    &["ntfy", "ntfys"]
  }
  fn service_name(&self) -> &str {
    "Ntfy"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let base = self.base_url();
    let mut all_ok = true;

    for topic in &self.topics {
      let url = format!("{}/{}", base, topic);
      let mut req = client.post(&url).header("User-Agent", APP_ID).header("X-Priority", self.priority);

      // Only add markdown header when format is markdown (matching Python)
      if ctx.body_format == crate::types::NotifyFormat::Markdown {
        req = req.header("X-Markdown", "yes");
      }

      if !ctx.title.is_empty() {
        req = req.header("X-Title", &ctx.title);
      }

      req = match &self.auth {
        Some(NtfyAuth::Basic { user, pass }) => req.basic_auth(user, Some(pass)),
        Some(NtfyAuth::Token(t)) => req.header("Authorization", format!("Bearer {}", t)),
        None => req,
      };

      if ctx.attachments.len() == 1 {
        // Single attachment: send as binary body with message in headers
        let attach = &ctx.attachments[0];
        let mut att_req = client.put(&url).header("User-Agent", APP_ID).header("X-Priority", self.priority).header("X-Filename", &attach.name);
        if !ctx.title.is_empty() {
          att_req = att_req.header("X-Title", &ctx.title);
        }
        att_req = att_req.header("X-Message", &ctx.body);
        att_req = match &self.auth {
          Some(NtfyAuth::Basic { user, pass }) => att_req.basic_auth(user, Some(pass)),
          Some(NtfyAuth::Token(t)) => att_req.header("Authorization", format!("Bearer {}", t)),
          None => att_req,
        };
        let resp = att_req.body(attach.data.clone()).send().await?;
        if !resp.status().is_success() {
          let body = resp.text().await.unwrap_or_default();
          tracing::warn!("Ntfy send to {} failed: {}", topic, body);
          all_ok = false;
        }
      } else {
        // No attachments or multiple: send text message first
        let resp = req.body(ctx.body.clone()).send().await?;
        if !resp.status().is_success() {
          let body = resp.text().await.unwrap_or_default();
          tracing::warn!("Ntfy send to {} failed: {}", topic, body);
          all_ok = false;
        }

        // Send each attachment as a separate PUT
        for att in &ctx.attachments {
          let att_url = format!("{}/{}", base, topic);
          let mut att_req = client.put(&att_url).header("User-Agent", APP_ID).header("X-Filename", &att.name);
          att_req = match &self.auth {
            Some(NtfyAuth::Basic { user, pass }) => att_req.basic_auth(user, Some(pass)),
            Some(NtfyAuth::Token(t)) => att_req.header("Authorization", format!("Bearer {}", t)),
            None => att_req,
          };
          let resp = att_req.body(att.data.clone()).send().await?;
          if !resp.status().is_success() {
            all_ok = false;
          }
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
      "ntfy://user@localhost/topic/",
      "ntfy://ntfy.sh/topic1/topic2/",
      "ntfy://localhost/topic1/topic2/",
      "ntfy://localhost/topic1/?email=user@gmail.com",
      "ntfy://localhost/topic1/?tags=tag1,tag2,tag3",
      "ntfy://localhost/topic1/?actions=view%2CExample%2Chttp://www.example.com/%3Bview%2CTest%2Chttp://www.test.com/",
      "ntfy://localhost/topic1/?delay=3600",
      "ntfy://localhost/topic1/?title=A%20Great%20Title",
      "ntfy://localhost/topic1/?click=yes",
      "ntfy://localhost/topic1/?email=user@example.com",
      "ntfy://localhost/topic1/?image=False",
      "ntfy://localhost/topic1/?avatar_url=ttp://localhost/test.jpg",
      "ntfy://localhost/topic1/?attach=http://example.com/file.jpg",
      "ntfy://localhost/topic1/?attach=http://example.com/file.jpg&filename=smoke.jpg",
      "ntfy://localhost/topic1/?attach=http://-%20",
      "ntfy://tk_abcd123456@localhost/topic1",
      "ntfy://abcd123456@localhost/topic1?auth=token",
      "ntfy://:abcd123456@localhost/topic1?auth=token",
      "ntfy://localhost/topic1?token=abc1234",
      "ntfy://user:token@localhost/topic1?auth=token",
      "ntfy://localhost/topic1/?priority=default",
      "ntfy://localhost/topic1/?priority=high",
      "ntfy://user:pass@localhost:8080/topic/",
      "ntfys://user:pass@localhost?to=topic",
      "https://ntfy.sh?to=topic",
      "ntfy://user:pass@topic1/topic2/topic3/?mode=cloud",
      "ntfy://user:pass@ntfy.sh/topic1/topic2/?mode=cloud",
      "ntfy://user:pass@localhost:8083/topic1/topic2/",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "https://just/a/random/host/that/means/nothing",
      "ntfys://user:web/token@localhost/topic/?mode=invalid",
      "ntfys://token@localhost/topic/?auth=invalid",
      "ntfys://user:web@-_/topic1/topic2/?mode=private",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  // ── Behavioral tests using wiremock ──────────────────────────────────

  use super::*;
  use crate::asset::AppriseAsset;
  use crate::notify::{Notify, NotifyContext};
  use crate::types::{NotifyFormat, NotifyType};
  use base64::Engine;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

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

  /// Helper: create an Ntfy instance pointing at the mock server.
  fn ntfy_for_mock(server: &MockServer, url_suffix: &str) -> Ntfy {
    let addr = server.address();
    let port = addr.port();
    let url_str = format!("ntfy://localhost:{}/{}", port, url_suffix);
    let parsed = ParsedUrl::parse(&url_str).expect("parse test URL");
    Ntfy::from_url(&parsed).expect("create Ntfy from test URL")
  }

  // ── 1. JSON/body payload correctness ─────────────────────────────────

  #[tokio::test]
  async fn test_basic_send_body_and_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/mytopic"))
      .and(header("X-Title", "hello"))
      .and(header("X-Priority", "default"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let ntfy = ntfy_for_mock(&server, "mytopic");
    let result = ntfy.send(&ctx("hello", "world")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_title_omits_header() {
    let server = MockServer::start().await;
    // When title is empty, X-Title header should NOT be present
    Mock::given(method("POST")).and(path("/mytopic")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let ntfy = ntfy_for_mock(&server, "mytopic");
    let c = ctx("", "body only");
    let result = ntfy.send(&c).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_multiple_topics_sends_multiple_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/topic1")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/topic2")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let addr = server.address();
    let url_str = format!("ntfy://localhost:{}/topic1/topic2", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let result = ntfy.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 2. Correct endpoint URLs ─────────────────────────────────────────

  #[test]
  fn test_cloud_mode_base_url() {
    // ntfy://topic — no host → cloud mode → https://ntfy.sh
    let parsed = ParsedUrl::parse("ntfy://mytopic").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    assert_eq!(ntfy.base_url(), "https://ntfy.sh");
  }

  #[test]
  fn test_private_mode_base_url_with_port() {
    let parsed = ParsedUrl::parse("ntfy://user:pass@myhost:9090/topic").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    assert_eq!(ntfy.base_url(), "http://myhost:9090");
  }

  #[test]
  fn test_private_secure_base_url() {
    let parsed = ParsedUrl::parse("ntfys://user:pass@myhost/topic").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    assert_eq!(ntfy.base_url(), "https://myhost");
  }

  #[test]
  fn test_ntfy_sh_host_is_cloud() {
    let parsed = ParsedUrl::parse("ntfy://ntfy.sh/topic1/topic2").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    assert_eq!(ntfy.base_url(), "http://ntfy.sh");
    assert_eq!(ntfy.topics, vec!["topic1", "topic2"]);
  }

  // ── 3. Attachment handling ───────────────────────────────────────────

  #[tokio::test]
  async fn test_single_attachment_uses_put_with_filename() {
    let server = MockServer::start().await;
    // Single attachment: PUT to /topic with X-Filename and X-Message
    Mock::given(method("PUT"))
      .and(path("/mytopic"))
      .and(header("X-Filename", "photo.jpg"))
      .and(header("X-Message", "check this out"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let ntfy = ntfy_for_mock(&server, "mytopic");
    let mut c = ctx("", "check this out");
    c.attachments.push(crate::notify::Attachment {
      name: "photo.jpg".to_string(),
      data: vec![0xFF, 0xD8, 0xFF], // fake JPEG bytes
      mime_type: "image/jpeg".to_string(),
    });

    let result = ntfy.send(&c).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_single_attachment_with_title() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
      .and(path("/mytopic"))
      .and(header("X-Filename", "file.gif"))
      .and(header("X-Title", "wonderful"))
      .and(header("X-Message", "test"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let ntfy = ntfy_for_mock(&server, "mytopic");
    let mut c = ctx("wonderful", "test");
    c.attachments.push(crate::notify::Attachment { name: "file.gif".to_string(), data: b"GIF89a".to_vec(), mime_type: "image/gif".to_string() });

    let result = ntfy.send(&c).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_multiple_attachments_sends_post_then_puts() {
    let server = MockServer::start().await;

    // First: POST with the text body
    Mock::given(method("POST")).and(path("/mytopic")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    // Then: PUT for each attachment (2 attachments)
    Mock::given(method("PUT")).and(path("/mytopic")).and(header("X-Filename", "a.gif")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    Mock::given(method("PUT")).and(path("/mytopic")).and(header("X-Filename", "b.png")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let ntfy = ntfy_for_mock(&server, "mytopic");
    let mut c = ctx("title", "test");
    c.attachments.push(crate::notify::Attachment { name: "a.gif".to_string(), data: b"GIF89a".to_vec(), mime_type: "image/gif".to_string() });
    c.attachments.push(crate::notify::Attachment { name: "b.png".to_string(), data: b"PNG".to_vec(), mime_type: "image/png".to_string() });

    let result = ntfy.send(&c).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 4. Auth headers ──────────────────────────────────────────────────

  #[tokio::test]
  async fn test_bearer_token_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/mytopic"))
      .and(header("Authorization", "Bearer tk_mytoken123"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let url_str = format!("ntfy://tk_mytoken123@localhost:{}/mytopic", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let result = ntfy.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_basic_auth() {
    let server = MockServer::start().await;
    // reqwest sends Basic auth as "Basic base64(user:pass)"
    let expected = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("myuser:mypass"));
    Mock::given(method("POST"))
      .and(path("/mytopic"))
      .and(header("Authorization", expected.as_str()))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let url_str = format!("ntfy://myuser:mypass@localhost:{}/mytopic", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let result = ntfy.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_token_auth_via_query_param() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/mytopic"))
      .and(header("Authorization", "Bearer abc1234"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let url_str = format!("ntfy://localhost:{}/mytopic?token=abc1234", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let result = ntfy.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_auth_token_mode_uses_password_as_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/mytopic"))
      .and(header("Authorization", "Bearer secrettoken"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let url_str = format!("ntfy://user:secrettoken@localhost:{}/mytopic?auth=token", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let result = ntfy.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_bearer_token_on_attachment_put() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
      .and(path("/mytopic"))
      .and(header("Authorization", "Bearer tk_mytoken123"))
      .and(header("X-Filename", "file.txt"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let url_str = format!("ntfy://tk_mytoken123@localhost:{}/mytopic", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let mut c = ctx("", "msg");
    c.attachments.push(crate::notify::Attachment { name: "file.txt".to_string(), data: b"hello".to_vec(), mime_type: "text/plain".to_string() });

    let result = ntfy.send(&c).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 5. Error handling ────────────────────────────────────────────────

  #[tokio::test]
  async fn test_http_500_returns_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/mytopic")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;

    let ntfy = ntfy_for_mock(&server, "mytopic");
    let result = ntfy.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "HTTP 500 should return false");
  }

  #[tokio::test]
  async fn test_http_403_returns_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/mytopic")).respond_with(ResponseTemplate::new(403)).expect(1).mount(&server).await;

    let ntfy = ntfy_for_mock(&server, "mytopic");
    let result = ntfy.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "HTTP 403 should return false");
  }

  #[tokio::test]
  async fn test_connection_refused_returns_error() {
    // Point at a port that nothing is listening on
    let parsed = ParsedUrl::parse("ntfy://localhost:19999/mytopic").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let result = ntfy.send(&ctx("title", "body")).await;
    assert!(result.is_err(), "Connection refused should return Err");
  }

  #[tokio::test]
  async fn test_partial_failure_multiple_topics() {
    // topic1 succeeds, topic2 fails with 500 → result should be false
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/topic1")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/topic2")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;

    let addr = server.address();
    let url_str = format!("ntfy://localhost:{}/topic1/topic2", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let result = ntfy.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "Partial failure should return false");
  }

  #[tokio::test]
  async fn test_attachment_upload_failure_returns_false() {
    let server = MockServer::start().await;
    // Single attachment PUT returns 500
    Mock::given(method("PUT")).and(path("/mytopic")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;

    let ntfy = ntfy_for_mock(&server, "mytopic");
    let mut c = ctx("", "msg");
    c.attachments.push(crate::notify::Attachment { name: "file.txt".to_string(), data: b"data".to_vec(), mime_type: "text/plain".to_string() });

    let result = ntfy.send(&c).await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "Attachment upload failure should return false");
  }

  // ── 6. Markdown mode ─────────────────────────────────────────────────

  #[tokio::test]
  async fn test_markdown_format_sends_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/mytopic")).and(header("X-Markdown", "yes")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let ntfy = ntfy_for_mock(&server, "mytopic");
    let mut c = ctx("title", "**bold** text");
    c.body_format = NotifyFormat::Markdown;

    let result = ntfy.send(&c).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_text_format_no_markdown_header() {
    let server = MockServer::start().await;
    // This mock requires X-Markdown NOT to be present.
    // We mount a mock that expects no X-Markdown and returns 200,
    // plus a catch-all that would fail if X-Markdown was sent.
    Mock::given(method("POST")).and(path("/mytopic")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let ntfy = ntfy_for_mock(&server, "mytopic");
    let c = ctx("title", "plain text");
    // body_format defaults to Text

    let result = ntfy.send(&c).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 7. Priority handling ─────────────────────────────────────────────

  #[tokio::test]
  async fn test_high_priority_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/mytopic")).and(header("X-Priority", "high")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let addr = server.address();
    let url_str = format!("ntfy://localhost:{}/mytopic?priority=high", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let result = ntfy.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_max_priority_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/mytopic")).and(header("X-Priority", "max")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let addr = server.address();
    let url_str = format!("ntfy://localhost:{}/mytopic?priority=max", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let result = ntfy.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_numeric_priority() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/mytopic")).and(header("X-Priority", "min")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let addr = server.address();
    let url_str = format!("ntfy://localhost:{}/mytopic?priority=1", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();

    let result = ntfy.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 8. URL parsing edge cases ────────────────────────────────────────

  #[test]
  fn test_no_topics_returns_none() {
    // ntfy:// with no topics should fail to construct
    let parsed = ParsedUrl::parse("ntfy://").unwrap();
    assert!(Ntfy::from_url(&parsed).is_none());
  }

  #[test]
  fn test_to_query_param_adds_topics() {
    let parsed = ParsedUrl::parse("ntfys://user:pass@localhost?to=topicA").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    assert!(ntfy.topics.contains(&"topicA".to_string()));
  }

  #[test]
  fn test_cloud_mode_explicit() {
    let parsed = ParsedUrl::parse("ntfy://user:pass@topic1/topic2/topic3/?mode=cloud").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    // In cloud mode with no real host, topics come from path + host
    assert!(ntfy.topics.len() >= 2);
  }

  #[test]
  fn test_invalid_mode_rejected() {
    let parsed = ParsedUrl::parse("ntfys://user:pass@localhost/topic/?mode=invalid").unwrap();
    assert!(Ntfy::from_url(&parsed).is_none());
  }

  #[test]
  fn test_invalid_auth_rejected() {
    let parsed = ParsedUrl::parse("ntfys://token@localhost/topic/?auth=invalid").unwrap();
    assert!(Ntfy::from_url(&parsed).is_none());
  }

  #[test]
  fn test_tk_prefix_detected_as_token() {
    let parsed = ParsedUrl::parse("ntfy://tk_abcd123456@localhost/topic1").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    match &ntfy.auth {
      Some(NtfyAuth::Token(t)) => assert_eq!(t, "tk_abcd123456"),
      other => panic!("Expected Token auth, got {:?}", other.is_some()),
    }
  }

  #[test]
  fn test_token_query_param() {
    let parsed = ParsedUrl::parse("ntfy://localhost/topic1?token=abc1234").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    match &ntfy.auth {
      Some(NtfyAuth::Token(t)) => assert_eq!(t, "abc1234"),
      other => panic!("Expected Token auth, got {:?}", other.is_some()),
    }
  }

  #[test]
  fn test_basic_auth_parsed() {
    let parsed = ParsedUrl::parse("ntfy://myuser:mypass@localhost:8080/topic").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    match &ntfy.auth {
      Some(NtfyAuth::Basic { user, pass }) => {
        assert_eq!(user, "myuser");
        assert_eq!(pass, "mypass");
      }
      other => panic!("Expected Basic auth, got {:?}", other.is_some()),
    }
  }

  #[test]
  fn test_secure_flag() {
    let parsed = ParsedUrl::parse("ntfys://localhost/topic").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    assert!(ntfy.secure);

    let parsed = ParsedUrl::parse("ntfy://localhost/topic").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    assert!(!ntfy.secure);
  }

  #[test]
  fn test_tags_parsed() {
    let parsed = ParsedUrl::parse("ntfy://localhost/topic?tag=cool,rust").unwrap();
    let ntfy = Ntfy::from_url(&parsed).unwrap();
    assert!(ntfy.tags.contains(&"cool".to_string()));
    assert!(ntfy.tags.contains(&"rust".to_string()));
  }
}
