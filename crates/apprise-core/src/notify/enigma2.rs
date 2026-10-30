use crate::error::NotifyError;
use crate::notify::custom_json::{basic_auth, custom_args, header_map};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;
use crate::utils::qsd;
use async_trait::async_trait;
use serde_json::Value;

/// Seconds the message stays on screen unless `?timeout=` says otherwise.
const DEFAULT_TIMEOUT: i64 = 13;

/// `-1` (no timeout) is the lowest timeout accepted; lower values are raised to it.
const MIN_TIMEOUT: i64 = -1;

pub struct Enigma2 {
  host: String,
  port: Option<u16>,
  /// Path in front of `/api/message`, still percent-encoded.
  fullpath: String,
  user: Option<String>,
  password: Option<String>,
  secure: bool,
  timeout: i64,
  headers: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Enigma2 {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let timeout = qsd::get(url, "timeout").and_then(|t| t.trim().parse::<i64>().ok()).map(|t| t.max(MIN_TIMEOUT)).unwrap_or(DEFAULT_TIMEOUT);
    Some(Self {
      host,
      port: url.port,
      fullpath: url.path.clone(),
      user: url.user.clone(),
      password: url.password.clone(),
      secure: url.schema.ends_with('s'),
      timeout,
      headers: custom_args(url, '+'),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Enigma2",
      service_url: Some("https://dreambox.de/"),
      setup_url: Some("https://appriseit.com/services/enigma2/"),
      protocols: vec!["enigma2", "enigma2s"],
      description: "Send notifications to Enigma2 receivers.",
      attachment_support: false,
    }
  }

  fn api_url(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let path = self.fullpath.trim_end_matches('/');
    let path = if path.is_empty() { String::new() } else { format!("/{}", path) };
    format!("{}://{}{}{}/api/message", schema, self.host, port_str, path)
  }
}

/// Enigma2 message types: 1 info, 2 warning, 3 error.
fn message_type(notify_type: &NotifyType) -> u8 {
  match notify_type {
    NotifyType::Warning => 2,
    NotifyType::Failure => 3,
    _ => 1,
  }
}

#[async_trait]
impl Notify for Enigma2 {
  fn schemas(&self) -> &[&str] {
    &["enigma2", "enigma2s"]
  }
  fn service_name(&self) -> &str {
    "Enigma2"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    1000
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.5
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = self.api_url();
    let params = [("text", ctx.body.clone()), ("type", message_type(&ctx.notify_type).to_string()), ("timeout", self.timeout.to_string())];
    let headers = header_map(&[("User-Agent", APP_ID)], &self.headers)?;

    tracing::debug!("Enigma2 POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let client = build_client(self.verify_certificate)?;
    let mut req = client.get(&url).headers(headers).query(&params);
    if let Some((user, password)) = basic_auth(&self.user, &self.password) {
      req = req.basic_auth(user, Some(password));
    }

    let resp = req.send().await?;
    let status = resp.status().as_u16();
    if status != 200 {
      tracing::warn!("Failed to send Enigma2 notification: error={}.", status);
      return Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() });
    }

    // The receiver acknowledges with {"result": true}
    let text = resp.text().await.unwrap_or_default();
    let acknowledged = serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("result").cloned()).is_some_and(|r| is_truthy(&r));
    if !acknowledged {
      tracing::warn!("Failed to send Enigma2 notification: There was no server acknowledgement.");
      return Ok(false);
    }
    tracing::info!("Sent Enigma2 notification.");
    Ok(true)
  }
}

/// Python truthiness of a JSON value.
fn is_truthy(v: &Value) -> bool {
  match v {
    Value::Null => false,
    Value::Bool(b) => *b,
    Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
    Value::String(s) => !s.is_empty(),
    Value::Array(a) => !a.is_empty(),
    Value::Object(o) => !o.is_empty(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use serde_json::json;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "enigma2://localhost",
      "enigma2://user@localhost",
      "enigma2://user@localhost?timeout=-1",
      "enigma2://user@localhost?timeout=-1000",
      "enigma2://user@localhost?timeout=invalid",
      "enigma2://user:pass@localhost",
      "enigma2://localhost:8080",
      "enigma2://user:pass@localhost:8080",
      "enigma2s://localhost",
      "enigma2s://user:pass@localhost",
      "enigma2s://localhost:8080/path/",
      "enigma2s://user:pass@localhost:8080",
      "enigma2://localhost:8080/path?+HeaderKey=HeaderValue",
      "enigma2://user:pass@localhost:8083",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["enigma2://:@/", "enigma2://", "enigma2s://"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  fn enigma(url: &str) -> Enigma2 {
    Enigma2::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_parse() {
    assert_eq!(enigma("enigma2://localhost").timeout, 13);
    assert_eq!(enigma("enigma2://localhost?timeout=-1000").timeout, -1);
    assert_eq!(enigma("enigma2://localhost?timeout=invalid").timeout, 13);
    assert_eq!(enigma("enigma2://localhost?timeout=30").timeout, 30);
    assert_eq!(enigma("enigma2s://localhost:8080/path/").api_url(), "https://localhost:8080/path/api/message");
    assert_eq!(enigma("enigma2://localhost").api_url(), "http://localhost/api/message");
    let e = enigma("enigma2://localhost?+HeaderKey=Header+Value");
    assert_eq!(e.headers, vec![("HeaderKey".to_string(), "Header+Value".to_string())]);
  }

  fn ctx(nt: NotifyType) -> NotifyContext {
    NotifyContext { title: String::new(), body: "body text".into(), notify_type: nt, ..Default::default() }
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/web/api/message"))
      .and(query_param("text", "body text"))
      .and(query_param("type", "3"))
      .and(query_param("timeout", "-1"))
      .and(header("User-Agent", APP_ID))
      .and(header("HeaderKey", "HeaderValue"))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": true})))
      .expect(1)
      .mount(&server)
      .await;

    let e = enigma(&format!("enigma2://user:pass@localhost:{}/web/?timeout=-1&+HeaderKey=HeaderValue", server.address().port()));
    assert!(e.send(&ctx(NotifyType::Failure)).await.unwrap());
    assert_eq!(message_type(&NotifyType::Success), 1);
    assert_eq!(message_type(&NotifyType::Warning), 2);
  }

  #[tokio::test]
  async fn test_acknowledgement_required() {
    for body in ["{", r#"{"result": false}"#, "{}", "[]"] {
      let server = MockServer::start().await;
      Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string(body)).expect(1).mount(&server).await;
      let e = enigma(&format!("enigma2://localhost:{}", server.address().port()));
      assert!(!e.send(&ctx(NotifyType::Info)).await.unwrap(), "{body}");
    }
  }

  #[tokio::test]
  async fn test_failure_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(500).set_body_json(json!({"result": true}))).expect(1).mount(&server).await;
    let e = enigma(&format!("enigma2://user@localhost:{}", server.address().port()));
    assert!(matches!(e.send(&ctx(NotifyType::Info)).await, Err(NotifyError::ServiceError { status: 500, .. })));
    assert!(enigma("enigma2://127.0.0.1:1").send(&ctx(NotifyType::Info)).await.is_err());
  }
}
