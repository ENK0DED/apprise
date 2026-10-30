use crate::asset::NotifyImageSize;
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::NotifyFormat;
use crate::utils::parse::{ParsedUrl, parse_bool, parse_list, unquote};
use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
  /// Post to an incoming webhook (`hooks/{webhook}`).
  Webhook,
  /// Post with a personal access token (user ID + token headers).
  Token,
  /// Log in with a user and password, post, then log out.
  Basic,
}

/// Find a `{schema}://[user:]{a}/{b}@...` webhook URL (upstream's
/// pre-parse regex). Returns the URL without the webhook and the raw
/// webhook text (`/` possibly written as `%2F`).
fn split_webhook(raw: &str) -> Option<(String, String)> {
  let (schema, rest) = raw.trim_start().split_once("://")?;
  let is_alnum = |c: char| c.is_ascii_alphanumeric();
  let try_at = |start: usize| -> Option<usize> {
    let s = &rest[start..];
    let a = s.chars().take_while(|c| is_alnum(*c)).count();
    if a == 0 {
      return None;
    }
    let sep = if s[a..].starts_with('/') {
      1
    } else if s.len() >= a + 3 && s[a..a + 3].eq_ignore_ascii_case("%2F") {
      3
    } else {
      return None;
    };
    let b = s[a + sep..].chars().take_while(|c| is_alnum(*c)).count();
    (b > 0 && s[a + sep + b..].starts_with('@') && s.len() > a + sep + b + 1).then_some(a + sep + b)
  };

  // `user:` prefix first (a user cannot contain ':'), then none
  if let Some(colon) = rest.find(':').filter(|i| *i > 0)
    && let Some(len) = try_at(colon + 1)
  {
    let user = &rest[..colon];
    let webhook = &rest[colon + 1..colon + 1 + len];
    return Some((format!("{}://{}@{}", schema, user, &rest[colon + 1 + len + 1..]), webhook.to_string()));
  }
  let len = try_at(0)?;
  Some((format!("{}://{}", schema, &rest[len + 1..]), rest[..len].to_string()))
}

