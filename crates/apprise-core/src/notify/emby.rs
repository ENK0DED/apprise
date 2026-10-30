use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::{ParsedUrl, parse_bool};
use async_trait::async_trait;
use md5::Md5;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

/// Device ID upstream registers with the server.
const DEVICE_ID: &str = "48df9504-6843-49be-9f2d-a685e25a0bc8";

/// How long a non-modal message stays on screen.
const MESSAGE_TIMEOUT_MS: u64 = 60000;

/// Both Emby and Jellyfin listen here unless told otherwise.
const DEFAULT_PORT: u16 = 8096;

/// The server products sharing this API. Jellyfin is an Emby fork that kept
/// the session messaging endpoints.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Flavor {
  Emby,
  Jellyfin,
}

impl Flavor {
  pub(crate) fn details(self) -> ServiceDetails {
    match self {
      Flavor::Emby => ServiceDetails {
        service_name: "Emby",
        service_url: Some("https://emby.media/"),
        setup_url: Some("https://appriseit.com/services/emby/"),
        protocols: vec!["emby", "embys"],
        description: "Send notifications to Emby.",
        attachment_support: false,
      },
      Flavor::Jellyfin => ServiceDetails {
        service_name: "Jellyfin",
        service_url: Some("https://jellyfin.org/"),
        setup_url: Some("https://appriseit.com/services/jellyfin/"),
        protocols: vec!["jellyfin", "jellyfins"],
        description: "Send notifications to Jellyfin.",
        attachment_support: false,
      },
    }
  }

  fn schemas(self) -> &'static [&'static str] {
    match self {
      Flavor::Emby => &["emby", "embys"],
      Flavor::Jellyfin => &["jellyfin", "jellyfins"],
    }
  }
}

/// An authenticated session: the access token and the user's ID.
struct Login {
  token: String,
  user_id: String,
}

pub struct Emby {
  flavor: Flavor,
  host: String,
  port: u16,
  user: String,
  password: Option<String>,
  secure: bool,
  modal: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Emby {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    Self::from_url_as(url, Flavor::Emby)
  }

