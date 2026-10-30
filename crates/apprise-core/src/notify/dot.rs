use async_trait::async_trait;
use base64::Engine;
use serde_json::{Map, Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool, unquote};

const DOT_API: &str = "https://dot.mindreset.tech";

/// Supported API modes; the first entry is the default.
const DOT_MODES: [&str; 2] = ["text", "image"];

/// Dot. e-ink displays (Text API v2 and Image API v2).
///
/// In `text` mode (the default) the body and title go to the Text API and an
/// `image=` value or the first attachment goes to the Image API, text first.
/// In `image` mode only the Image API is called.
pub struct Dot {
  apikey: Option<String>,
  device_id: String,
  mode: &'static str,
  refresh_now: bool,
  signature: Option<String>,
  icon: Option<String>,
  image_data: Option<String>,
  link: Option<String>,
  border: Option<i64>,
  dither_type: Option<String>,
  dither_kernel: Option<String>,
  task_key: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Dot {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let device_id = url.host.clone()?;

    // An explicit ?mode= wins; the first path element is kept for backward
    // compatibility (dot://apikey@device/image/).
    let candidate = match url.get("mode").filter(|m| !m.is_empty()) {
      Some(m) => Some(m.trim().to_lowercase()),
      None => url.path_parts.first().map(|p| p.to_lowercase()),
    };
    let mode = match candidate {
      Some(c) => DOT_MODES.iter().copied().find(|m| *m == c).unwrap_or_else(|| {
        tracing::warn!("Unsupported Dot mode ({}) specified; defaulting to '{}'.", c, DOT_MODES[0]);
        DOT_MODES[0]
      }),
      None => DOT_MODES[0],
    };

    // Upstream unquotes query values a second time
    let opt = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(|v| unquote(v.trim()));

    Some(Self {
      apikey: url.user.clone(),
      device_id,
      mode,
      refresh_now: url.get("refresh").filter(|v| !v.is_empty()).map(|v| parse_bool(v.trim())).unwrap_or(true),
      signature: opt("signature"),
      icon: opt("icon"),
      image_data: opt("image"),
      link: opt("link"),
      border: url.get("border").and_then(|v| v.trim().parse().ok()),
      dither_type: opt("dither_type"),
      dither_kernel: opt("dither_kernel"),
      task_key: opt("task_key"),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Dot.",
      service_url: Some("https://dot.mindreset.tech"),
      setup_url: Some("https://appriseit.com/services/dot/"),
      protocols: vec!["dot"],
      description: "Send text and images to Dot. e-ink displays.",
      attachment_support: true,
    }
  }

  async fn post(&self, client: &reqwest::Client, apikey: &str, api: &str, payload: Value) -> bool {
    let url = format!("{}/api/authV2/open/device/{}/{}", service_base(DOT_API), self.device_id, api);
    tracing::debug!("Dot POST URL: {} (cert_verify={})", url, self.verify_certificate);
    crate::notify::throttle().await;
    let resp = client
      .post(&url)
      .header("Authorization", format!("Bearer {}", apikey))
      .header("Content-Type", "application/json")
      .header("User-Agent", APP_ID)
      .body(payload.to_string())
      .send()
      .await;
    match resp {
      Ok(r) if r.status().as_u16() == 200 => {
        tracing::info!("Sent Dot notification to {}.", self.device_id);
        true
      }
      Ok(r) => {
        let status = r.status();
        let reason = status.canonical_reason().unwrap_or("");
        tracing::warn!(
          "Failed to send Dot notification to {}: {}{}error={}.",
          self.device_id,
          reason,
          if reason.is_empty() { "" } else { ", " },
          status.as_u16()
        );
        false
      }
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Dot notification to {}.", self.device_id);
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        false
      }
    }
  }

  fn text_payload(&self, title: &str, body: &str) -> Value {
    let mut p = Map::new();
    p.insert("refreshNow".into(), json!(self.refresh_now));
    if !title.is_empty() {
      p.insert("title".into(), json!(title));
    }
    if !body.is_empty() {
      p.insert("message".into(), json!(body));
    }
    if let Some(v) = &self.signature {
      p.insert("signature".into(), json!(v));
    }
    if let Some(v) = &self.icon {
      p.insert("icon".into(), json!(v));
    }
    if let Some(v) = &self.link {
      p.insert("link".into(), json!(v));
    }
    if let Some(v) = &self.task_key {
      p.insert("taskKey".into(), json!(v));
    }
    Value::Object(p)
  }

