use crate::error::NotifyError;
use crate::notify::custom_json::{basic_auth, header_map};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::{ParsedUrl, unquote};
use async_trait::async_trait;
use serde_json::json;

pub struct Synology {
  host: String,
  port: Option<u16>,
  token: String,
  /// Path in front of `/webapi/entry.cgi`, percent-encoded, without a
  /// trailing slash.
  fullpath: String,
  file_url: Option<String>,
  headers: Vec<(String, String)>,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Synology {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.verified_host()?;
    let encoded: Vec<&str> = url.path.split('/').filter(|s| !s.is_empty()).collect();

    // The token is ?token= or the first path element; the rest of the
    // path is where the chat API lives. Upstream cuts the token off the
    // encoded path by its decoded length and doubles the slash before
    // /webapi; both are avoided here.
    let (token, rest) = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => (Some(unquote(t)), &encoded[..]),
      None => (url.path_parts.first().cloned(), encoded.get(1..).unwrap_or(&[])),
    };
    let Some(token) = token.filter(|t| !t.is_empty()) else {
      tracing::warn!("An invalid Synology Token was specified.");
      return None;
    };
    let fullpath = if rest.is_empty() { String::new() } else { format!("/{}", rest.join("/")) };

    Some(Self {
      host,
      port: url.port,
      token,
      fullpath,
      file_url: url.get("file_url").filter(|f| !f.is_empty()).map(unquote),
      // Upstream unquotes the +header arguments once more
      headers: url.qsd_add.iter().map(|(k, v)| (unquote(k), unquote(v))).collect(),
      secure: url.schema.ends_with('s'),
      user: url.user.clone(),
      password: url.password.clone(),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Synology Chat",
      service_url: Some("https://www.synology.com/"),
      setup_url: Some("https://appriseit.com/services/synology_chat/"),
      protocols: vec!["synology", "synologys"],
      description: "Send via Synology Chat.",
      attachment_support: false,
    }
  }

  fn api_url(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    format!("{}://{}{}{}/webapi/entry.cgi", schema, self.host, port_str, self.fullpath)
  }
}

#[async_trait]
impl Notify for Synology {
  fn schemas(&self) -> &[&str] {
    &["synology", "synologys"]
  }
  fn service_name(&self) -> &str {
    "Synology Chat"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn request_rate_per_sec(&self) -> f64 {
    // Upstream disables throttling for this service
    0.0
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let mut payload = json!({"text": ctx.body});
    if let Some(f) = &self.file_url {
      payload["file_url"] = json!(f);
    }
    let params = [("api", "SYNO.Chat.External"), ("method", "incoming"), ("version", "2"), ("token", self.token.as_str())];
    let headers = header_map(&[("User-Agent", APP_ID), ("Content-Type", "application/x-www-form-urlencoded"), ("Accept", "*/*")], &self.headers)?;

    let url = self.api_url();
    tracing::debug!("Synology Chat POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let client = build_client(self.verify_certificate)?;
    // .form() is applied before the headers so a + override can replace them
    let mut req = client.post(&url).query(&params).form(&[("payload", payload.to_string())]).headers(headers);
    if let Some((user, password)) = basic_auth(&self.user, &self.password) {
      req = req.basic_auth(user, Some(password));
    }

    crate::notify::throttle().await;
    let resp = req.send().await?;
    let status = resp.status();
    if status.is_success() {
      tracing::info!("Sent Synology Chat notification.");
      Ok(true)
    } else {
      tracing::warn!("Failed to send Synology Chat notification: error={}.", status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use serde_json::Value;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_urls() {
    for url in ["synology://:@/", "synology://", "synologys://", "synology://user@localhost"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    for url in [
      "synology://localhost/token",
      "synology://localhost/token?file_url=http://reddit.com/test.jpg",
      "synology://user:pass@localhost/token",
      "synology://user@localhost/token",
      "synology://localhost:8080/token",
      "synologys://localhost/?token=mytoken",
      "synologys://localhost:8080/token/path/",
      "synology://localhost:8080/path?+HeaderKey=HeaderValue",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_verifies_host() {
    // Upstream parses this URL with host verification
    crate::notify::custom_json::assert_verifies_host("synology://u:p@{h}/token");
  }

  fn syn(url: &str) -> Synology {
    Synology::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_parse() {
    assert_eq!(syn("synology://localhost/token").api_url(), "http://localhost/webapi/entry.cgi");
    let s = syn("synologys://user@localhost:8080/token/path/?file_url=http://reddit.com/test.jpg");
    assert_eq!(
      (s.token.as_str(), s.api_url(), s.file_url.as_deref()),
      ("token", "https://localhost:8080/path/webapi/entry.cgi".to_string(), Some("http://reddit.com/test.jpg"))
    );
    let s = syn("synologys://localhost/chat/?token=my+token");
    assert_eq!((s.token.as_str(), s.api_url()), ("my+token", "https://localhost/chat/webapi/entry.cgi".to_string()));
    let s = syn("synology://localhost/a%20b/c%2Fd");
    assert_eq!((s.token.as_str(), s.api_url()), ("a b", "http://localhost/c%2Fd/webapi/entry.cgi".to_string()));
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    let p = "/path/webapi/entry.cgi";
    Mock::given(method("POST"))
      .and(path(p))
      .and(query_param("api", "SYNO.Chat.External"))
      .and(query_param("method", "incoming"))
      .and(query_param("version", "2"))
      .and(query_param("token", "token"))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(header("Accept", "*/*"))
      .and(header("X-H", "1"))
      .and(header("Authorization", "Basic dXNlcjpOb25l"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let s = syn(&format!("synology://user@localhost:{}/token/path/?file_url=http://reddit.com/test.jpg&+X-H=1", server.address().port()));
    let ctx = NotifyContext { body: "hi \"there\"".into(), ..Default::default() };
    assert!(s.send(&ctx).await.unwrap());

    let form: Vec<(String, String)> = url::form_urlencoded::parse(&requests_to(&server, "POST", p).await[0].body).into_owned().collect();
    assert_eq!(form.len(), 1);
    assert_eq!(form[0].0, "payload");
    assert_eq!(serde_json::from_str::<Value>(&form[0].1).unwrap(), json!({"text": "hi \"there\"", "file_url": "http://reddit.com/test.jpg"}));
  }

  #[tokio::test]
  async fn test_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/webapi/entry.cgi")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    let s = syn(&format!("synology://localhost:{}/token", server.address().port()));
    assert!(matches!(s.send(&NotifyContext::default()).await, Err(NotifyError::ServiceError { status: 500, .. })));
    assert!(syn("synology://127.0.0.1:1/token").send(&NotifyContext::default()).await.is_err());
  }
}
