use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use tokio::sync::Mutex;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::{is_email, parse_list};
use crate::utils::parse::{ParsedUrl, unquote};

const TWIST_API: &str = "https://api.twist.com";

const DEFAULT_CHANNEL: &str = "general";

/// Session and lookup state. Named channels are resolved to
/// `workspace:channel` ids once and then dropped, as upstream does.
#[derive(Default)]
struct State {
  token: Option<String>,
  default_workspace: Option<String>,
  /// Lowercase workspace name -> id
  workspaces: HashMap<String, String>,
  /// Workspace id -> (lowercase channel name -> channel id)
  channels_by_workspace: HashMap<String, HashMap<String, String>>,
  /// Named channels still to resolve (`[workspace:]channel`, lowercase)
  channels: BTreeSet<String>,
  /// `[workspace:]channel` numeric ids
  channel_ids: BTreeSet<String>,
}

pub struct Twist {
  email: String,
  password: String,
  verify_certificate: bool,
  tags: Vec<String>,
  state: Mutex<State>,
}

impl Twist {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // twist://{password}:{user}@{domain}/{targets}
    // twist://{user}@{domain}/{password}/{targets}
    // Upstream parses this URL with host verification
    let host = url.verified_host()?;
    let user = url.user.clone().filter(|u| !u.is_empty())?;
    let mut targets = url.path_parts.clone();