  fn image_payload(&self, image: &str) -> Value {
    let mut p = Map::new();
    p.insert("image".into(), json!(image));
    p.insert("refreshNow".into(), json!(self.refresh_now));
    if let Some(v) = &self.link {
      p.insert("link".into(), json!(v));
    }
    if let Some(v) = self.border {
      p.insert("border".into(), json!(v));
    }
    if let Some(v) = &self.dither_type {
      p.insert("ditherType".into(), json!(v));
    }
    if let Some(v) = &self.dither_kernel {
      p.insert("ditherKernel".into(), json!(v));
    }
    if let Some(v) = &self.task_key {
      p.insert("taskKey".into(), json!(v));
    }
    Value::Object(p)
  }

  /// The `image=` value, or else the first attachment encoded as base64.
  fn resolve_image(&self, attachments: &[Attachment]) -> Option<String> {
    if let Some(data) = &self.image_data {
      return Some(data.clone());
    }
    if attachments.len() > 1 {
      tracing::warn!("Multiple attachments provided; only the first one will be used as image.");
    }
    attachments.first().map(|a| base64::engine::general_purpose::STANDARD.encode(&a.data))
  }
}

#[async_trait]
impl Notify for Dot {
  fn schemas(&self) -> &[&str] {
    &["dot"]
  }
  fn service_name(&self) -> &str {
    "Dot."
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

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let Some(apikey) = self.apikey.as_deref() else {
      tracing::warn!("No API key was specified");
      return Ok(false);
    };
    let client = build_client(self.verify_certificate)?;
    let image = self.resolve_image(&ctx.attachments);

    if self.mode == "image" {
      // Image-only mode: body and title are ignored.
      if !ctx.title.is_empty() || !ctx.body.is_empty() {
        tracing::warn!("Title and body are not supported in image mode and will be ignored.");
      }
      let Some(image) = image else {
        tracing::warn!("Image mode selected but no image data was provided.");
        return Ok(false);
      };
      return Ok(self.post(&client, apikey, "image", self.image_payload(&image)).await);
    }

    let has_text = !ctx.body.is_empty() || !ctx.title.is_empty();
    if !has_text && image.is_none() {
      tracing::warn!("Nothing to send to Dot. device {}.", self.device_id);
      return Ok(false);
    }

    let mut has_error = false;
    if has_text && !self.post(&client, apikey, "text", self.text_payload(&ctx.title, &ctx.body)).await {
      has_error = true;
    }
    if let Some(image) = image
      && !self.post(&client, apikey, "image", self.image_payload(&image)).await
    {
      has_error = true;
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path, path_regex};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse_dot(url: &str) -> Dot {
    Dot::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  async fn bodies(server: &MockServer) -> Vec<(String, Value)> {
    let requests = server.received_requests().await.unwrap();
    requests.iter().filter(|r| r.method.as_str() == "POST").map(|r| (r.url.path().to_string(), serde_json::from_slice(&r.body).unwrap())).collect()
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "dot://@device_id",
      "dot://apikey@device_id/",
      "dot://apikey@device_id/?refresh=no",
      "dot://apikey@device_id/?signature=test_signature",
      "dot://apikey@device_id/?link=https://example.com",
      "dot://apikey@device_id/?mode=text",
      "dot://apikey@device_id/text/",
      "dot://apikey@device_id/?mode=image&image=ZmFrZUJhc2U2NA==",
      "dot://apikey@device_id/?image=ZmFrZUJhc2U2NA==",
      "dot://apikey@device_id/image/?link=https://example.com&border=1&dither_type=ORDERED&dither_kernel=ATKINSON",
      "dot://apikey@device_id/image/?image=ZmFrZUJhc2U2NA==&link=https://example.com&border=1&dither_type=DIFFUSION&dither_kernel=FLOYD_STEINBERG",
      "dot://apikey@device_id/unknown/",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["dot://", "dot://@", "dot://apikey@"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let d = parse_dot("dot://apikey@device_id/");
    assert_eq!(d.apikey.as_deref(), Some("apikey"));
    assert_eq!(d.device_id, "device_id");
    assert_eq!(d.mode, "text");
    assert!(d.refresh_now);

    // Path keeps backward compatibility; ?mode= wins over it
    assert_eq!(parse_dot("dot://apikey@device_id/image/").mode, "image");
    assert_eq!(parse_dot("dot://apikey@device_id/image/?mode=text").mode, "text");
    assert_eq!(parse_dot("dot://apikey@device_id/?mode=IMAGE").mode, "image");
    assert_eq!(parse_dot("dot://apikey@device_id/?mode=bogus").mode, "text");
    assert_eq!(parse_dot("dot://apikey@device_id/unknown/").mode, "text");

    let d = parse_dot("dot://k@d/?border=x&refresh=no&task_key=abc");
    assert_eq!(d.border, None);
    assert!(!d.refresh_now);
    assert_eq!(d.task_key.as_deref(), Some("abc"));
    assert!(parse_dot("dot://@device_id").apikey.is_none());
  }

  #[tokio::test]
  async fn test_send_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/authV2/open/device/device123/text"))
      .and(header("Authorization", "Bearer apikey"))
      .and(header("Content-Type", "application/json"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("dot://apikey@device123/?refresh=no&signature=Sig&icon=ICON&link=https://example.com").unwrap();
    let ctx = NotifyContext { title: "title".into(), body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let reqs = bodies(&server).await;
    assert_eq!(reqs[0].1, json!({"refreshNow": false, "title": "title", "message": "body", "signature": "Sig", "icon": "ICON", "link": "https://example.com"}));
  }

  #[tokio::test]
  async fn test_send_task_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path_regex("^/api/authV2/open/device/")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("dot://apikey@device123/?task_key=abc123").unwrap();
    let ctx = NotifyContext { title: "title".into(), body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(bodies(&server).await[0].1, json!({"refreshNow": true, "title": "title", "message": "body", "taskKey": "abc123"}));
  }

  #[tokio::test]
  async fn test_send_image_mode() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/authV2/open/device/device123/image")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("dot://apikey@device123/?mode=image&image=IMGDATA&border=1&dither_type=ORDERED&dither_kernel=ATKINSON").unwrap();
    let ctx = NotifyContext { title: "ignored".into(), body: "ignored".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(bodies(&server).await[0].1, json!({"image": "IMGDATA", "refreshNow": true, "border": 1, "ditherType": "ORDERED", "ditherKernel": "ATKINSON"}));
  }

  #[tokio::test]
  async fn test_send_image_mode_without_image_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path_regex("^/api/authV2/open/device/")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
    let svc = from_url("dot://apikey@device_id/image/?link=https://example.com&border=1").unwrap();
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_text_then_attachment() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path_regex("^/api/authV2/open/device/")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;
    let svc = from_url("dot://apikey@device123/?refresh=no&signature=Sig&icon=ICON&link=https://example.com").unwrap();
    let mut ctx = NotifyContext { title: "title".into(), body: "body".into(), ..Default::default() };
    ctx.attachments.push(Attachment { name: "a.png".into(), data: b"PNG1".to_vec(), mime_type: "image/png".into() });
    ctx.attachments.push(Attachment { name: "b.png".into(), data: b"PNG2".to_vec(), mime_type: "image/png".into() });
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let reqs = bodies(&server).await;
    assert_eq!(reqs[0].0, "/api/authV2/open/device/device123/text");
    assert_eq!(reqs[1].0, "/api/authV2/open/device/device123/image");
    assert_eq!(reqs[1].1, json!({"image": "UE5HMQ==", "refreshNow": false, "link": "https://example.com"}));
  }

  #[tokio::test]
  async fn test_send_image_only_in_text_mode() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/authV2/open/device/d/image")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("dot://apikey@d/?image=ZmFrZQ==").unwrap();
    let ctx = NotifyContext::default();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path_regex("^/api/authV2/open/device/")).respond_with(ResponseTemplate::new(999)).expect(1).mount(&server).await;
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    // Non-200 status
    let svc = from_url("dot://apikey@device_id/").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    // No API key: nothing is sent
    let svc = from_url("dot://@device_id").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    // Nothing to send
    let svc = from_url("dot://apikey@device_id/").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.unwrap());
  }

  #[test]
  fn test_service_details() {
    let details = Dot::static_details();
    assert_eq!(details.service_name, "Dot.");
    assert_eq!(details.protocols, vec!["dot"]);
    assert!(details.attachment_support);
  }
}
