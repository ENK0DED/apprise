use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{email_address, parse_list, split_path, unquote};
use async_trait::async_trait;
use serde_json::{Value, json};

/// The recipient used when no target is given: every device.
const PUSHBULLET_SEND_TO_ALL: &str = "ALL_DEVICES";

pub struct Pushbullet {
  accesstoken: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Pushbullet {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let Some(accesstoken) = url.host.as_deref().map(unquote).and_then(|t| t.split_whitespace().next().map(str::to_string)) else {
      tracing::warn!("An invalid PushBullet Access Token was specified.");
      return None;
    };
    let mut targets = split_path(url);
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.push(to.to_string());
    }
    let mut targets = parse_list(targets, true, true);
    if targets.is_empty() {
      targets.push(PUSHBULLET_SEND_TO_ALL.to_string());
    }
    Some(Self { accesstoken, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pushbullet",
      service_url: Some("https://www.pushbullet.com/"),
      setup_url: Some("https://appriseit.com/services/pushbullet/"),
      protocols: vec!["pbul"],
      description: "Send push notifications via Pushbullet.",
      attachment_support: true,
    }
  }

  /// Address a push to `recipient`: an email, a `#channel`, a device, or
  /// (with no target) every device.
  fn address(payload: &mut Value, recipient: &str) {
    if let Some(email) = email_address(recipient) {
      payload["email"] = json!(email);
    } else if recipient == PUSHBULLET_SEND_TO_ALL {
      // No recipient field reaches every device
    } else if let Some(channel) = recipient.strip_prefix('#') {
      payload["channel_tag"] = json!(channel);
    } else {
      payload["device_iden"] = json!(recipient);
    }
  }

  /// POST to Pushbullet; returns the parsed response on 200 or 204.
  async fn post(&self, request: reqwest::RequestBuilder) -> Option<Value> {
    let resp = match request.header("User-Agent", APP_ID).basic_auth(&self.accesstoken, Some("")).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred communicating with PushBullet.");
        tracing::debug!("Socket Exception: {}", e);
        return None;
      }
    };
    let status = resp.status();
    let body = resp.bytes().await.unwrap_or_default();
    if !matches!(status.as_u16(), 200 | 204) {
      let reason = if status.as_u16() == 401 { "Unauthorized - Invalid Token." } else { status.canonical_reason().unwrap_or("") };
      tracing::warn!("Failed to deliver payload to PushBullet: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      return None;
    }
    Some(serde_json::from_slice(&body).unwrap_or(Value::Null))
  }

  async fn push(&self, client: &reqwest::Client, payload: &Value) -> bool {
    let url = format!("{}/v2/pushes", service_base("https://api.pushbullet.com"));
    let Ok(body) = serde_json::to_vec(payload) else { return false };
    self.post(client.post(url).header("Content-Type", "application/json").body(body)).await.is_some()
  }
}