pub struct RocketChat {
  host: String,
  port: Option<u16>,
  user: Option<String>,
  password: Option<String>,
  webhook: Option<String>,
  mode: Mode,
  channels: Vec<String>,
  rooms: Vec<String>,
  users: Vec<String>,
  avatar: bool,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl RocketChat {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // A webhook in the credentials is lifted out before parsing
    let (parsed, mut webhook, mut password) = match split_webhook(&url.raw) {
      Some((stripped, hook)) => {
        let p = ParsedUrl::parse(&stripped)?;
        (p, Some(unquote(&hook)), Some(hook))
      }
      None => (url.clone(), None, url.password.clone()),
    };
    let url = &parsed;
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    if let Some(w) = url.get("webhook").filter(|w| !w.is_empty()) {
      webhook = Some(unquote(w));
    }
    password = password.filter(|p| !p.is_empty());
    let user = url.user.clone().filter(|u| !u.is_empty());

    let mode = match url.get("mode").filter(|m| !m.is_empty()).map(|m| unquote(m).to_lowercase()) {
      Some(m) if m == "webhook" => Mode::Webhook,
      Some(m) if m == "token" => Mode::Token,
      Some(m) if m == "basic" => Mode::Basic,
      Some(m) => {
        tracing::warn!("The authentication mode specified ({}) is invalid.", m);
        return None;
      }
      None if webhook.is_some() => Mode::Webhook,
      // A long password is a personal access token
      None if password.as_ref().is_some_and(|p| p.chars().count() > 32) => Mode::Token,
      None => Mode::Basic,
    };
    match mode {
      Mode::Basic | Mode::Token if user.is_none() || password.is_none() => {
        tracing::warn!("No Rocket.Chat {} was specified.", if mode == Mode::Basic { "user/pass combo" } else { "user/apikey" });
        return None;
      }
      Mode::Webhook if webhook.is_none() => {
        tracing::warn!("No Rocket.Chat Incoming Webhook was specified.");
        return None;
      }
      _ => {}
    }

    let mut targets: Vec<String> = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      // Upstream's URLBase.parse_list() also unquotes each entry
      targets.extend(parse_list(to).iter().map(|t| unquote(t)));
    }
    let (mut channels, mut rooms, mut users) = (Vec::new(), Vec::new(), Vec::new());
    let name_ok = |s: &str, extra: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c));
    for target in crate::utils::email::parse_list(&targets) {
      if let Some(c) = target.strip_prefix('#').filter(|c| name_ok(c, "_-")) {
        channels.push(c.to_string());
      } else if name_ok(&target, "") {
        rooms.push(target);
      } else if let Some(u) = target.strip_prefix('@').filter(|u| name_ok(u, "._-")) {
        users.push(u.to_string());
      } else {
        tracing::warn!("Dropped invalid channel/room/user ({}) specified.", target);
      }
    }
    if mode == Mode::Basic && rooms.is_empty() && channels.is_empty() {
      tracing::warn!("No Rocket.Chat room and/or channels specified to notify.");
      return None;
    }

    // The avatar defaults off for basic logins and on otherwise
    let avatar = url.get("avatar").filter(|a| !a.is_empty()).map(parse_bool).unwrap_or(mode != Mode::Basic);

    Some(Self {
      host,
      port: url.port,
      user,
      password,
      webhook,
      mode,
      channels,
      rooms,
      users,
      avatar,
      secure: url.schema == "rockets",
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Rocket.Chat",
      service_url: Some("https://rocket.chat/"),
      setup_url: Some("https://appriseit.com/services/rocketchat/"),
      protocols: vec!["rocket", "rockets"],
      description: "Send via Rocket.Chat webhooks or the chat API.",
      attachment_support: false,
    }
  }

  fn api_url(&self, path: &str) -> String {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    format!("{}://{}{}/{}", schema, self.host, port_str, path)
  }

  /// Post one message; `auth` holds the user ID and token headers.
  async fn post(&self, client: &reqwest::Client, path: &str, payload: &Value, auth: Option<&(String, String)>) -> bool {
    let url = self.api_url(path);
    tracing::debug!("Rocket.Chat POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let mut req = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(payload.to_string());
    if let Some((user_id, token)) = auth {
      req = req.header("X-User-Id", user_id).header("X-Auth-Token", token);
    }
    crate::notify::throttle().await;
    match req.send().await {
      Ok(r) if r.status().as_u16() == 200 => {
        tracing::info!("Sent Rocket.Chat notification.");
        true
      }
      Ok(r) => {
        tracing::warn!("Failed to send Rocket.Chat notification: error={}.", r.status().as_u16());
        false
      }
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Rocket.Chat notification.");
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        false
      }
    }
  }

  /// Log in; returns the user ID and auth token.
  async fn login(&self, client: &reqwest::Client) -> Option<(String, String)> {
    let form = [("username", self.user.clone().unwrap_or_default()), ("password", self.password.clone().unwrap_or_default())];
    let resp = match client.post(self.api_url("api/v1/login")).form(&form).send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A connection error occurred authenticating {} on Rocket.Chat.", self.user.as_deref().unwrap_or(""));
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return None;
      }
    };
    if resp.status().as_u16() != 200 {
      tracing::warn!("Failed to authenticate {} with Rocket.Chat: error={}.", self.user.as_deref().unwrap_or(""), resp.status().as_u16());
      return None;
    }
    let body: Value = resp.json().await.ok()?;
    if body.get("status").and_then(Value::as_str) != Some("success") {
      tracing::warn!("Could not authenticate {} with Rocket.Chat.", self.user.as_deref().unwrap_or(""));
      return None;
    }
    let field = |k: &str| body.pointer(&format!("/data/{}", k)).and_then(Value::as_str).unwrap_or("").to_string();
    Some((field("userId"), field("authToken")))
  }

  async fn logout(&self, client: &reqwest::Client, auth: &(String, String)) {
    let req = client.post(self.api_url("api/v1/logout")).header("X-Auth-Token", &auth.1).header("X-User-Id", &auth.0);
    match req.send().await {
      Ok(r) if r.status().as_u16() == 200 => tracing::debug!("Rocket.Chat log off successful"),
      Ok(r) => tracing::warn!("Failed to logoff {} from Rocket.Chat: error={}.", self.user.as_deref().unwrap_or(""), r.status().as_u16()),
      Err(e) => tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e)),
    }
  }
}