  pub(crate) fn from_url_as(url: &ParsedUrl, flavor: Flavor) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let Some(user) = url.user.clone().filter(|u| !u.is_empty()) else {
      tracing::warn!("No {} username was specified.", flavor.details().service_name);
      return None;
    };
    Some(Self {
      flavor,
      host,
      port: url.port.unwrap_or(DEFAULT_PORT),
      user,
      password: url.password.clone().filter(|p| !p.is_empty()),
      secure: url.schema.ends_with('s'),
      modal: url.get("modal").map(parse_bool).unwrap_or(false),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    Flavor::Emby.details()
  }

  fn base_url(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    format!("{}://{}:{}", schema, self.host, self.port)
  }

  /// The `X-Emby-Authorization` header. Once logged in it also names the
  /// user (upstream sends the user name, not the ID).
  fn auth_header(&self, ctx: &NotifyContext, logged_in: bool) -> String {
    let app = ctx.asset.app_id.as_str();
    let mut args = vec![("MediaBrowser Client", app), ("Device", app), ("DeviceId", DEVICE_ID), ("Version", env!("CARGO_PKG_VERSION"))];
    if logged_in {
      args.push(("UserId", self.user.as_str()));
    }
    args.iter().map(|(k, v)| format!("{}=\"{}\"", k, v)).collect::<Vec<_>>().join(", ")
  }

  fn request(&self, client: &reqwest::Client, method: reqwest::Method, url: &str, ctx: &NotifyContext, login: Option<&Login>) -> reqwest::RequestBuilder {
    let mut req = client
      .request(method, url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .header("X-Emby-Authorization", self.auth_header(ctx, login.is_some()));
    if let Some(l) = login {
      req = req.header("X-MediaBrowser-Token", &l.token);
    }
    req
  }

  async fn login(&self, client: &reqwest::Client, ctx: &NotifyContext) -> Result<Option<Login>, NotifyError> {
    let url = format!("{}/Users/AuthenticateByName", self.base_url());
    let payload = match &self.password {
      Some(p) => json!({
        "Username": self.user,
        "pw": p,
        "passwordMd5": hex::encode(Md5::digest(p.as_bytes())),
        "password": hex::encode(Sha1::digest(p.as_bytes())),
      }),
      None => json!({"Username": self.user, "password": "", "passwordMd5": "", "pw": ""}),
    };

    let resp = self.request(client, reqwest::Method::POST, &url, ctx, None).body(payload.to_string()).send().await?;
    let status = resp.status().as_u16();
    if status != 200 {
      tracing::warn!("Failed to authenticate {} user {} details: error={}.", self.flavor.details().service_name, self.user, status);
      return Err(NotifyError::Auth(format!("{} authentication failed with HTTP {}", self.flavor.details().service_name, status)));
    }
    let Ok(results) = resp.json::<Value>().await else { return Ok(None) };
    let token = results.get("AccessToken").and_then(Value::as_str).filter(|t| !t.is_empty());
    let user_id = results.get("Id").and_then(Value::as_str).filter(|t| !t.is_empty()).or_else(|| results.pointer("/User/Id").and_then(Value::as_str));
    Ok(match (token, user_id.filter(|u| !u.is_empty())) {
      (Some(token), Some(user_id)) => Some(Login { token: token.to_string(), user_id: user_id.to_string() }),
      _ => None,
    })
  }

  /// The IDs of the sessions this user controls.
  async fn sessions(&self, client: &reqwest::Client, ctx: &NotifyContext, login: &Login) -> Vec<String> {
    let url = format!("{}/Sessions?ControllableByUserId={}", self.base_url(), login.user_id);
    let resp = match self.request(client, reqwest::Method::GET, &url, ctx, Some(login)).send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred querying {} for session information at {}.", self.flavor.details().service_name, self.host);
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return Vec::new();
      }
    };
    if resp.status().as_u16() != 200 {
      tracing::warn!("Failed to acquire {} session for user {}: error={}.", self.flavor.details().service_name, self.user, resp.status().as_u16());
      return Vec::new();
    }
    let entries: Vec<Value> = resp.json().await.unwrap_or_default();
    let mut ids: Vec<String> = Vec::new();
    for id in entries.iter().filter_map(|e| e.get("Id").and_then(Value::as_str)).filter(|id| !id.is_empty()) {
      if !ids.iter().any(|i| i == id) {
        ids.push(id.to_string());
      }
    }
    ids
  }

  async fn logout(&self, client: &reqwest::Client, ctx: &NotifyContext, login: &Login) {
    let url = format!("{}/Sessions/Logout", self.base_url());
    match self.request(client, reqwest::Method::POST, &url, ctx, Some(login)).send().await {
      Ok(r) if [200, 204, 401].contains(&r.status().as_u16()) => {}
      Ok(r) => tracing::warn!("Failed to logoff {} user {}: error={}.", self.flavor.details().service_name, self.user, r.status().as_u16()),
      Err(e) => tracing::debug!("Socket Exception: {}", e),
    }
  }
}