    let (user, password) = match url.password.clone().filter(|p| !p.is_empty()) {
      // The URL reads password:user, so swap them
      Some(user_part) => (user_part, user),
      None => {
        if targets.is_empty() {
          return None;
        }
        // Upstream quotes a password taken from the path
        (user, urlencoding::encode(&targets.remove(0)).into_owned())
      }
    };
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      // Upstream's URLBase.parse_list() also unquotes each entry
      targets.extend(parse_list(&[to]).iter().map(|t| unquote(t)));
    }

    // A YAML `email:` replaces the account the URL gives (upstream passes
    // it to the constructor)
    let email = url.kwarg("email").map_or_else(|| format!("{}@{}", user, host), str::to_string);
    let Some(email) = is_email(&email).map(|e| e.full_email) else {
      tracing::warn!("The Twist Auth email specified ({}) is invalid.", email);
      return None;
    };
    if password.is_empty() {
      tracing::warn!("No Twist password was specified with account: {}", email);
      return None;
    }

    let mut channels = BTreeSet::new();
    let mut channel_ids = BTreeSet::new();
    for recipient in parse_list(&targets) {
      if channel_id_parts(&recipient).is_some() {
        channel_ids.insert(recipient);
      } else if let Some(name) = channel_name(&recipient) {
        channels.insert(name.to_lowercase());
      } else {
        tracing::warn!("Dropped invalid channel/id ({}) specified.", recipient);
      }
    }
    if channels.is_empty() && channel_ids.is_empty() {
      tracing::warn!("Added default notification channel {}", DEFAULT_CHANNEL);
      channels.insert(DEFAULT_CHANNEL.to_string());
    }

    Some(Self {
      email,
      password,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      state: Mutex::new(State { channels, channel_ids, ..Default::default() }),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Twist",
      service_url: Some("https://twist.com"),
      setup_url: Some("https://appriseit.com/services/twist/"),
      protocols: vec!["twist"],
      description: "Start threads in Twist channels.",
      attachment_support: false,
    }
  }

  /// POST a form to the API. Returns whether it succeeded (HTTP 200) and
  /// the decoded JSON reply (`{}` when the reply is not JSON). An expired
  /// session (error_code 120 or 200) is renewed once and the call retried.
  async fn fetch(&self, client: &reqwest::Client, state: &mut State, path: &str, form: &[(&str, &str)], login: bool) -> (bool, Value) {
    let url = format!("{}/api/v3/{}", service_base(TWIST_API), path);
    let body = (!form.is_empty()).then(|| url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish());

    let mut relogged = false;
    loop {
      tracing::debug!("Twist POST URL: {} (cert_verify={})", url, self.verify_certificate);
      let mut builder = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8");
      if let Some(token) = &state.token {
        builder = builder.header("Authorization", format!("Bearer {}", token));
      }
      if let Some(body) = &body {
        builder = builder.body(body.clone());
      }
      crate::notify::throttle().await;
      let r = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("Exception received when sending Twist POST to {}", url);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          return (false, json!({}));
        }
      };
      let status = r.status();
      let raw = r.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
      let content = serde_json::from_slice::<Value>(&raw).unwrap_or_else(|_| {
        tracing::debug!("Failed to parse Twist JSON response; body: {}", String::from_utf8_lossy(&raw[..raw.len().min(2000)]));
        json!({})
      });

      if status.as_u16() != 200
        && !login
        && !relogged
        && matches!(content.get("error_code").and_then(Value::as_i64), Some(120) | Some(200))
        && Box::pin(self.login(client, state)).await
      {
        relogged = true;
        continue;
      }

      if status.as_u16() != 200 {
        let reason = status.canonical_reason().unwrap_or("");
        tracing::warn!("Failed to send Twist POST to {}: {}{}error={}.", url, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
        tracing::debug!("Response Details:\r\n{}", String::from_utf8_lossy(&raw[..raw.len().min(2000)]));
        return (false, content);
      }
      return (true, content);
    }
  }

  async fn login(&self, client: &reqwest::Client, state: &mut State) -> bool {
    state.default_workspace = None;
    state.workspaces.clear();
    state.channels_by_workspace.clear();

    let (ok, response) = self.fetch(client, state, "users/login", &[("email", &self.email), ("password", &self.password)], true).await;
    if !ok || !is_truthy(&response) {
      state.token = None;
      return false;
    }
    state.default_workspace = response.get("default_workspace").and_then(id_string);
    state.token = response.get("token").and_then(id_string);
    tracing::info!("Authenticated to Twist as {}", self.email);
    true
  }

  /// `{lowercase name: id}` for a list reply of `[{name, id}, ...]`.
  async fn get_named(&self, client: &reqwest::Client, state: &mut State, path: &str, form: &[(&str, &str)]) -> HashMap<String, String> {
    if state.token.is_none() && !self.login(client, state).await {
      return HashMap::new();
    }
    let (ok, response) = self.fetch(client, state, path, form, false).await;
    if !ok {
      return HashMap::new();
    }
    response
      .as_array()
      .into_iter()
      .flatten()
      .map(|e| (e.get("name").and_then(Value::as_str).unwrap_or_default().to_lowercase(), e.get("id").and_then(id_string).unwrap_or_default()))
      .collect()
  }

  /// Resolve named channels to `workspace:channel` ids.
  async fn channel_migration(&self, client: &reqwest::Client, state: &mut State) -> bool {
    if let Some(wid) = state.default_workspace.clone()
      && !state.channels_by_workspace.contains_key(&wid)
    {
      let channels = self.get_named(client, state, "channels/get", &[("workspace_id", &wid)]).await;
      state.channels_by_workspace.insert(wid, channels);
    }

    let mut has_error = false;
    while let Some(name) = state.channels.pop_last() {
      let (workspace, channel) = match split_workspace(&name) {
        (Some(w), c) => (Some(w.to_lowercase()), c.to_lowercase()),
        (None, c) => (None, c.to_lowercase()),
      };

      let workspace_id = match &workspace {
        Some(ws) => {
          if state.workspaces.is_empty() {
            state.workspaces = self.get_named(client, state, "workspaces/get", &[]).await;
          }
          match state.workspaces.get(ws) {
            Some(id) => Some(id.clone()),
            None => {
              tracing::warn!("The Twist User {} is not associated with the Team {}", self.email, ws);
              has_error = true;
              continue;
            }
          }
        }
        None => state.default_workspace.clone(),
      };

      if let Some(wid) = workspace_id
        && let Some(cid) = state.channels_by_workspace.get(&wid).and_then(|c| c.get(&channel))
      {
        state.channel_ids.insert(format!("{}:{}", wid, cid));
        continue;
      }
      tracing::warn!("The Channel #{} was not found{}.", channel, workspace.map(|w| format!(" with Team {}", w)).unwrap_or_default());
      has_error = true;
    }
    !has_error
  }
}

