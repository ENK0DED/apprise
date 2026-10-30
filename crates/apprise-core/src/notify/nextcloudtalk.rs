use crate::error::NotifyError;
use crate::notify::custom_json::{custom_args, header_map};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::{ParsedUrl, parse_bool, parse_list, unquote};
use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::Sha256;

pub struct NextcloudTalk {
  host: String,
  port: Option<u16>,
  user: Option<String>,
  password: Option<String>,
  /// `?secret=`: post as a bot, signing each message, instead of as a user.
  secret: Option<String>,
  rooms: Vec<String>,
  silent: bool,
  url_prefix: String,
  headers: Vec<(String, String)>,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl NextcloudTalk {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let secret = url.get("secret").filter(|s| !s.is_empty()).map(String::from);
    if secret.is_none() && (url.user.is_none() || url.password.is_none()) {
      tracing::warn!("Specify a Nextcloud Talk user and password or bot secret.");
      return None;
    }
    Some(Self {
      host,
      port: url.port,
      user: url.user.clone(),
      password: url.password.clone(),
      secret,
      rooms: parse_list(&url.path_parts.join(" ")),
      silent: url.get("silent").is_some_and(parse_bool),
      // Upstream unquotes this a second time
      url_prefix: url.get("url_prefix").map(|p| unquote(p).trim_matches('/').to_string()).unwrap_or_default(),
      headers: custom_args(url, '+'),
      secure: url.schema == "nctalks",
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Nextcloud Talk",
      service_url: Some("https://nextcloud.com/talk"),
      setup_url: Some("https://appriseit.com/services/nextcloudtalk/"),
      protocols: vec!["nctalk", "nctalks"],
      description: "Send Nextcloud Talk messages.",
      attachment_support: false,
    }
  }

  fn room_url(&self, room: &str) -> String {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let prefix = if self.url_prefix.is_empty() { String::new() } else { format!("/{}", self.url_prefix) };
    let endpoint = if self.secret.is_some() { format!("bot/{}/message", room) } else { format!("chat/{}", room) };
    format!("{}://{}{}{}/ocs/v2.php/apps/spreed/api/v1/{}", schema, self.host, port_str, prefix, endpoint)
  }
}

/// The bot signature: hex HMAC-SHA256 of `random + message` keyed by the
/// bot secret. It covers the message text, not its JSON encoding.
fn bot_signature(secret: &str, random: &str, message: &str) -> String {
  let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
  mac.update(random.as_bytes());
  mac.update(message.as_bytes());
  hex::encode(mac.finalize().into_bytes())
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
  fn title_maxlen(&self) -> usize {
    255
  }
  fn body_maxlen(&self) -> usize {
    32000
  }
  /// The title leads the message text, so both count against the limit
  fn overflow_amalgamate_title(&self) -> bool {
    true
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.rooms.is_empty() {
      tracing::warn!("There were no Nextcloud Talk targets to notify.");
      return Ok(false);
    }

    // The title (or app description) leads the message
    let lead = if ctx.title.is_empty() { ctx.asset.app_desc.as_str() } else { ctx.title.as_str() };
    let message = if ctx.body.is_empty() { lead.to_string() } else { format!("{}\r\n{}", lead, ctx.body) };
    let mut payload = json!({"message": message});
    if self.silent {
      payload["silent"] = json!(true);
    }
    let body = payload.to_string();

    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for room in &self.rooms {
      // Skip a room that already accepted this message so a retry does not
      // deliver it twice
      if crate::delivery::is_delivered(room, false) {
        continue;
      }
      let url = self.room_url(room);
      let mut overrides = self.headers.clone();
      if let Some(secret) = &self.secret {
        let random = hex::encode(rand::random::<[u8; 32]>());
        overrides.push(("X-Nextcloud-Talk-Bot-Signature".into(), bot_signature(secret, &random, &message)));
        overrides.push(("X-Nextcloud-Talk-Bot-Random".into(), random));
      }
      let headers =
        header_map(&[("User-Agent", APP_ID), ("OCS-APIRequest", "true"), ("Accept", "application/json"), ("Content-Type", "application/json")], &overrides)?;

      tracing::debug!("Nextcloud Talk POST URL: {} (cert_verify={})", url, self.verify_certificate);
      let mut req = client.post(&url).headers(headers).body(body.clone());
      if self.secret.is_none() {
        req = req.basic_auth(self.user.clone().unwrap_or_default(), self.password.clone());
      }
      crate::notify::throttle().await;
      match req.send().await {
        Ok(r) if [200, 201].contains(&r.status().as_u16()) => {
          tracing::info!("Sent Nextcloud Talk notification.");
          crate::delivery::mark_delivered(room, false);
        }
        Ok(r) => {
          tracing::warn!("Failed to send Nextcloud Talk notification: error={}.", r.status().as_u16());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Nextcloud Talk notification.");
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use serde_json::Value;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_urls() {
    for url in [
      "nctalk://:@/",
      "nctalk://",
      "nctalks://",
      "nctalk://localhost",
      "nctalk://localhost/roomid",
      "nctalk://user@localhost/roomid",
      "nctalk://localhost/roomid?secret=",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    for url in [
      "nctalk://user:pass@localhost",
      "nctalk://user:pass@localhost/roomid1/roomid2",
      "nctalk://user:pass@localhost:8080/roomid?url_prefix=/prefix",
      "nctalks://user:pass@localhost:8080/roomid/",
      "nctalk://user:pass@localhost:8080/roomid?+HeaderKey=HeaderValue",
      "nctalk://localhost?secret=abcd",
      "nctalks://localhost/roomid1/roomid2?secret=abcd",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  fn talk(url: &str) -> NextcloudTalk {
    NextcloudTalk::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_parse() {
    let t = talk("nctalk://user:pass@localhost/b/a/b?url_prefix=/a/longer/path/abcd/&silent=yes");
    assert_eq!(t.rooms, vec!["a", "b"]);
    assert!(t.silent);
    assert_eq!(t.room_url("a"), "http://localhost/a/longer/path/abcd/ocs/v2.php/apps/spreed/api/v1/chat/a");
    assert_eq!(talk("nctalk://user:pass@localhost/r").room_url("r"), "http://localhost/ocs/v2.php/apps/spreed/api/v1/chat/r");
    // A + in the secret survives URL parsing
    let t = talk("nctalks://localhost:8080/room1?secret=a+b%26c");
    assert_eq!(t.secret.as_deref(), Some("a+b&c"));
    assert_eq!(t.room_url("room1"), "https://localhost:8080/ocs/v2.php/apps/spreed/api/v1/bot/room1/message");
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_user_chat() {
    let server = MockServer::start().await;
    let p = "/ocs/v2.php/apps/spreed/api/v1/chat/room1";
    Mock::given(method("POST"))
      .and(path(p))
      .and(header("Authorization", "Basic dXNlcjpwYXNzd29yZA=="))
      .and(header("OCS-APIRequest", "true"))
      .and(header("Accept", "application/json"))
      .and(header("Content-Type", "application/json"))
      .and(header("HeaderKey", "HeaderValue"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let t = talk(&format!("nctalk://user:password@localhost:{}/room1?+HeaderKey=HeaderValue", server.address().port()));
    assert!(t.send(&ctx("Title", "hello")).await.unwrap());
    let r = &requests_to(&server, "POST", p).await[0];
    assert!(r.headers.get("X-Nextcloud-Talk-Bot-Signature").is_none());
    assert_eq!(serde_json::from_slice::<Value>(&r.body).unwrap(), json!({"message": "Title\r\nhello"}));
  }

  #[tokio::test]
  async fn test_bot_signature() {
    for (title, body, expected) in
      [("Task", "Hello \u{4e16}\u{754c}", "Task\r\nHello \u{4e16}\u{754c}"), ("Done", "", "Done"), ("", "", "Apprise Notifications")]
    {
      let server = MockServer::start().await;
      let p = "/nextcloud/ocs/v2.php/apps/spreed/api/v1/bot/room1/message";
      Mock::given(method("POST")).and(path(p)).and(header("OCS-APIRequest", "true")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
      let t = talk(&format!("nctalk://localhost:{}/room1?secret=test-bot-secret&url_prefix=nextcloud&silent=yes", server.address().port()));
      assert!(t.send(&ctx(title, body)).await.unwrap());

      let r = &requests_to(&server, "POST", p).await[0];
      assert!(r.headers.get("Authorization").is_none());
      let payload: Value = serde_json::from_slice(&r.body).unwrap();
      assert_eq!(payload, json!({"message": expected, "silent": true}));
      let random = r.headers.get("X-Nextcloud-Talk-Bot-Random").unwrap().to_str().unwrap();
      assert_eq!(random.len(), 64);
      let signature = r.headers.get("X-Nextcloud-Talk-Bot-Signature").unwrap().to_str().unwrap();
      assert_eq!(signature, bot_signature("test-bot-secret", random, expected));
    }
    // Known vector, from Python's hmac.new(b"key", b"rnd" + b"msg", sha256)
    assert_eq!(bot_signature("key", "rnd", "msg"), "d9e09d1c663779a9ba292022b9d6d44fdce38ef6a3eb194d0c9dc27b3268660c");
  }

  #[tokio::test]
  async fn test_failures() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/bot/room1/message"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/spreed/api/v1/bot/room2/message"))
      .respond_with(ResponseTemplate::new(401))
      .expect(1)
      .mount(&server)
      .await;
    // Every room is tried; one failure fails the send
    assert!(!talk(&format!("nctalk://localhost:{}/room1/room2?secret=s", server.address().port())).send(&ctx("", "hello")).await.unwrap());
    assert!(!talk("nctalk://localhost?secret=abcd").send(&ctx("", "hello")).await.unwrap());
    assert!(!talk("nctalk://user:pass@127.0.0.1:1/room").send(&ctx("", "hello")).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_rooms_that_accepted() {
    let server = MockServer::start().await;
    let room = |id: &str| Mock::given(method("POST")).and(path(format!("/ocs/v2.php/apps/spreed/api/v1/chat/{}", id)));
    room("room1").respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    // The second room fails once, then accepts the retry
    room("room2").respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    room("room2").respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("nctalk://user:pass@localhost:{}/room1/room2?retry=1&wait=0", server.address().port())));
    assert!(apprise.notify(&ctx("t", "hello")).await.is_success());
    assert!(talk("nctalk://user:pass@localhost/room").overflow_amalgamate_title());
  }
}
