use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
pub struct NextcloudTalk {
  host: String,
  port: Option<u16>,
  user: String,
  password: String,
  rooms: Vec<String>,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl NextcloudTalk {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone()?;
    let user = url.user.clone()?;
    let password = url.password.clone()?;
    let mut rooms = url.path_parts.clone();
    if let Some(to) = url.get("to") {
      rooms.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    Some(Self { host, port: url.port, user, password, rooms, secure: url.schema == "nctalks", verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Nextcloud Talk",
      service_url: Some("https://nextcloud.com"),
      setup_url: None,
      protocols: vec!["nctalk", "nctalks"],
      description: "Send Nextcloud Talk messages.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for NextcloudTalk {
  fn schemas(&self) -> &[&str] {
    &["nctalk", "nctalks"]
  }
  fn service_name(&self) -> &str {
    "Nextcloud Talk"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
    let client = build_client(self.verify_certificate)?;
    let mut all_ok = true;
    for room in &self.rooms {
      let url = format!("{}://{}{}/ocs/v2.php/apps/spreed/api/v1/chat/{}", schema, self.host, port_str, room);
      let params = [("message", msg.as_str())];
      let resp = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("OCS-APIREQUEST", "true")
        .basic_auth(&self.user, Some(&self.password))
        .form(&params)
        .send()
        .await?;
      if !resp.status().is_success() {
        all_ok = false;
      }
    }
    Ok(all_ok)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::NotifyContext;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "nctalk://user:pass@localhost",
      "nctalk://user:pass@localhost/roomid1/roomid2",
      "nctalk://user:pass@localhost:8080/roomid",
      "nctalk://user:pass@localhost:8080/roomid?url_prefix=/prefix",
      "nctalks://user:pass@localhost/roomid",
      "nctalks://user:pass@localhost:8080/roomid/",
      "nctalk://user:pass@localhost:8080/roomid?+HeaderKey=HeaderValue",
      "nctalk://user:pass@localhost:8083/roomid1/roomid2/roomid3",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["nctalk://:@/", "nctalk://", "nctalks://", "nctalk://localhost", "nctalk://localhost/roomid", "nctalk://user@localhost/roomid"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  fn nctalk_for_mock(server: &MockServer, user: &str, pass: &str, rooms: &[&str]) -> NextcloudTalk {
    let addr = server.address();
    let rooms_path = rooms.iter().map(|r| format!("/{}", r)).collect::<String>();
    let url_str = format!("nctalk://{}:{}@{}:{}{}", user, pass, addr.ip(), addr.port(), rooms_path);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    NextcloudTalk::from_url(&parsed).unwrap()
  }

  #[tokio::test]
  async fn test_send_single_room_success() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/chat/roomid1"))
      .and(header("OCS-APIREQUEST", "true"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;

    let nc = nctalk_for_mock(&server, "admin", "pass", &["roomid1"]);
    let ctx = NotifyContext { title: "Test Title".into(), body: "Test Body".into(), ..Default::default() };
    let result = nc.send(&ctx).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_send_multiple_rooms() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/chat/room1"))
      .and(header("OCS-APIREQUEST", "true"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;

    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/chat/room2"))
      .and(header("OCS-APIREQUEST", "true"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;

    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/chat/room3"))
      .and(header("OCS-APIREQUEST", "true"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;

    let nc = nctalk_for_mock(&server, "user", "pass", &["room1", "room2", "room3"]);
    let ctx = NotifyContext { title: "Title".into(), body: "Body".into(), ..Default::default() };
    let result = nc.send(&ctx).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_send_basic_auth_header() {
    let server = MockServer::start().await;

    // Verify basic auth is sent (base64 of "admin:secret")
    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/chat/myroom"))
      .and(header("Authorization", "Basic YWRtaW46c2VjcmV0"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;

    let nc = nctalk_for_mock(&server, "admin", "secret", &["myroom"]);
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    let result = nc.send(&ctx).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_send_message_format_with_title() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/chat/roomA"))
      .and(wiremock::matchers::body_string_contains("My+Title%0AMy+Body"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;

    let nc = nctalk_for_mock(&server, "u", "p", &["roomA"]);
    let ctx = NotifyContext { title: "My Title".into(), body: "My Body".into(), ..Default::default() };
    let result = nc.send(&ctx).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_send_body_only() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/chat/roomA"))
      .and(wiremock::matchers::body_string_contains("message=Just+body"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;

    let nc = nctalk_for_mock(&server, "u", "p", &["roomA"]);
    let ctx = NotifyContext { body: "Just body".into(), ..Default::default() };
    let result = nc.send(&ctx).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_send_server_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST")).and(path("/ocs/v2.php/apps/spreed/api/v1/chat/roomid")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;

    let nc = nctalk_for_mock(&server, "user", "pass", &["roomid"]);
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    let result = nc.send(&ctx).await;
    assert!(result.is_ok());
    // Server error returns Ok(false) since all_ok becomes false
    assert!(!result.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_rooms_returns_true() {
    // No rooms means the loop doesn't execute, all_ok stays true
    let server = MockServer::start().await;
    let addr = server.address();
    let url_str = format!("nctalk://user:pass@{}:{}", addr.ip(), addr.port());
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    let nc = NextcloudTalk::from_url(&parsed).unwrap();

    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    let result = nc.send(&ctx).await;
    assert!(result.is_ok());
    // No rooms to send to, returns true (loop never runs)
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_send_partial_failure() {
    let server = MockServer::start().await;

    // First room succeeds
    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/chat/good_room"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;

    // Second room fails
    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/chat/bad_room"))
      .respond_with(ResponseTemplate::new(500))
      .expect(1)
      .mount(&server)
      .await;

    let nc = nctalk_for_mock(&server, "user", "pass", &["good_room", "bad_room"]);
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    let result = nc.send(&ctx).await;
    assert!(result.is_ok());
    // One room failed, so all_ok should be false
    assert!(!result.unwrap());
  }

  #[test]
  fn test_secure_vs_insecure() {
    let parsed = crate::utils::parse::ParsedUrl::parse("nctalk://user:pass@host/room").unwrap();
    let nc = NextcloudTalk::from_url(&parsed).unwrap();
    assert!(!nc.secure);

    let parsed = crate::utils::parse::ParsedUrl::parse("nctalks://user:pass@host/room").unwrap();
    let nc = NextcloudTalk::from_url(&parsed).unwrap();
    assert!(nc.secure);
  }
}