#[async_trait]
impl Notify for Twist {
  fn schemas(&self) -> &[&str] {
    &["twist"]
  }
  fn service_name(&self) -> &str {
    "Twist"
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
  fn body_maxlen(&self) -> usize {
    1000
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.2
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let mut state = self.state.lock().await;

    if state.token.is_none() && !self.login(&client, &mut state).await {
      return Ok(false);
    }
    if !state.channels.is_empty() {
      self.channel_migration(&client, &mut state).await;
    }
    if state.channel_ids.is_empty() {
      tracing::warn!("There are no Twist targets to notify");
      return Ok(false);
    }

    let mut has_error = false;
    let ids: Vec<String> = state.channel_ids.iter().rev().cloned().collect();
    for name in ids {
      let Some((_, channel)) = channel_id_parts(&name) else { continue };
      // Upstream sends the channel as an integer (leading zeros dropped)
      let channel_id = channel.parse::<u128>().map(|n| n.to_string()).unwrap_or_else(|_| channel.to_string());
      // A retry skips channels that already accepted the message
      if crate::delivery::is_delivered(&channel_id, false) {
        continue;
      }
      let (ok, _) = self.fetch(&client, &mut state, "threads/add", &[("channel_id", &channel_id), ("title", &ctx.title), ("content", &ctx.body)], false).await;
      if !ok {
        has_error = true;
        continue;
      }
      tracing::info!("Sent Twist notification to {}.", name);
      crate::delivery::mark_delivered(&channel_id, false);
    }
    Ok(!has_error)
  }
}

/// Match `^((?P<workspace>[0-9]+):)?(?P<channel>[0-9]+)$`.
fn channel_id_parts(s: &str) -> Option<(Option<&str>, &str)> {
  let digits = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit());
  match s.split_once(':') {
    Some((w, c)) if digits(w) && digits(c) => Some((Some(w), c)),
    None if digits(s) => Some((None, s)),
    _ => None,
  }
}

/// Match `^#?(?P<name>(([A-Za-z0-9_-]+):)?([^\s]{1,64}))$`, returning `name`.
fn channel_name(s: &str) -> Option<&str> {
  let name = s.strip_prefix('#').unwrap_or(s);
  if name.is_empty() || name.chars().any(char::is_whitespace) {
    return None;
  }
  let short = |c: &str| (1..=64).contains(&c.chars().count());
  (short(split_workspace(name).1) || short(name)).then_some(name)
}

/// Split `[workspace:]channel` the way upstream's IS_CHANNEL regex does.
fn split_workspace(name: &str) -> (Option<&str>, &str) {
  match name.split_once(':') {
    Some((w, c)) if !w.is_empty() && !c.is_empty() && w.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') => (Some(w), c),
    _ => (None, name),
  }
}

fn id_string(v: &Value) -> Option<String> {
  match v {
    Value::String(s) => Some(s.clone()),
    Value::Number(n) => Some(n.to_string()),
    _ => None,
  }
}

