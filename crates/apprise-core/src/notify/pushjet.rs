use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::{ParsedUrl, unquote};
use async_trait::async_trait;
use serde_json::json;

pub struct PushJet {
  host: String,
  port: Option<u16>,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  secret_key: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl PushJet {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // pjet[s]://[user:pass@]host[:port]/{secret_key}
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    // Upstream unquotes ?secret= once more
    let secret_key = url.get("secret").filter(|s| !s.is_empty()).map(unquote).or_else(|| url.path_parts.first().cloned());
    let Some(secret_key) = secret_key.as_deref().and_then(|s| s.split_whitespace().next()).map(str::to_string) else {
      tracing::warn!("An invalid Pushjet Secret Key was specified.");
      return None;
    };
    Some(Self {
      host,
      port: url.port,
      secure: url.schema == "pjets",
      user: url.user.clone().filter(|u| !u.is_empty()),
      password: url.password.clone(),
      secret_key,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pushjet",
      service_url: None,
      setup_url: Some("https://appriseit.com/services/pushjet/"),
      protocols: vec!["pjet", "pjets"],
      description: "Send notifications via a self-hosted Pushjet server.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for PushJet {
  fn schemas(&self) -> &[&str] {
    &["pjet", "pjets"]
  }
  fn service_name(&self) -> &str {
    "Pushjet"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  /// Upstream does not throttle Pushjet
  fn request_rate_per_sec(&self) -> f64 {
    0.0
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = format!("{}://{}{}/message/", if self.secure { "https" } else { "http" }, self.host, self.port.map(|p| format!(":{}", p)).unwrap_or_default());
    // Upstream posts JSON under a form content type
    let payload = json!({ "message": ctx.body, "title": ctx.title, "link": null, "level": null });
    let client = build_client(self.verify_certificate)?;
    let mut request = client
      .post(&url)
      .query(&[("secret", self.secret_key.as_str())])
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8")
      .body(serde_json::to_vec(&payload)?);
    if let Some(user) = &self.user {
      request = request.basic_auth(user, Some(self.password.as_deref().unwrap_or("")));
    }

    crate::notify::throttle().await;
    let resp = request.send().await?;
    let status = resp.status();
    if status.as_u16() == 200 {
      tracing::info!("Sent Pushjet notification.");
      return Ok(true);
    }
    let reason = status.canonical_reason().unwrap_or("");
    tracing::warn!("Failed to send Pushjet notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
    Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_urls() {
    let a = "a".repeat(32);
    for url in [format!("pjet://user:pass@localhost/{}", a), format!("pjets://localhost/{}", a), format!("pjet://localhost:8080/?secret={}", a)] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
    for url in
      ["pjet://".to_string(), "pjets://".to_string(), "pjet://:@/".to_string(), "pjet://localhost".to_string(), "pjet://localhost/%20?secret=%20".to_string()]
    {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/message/"))
      .and(query_param("secret", "abc123"))
      .and(header("authorization", "Basic dXNlcjpwYXNz"))
      .and(header("content-type", "application/x-www-form-urlencoded; charset=utf-8"))
      .and(body_json(json!({"message": "B", "title": "T", "link": null, "level": null})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let addr = server.address();
    let svc = from_url(&format!("pjet://user:pass@{}:{}/abc123", addr.ip(), addr.port())).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/message/")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let addr = server.address();
      let svc = from_url(&format!("pjet://{}:{}/abc123", addr.ip(), addr.port())).unwrap();
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(svc.send(&ctx).await.is_err());
    }
  }

  #[test]
  fn test_service_details() {
    let details = PushJet::static_details();
    assert_eq!(details.service_name, "Pushjet");
    assert_eq!(details.protocols, vec!["pjet", "pjets"]);
  }
}
