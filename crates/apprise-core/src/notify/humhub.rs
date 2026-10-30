use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
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

/// HumHub self-hosted social network posts (REST API module).
///
/// `humhubs://{token}@{host}/{container_id}` (Bearer token) or
/// `humhubs://{user}:{password}@{host}/{container_id}` (Basic auth);
/// `humhub://` uses plain HTTP.
pub struct HumHub {
  user: String,
  password: Option<String>,
  host: String,
  port: Option<u16>,
  secure: bool,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl HumHub {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let Some(user) = url.user.clone().filter(|u| !u.is_empty()) else {
      tracing::warn!("A HumHub bearer token or username must be specified.");
      return None;
    };

    // Container IDs come from the path and ?to=
    let path_entries: Vec<String> = url
      .path
      .split([' ', '\t', '\r', '\n', ',', '\\', '/'])
      .filter(|s| !s.is_empty())
      .map(|s| urlencoding::decode(s).map(|d| d.into_owned()).unwrap_or_else(|_| s.to_string()))
      .collect();
    let to = url.get("to").map(str::trim).filter(|t| !t.is_empty());

    // Container IDs must be positive integers
    let mut targets = Vec::new();
    for target in parse_list(path_entries.iter().map(String::as_str).chain(to)) {
      match target.trim().parse::<i64>() {
        Ok(cid) if cid > 0 => targets.push(cid.to_string()),
        _ => tracing::warn!("Dropping invalid HumHub container ID: {}", target),
      }
    }
    if targets.is_empty() {
      tracing::warn!("No valid HumHub container ID(s) were specified.");
      return None;
    }

    Some(Self {
      user,
      password: url.password.clone().filter(|p| !p.is_empty()),
      host,
      port: url.port,
      secure: url.schema == "humhubs",
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "HumHub",
      service_url: Some("https://www.humhub.com/"),
      setup_url: Some("https://appriseit.com/services/humhub/"),
      protocols: vec!["humhub", "humhubs"],
      description: "Post to HumHub spaces through its REST API.",
      attachment_support: true,
    }
  }

  fn base_url(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    let port = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    format!("{}://{}{}", schema, self.host, port)
  }

  /// Basic auth when a password is set, a Bearer token otherwise
  fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match &self.password {
      Some(password) => req.basic_auth(&self.user, Some(password)),
      None => req.header("Authorization", format!("Bearer {}", self.user)),
    }
  }

  /// POST and return the response body on HTTP 200, `None` otherwise.
  async fn post(&self, req: reqwest::RequestBuilder, what: &str) -> Option<String> {
    let resp = match self.authorize(req.header("User-Agent", APP_ID)).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending HumHub {}.", what);
        tracing::debug!("Socket Exception: {}", e);
        return None;
      }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status != 200 {
      tracing::warn!("Failed to send HumHub {}: error={}.", what, status);
      tracing::debug!("Response Details:\r\n{}", text);
      return None;
    }
    Some(text)
  }
}