#[async_trait]
impl Notify for Emby {
  fn schemas(&self) -> &[&str] {
    self.flavor.schemas()
  }
  fn service_name(&self) -> &str {
    self.flavor.details().service_name
  }
  fn details(&self) -> ServiceDetails {
    self.flavor.details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let Some(login) = self.login(&client, ctx).await? else {
      return Ok(false);
    };

    let sessions = self.sessions(&client, ctx, &login).await;
    if sessions.is_empty() {
      tracing::warn!("There were no {} sessions to notify.", self.flavor.details().service_name);
      self.logout(&client, ctx, &login).await;
      return Ok(true);
    }

    let mut payload = json!({"Header": ctx.title, "Text": ctx.body});
    if !self.modal {
      payload["TimeoutMs"] = json!(MESSAGE_TIMEOUT_MS);
    }
    let body = payload.to_string();

    let mut has_error = false;
    for session in &sessions {
      // Skip a session that already accepted this message so a retry does
      // not deliver it twice
      if crate::delivery::is_delivered(session, false) {
        continue;
      }
      let url = format!("{}/Sessions/{}/Message", self.base_url(), session);
      crate::notify::throttle().await;
      match self.request(&client, reqwest::Method::POST, &url, ctx, Some(&login)).body(body.clone()).send().await {
        Ok(r) if [200, 204].contains(&r.status().as_u16()) => {
          tracing::info!("Sent {} notification.", self.flavor.details().service_name);
          crate::delivery::mark_delivered(session, false);
        }
        Ok(r) => {
          tracing::warn!("Failed to send {} notification: error={}.", self.flavor.details().service_name, r.status().as_u16());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending {} notification to {}.", self.flavor.details().service_name, self.host);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
        }
      }
    }

    // Upstream logs out when the plugin is dropped, which a CLI run does
    // right after sending; do it here so no session is left behind
    self.logout(&client, ctx, &login).await;
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const AUTH: &str = "MediaBrowser Client=\"Apprise\", Device=\"Apprise\", DeviceId=\"48df9504-6843-49be-9f2d-a685e25a0bc8\", Version=\"";

  #[test]
  fn test_urls() {
    for url in ["emby://", "embys://", "emby://localhost", "emby://:@/", "emby://:pass@localhost"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    for url in ["emby://l2g@localhost", "embys://l2g:password@localhost", "emby://l2g:pass@localhost:1234?modal=yes"] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
    let e = Emby::from_url(&ParsedUrl::parse("embys://l2g:password@localhost").unwrap()).unwrap();
    assert_eq!(e.base_url(), "https://localhost:8096");
    assert!(!e.modal);
    assert!(Emby::from_url(&ParsedUrl::parse("emby://l2g@localhost?modal=yes").unwrap()).unwrap().modal);
  }

  fn emby_for(server: &MockServer, auth: &str, query: &str) -> Emby {
    Emby::from_url(&ParsedUrl::parse(&format!("emby://{}@localhost:{}{}", auth, server.address().port(), query)).unwrap()).unwrap()
  }

  fn ctx() -> NotifyContext {
    NotifyContext { title: "title".into(), body: "body".into(), ..Default::default() }
  }

  async fn mount_login(server: &MockServer, response: Value) {
    Mock::given(method("POST")).and(path("/Users/AuthenticateByName")).respond_with(ResponseTemplate::new(200).set_body_json(response)).mount(server).await;
  }

  #[tokio::test]
  async fn test_full_flow() {
    let server = MockServer::start().await;
    let version = env!("CARGO_PKG_VERSION");
    Mock::given(method("POST"))
      .and(path("/Users/AuthenticateByName"))
      .and(header("Content-Type", "application/json"))
      .and(body_json(json!({
        "Username": "l2g", "pw": "pass",
        "passwordMd5": "1a1dc91c907325c69271ddf0c944bc72",
        "password": "9d4e1e23bd5b727046a9e3b4b7db57bd8d6ee684",
      })))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"AccessToken": "tok", "User": {"Id": "uid"}})))
      .expect(1)
      .mount(&server)
      .await;
    let logged_in = format!("{}{}\", UserId=\"l2g\"", AUTH, version);
    Mock::given(method("GET"))
      .and(path("/Sessions"))
      .and(query_param("ControllableByUserId", "uid"))
      .and(header("X-MediaBrowser-Token", "tok"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"Id": "s1"}, {"Id": "s2"}, {"x": 1}])))
      .expect(1)
      .mount(&server)
      .await;
    for s in ["s1", "s2"] {
      Mock::given(method("POST"))
        .and(path(format!("/Sessions/{}/Message", s)))
        .and(header("X-MediaBrowser-Token", "tok"))
        .and(body_json(json!({"Header": "title", "Text": "body", "TimeoutMs": 60000})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    }
    Mock::given(method("POST"))
      .and(path("/Sessions/Logout"))
      .and(header("X-MediaBrowser-Token", "tok"))
      .respond_with(ResponseTemplate::new(204))
      .expect(1)
      .mount(&server)
      .await;

    assert!(emby_for(&server, "l2g:pass", "").send(&ctx()).await.unwrap());

    // wiremock splits header values on commas, so compare these directly
    let auth = |r: &wiremock::Request| r.headers.get("X-Emby-Authorization").unwrap().to_str().unwrap().to_string();
    let login = requests_to(&server, "POST", "/Users/AuthenticateByName").await;
    assert_eq!(auth(&login[0]), format!("{}{}\"", AUTH, version));
    for (m, p) in [("GET", "/Sessions"), ("POST", "/Sessions/s1/Message"), ("POST", "/Sessions/s2/Message"), ("POST", "/Sessions/Logout")] {
      let requests = requests_to(&server, m, p).await;
      assert_eq!(requests.len(), 1, "{m} {p}");
      assert_eq!(auth(&requests[0]), logged_in);
    }
  }

  #[tokio::test]
  async fn test_modal_and_no_password() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/Users/AuthenticateByName"))
      .and(body_json(json!({"Username": "l2g", "password": "", "passwordMd5": "", "pw": ""})))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"AccessToken": "tok", "Id": "uid"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("GET")).and(path("/Sessions")).respond_with(ResponseTemplate::new(200).set_body_json(json!([{"Id": "s1"}]))).mount(&server).await;
    Mock::given(method("POST"))
      .and(path("/Sessions/s1/Message"))
      .and(body_json(json!({"Header": "title", "Text": "body"})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/Sessions/Logout")).respond_with(ResponseTemplate::new(401)).mount(&server).await;

    assert!(emby_for(&server, "l2g", "?modal=yes").send(&ctx()).await.unwrap());
  }

  #[tokio::test]
  async fn test_no_sessions_is_success() {
    let server = MockServer::start().await;
    mount_login(&server, json!({"AccessToken": "tok", "Id": "uid"})).await;
    Mock::given(method("GET")).and(path("/Sessions")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    Mock::given(method("POST")).and(path("/Sessions/Logout")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    assert!(emby_for(&server, "l2g:pass", "").send(&ctx()).await.unwrap());
  }

  #[tokio::test]
  async fn test_failures() {
    // Login rejected
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/Users/AuthenticateByName")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    assert!(emby_for(&server, "l2g:pass", "").send(&ctx()).await.is_err());

    // Login without a token or user ID, or with bad JSON
    for body in [json!({"AccessToken": "tok"}), json!({"Id": "uid"})] {
      let server = MockServer::start().await;
      mount_login(&server, body).await;
      assert!(!emby_for(&server, "l2g:pass", "").send(&ctx()).await.unwrap());
    }
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/Users/AuthenticateByName")).respond_with(ResponseTemplate::new(200).set_body_string("{")).mount(&server).await;
    assert!(!emby_for(&server, "l2g:pass", "").send(&ctx()).await.unwrap());

    // One session fails
    let server = MockServer::start().await;
    mount_login(&server, json!({"AccessToken": "tok", "Id": "uid"})).await;
    Mock::given(method("GET"))
      .and(path("/Sessions"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"Id": "s1"}, {"Id": "s2"}])))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/Sessions/s1/Message")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/Sessions/s2/Message")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/Sessions/Logout")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    assert!(!emby_for(&server, "l2g:pass", "").send(&ctx()).await.unwrap());

    assert!(matches!(Emby::from_url(&ParsedUrl::parse("emby://l2g@127.0.0.1:1").unwrap()).unwrap().send(&ctx()).await, Err(NotifyError::Http(_))));
  }

  #[tokio::test]
  async fn test_retry_skips_sessions_that_accepted() {
    let server = MockServer::start().await;
    mount_login(&server, json!({"AccessToken": "tok", "Id": "uid"})).await;
    Mock::given(method("GET"))
      .and(path("/Sessions"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"Id": "s1"}, {"Id": "s2"}])))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/Sessions/s1/Message")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    // The second session fails once, then accepts the retry
    let s2 = || Mock::given(method("POST")).and(path("/Sessions/s2/Message"));
    s2().respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    s2().respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/Sessions/Logout")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("emby://l2g:pass@localhost:{}?retry=1&wait=0", server.address().port())));
    assert!(apprise.notify(&ctx()).await.is_success());
  }
}