#[async_trait]
impl Notify for Pushbullet {
  fn schemas(&self) -> &[&str] {
    &["pbul"]
  }
  fn service_name(&self) -> &str {
    "Pushbullet"
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
  fn request_rate_per_sec(&self) -> f64 {
    1.2
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let base = service_base("https://api.pushbullet.com");

    // Upload every attachment first; any failure fails the notification
    let mut files: Vec<Value> = Vec::new();
    for (no, attachment) in ctx.attachments.iter().enumerate() {
      let file_name = if attachment.name.is_empty() { format!("file{:03}.dat", no + 1) } else { attachment.name.clone() };
      let request = json!({ "file_name": file_name, "file_type": attachment.mime_type });
      let Some(response) =
        self.post(client.post(format!("{}/v2/upload-request", base)).header("Content-Type", "application/json").body(serde_json::to_vec(&request)?)).await
      else {
        return Ok(false);
      };

      let field = |k: &str| response.get(k).and_then(Value::as_str).map(str::to_string);
      let (Some(name), Some(file_type), Some(file_url), Some(upload_url)) = (field("file_name"), field("file_type"), field("file_url"), field("upload_url"))
      else {
        tracing::warn!("PushBullet returned an incomplete upload request.");
        return Ok(false);
      };

      let part = reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(file_name.clone());
      if self.post(client.post(&upload_url).multipart(reqwest::multipart::Form::new().part("file", part))).await.is_none() {
        return Ok(false);
      }

      let mut push = json!({ "type": "file", "file_name": name, "file_type": file_type, "file_url": file_url });
      if file_type.starts_with("image/") {
        push["image_url"] = json!(file_url);
      }
      files.push(push);
    }

    let mut has_error = false;
    for recipient in &self.targets {
      if !ctx.body.is_empty() {
        let mut note = json!({ "type": "note", "title": ctx.title, "body": ctx.body });
        Self::address(&mut note, recipient);
        if !self.push(&client, &note).await {
          has_error = true;
          continue;
        }
        tracing::info!("Sent PushBullet notification to \"{}\".", recipient);
      }

      // Each file is addressed to the recipient too (as apprise-go does;
      // upstream sends them to every device)
      for file in &files {
        let mut push = file.clone();
        Self::address(&mut push, recipient);
        if !self.push(&client, &push).await {
          has_error = true;
          continue;
        }
        tracing::info!("Sent PushBullet attachment ({}) to \"{}\".", file["file_name"].as_str().unwrap_or(""), recipient);
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
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn body(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
  }

  #[test]
  fn test_urls() {
    let a = "a".repeat(32);
    for rest in [
      "",
      "#channel/",
      "?to=#channel",
      "#channel1/#channel2",
      "device/",
      "device1/device2/",
      "user@example.com/",
      "user@example.com/abc@def.com/",
      "device/#channel/user@example.com/",
    ] {
      let url = format!("pbul://{}/{}", a, rest);
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
    for url in ["pbul://", "pbul://:@/"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    let obj = Pushbullet::from_url(&ParsedUrl::parse(&format!("pbul://{}/dev1/%23chan/user@example.com?to=dev1", a)).unwrap()).unwrap();
    assert_eq!(obj.targets, vec!["#chan", "dev1", "user@example.com"]);
    let obj = Pushbullet::from_url(&ParsedUrl::parse(&format!("pbul://{}", a)).unwrap()).unwrap();
    assert_eq!(obj.targets, vec![PUSHBULLET_SEND_TO_ALL]);
  }

  #[tokio::test]
  async fn test_send_notes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v2/pushes"))
      .and(header("authorization", "Basic dG9rMTIzOg=="))
      .and(header("content-type", "application/json"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
      .expect(3)
      .mount(&server)
      .await;
    let svc = from_url("pbul://tok123/dev1/%23chan/user@example.com").unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(body(&requests[0]), json!({"type": "note", "title": "T", "body": "B", "channel_tag": "chan"}));
    assert_eq!(body(&requests[1]), json!({"type": "note", "title": "T", "body": "B", "device_iden": "dev1"}));
    assert_eq!(body(&requests[2]), json!({"type": "note", "title": "T", "body": "B", "email": "user@example.com"}));
  }

  #[tokio::test]
  async fn test_send_all_devices_with_attachment() {
    let server = MockServer::start().await;
    let upload_url = format!("{}/upload/here", server.uri());
    Mock::given(method("POST"))
      .and(path("/v2/upload-request"))
      .respond_with(
        ResponseTemplate::new(200)
          .set_body_json(json!({"file_name": "photo.jpg", "file_type": "image/jpeg", "file_url": "https://f/x.jpg", "upload_url": upload_url})),
      )
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/upload/here")).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/v2/pushes")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;

    let svc = from_url("pbul://tok123").unwrap();
    let attachment = Attachment { name: "photo.jpg".into(), data: b"jpeg-bytes".to_vec(), mime_type: "image/jpeg".into() };
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), attachments: vec![attachment], ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    assert_eq!(body(&requests[0]), json!({"file_name": "photo.jpg", "file_type": "image/jpeg"}));
    assert!(requests[1].headers.get("content-type").unwrap().to_str().unwrap().starts_with("multipart/form-data"));
    assert!(String::from_utf8_lossy(&requests[1].body).contains("jpeg-bytes"));
    assert_eq!(body(&requests[2]), json!({"type": "note", "title": "T", "body": "B"}));
    assert_eq!(
      body(&requests[3]),
      json!({"type": "file", "file_name": "photo.jpg", "file_type": "image/jpeg", "file_url": "https://f/x.jpg", "image_url": "https://f/x.jpg"})
    );
  }

  #[tokio::test]
  async fn test_file_pushes_are_addressed() {
    let server = MockServer::start().await;
    let upload_url = format!("{}/upload/here", server.uri());
    Mock::given(method("POST"))
      .and(path("/v2/upload-request"))
      .respond_with(
        ResponseTemplate::new(200)
          .set_body_json(json!({"file_name": "a.txt", "file_type": "text/plain", "file_url": "https://f/a.txt", "upload_url": upload_url})),
      )
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/upload/here")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    Mock::given(method("POST")).and(path("/v2/pushes")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    // No body: only the file is pushed
    let svc = from_url("pbul://tok123/dev1").unwrap();
    let ctx =
      NotifyContext { attachments: vec![Attachment { name: "a.txt".into(), data: b"x".to_vec(), mime_type: "text/plain".into() }], ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
      body(&requests[2]),
      json!({"type": "file", "file_name": "a.txt", "file_type": "text/plain", "file_url": "https://f/a.txt", "device_iden": "dev1"})
    );
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    for status in [401, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(2).mount(&server).await;
      let svc = from_url("pbul://tok123/d1/d2").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }

    // A failed or incomplete upload request fails before anything is pushed
    for response in
      [ResponseTemplate::new(500), ResponseTemplate::new(200).set_body_json(json!({"file_name": "x"})), ResponseTemplate::new(200).set_body_string("bad")]
    {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/v2/upload-request")).respond_with(response).mount(&server).await;
      Mock::given(method("POST")).and(path("/v2/pushes")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
      let svc = from_url("pbul://tok123").unwrap();
      let ctx = NotifyContext {
        body: "B".into(),
        attachments: vec![Attachment { name: String::new(), data: b"x".to_vec(), mime_type: "text/plain".into() }],
        ..Default::default()
      };
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }
  }

  #[test]
  fn test_service_details() {
    let details = Pushbullet::static_details();
    assert_eq!(details.service_name, "Pushbullet");
    assert!(details.attachment_support);
  }
}
