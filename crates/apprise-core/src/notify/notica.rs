use crate::error::NotifyError;
use crate::notify::custom_json::{basic_auth, custom_args, header_map};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

pub struct Notica {
  token: String,
  /// `None` for the official notica.us service.
  host: Option<String>,
  port: Option<u16>,
  /// Self-hosted path in front of `?token=`, with surrounding slashes.
  fullpath: String,
  user: Option<String>,
  password: Option<String>,
  headers: Vec<(String, String)>,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Notica {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Native https://notica.us/?{token} URLs arrive here already rewritten
    // by the registry as notica://{token}/
    let encoded: Vec<&str> = url.path.split('/').filter(|s| !s.is_empty()).collect();
    match encoded.split_last() {
      // notica://{token}: the official service
      None => Self::build(url, url.host.clone(), None, String::new()),
      Some((_, dirs)) => {
        let fullpath = if dirs.is_empty() { "/".to_string() } else { format!("/{}/", dirs.join("/")) };
        Self::build(url, url.path_parts.last().cloned(), url.host.clone(), fullpath)
      }
    }
  }

  fn build(url: &ParsedUrl, token: Option<String>, host: Option<String>, fullpath: String) -> Option<Self> {
    // A YAML `token:` replaces the URL's (upstream passes it to the
    // constructor); the URL still tells the official service from a
    // self-hosted one
    let token = url.kwarg("token").map(str::to_string).or(token);
    let Some(token) = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) else {
      tracing::warn!("An invalid Notica Token was specified.");
      return None;
    };
    let selfhosted = host.is_some();
    Some(Self {
      token,
      host,
      port: url.port,
      fullpath,
      user: url.user.clone(),
      password: url.password.clone(),
      // Header overrides only apply to self-hosted servers
      headers: if selfhosted { custom_args(url, '+') } else { Vec::new() },
      secure: url.schema.ends_with('s'),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Notica",
      service_url: Some("https://notica.us/"),
      setup_url: Some("https://appriseit.com/services/notica/"),
      protocols: vec!["notica", "noticas"],
      description: "Send push notifications via Notica.",
      attachment_support: false,
    }
  }

  fn notify_url(&self) -> String {
    match &self.host {
      None => format!("{}/?{}", service_base("https://notica.us"), self.token),
      Some(host) => {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        format!("{}://{}{}{}?token={}", schema, host, port_str, self.fullpath, self.token)
      }
    }
  }
}

#[async_trait]
impl Notify for Notica {
  fn schemas(&self) -> &[&str] {
    &["notica", "noticas"]
  }
  fn service_name(&self) -> &str {
    "Notica"
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

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = self.notify_url();
    let headers = header_map(&[("User-Agent", APP_ID), ("Content-Type", "application/x-www-form-urlencoded")], &self.headers)?;

    tracing::debug!("Notica POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let client = build_client(self.verify_certificate)?;
    // The body is sent as-is after "d:", not form encoded
    let mut req = client.post(&url).headers(headers).body(format!("d:{}", ctx.body));
    if self.host.is_some()
      && let Some((user, password)) = basic_auth(&self.user, &self.password)
    {
      req = req.basic_auth(user, Some(password));
    }

    crate::notify::throttle().await;
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    if status == 200 {
      tracing::info!("Sent Notica notification.");
      Ok(true)
    } else {
      tracing::warn!("Failed to send Notica notification: error={}.", status);
      Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_urls() {
    for url in ["notica://", "notica://:@/", "https://notica.us/", "https://sub.notica.us/?abc"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    let urls = vec![
      format!("https://notica.us/?{}", "z".repeat(6)),
      format!("https://notica.us/?{}&overflow=upstream", "z".repeat(6)),
      format!("notica://{}", "a".repeat(6)),
      format!("notica://localhost/{}", "b".repeat(6)),
      format!("notica://user@localhost/{}", "c".repeat(6)),
      format!("notica://user:pass@localhost/{}/", "d".repeat(6)),
      format!("notica://user:pass@localhost/a/path/{}/", "r".repeat(6)),
      format!("notica://localhost:8080/{}", "a".repeat(6)),
      format!("noticas://localhost:8080/path/{}", "5".repeat(6)),
      format!("notica://localhost:8080//{}/?+HeaderKey=HeaderValue", "7".repeat(6)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  fn notica(url: &str) -> Notica {
    Notica::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_parse() {
    // What the registry makes of https://notica.us/?zzzzzz&overflow=upstream
    let n = notica("notica://zzzzzz/?overflow=upstream");
    assert_eq!((n.token.as_str(), n.host.is_none()), ("zzzzzz", true));
    let n = notica("notica://aaaaaa");
    assert_eq!((n.token.as_str(), n.host.is_none()), ("aaaaaa", true));
    let n = notica("noticas://user:pass@localhost:8080/a/path/rrrrrr/");
    assert_eq!(n.notify_url(), "https://localhost:8080/a/path/?token=rrrrrr");
    assert_eq!(notica("notica://localhost:8080//777777/").notify_url(), "http://localhost:8080/?token=777777");
  }

  fn ctx() -> NotifyContext {
    NotifyContext { title: String::new(), body: "body & text".into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_official() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/"))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(header("User-Agent", APP_ID))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let n = notica("notica://abc123");
    assert!(crate::notify::with_test_base(server.uri(), n.send(&ctx())).await.unwrap());
    let r = &requests_to(&server, "POST", "/").await[0];
    assert_eq!(r.url.query(), Some("abc123"));
    assert_eq!(r.body, b"d:body & text");
  }

  #[tokio::test]
  async fn test_selfhosted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/a/path/"))
      .and(query_param("token", "tok"))
      .and(header("HeaderKey", "HeaderValue"))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let n = notica(&format!("notica://user:pass@localhost:{}/a/path/tok/?+HeaderKey=HeaderValue", server.address().port()));
    assert!(n.send(&ctx()).await.unwrap());
    assert_eq!(requests_to(&server, "POST", "/a/path/").await[0].body, b"d:body & text");
  }

  #[tokio::test]
  async fn test_failures() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/")).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    let n = notica(&format!("notica://localhost:{}/tok", server.address().port()));
    assert!(matches!(n.send(&ctx()).await, Err(NotifyError::ServiceError { status: 204, .. })));
    assert!(notica("notica://127.0.0.1:1/tok").send(&ctx()).await.is_err());
  }

  /// A YAML `token:` replaces the URL's, as upstream's constructor argument
  /// does; the URL still tells the official service from a self-hosted one
  /// (upstream: POST /?bbbbbb; POST /sub/?token=bbbbbb).
  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_yaml_token_replaces_the_urls() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let doc = format!("urls:\n  - notica://aaaaaa:\n      token: bbbbbb\n  - notica://{}/sub/aaaaaa:\n      token: bbbbbb\n", server.address());
    let (a, _) = crate::config::yaml::load_for_test(&doc).await;
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx())).await.is_success());
    let mut sent: Vec<String> = server
      .received_requests()
      .await
      .unwrap()
      .iter()
      .filter(|r| r.method.as_str() == "POST")
      .map(|r| format!("{}?{}", r.url.path(), r.url.query().unwrap_or_default()))
      .collect();
    sent.sort();
    assert_eq!(sent, ["/?bbbbbb", "/sub/?token=bbbbbb"]);
  }
}