fn is_truthy(v: &Value) -> bool {
  match v {
    Value::Null => false,
    Value::Bool(b) => *b,
    Value::String(s) => !s.is_empty(),
    Value::Array(a) => !a.is_empty(),
    Value::Object(o) => !o.is_empty(),
    Value::Number(n) => n.as_f64() != Some(0.0),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, body_string_contains, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse_twist(url: &str) -> Twist {
    Twist::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  async fn mount_login(server: &MockServer) {
    Mock::given(method("POST"))
      .and(path("/api/v3/users/login"))
      .and(header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8"))
      .and(body_string("email=user%40example.com&password=password"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "token", "default_workspace": 12345})))
      .mount(server)
      .await;
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "twist://user@example.com/password",
      "twist://password:user1@example.com",
      "twist://password:user2@example.com",
      "twist://password:user@example.com/#Channel",
      "twist://password:user@example.com/12345",
      "twist://password:user@example.com?to=%23channel",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["twist://", "twist://:@/", "twist://user@example.com/", "twist://example.com"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[tokio::test]
  async fn test_from_url_fields() {
    // password:user@domain is swapped into an email and password
    let t = parse_twist("twist://password:user@example.com/123:456/#Team:Chan/General/789");
    assert_eq!(t.email, "user@example.com");
    assert_eq!(t.password, "password");
    let s = t.state.lock().await;
    assert_eq!(s.channel_ids.iter().cloned().collect::<Vec<_>>(), vec!["123:456", "789"]);
    assert_eq!(s.channels.iter().cloned().collect::<Vec<_>>(), vec!["general", "team:chan"]);
    drop(s);

    // The password taken from the path is quoted, as upstream does
    let t = parse_twist("twist://user@example.com/pa%20ss/12");
    assert_eq!(t.password, "pa%20ss");

    // No targets: the general channel; an over-long name is dropped
    let t = parse_twist(&format!("twist://password:user@example.com/{}", "a".repeat(65)));
    assert_eq!(t.state.lock().await.channels.iter().cloned().collect::<Vec<_>>(), vec!["general"]);
  }

  #[tokio::test]
  async fn test_send_channel_ids() {
    let server = MockServer::start().await;
    mount_login(&server).await;
    Mock::given(method("POST"))
      .and(path("/api/v3/threads/add"))
      .and(header("Authorization", "Bearer token"))
      .and(header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8"))
      .and(body_string("channel_id=456&title=Test+Subject&content=hello"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("twist://password:user@example.com/123:456").unwrap();
    let ctx = NotifyContext { title: "Test Subject".into(), body: "hello".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_resolves_named_channels() {
    let server = MockServer::start().await;
    mount_login(&server).await;
    Mock::given(method("POST"))
      .and(path("/api/v3/channels/get"))
      .and(body_string("workspace_id=12345"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"name": "General", "id": 1}, {"name": "Other", "id": 2}])))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/api/v3/workspaces/get"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"name": "Team", "id": 12345}])))
      .expect(1)
      .mount(&server)
      .await;
    // Two threads per notification, two notifications
    Mock::given(method("POST")).and(path("/api/v3/threads/add")).respond_with(ResponseTemplate::new(200)).expect(4).mount(&server).await;

    // #General resolves in the default workspace, #Team:Other through the
    // workspace list, #missing is reported and skipped
    let svc = from_url("twist://password:user@example.com/%23General/%23Team:Other/%23missing").unwrap();
    let ctx = NotifyContext { title: "t".into(), body: "b".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = crate::notify::custom_json::requests_to(&server, "POST", "/api/v3/threads/add").await;
    let mut posted: Vec<String> = reqs.iter().map(|r| String::from_utf8_lossy(&r.body).into_owned()).collect();
    posted.sort();
    assert_eq!(posted, vec!["channel_id=1&title=t&content=b", "channel_id=2&title=t&content=b"]);

    // Resolved channels are kept; the next notification skips the lookups
    // (channels/get and workspaces/get each expect a single call)
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_relogin_on_expired_session() {
    let server = MockServer::start().await;
    mount_login(&server).await;
    Mock::given(method("POST"))
      .and(path("/api/v3/threads/add"))
      .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error_code": 200, "error_string": "Invalid token"})))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/api/v3/threads/add")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("twist://password:user@example.com/42").unwrap();
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let logins = crate::notify::custom_json::requests_to(&server, "POST", "/api/v3/users/login").await.len();
    assert_eq!(logins, 2);
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };

    // Login rejected
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/v3/users/login")).respond_with(ResponseTemplate::new(403)).mount(&server).await;
    Mock::given(method("POST")).and(path("/api/v3/threads/add")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
    let svc = from_url("twist://password:user@example.com/42").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // No channel resolves
    let server = MockServer::start().await;
    mount_login(&server).await;
    Mock::given(method("POST")).and(path("/api/v3/channels/get")).respond_with(ResponseTemplate::new(200).set_body_json(json!([]))).mount(&server).await;
    let svc = from_url("twist://password:user@example.com/").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // A failed thread
    let server = MockServer::start().await;
    mount_login(&server).await;
    Mock::given(method("POST")).and(path("/api/v3/threads/add")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    let svc = from_url("twist://password:user@example.com/42").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[test]
  fn test_service_details() {
    let details = Twist::static_details();
    assert_eq!(details.service_name, "Twist");
    assert!(details.protocols.contains(&"twist"));
    assert!(!details.attachment_support);
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    mount_login(&server).await;
    // The second channel fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/api/v3/threads/add"))
      .and(body_string_contains("channel_id=456"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/api/v3/threads/add")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("twist://password:user@example.com/123:456/123:789?retry=1&wait=0"));
    assert!(
      crate::notify::with_test_base(server.uri(), a.notify(&NotifyContext { title: "t".into(), body: "b".into(), ..Default::default() })).await.is_success()
    );
    let sent: Vec<String> = crate::notify::custom_json::requests_to(&server, "POST", "/api/v3/threads/add")
      .await
      .iter()
      .map(|r| String::from_utf8_lossy(&r.body).split('&').next().unwrap().to_string())
      .collect();
    assert_eq!(sent, vec!["channel_id=789", "channel_id=456", "channel_id=456"]);
  }

  /// A YAML `email:` replaces the account the URL gives, as upstream's
  /// constructor argument does (upstream: login as other@example.org with
  /// the URL's password).
  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_yaml_email_replaces_the_urls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v3/users/login"))
      .and(body_string("email=other%40example.org&password=password"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "token", "default_workspace": 12345})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/api/v3/threads/add"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
      .expect(1)
      .mount(&server)
      .await;
    let doc = "urls:\n  - twist://password:user@example.com/123:456:\n      email: other@example.org\n";
    let (a, _) = crate::config::yaml::load_for_test(doc).await;
    let ctx = NotifyContext { title: "title".into(), body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
  }
}
