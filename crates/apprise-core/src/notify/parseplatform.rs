use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::unquote;
use async_trait::async_trait;
use serde_json::json;

/// Device types (`PARSE_PLATFORM_DEVICES`); `all` expands to the others.
const PARSE_PLATFORM_DEVICES: &[&str] = &["all", "ios", "android"];

pub struct ParsePlatform {
  host: String,
  port: Option<u16>,
  secure: bool,
  /// The URL path, still percent-encoded (upstream 8b89b08b)
  fullpath: String,
  application_id: String,
  master_key: String,
  devices: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl ParsePlatform {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // parsep[s]://{app_id}:{master_key}@{host}[:{port}][/{path}]
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let qsd = |k: &str| url.get(k).filter(|v| !v.is_empty()).map(str::to_string);
    let token = |v: Option<String>| v.as_deref().and_then(|v| v.split_whitespace().next()).map(str::to_string);

    let Some(application_id) = token(qsd("app_id").or_else(|| url.user.as_deref().map(unquote))) else {
      tracing::warn!("An invalid Parse Platform Application ID was specified.");
      return None;
    };
    let Some(master_key) = token(qsd("master_key").or_else(|| url.password.as_deref().map(unquote))) else {
      tracing::warn!("An invalid Parse Platform Master Key was specified.");
      return None;
    };

    let devices = match qsd("device") {
      None => vec!["ios".to_string(), "android".to_string()],
      Some(device) if device == "all" => vec!["ios".to_string(), "android".to_string()],
      Some(device) if PARSE_PLATFORM_DEVICES.contains(&device.as_str()) => vec![device],
      Some(device) => {
        tracing::warn!("An invalid Parse Platform device ({}) was specified.", device);
        return None;
      }
    };

    Some(Self {
      host,
      port: url.port,
      secure: url.schema == "parseps",
      fullpath: url.path.trim_end_matches('/').to_string(),
      application_id,
      master_key,
      devices,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Parse Platform",
      service_url: Some("https://parseplatform.org/"),
      setup_url: Some("https://appriseit.com/services/parseplatform/"),
      protocols: vec!["parsep", "parseps"],
      description: "Send push notifications via Parse Platform.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for ParsePlatform {
  fn schemas(&self) -> &[&str] {
    &["parsep", "parseps"]
  }
  fn service_name(&self) -> &str {
    "Parse Platform"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let path = if self.fullpath.is_empty() { String::new() } else { format!("/{}", self.fullpath) };
    let url =
      format!("{}://{}{}{}/parse/push/", if self.secure { "https" } else { "http" }, self.host, self.port.map(|p| format!(":{}", p)).unwrap_or_default(), path);
    let payload = json!({
      "where": { "deviceType": { "$in": self.devices } },
      "data": { "title": ctx.title, "alert": ctx.body },
    });

    let client = build_client(self.verify_certificate)?;
    let resp = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .header("X-Parse-Application-Id", &self.application_id)
      .header("X-Parse-Master-Key", &self.master_key)
      .body(serde_json::to_vec(&payload)?)
      .send()
      .await?;
    let status = resp.status();
    if status.as_u16() == 200 {
      tracing::info!("Sent Parse Platform notification.");
      return Ok(true);
    }
    let reason = status.canonical_reason().unwrap_or("");
    tracing::warn!("Failed to send Parse Platform notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
    Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_urls() {
    let a = "a".repeat(32);
    for url in [
      "parseps://localhost?app_id=app&master_key=key".to_string(),
      "parsep://app_id:master_key@localhost:8080?device=ios".to_string(),
      "parseps://app_id:master_key@localhost".to_string(),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
    for url in [
      "parsep://".to_string(),
      "parsep://:@/".to_string(),
      format!("parsep://{}", a),
      format!("parsep://app_id@{}", a),
      format!("parseps://:master_key@{}", a),
      "parsep://app_id:master_key@localhost?device=invalid".to_string(),
      "parsep://app_id:master_key@localhost?device=IOS".to_string(),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
    let obj = ParsePlatform::from_url(&ParsedUrl::parse("parsep://a:b@localhost").unwrap()).unwrap();
    assert_eq!(obj.devices, vec!["ios", "android"]);
    let obj = ParsePlatform::from_url(&ParsedUrl::parse("parsep://a:b@localhost?device=android&app_id=x").unwrap()).unwrap();
    assert_eq!((obj.devices.clone(), obj.application_id.as_str()), (vec!["android".to_string()], "x"));
  }

  #[tokio::test]
  async fn test_send() {
    // Matches pycapture.py: the encoded path is kept as-is
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/my%2Fapp/sub/parse/push/"))
      .and(header("x-parse-application-id", "app"))
      .and(header("x-parse-master-key", "key"))
      .and(body_json(json!({"where": {"deviceType": {"$in": ["ios"]}}, "data": {"title": "T", "alert": "B"}})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let addr = server.address();
    let svc = from_url(&format!("parsep://app:key@{}:{}/my%2Fapp/sub/?device=ios", addr.ip(), addr.port())).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_root_path_and_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/parse/push/")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let addr = server.address();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(from_url(&format!("parsep://app:key@{}:{}", addr.ip(), addr.port())).unwrap().send(&ctx).await.unwrap());

    for status in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let addr = server.address();
      assert!(from_url(&format!("parsep://app:key@{}:{}", addr.ip(), addr.port())).unwrap().send(&ctx).await.is_err());
    }
  }

  #[test]
  fn test_service_details() {
    let details = ParsePlatform::static_details();
    assert_eq!(details.service_name, "Parse Platform");
    assert_eq!(details.protocols, vec!["parsep", "parseps"]);
  }
}
