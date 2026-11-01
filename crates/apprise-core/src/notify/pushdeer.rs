use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

/// The public PushDeer server, used when the URL names no host.
const DEFAULT_HOSTNAME: &str = "api2.pushdeer.com";

pub struct PushDeer {
  push_key: String,
  host: Option<String>,
  port: Option<u16>,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl PushDeer {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // pushdeer://{pushkey} or pushdeer://{host}[:{port}]/{pushkey}; the
    // last path entry is the key
    let (push_key, host) = match url.path_parts.last() {
      Some(key) => (key.clone(), url.host.clone()),
      None => (url.host.clone().unwrap_or_default(), None),
    };
    // A YAML `pushkey:` replaces the URL's (upstream passes it to the
    // constructor); the URL still tells the official server from another
    let push_key = url.kwarg("pushkey").map_or(push_key, str::to_string);
    let push_key = push_key.trim().to_string();
    if push_key.is_empty() || !push_key.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("An invalid PushDeer API Pushkey was specified.");
      return None;
    }
    Some(Self {
      push_key,
      port: host.as_ref().and(url.port),
      host,
      secure: url.schema == "pushdeers",
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "PushDeer",
      service_url: Some("https://www.pushdeer.com/"),
      setup_url: Some("https://appriseit.com/services/pushdeer/"),
      protocols: vec!["pushdeer", "pushdeers"],
      description: "Send notifications via PushDeer.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for PushDeer {
  fn schemas(&self) -> &[&str] {
    &["pushdeer", "pushdeers"]
  }
  fn service_name(&self) -> &str {
    "PushDeer"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let schema = if self.secure { "https" } else { "http" };
    let port = self.port.unwrap_or(if self.secure { 443 } else { 80 });
    let base = match &self.host {
      Some(host) => format!("{}://{}:{}", schema, host, port),
      None => service_base(&format!("{}://{}:{}", schema, DEFAULT_HOSTNAME, port)),
    };
    let url = format!("{}/message/push?pushkey={}", base, self.push_key);

    let has_title = !ctx.title.is_empty();
    let form =
      [("text", if has_title { ctx.title.as_str() } else { ctx.body.as_str() }), ("type", "text"), ("desp", if has_title { ctx.body.as_str() } else { "" })];
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client.post(&url).header("User-Agent", APP_ID).form(&form).send().await?;
    let status = resp.status();
    if status.as_u16() == 200 {
      tracing::info!("Sent PushDeer notification.");
      return Ok(true);
    }
    let reason = status.canonical_reason().unwrap_or("");
    tracing::warn!("Failed to send PushDeer notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
    Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn pushdeer(url: &str) -> PushDeer {
    PushDeer::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_urls() {
    let a8 = "a".repeat(8);
    for url in [format!("pushdeer://localhost/{}", a8), format!("pushdeer://localhost:80/{}", a8), format!("pushdeer://{}", a8), format!("pushdeers://{}", a8)]
    {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
    for url in ["pushdeer://", "pushdeers://", "pushdeer://bad-key", "pushdeer://host/bad_key"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    let obj = pushdeer("pushdeers://h.example:8443/x/abc123");
    assert_eq!((obj.push_key.as_str(), obj.host.as_deref(), obj.port, obj.secure), ("abc123", Some("h.example"), Some(8443), true));
    let obj = pushdeer("pushdeer://abc123");
    assert_eq!((obj.push_key.as_str(), obj.host.as_deref()), ("abc123", None));
  }

  #[tokio::test]
  async fn test_send() {
    // Matches pycapture.py for pushdeer://abc123
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/message/push"))
      .and(query_param("pushkey", "abc123"))
      .and(body_string("text=T&type=text&desp=B"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), from_url("pushdeer://abc123").unwrap().send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_own_host_without_title() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/message/push"))
      .and(body_string("text=B&type=text&desp="))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let addr = server.address();
    let svc = from_url(&format!("pushdeer://{}:{}/abc123", addr.ip(), addr.port())).unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/message/push")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), from_url("pushdeer://abc123").unwrap().send(&ctx)).await.is_err());
    }
  }

  #[test]
  fn test_service_details() {
    let details = PushDeer::static_details();
    assert_eq!(details.service_name, "PushDeer");
    assert_eq!(details.protocols, vec!["pushdeer", "pushdeers"]);
  }

  /// A YAML `pushkey:` replaces the URL's, as upstream's constructor
  /// argument does, for the official server and another (upstream: POST
  /// /message/push?pushkey=bbbbbbbb to each).
  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_yaml_pushkey_replaces_the_urls() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/message/push")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let doc = format!("urls:\n  - pushdeer://aaaaaaaa:\n      pushkey: bbbbbbbb\n  - pushdeer://{}/aaaaaaaa:\n      pushkey: bbbbbbbb\n", server.address());
    let (a, _) = crate::config::yaml::load_for_test(&doc).await;
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
    let sent = crate::notify::custom_json::requests_to(&server, "POST", "/message/push").await;
    assert_eq!(sent.iter().map(|r| r.url.query().unwrap_or_default().to_string()).collect::<Vec<_>>(), ["pushkey=bbbbbbbb", "pushkey=bbbbbbbb"]);
  }
}