#[async_trait]
impl Notify for RocketChat {
  fn schemas(&self) -> &[&str] {
    &["rocket", "rockets"]
  }
  fn service_name(&self) -> &str {
    "Rocket.Chat"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    1000
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let mut payload = json!({"text": ctx.body});
    if self.avatar
      && let Some(image) = ctx.image_url(NotifyImageSize::Xy128)
    {
      payload["avatar"] = json!(image);
    }

    let named: Vec<String> = self.users.iter().map(|u| format!("@{}", u)).chain(self.channels.iter().map(|c| format!("#{}", c))).collect();
    let mut has_error = false;

    if self.mode == Mode::Webhook {
      let path = format!("hooks/{}", self.webhook.as_deref().unwrap_or(""));
      let targets: Vec<String> = named.into_iter().chain(self.rooms.iter().cloned()).collect();
      if targets.is_empty() {
        return Ok(self.post(&client, &path, &payload, None).await);
      }
      for target in targets {
        // A retry skips targets that already accepted the message
        if crate::delivery::is_delivered(&target, false) {
          continue;
        }
        payload["channel"] = json!(target);
        if !self.post(&client, &path, &payload, None).await {
          has_error = true;
          continue;
        }
        crate::delivery::mark_delivered(&target, false);
      }
      return Ok(!has_error);
    }

    let auth = match self.mode {
      Mode::Basic => match self.login(&client).await {
        Some(a) => a,
        None => return Ok(false),
      },
      _ => (self.user.clone().unwrap_or_default(), self.password.clone().unwrap_or_default()),
    };
    for channel in named {
      if crate::delivery::is_delivered(&channel, false) {
        continue;
      }
      let mut p = payload.clone();
      p["channel"] = json!(channel);
      if !self.post(&client, "api/v1/chat.postMessage", &p, Some(&auth)).await {
        has_error = true;
        continue;
      }
      crate::delivery::mark_delivered(&channel, false);
    }
    for room in &self.rooms {
      if crate::delivery::is_delivered(room, false) {
        continue;
      }
      let mut p = payload.clone();
      p["roomId"] = json!(room);
      if !self.post(&client, "api/v1/chat.postMessage", &p, Some(&auth)).await {
        has_error = true;
        continue;
      }
      crate::delivery::mark_delivered(room, false);
    }
    if self.mode == Mode::Basic {
      self.logout(&client, &auth).await;
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, body_partial_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_invalid_urls() {
    for url in [
      "rocket://",
      "rockets://",
      "rocket://:@/",
      "rocket://localhost",
      "rocket://user:pass@localhost",
      "rocket://user:pass@localhost/#/!/@",
      "rocket://user@localhost/room/",
      "rocket://localhost/room/",
      "rockets://user:web/token@localhost/@user/?mode=invalid",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let long = format!("rockets://user:{}@localhost/#channel", "t".repeat(40));
    for url in [
      "rocket://user:pass@localhost:8080/room/",
      "rockets://user:pass@localhost?to=#channel",
      "rockets://user:pass@localhost/#channel",
      "rockets://user:token@localhost/#channel?mode=token",
      long.as_str(),
      "rocket://user:pass@localhost/#channel1/#channel2/?avatar=Yes",
      "rocket://user:pass@localhost/room1/room2",
      "rocket://user:pass@localhost/room/#channel?mode=basic&avatar=Yes",
      "rockets://user:pass%2Fwithslash@localhost/#channel/?mode=basic",
      "rockets://web/token@localhost/@user/#channel/roomid",
      "rockets://user:web/token@localhost/@user/?mode=webhook",
      "rockets://user:web/token@localhost?to=@user2,#channel2",
      "rockets://web/token@localhost/?avatar=No",
      "rockets://localhost/@user/?mode=webhook&webhook=web/token",
      "rocket://user:pass@localhost:8083/#chan1/#chan2/room",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  fn rc(url: &str) -> RocketChat {
    RocketChat::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_parse() {
    assert_eq!(split_webhook("rockets://web/token@localhost/@user"), Some(("rockets://localhost/@user".to_string(), "web/token".to_string())));
    assert_eq!(split_webhook("rockets://user:web%2Ftoken@localhost:8080/"), Some(("rockets://user@localhost:8080/".to_string(), "web%2Ftoken".to_string())));
    assert_eq!(split_webhook("rocket://user:pass@localhost/room"), None);

    let r = rc("rockets://web/token@localhost/@user/#channel/roomid");
    assert_eq!((r.mode, r.webhook.as_deref(), r.avatar), (Mode::Webhook, Some("web/token"), true));
    assert_eq!((r.users.clone(), r.channels.clone(), r.rooms.clone()), (vec!["user".to_string()], vec!["channel".to_string()], vec!["roomid".to_string()]));

    let r = rc("rockets://user:web/token@localhost:8080/@user/?mode=webhook");
    assert_eq!((r.mode, r.user.as_deref(), r.port, r.webhook.as_deref()), (Mode::Webhook, Some("user"), Some(8080), Some("web/token")));

    // An explicit basic mode keeps the slash-containing password
    let r = rc("rockets://user:pass%2Fwithslash@localhost/#channel/?mode=basic");
    assert_eq!((r.mode, r.password.as_deref(), r.avatar), (Mode::Basic, Some("pass%2Fwithslash"), false));

    let r = rc(&format!("rockets://user:{}@localhost/#channel", "t".repeat(40)));
    assert_eq!((r.mode, r.avatar), (Mode::Token, true));
    assert!(rc("rocket://user:pass@localhost/room?avatar=yes").avatar);
  }

  fn ctx() -> NotifyContext {
    NotifyContext { title: String::new(), body: "**hello**".into(), ..Default::default() }
  }

  const ICON: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-128x128.png";

  #[tokio::test]
  async fn test_basic_login_post_logout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/login"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "success", "data": {"authToken": "AT", "userId": "UID"}})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/api/v1/chat.postMessage"))
      .and(header("X-Auth-Token", "AT"))
      .and(header("X-User-Id", "UID"))
      .and(header("Content-Type", "application/json"))
      .respond_with(ResponseTemplate::new(200))
      .expect(3)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/api/v1/logout"))
      .and(header("X-Auth-Token", "AT"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let r = rc(&format!("rocket://user:pass@localhost:{}/room/#channel/@bob?avatar=yes", server.address().port()));
    assert!(r.send(&ctx()).await.unwrap());

    let login = &requests_to(&server, "POST", "/api/v1/login").await[0];
    let form: Vec<(String, String)> = url::form_urlencoded::parse(&login.body).into_owned().collect();
    assert_eq!(form, vec![("username".to_string(), "user".to_string()), ("password".to_string(), "pass".to_string())]);
    let bodies: Vec<Value> = requests_to(&server, "POST", "/api/v1/chat.postMessage").await.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
    assert_eq!(
      bodies,
      vec![
        json!({"text": "**hello**", "avatar": ICON, "channel": "@bob"}),
        json!({"text": "**hello**", "avatar": ICON, "channel": "#channel"}),
        json!({"text": "**hello**", "avatar": ICON, "roomId": "room"}),
      ]
    );
  }

  #[tokio::test]
  async fn test_token_mode() {
    let server = MockServer::start().await;
    let token = "t".repeat(40);
    Mock::given(method("POST"))
      .and(path("/api/v1/chat.postMessage"))
      .and(header("X-User-Id", "user"))
      .and(header("X-Auth-Token", token.as_str()))
      .and(body_json(json!({"text": "**hello**", "avatar": ICON, "channel": "#channel"})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let r = rc(&format!("rocket://user:{}@localhost:{}/#channel", token, server.address().port()));
    assert!(r.send(&ctx()).await.unwrap());
  }

  #[tokio::test]
  async fn test_webhook_mode() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/hooks/web/token")).respond_with(ResponseTemplate::new(200)).expect(3).mount(&server).await;

    let r = rc(&format!("rocket://user:web/token@localhost:{}/?to=@user2,#channel2&avatar=no", server.address().port()));
    assert!(r.send(&ctx()).await.unwrap());
    let bodies: Vec<Value> = requests_to(&server, "POST", "/hooks/web/token").await.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
    assert_eq!(bodies, vec![json!({"text": "**hello**", "channel": "@user2"}), json!({"text": "**hello**", "channel": "#channel2"})]);

    // Without targets the webhook's own channel is used
    let r = rc(&format!("rocket://web/token@localhost:{}/", server.address().port()));
    assert!(r.send(&ctx()).await.unwrap());
    assert_eq!(
      serde_json::from_slice::<Value>(&requests_to(&server, "POST", "/hooks/web/token").await[2].body).unwrap(),
      json!({"text": "**hello**", "avatar": ICON})
    );
  }

  #[tokio::test]
  async fn test_failures() {
    // Login rejected
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/login"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "error"})))
      .expect(1)
      .mount(&server)
      .await;
    assert!(!rc(&format!("rocket://user:pass@localhost:{}/room", server.address().port())).send(&ctx()).await.unwrap());

    // A failed post still logs out and tries the other targets
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/login"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "success", "data": {"authToken": "AT", "userId": "UID"}})))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/api/v1/chat.postMessage")).respond_with(ResponseTemplate::new(500)).expect(2).mount(&server).await;
    Mock::given(method("POST")).and(path("/api/v1/logout")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    assert!(!rc(&format!("rocket://user:pass@localhost:{}/rooma/#channela", server.address().port())).send(&ctx()).await.unwrap());

    assert!(!rc("rocket://web/token@127.0.0.1:1/").send(&ctx()).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second target fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/hooks/web/token"))
      .and(body_partial_json(json!({"channel": "#channel2"})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/hooks/web/token")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("rocket://user:web/token@localhost:{}/?to=@user2,%23channel2&avatar=no&retry=1&wait=0", server.address().port())));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&NotifyContext { body: "hello".into(), ..Default::default() })).await.is_success());
    let sent: Vec<Value> = crate::notify::custom_json::requests_to(&server, "POST", "/hooks/web/token")
      .await
      .iter()
      .map(|r| r.body_json::<Value>().unwrap()["channel"].clone())
      .collect();
    assert_eq!(sent, vec![json!("@user2"), json!("#channel2"), json!("#channel2")]);
  }
}