#[async_trait]
impl Notify for HumHub {
  fn schemas(&self) -> &[&str] {
    &["humhub", "humhubs"]
  }
  fn service_name(&self) -> &str {
    "HumHub"
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
  /// Maximum post length (characters)
  fn body_maxlen(&self) -> usize {
    4000
  }
  /// HumHub posts have no native title field
  fn title_maxlen(&self) -> usize {
    0
  }
  /// Self-hosted service; upstream waits only 0.02s between requests
  fn request_rate_per_sec(&self) -> f64 {
    50.0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    // HumHub wraps the message in a data object
    let payload = json!({ "data": { "message": ctx.body } }).to_string();
    let base = self.base_url();
    let mut has_error = false;

    // Post to each container in turn
    for container_id in &self.targets {
      let url = format!("{}/api/v1/post/container/{}", base, container_id);
      tracing::debug!("HumHub POST URL: {} (cert_verify={})", url, self.verify_certificate);
      let Some(content) = self.post(client.post(&url).header("Content-Type", "application/json").body(payload.clone()), "notification").await else {
        has_error = true;
        continue;
      };
      tracing::info!("Sent HumHub notification to container {}.", container_id);

      if ctx.attachments.is_empty() {
        continue;
      }

      // Attachments are uploaded to the post just created
      let post_id = match serde_json::from_str::<Value>(&content).ok().as_ref().and_then(|r| r.get("id")) {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
      };
      let Some(post_id) = post_id else {
        tracing::warn!("Failed to parse HumHub post ID from response; attachments will not be sent to container {}.", container_id);
        has_error = true;
        continue;
      };

      let attach_url = format!("{}/api/v1/post/{}/upload-files", base, post_id);
      for attachment in &ctx.attachments {
        let part = reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(attachment.name.clone());
        let form = reqwest::multipart::Form::new().part("files[]", part);
        tracing::debug!("HumHub POST URL: {} (cert_verify={})", attach_url, self.verify_certificate);
        if self.post(client.post(&attach_url).multipart(form), "attachment").await.is_none() {
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
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Option<HumHub> {
    HumHub::from_url(&ParsedUrl::parse(url).unwrap())
  }

  fn base(server: &MockServer) -> String {
    let addr = server.address();
    format!("{}:{}", addr.ip(), addr.port())
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      "humhub://mytoken@hostname/1",
      "humhubs://mytoken@hostname/1",
      "humhubs://user:pass@hostname/1",
      "humhubs://mytoken@hostname/1/2/3",
      "humhubs://mytoken@hostname/?to=1,2,3",
      "humhubs://mytoken@hostname:8443/1",
      // Mixed valid and invalid IDs; the bad ones are dropped
      "humhubs://mytoken@hostname/1/bad/2",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "humhub://",
      // No user/token
      "humhub://hostname",
      "humhub://hostname/1",
      // No container ID
      "humhub://token@hostname",
      // Invalid container IDs
      "humhubs://token@hostname/invalid",
      "humhubs://token@hostname/-1",
      "humhubs://token@hostname/0",
      "humhubs://token@hostname/bad/0/-5",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // IDs are sorted as strings (upstream parse_list), then normalised
    let h = parse("humhubs://user:pass@host:8443/10/2/007?to=bad").unwrap();
    assert_eq!(h.targets, vec!["7", "10", "2"]);
    assert_eq!(h.user, "user");
    assert_eq!(h.password.as_deref(), Some("pass"));
    assert!(h.secure);
    assert_eq!(h.port, Some(8443));

    let h = parse("humhub://bearertoken@hostname/42").unwrap();
    assert_eq!(h.targets, vec!["42"]);
    assert_eq!(h.password, None);
    assert!(!h.secure);
  }

  #[tokio::test]
  async fn test_send_bearer() {
    let server = MockServer::start().await;
    for id in ["1", "2"] {
      Mock::given(method("POST"))
        .and(path(format!("/api/v1/post/container/{}", id)))
        .and(header("Authorization", "Bearer mytoken"))
        .and(header("Content-Type", "application/json"))
        .and(header("User-Agent", APP_ID))
        .and(body_json(json!({"data": {"message": "T\r\nB"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&format!("humhub://mytoken@{}/2/1", base(&server))).unwrap();
    assert!(svc.send(&NotifyContext { body: "T\r\nB".into(), ..Default::default() }).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_basic_auth_with_attachments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/post/container/5"))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 55})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/api/v1/post/55/upload-files"))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .respond_with(ResponseTemplate::new(200))
      .expect(2)
      .mount(&server)
      .await;

    let svc = from_url(&format!("humhub://user:pass@{}/5", base(&server))).unwrap();
    let mut ctx = NotifyContext { body: "B".into(), ..Default::default() };
    ctx.attachments.push(Attachment { name: "a.png".into(), data: b"PNGDATA".to_vec(), mime_type: "image/png".into() });
    ctx.attachments.push(Attachment { name: "b.txt".into(), data: b"TXT".to_vec(), mime_type: "text/plain".into() });
    assert!(svc.send(&ctx).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    let upload = requests.iter().find(|r| r.url.path() == "/api/v1/post/55/upload-files").unwrap();
    let content_type = upload.headers.get("Content-Type").unwrap().to_str().unwrap();
    assert!(content_type.starts_with("multipart/form-data"));
    let body = String::from_utf8_lossy(&upload.body);
    assert!(body.contains("name=\"files[]\"; filename=\"a.png\""));
    assert!(body.contains("PNGDATA"));
  }

  #[tokio::test]
  async fn test_send_attachment_without_post_id() {
    // The post goes out, but attachments cannot follow without its ID
    for reply in [json!({}), json!([1]), json!({"id": ""})] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path("/api/v1/post/container/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url(&format!("humhub://mytoken@{}/1", base(&server))).unwrap();
      let mut ctx = NotifyContext { body: "B".into(), ..Default::default() };
      ctx.attachments.push(Attachment { name: "a.png".into(), data: b"PNG".to_vec(), mime_type: "image/png".into() });
      assert!(!svc.send(&ctx).await.unwrap());
    }
  }

  #[tokio::test]
  async fn test_send_partial_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/v1/post/container/1")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/api/v1/post/container/2")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url(&format!("humhub://mytoken@{}/1/2", base(&server))).unwrap();
    assert!(!svc.send(&NotifyContext { body: "B".into(), ..Default::default() }).await.unwrap());

    // A failed upload fails the notification too
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/post/container/1"))
      .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id": "9"}"#))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/api/v1/post/9/upload-files")).respond_with(ResponseTemplate::new(999)).expect(1).mount(&server).await;
    let svc = from_url(&format!("humhub://mytoken@{}/1", base(&server))).unwrap();
    let mut ctx = NotifyContext { body: "B".into(), ..Default::default() };
    ctx.attachments.push(Attachment { name: "a.png".into(), data: b"PNG".to_vec(), mime_type: "image/png".into() });
    assert!(!svc.send(&ctx).await.unwrap());
  }

  #[test]
  fn test_service_details() {
    let details = HumHub::static_details();
    assert_eq!(details.service_name, "HumHub");
    assert_eq!(details.protocols, vec!["humhub", "humhubs"]);
    assert!(details.attachment_support);
    let svc = parse("humhub://t@h/1").unwrap();
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.body_maxlen(), 4000);
  }
}
