use crate::conversion::commonmark::commonmark_decode_backslash_escapes;
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle_for};
use crate::types::NotifyFormat;
use crate::utils::parse::{ParseOptions, ParsedUrl};
use crate::utils::plugin_args::{qsd_bool, split_path};
use crate::utils::qsd;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Upstream `REDDIT_MESSAGE_KINDS`.
const REDDIT_MESSAGE_KINDS: &[&str] = &["auto", "self", "link"];

/// Upstream `clock_skew`: added to a rate-limit wait as a grace period.
const CLOCK_SKEW: Duration = Duration::from_secs(10);

/// Whether `value` is a Reddit application ID or secret (`^[a-z0-9_-]+$`).
fn is_app_key(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Python's truthiness of a decoded JSON value.
fn truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(b) => *b,
    Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
    Value::String(s) => !s.is_empty(),
    Value::Array(a) => !a.is_empty(),
    Value::Object(o) => !o.is_empty(),
  }
}

/// A Python `bool` as `requests` writes it into a form body.
fn py_bool(value: bool) -> String {
  if value { "True" } else { "False" }.to_string()
}

/// The login and rate-limit state upstream keeps on the plugin instance.
struct Session {
  access_token: Option<String>,
  ratelimit_remaining: f64,
  ratelimit_reset: SystemTime,
}

pub struct Reddit {
  user: String,
  password: String,
  client_id: String,
  client_secret: String,
  subreddits: Vec<String>,
  kind: String,
  nsfw: bool,
  sendreplies: bool,
  resubmit: bool,
  spoiler: bool,
  advertisement: bool,
  flair_id: Option<String>,
  flair_text: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
  session: Mutex<Session>,
}

impl Reddit {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // reddit://{user}:{password}@{app_id}/{app_secret}/{targets}
    let mut targets = split_path(url);
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.extend(qsd::parse_list(to).iter().map(|t| qsd::unquote(t)).filter(|t| !t.is_empty()));
    }

    let kind = url.get("kind").filter(|k| !k.is_empty()).map(|k| qsd::unquote(&k.trim().to_lowercase())).unwrap_or_else(|| "auto".to_string());
    let kind = kind.trim().to_lowercase();
    if !REDDIT_MESSAGE_KINDS.contains(&kind.as_str()) {
      tracing::warn!("An invalid Reddit message kind ({}) was specified", kind);
      return None;
    }

    let Some(user) = url.user.as_deref().and_then(qsd::validate_token) else {
      tracing::warn!("An invalid Reddit User ID was specified");
      return None;
    };
    let Some(password) = url.password.as_deref().and_then(qsd::validate_token) else {
      tracing::warn!("An invalid Reddit Password was specified");
      return None;
    };

    // The App ID is the host and the App Secret the first path entry
    let app_id = match url.get("app_id") {
      Some(id) => qsd::unquote(id),
      None => qsd::unquote(url.host.as_deref().unwrap_or("")),
    };
    let app_secret = match url.get("app_secret") {
      Some(secret) => qsd::unquote(secret),
      None if targets.is_empty() => String::new(),
      None => targets.remove(0),
    };
    if !is_app_key(&app_id) {
      tracing::warn!("An invalid Reddit App ID was specified");
      return None;
    }
    if !is_app_key(&app_secret) {
      tracing::warn!("An invalid Reddit App Secret was specified");
      return None;
    }

    let subreddits: Vec<String> =
      crate::utils::plugin_args::parse_list(&targets, true, true).iter().map(|sr| sr.trim_start_matches('#').to_string()).filter(|sr| !sr.is_empty()).collect();
    if subreddits.is_empty() {
      tracing::warn!("No subreddits were identified to be notified");
    }

    Some(Self {
      user,
      password,
      client_id: app_id.trim().to_string(),
      client_secret: app_secret.trim().to_string(),
      subreddits,
      kind,
      nsfw: qsd_bool(url, "nsfw", false),
      sendreplies: qsd_bool(url, "replies", true),
      resubmit: qsd_bool(url, "resubmit", false),
      spoiler: qsd_bool(url, "spoiler", false),
      advertisement: qsd_bool(url, "ad", false),
      flair_id: url.get("flair_id").map(qsd::unquote).filter(|f| !f.is_empty()),
      flair_text: url.get("flair_text").map(qsd::unquote).filter(|f| !f.is_empty()),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      session: Mutex::new(Session { access_token: None, ratelimit_remaining: 1.0, ratelimit_reset: SystemTime::now() }),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Reddit",
      service_url: Some("https://reddit.com"),
      setup_url: Some("https://appriseit.com/services/reddit/"),
      protocols: vec!["reddit"],
      description: "Post to Reddit subreddits.",
      attachment_support: false,
    }
  }

  fn session(&self) -> std::sync::MutexGuard<'_, Session> {
    self.session.lock().unwrap_or_else(|e| e.into_inner())
  }

  fn auth_url() -> String {
    format!("{}/api/v1/access_token", service_base("https://www.reddit.com"))
  }

  fn submit_url() -> String {
    format!("{}/api/submit", service_base("https://oauth.reddit.com"))
  }

  /// Wait out an exhausted rate limit before the next request.
  async fn throttle(&self) {
    let wait = {
      let session = self.session();
      if session.ratelimit_remaining <= 0.0 { session.ratelimit_reset.duration_since(SystemTime::now()).ok().map(|d| d + CLOCK_SKEW) } else { None }
    };
    throttle_for(wait).await;
  }

  /// POST `payload`, with the bearer token when given, else with the app
  /// credentials.
  async fn post(&self, client: &reqwest::Client, url: &str, payload: &[(&str, String)], token: Option<&str>) -> Option<reqwest::Response> {
    tracing::debug!("Reddit POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let request = client.post(url).header("User-Agent", APP_ID).form(payload);
    let request = match token {
      Some(token) => request.bearer_auth(token),
      None => request.basic_auth(&self.client_id, Some(&self.client_secret)),
    };
    match request.send().await {
      Ok(resp) => Some(resp),
      Err(e) => {
        tracing::warn!("Exception received when sending Reddit to {}", url);
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        None
      }
    }
  }

  /// Read a Reddit response: a JSON body, a 200 status and no `errors`.
  /// Records the rate-limit headers of a successful response.
  async fn read_response(&self, url: &str, resp: reqwest::Response) -> Option<Value> {
    let status = resp.status();
    let headers = resp.headers().clone();
    let text = resp.text().await.unwrap_or_default();
    let reason = if status.as_u16() == 401 { Some("Unauthorized - Invalid Token") } else { status.canonical_reason() };
    let sep = if reason.is_some() { ", " } else { "" };

    let Ok(content) = serde_json::from_str::<Value>(&text) else {
      tracing::warn!("Failed to send to Reddit after countermeasures {}: {}{}error={}", url, reason.unwrap_or(""), sep, status.as_u16());
      tracing::debug!("Response Details:\r\n{}", text);
      return None;
    };

    if status.as_u16() != 200 {
      tracing::warn!("Failed to send to Reddit {}: {}{}error={}", url, reason.unwrap_or(""), sep, status.as_u16());
      tracing::debug!("Response Details:\r\n{}", text);
      return None;
    }

    // Upstream reads content["json"]["errors"]; anything else is a failure
    let errors = if truthy(&content) {
      match content.as_object().map(|o| o.get("json")) {
        Some(None) => Value::Null,
        Some(Some(Value::Object(j))) => j.get("errors").cloned().unwrap_or(Value::Null),
        _ => {
          tracing::warn!("Failed to send to Reddit {}: unexpected response", url);
          tracing::debug!("Response Details:\r\n{}", text);
          return None;
        }
      }
    } else {
      Value::Null
    };
    if truthy(&errors) {
      tracing::warn!("Failed to send to Reddit {}: {}", url, errors);
      tracing::debug!("Response Details:\r\n{}", text);
      return None;
    }

    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(|v| v.trim().to_string());
    if let Some(remaining) = header("X-RateLimit-Remaining").and_then(|v| v.parse::<f64>().ok()) {
      let mut session = self.session();
      session.ratelimit_remaining = remaining;
      if let Some(reset) = header("X-RateLimit-Reset").and_then(|v| v.parse::<i64>().ok()) {
        session.ratelimit_reset =
          if reset >= 0 { UNIX_EPOCH + Duration::from_secs(reset as u64) } else { UNIX_EPOCH - Duration::from_secs(reset.unsigned_abs()) };
      }
    }
    Some(content)
  }

  /// Authenticate with the Reddit server (upstream `login()`).
  async fn login(&self, client: &reqwest::Client) -> bool {
    self.session().access_token = None;
    let payload = [("grant_type", "password".to_string()), ("username", self.user.clone()), ("password", self.password.clone())];

    self.throttle().await;
    let url = Self::auth_url();
    let Some(resp) = self.post(client, &url, &payload, None).await else { return false };
    let Some(response) = self.read_response(&url, resp).await.filter(truthy) else { return false };

    let token = response.get("access_token").filter(|t| truthy(t)).map(|t| t.as_str().map(str::to_string).unwrap_or_else(|| t.to_string()));
    let ok = token.is_some();
    self.session().access_token = token;
    if ok {
      tracing::info!("Authenticated to Reddit as {}", self.user);
    } else {
      tracing::warn!("Failed to authenticate to Reddit as {}", self.user);
    }
    ok
  }

  /// Submit one post, logging in again once if the token is refused.
  async fn submit(&self, client: &reqwest::Client, payload: &[(&str, String)]) -> bool {
    let Some(token) = self.session().access_token.clone() else { return false };
    let url = Self::submit_url();

    self.throttle().await;
    let Some(mut resp) = self.post(client, &url, payload, Some(&token)).await else { return false };
    if resp.status().as_u16() != 200 {
      tracing::debug!("Taking countermeasures after failed to send to Reddit {}: error={}", url, resp.status().as_u16());
      // The token may have expired: log in once more and retry
      if !self.login(client).await {
        return false;
      }
      let Some(token) = self.session().access_token.clone() else { return false };
      resp = match self.post(client, &url, payload, Some(&token)).await {
        Some(resp) => resp,
        None => return false,
      };
    }
    self.read_response(&url, resp).await.is_some()
  }
}

#[async_trait]
impl Notify for Reddit {
  fn schemas(&self) -> &[&str] {
    &["reddit"]
  }
  fn service_name(&self) -> &str {
    "Reddit"
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
    6000
  }
  fn title_maxlen(&self) -> usize {
    300
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.0
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["reddit".into(), self.client_id.clone(), self.client_secret.clone(), self.user.clone(), self.password.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;

    if self.session().access_token.is_none() && !self.login(&client).await {
      return Ok(false);
    }

    if self.subreddits.is_empty() {
      tracing::warn!("There are no Reddit targets to notify");
      return Ok(false);
    }

    // A link post's url field is not Markdown, so the CommonMark escapes
    // the conversion added (e.g. "my\_page") are removed from it. A body
    // with no declared format had none added and is used as written.
    let link =
      if ctx.body_format == NotifyFormat::Markdown && !ctx.body_passthrough { commonmark_decode_backslash_escapes(&ctx.body) } else { ctx.body.clone() };

    let kind = if self.kind == "auto" {
      let opts = ParseOptions { verify_host: true, ..Default::default() };
      let is_link = ParsedUrl::parse_with(&link, opts).is_some_and(|p| p.schema.starts_with("http") && p.host.is_some());
      if is_link { "link" } else { "self" }
    } else {
      self.kind.as_str()
    };

    let title = if ctx.title.is_empty() { ctx.asset.app_desc.clone() } else { ctx.title.clone() };

    let mut has_error = false;
    // Upstream pops its (sorted) subreddits from the end
    for subreddit in self.subreddits.iter().rev() {
      // Skip a subreddit that already accepted this message
      if is_delivered(subreddit, false) {
        continue;
      }

      let mut payload: Vec<(&str, String)> = vec![
        ("ad", py_bool(self.advertisement)),
        ("api_type", "json".into()),
        ("extension", "json".into()),
        ("sr", subreddit.clone()),
        ("title", title.clone()),
        ("kind", kind.into()),
        ("nsfw", py_bool(self.nsfw)),
        ("resubmit", py_bool(self.resubmit)),
        ("sendreplies", py_bool(self.sendreplies)),
        ("spoiler", py_bool(self.spoiler)),
      ];
      if let Some(flair_id) = &self.flair_id {
        payload.push(("flair_id", flair_id.clone()));
      }
      if let Some(flair_text) = &self.flair_text {
        payload.push(("flair_text", flair_text.clone()));
      }
      if kind == "link" {
        payload.push(("url", link.clone()));
      } else {
        payload.push(("text", ctx.body.clone()));
      }
      tracing::debug!("Reddit Payload: {:?}", payload);

      if !self.submit(&client, &payload).await {
        has_error = true;
        continue;
      }

      tracing::info!("Sent Reddit notification to {}", subreddit);
      mark_delivered(subreddit, false);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use crate::notify::with_test_base;
  use serde_json::json;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const AUTH: &str = "/api/v1/access_token";
  const SUBMIT: &str = "/api/submit";

  fn reddit(url: &str) -> Reddit {
    Reddit::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, format: NotifyFormat) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), body_format: format, ..Default::default() }
  }

  fn form(req: &wiremock::Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&req.body).into_owned().collect()
  }

  fn field(req: &wiremock::Request, key: &str) -> Option<String> {
    form(req).into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
  }

  fn good() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
      "access_token": "abc123", "token_type": "bearer", "expires_in": 100000, "scope": "*",
      "refresh_token": "def456", "json": {"errors": []}
    }))
  }

  async fn server_with(auth: ResponseTemplate, submit: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(AUTH)).respond_with(auth).mount(&server).await;
    Mock::given(method("POST")).and(path(SUBMIT)).respond_with(submit).mount(&server).await;
    server
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "reddit://",
      "reddit://:@/",
      // No password
      "reddit://user@app_id/app_secret/",
      // No app secret
      "reddit://user:password@app_id/",
      // An app ID or secret with a percent sign
      "reddit://user:password@app%id/appsecret/apprise",
      "reddit://user:password@app%id/app_secret/apprise",
      "reddit://user:password@app-id/app-secret/apprise?kind=invalid",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "reddit://user:password@app-id/app-secret/apprise",
      // No subreddit loads, but sending fails
      "reddit://user:password@app-id/app-secret",
      "reddit://user:password@app-id/app-secret/apprise/subreddit2",
      "reddit://user:pass@id/secret/sub/?ad=yes&nsfw=yes&replies=no&resubmit=yes&spoiler=yes&kind=self",
      "reddit://?user=l2g&pass=pass&app_secret=abc123&app_id=54321&to=sub1,sub2",
      "reddit://user:pass@id/secret/sub7/sub6/sub5/?flair_id=wonder&flair_text=not%20for%20you",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_url_fields() {
    // The URL user is the Reddit user and the host the App ID
    let r = reddit("reddit://user:password@app-id/app-secret/apprise/subreddit2");
    assert_eq!(r.user, "user");
    assert_eq!(r.password, "password");
    assert_eq!(r.client_id, "app-id");
    assert_eq!(r.client_secret, "app-secret");
    assert_eq!(r.subreddits, vec!["apprise", "subreddit2"]);
    assert_eq!(r.kind, "auto");
    assert!(!r.advertisement && !r.nsfw && !r.resubmit && !r.spoiler && r.sendreplies);

    let r = reddit("reddit://?user=l2g&pass=pass&app_secret=abc123&app_id=54321&to=sub2,%23sub1,sub2");
    assert_eq!((r.user.as_str(), r.password.as_str(), r.client_id.as_str(), r.client_secret.as_str()), ("l2g", "pass", "54321", "abc123"));
    assert_eq!(r.subreddits, vec!["sub1", "sub2"]);

    let r = reddit("reddit://user:pass@id/secret/sub/?ad=yes&nsfw=yes&replies=no&resubmit=yes&spoiler=yes&kind=SELF");
    assert!(r.advertisement && r.nsfw && r.resubmit && r.spoiler && !r.sendreplies);
    assert_eq!(r.kind, "self");

    let r = reddit("reddit://user:pass@id/secret/sub7/sub6/sub5/?flair_id=wonder&flair_text=not%20for%20you");
    assert_eq!(r.flair_id.as_deref(), Some("wonder"));
    assert_eq!(r.flair_text.as_deref(), Some("not for you"));
    assert!(reddit("reddit://user:password@app-id/app-secret").subreddits.is_empty());
  }

  #[tokio::test]
  async fn test_send_requests() {
    // Verified against upstream with pycapture.py
    let server = server_with(good(), good()).await;
    let r = reddit("reddit://user:pass@app-id/app-secret/apprise/sub2?ad=yes&replies=no&flair_id=f1&flair_text=hi%20there");
    let c = ctx("T", "Hello **world** my_var", NotifyFormat::Markdown);
    assert!(with_test_base(server.uri(), r.send(&c)).await.unwrap());

    let auth = requests_to(&server, "POST", AUTH).await;
    assert_eq!(auth.len(), 1);
    // Basic auth with the App ID and secret
    assert_eq!(auth[0].headers.get("authorization").unwrap(), "Basic YXBwLWlkOmFwcC1zZWNyZXQ=");
    assert_eq!(auth[0].headers.get("user-agent").unwrap(), APP_ID);
    assert_eq!(form(&auth[0]), vec![("grant_type".into(), "password".into()), ("username".into(), "user".into()), ("password".into(), "pass".into())]);

    let posts = requests_to(&server, "POST", SUBMIT).await;
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0].headers.get("authorization").unwrap(), "Bearer abc123");
    let expected = |sr: &str| -> Vec<(String, String)> {
      [
        ("ad", "True"),
        ("api_type", "json"),
        ("extension", "json"),
        ("sr", sr),
        ("title", "T"),
        ("kind", "self"),
        ("nsfw", "False"),
        ("resubmit", "False"),
        ("sendreplies", "False"),
        ("spoiler", "False"),
        ("flair_id", "f1"),
        ("flair_text", "hi there"),
        ("text", "Hello **world** my_var"),
      ]
      .iter()
      .map(|(k, v)| (k.to_string(), v.to_string()))
      .collect()
    };
    // Subreddits are sent last to first
    assert_eq!(form(&posts[0]), expected("sub2"));
    assert_eq!(form(&posts[1]), expected("apprise"));

    // The token is reused by later notifications
    assert!(with_test_base(server.uri(), r.send(&c)).await.unwrap());
    assert_eq!(requests_to(&server, "POST", AUTH).await.len(), 1);
  }

  #[tokio::test]
  async fn test_passthrough_link_keeps_its_backslashes() {
    // What upstream sends for notify(body_format=None), then "markdown"
    let link = r"https://example.com/my\_page?a=1&b=2";
    for (query, body, passthrough, kind, field_name, sent) in [
      ("", link, true, "link", "url", link),
      ("", link, false, "link", "url", "https://example.com/my_page?a=1&b=2"),
      ("", r"my\_var 2\*3", true, "self", "text", r"my\_var 2\*3"),
      ("?kind=link", r"my\_var 2\*3", true, "link", "url", r"my\_var 2\*3"),
      ("?kind=link", r"my\_var 2\*3", false, "link", "url", "my_var 2*3"),
    ] {
      let server = server_with(good(), good()).await;
      let mut a = crate::Apprise::new();
      assert!(a.add(&format!("reddit://user:pass@app_id/secret/apprise{query}")));
      let format = if passthrough { NotifyFormat::Text } else { NotifyFormat::Markdown };
      let c = NotifyContext { body_passthrough: passthrough, ..ctx("", body, format) };
      assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success());
      let post = &requests_to(&server, "POST", SUBMIT).await[0];
      let label = format!("{query} {body} passthrough={passthrough}");
      assert_eq!((field(post, "kind").as_deref(), field(post, field_name).as_deref()), (Some(kind), Some(sent)), "{label}");
    }
  }

  #[tokio::test]
  async fn test_default_title_and_flags() {
    let server = server_with(good(), good()).await;
    let r = reddit("reddit://user:pass@id/secret/sub/?ad=yes&nsfw=yes&replies=no&resubmit=yes&spoiler=yes&kind=self");
    assert!(with_test_base(server.uri(), r.send(&ctx("", "http://hostname", NotifyFormat::Markdown))).await.unwrap());
    let post = &requests_to(&server, "POST", SUBMIT).await[0];
    assert_eq!(field(post, "title").as_deref(), Some("Apprise Notifications"));
    // kind=self is kept even for a link
    assert_eq!(field(post, "kind").as_deref(), Some("self"));
    for (key, value) in [("ad", "True"), ("nsfw", "True"), ("resubmit", "True"), ("sendreplies", "False"), ("spoiler", "True")] {
      assert_eq!(field(post, key).as_deref(), Some(value), "{}", key);
    }
  }

  #[tokio::test]
  async fn test_link_detection() {
    let url = "https://example.com/my_page?a=1&b=2";
    for reddit_url in ["reddit://user:pass@app_id/secret/apprise", "reddit://user:pass@app_id/secret/apprise?kind=link"] {
      let server = server_with(good(), good()).await;
      let r = reddit(reddit_url);
      // The orchestrator escapes text converted to Markdown; the link has
      // those escapes removed again
      let body = crate::conversion::convert_between(&NotifyFormat::Text, &NotifyFormat::Markdown, url);
      assert!(with_test_base(server.uri(), r.send(&ctx("", &body, NotifyFormat::Markdown))).await.unwrap());
      let post = &requests_to(&server, "POST", SUBMIT).await[0];
      assert_eq!(field(post, "kind").as_deref(), Some("link"));
      assert_eq!(field(post, "url").as_deref(), Some(url));
      assert_eq!(field(post, "text"), None);
    }

    // A self post keeps the escapes so the text renders literally
    let server = server_with(good(), good()).await;
    let body = crate::conversion::convert_between(&NotifyFormat::Text, &NotifyFormat::Markdown, "my_var 2*3");
    assert!(with_test_base(server.uri(), reddit("reddit://user:pass@app_id/secret/apprise").send(&ctx("", &body, NotifyFormat::Markdown))).await.unwrap());
    let post = &requests_to(&server, "POST", SUBMIT).await[0];
    assert_eq!(field(post, "kind").as_deref(), Some("self"));
    assert_eq!(field(post, "text").as_deref(), Some("my\\_var 2\\*3"));

    // A body sent as text is used as the link unchanged
    let server = server_with(good(), good()).await;
    let r = reddit("reddit://user:pass@app_id/secret/apprise?kind=link&format=text");
    assert!(with_test_base(server.uri(), r.send(&ctx("", "https://example.com/a\\_b", NotifyFormat::Text))).await.unwrap());
    assert_eq!(field(&requests_to(&server, "POST", SUBMIT).await[0], "url").as_deref(), Some("https://example.com/a\\_b"));
  }

  #[tokio::test]
  async fn test_ad_and_replies_reach_the_post() {
    let server = server_with(good(), good()).await;
    let r = reddit("reddit://user:pass@app_id/secret/apprise?ad=yes&replies=no");
    assert!(r.advertisement);
    assert!(!r.sendreplies);
    assert!(with_test_base(server.uri(), r.send(&ctx("", "test", NotifyFormat::Markdown))).await.unwrap());
    let post = &requests_to(&server, "POST", SUBMIT).await[0];
    assert_eq!(field(post, "ad").as_deref(), Some("True"));
    assert_eq!(field(post, "sendreplies").as_deref(), Some("False"));

    let r = reddit("reddit://user:pass@app_id/secret/apprise");
    assert!(!r.advertisement);
    assert!(r.sendreplies);
  }

  #[tokio::test]
  async fn test_login_failures() {
    let c = ctx("", "test", NotifyFormat::Markdown);
    for auth in [
      ResponseTemplate::new(401),
      ResponseTemplate::new(200).set_body_string("{"),
      ResponseTemplate::new(200).set_body_string("{}"),
      ResponseTemplate::new(200).set_body_string(""),
      ResponseTemplate::new(200).set_body_json(json!({"access_token": "", "json": {"errors": []}})),
      ResponseTemplate::new(200).set_body_json(json!({"access_token": "abc123", "json": {"errors": [["KEY", "DESC", "INFO"]]}})),
      ResponseTemplate::new(999),
    ] {
      let server = server_with(auth, good()).await;
      let r = reddit("reddit://user:password@app-id/app-secret/apprise");
      assert!(!with_test_base(server.uri(), r.send(&c)).await.unwrap());
      assert!(requests_to(&server, "POST", SUBMIT).await.is_empty());
    }

    // Logged in, but there is no subreddit to post to
    let server = server_with(good(), good()).await;
    assert!(!with_test_base(server.uri(), reddit("reddit://user:password@app-id/app-secret").send(&c)).await.unwrap());
    assert!(requests_to(&server, "POST", SUBMIT).await.is_empty());

    // A connection failure
    let r = reddit("reddit://user:password@app-id/app-secret/apprise");
    assert!(!with_test_base("http://127.0.0.1:1", r.send(&c)).await.unwrap());
  }

  #[tokio::test]
  async fn test_submit_errors() {
    let c = ctx("", "test", NotifyFormat::Markdown);
    // Reddit reports errors in a 200 response
    let errors = ResponseTemplate::new(200).set_body_json(json!({"json": {"errors": [["KEY", "DESC", "INFO"]]}}));
    let server = server_with(good(), errors).await;
    assert!(!with_test_base(server.uri(), reddit("reddit://user:password@app-id/app-secret/apprise").send(&c)).await.unwrap());

    // Invalid JSON
    let server = server_with(good(), ResponseTemplate::new(200).set_body_string("{")).await;
    assert!(!with_test_base(server.uri(), reddit("reddit://user:password@app-id/app-secret/apprise").send(&c)).await.unwrap());

    // One failing subreddit fails the notification, the other still posts
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(AUTH)).respond_with(good()).mount(&server).await;
    Mock::given(method("POST")).and(path(SUBMIT)).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).with_priority(1).mount(&server).await;
    Mock::given(method("POST")).and(path(SUBMIT)).respond_with(good()).with_priority(2).mount(&server).await;
    // The first failure triggers a login and a retry that succeeds
    assert!(with_test_base(server.uri(), reddit("reddit://user:password@app-id/app-secret/a/b").send(&c)).await.unwrap());
    assert_eq!(requests_to(&server, "POST", AUTH).await.len(), 2);
    assert_eq!(requests_to(&server, "POST", SUBMIT).await.len(), 3);
  }

  #[tokio::test]
  async fn test_reauthentication() {
    let c = ctx("", "test", NotifyFormat::Markdown);
    let server = MockServer::start().await;
    let fresh = ResponseTemplate::new(200).set_body_json(json!({"access_token": "fresh", "json": {"errors": []}}));
    Mock::given(method("POST")).and(path(AUTH)).respond_with(good()).up_to_n_times(1).with_priority(1).mount(&server).await;
    Mock::given(method("POST")).and(path(AUTH)).respond_with(fresh).with_priority(2).mount(&server).await;
    Mock::given(method("POST")).and(path(SUBMIT)).and(header("authorization", "Bearer fresh")).respond_with(good()).mount(&server).await;
    Mock::given(method("POST")).and(path(SUBMIT)).respond_with(ResponseTemplate::new(401)).mount(&server).await;

    // A refused token is replaced and the post retried with the new one
    let r = reddit("reddit://user:password@app-id/app-secret/apprise");
    assert!(with_test_base(server.uri(), r.send(&c)).await.unwrap());
    let posts = requests_to(&server, "POST", SUBMIT).await;
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0].headers.get("authorization").unwrap(), "Bearer abc123");
    assert_eq!(posts[1].headers.get("authorization").unwrap(), "Bearer fresh");

    // The login after a refused token fails too
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(AUTH)).respond_with(good()).up_to_n_times(1).with_priority(1).mount(&server).await;
    Mock::given(method("POST")).and(path(AUTH)).respond_with(ResponseTemplate::new(401)).with_priority(2).mount(&server).await;
    Mock::given(method("POST")).and(path(SUBMIT)).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    assert!(!with_test_base(server.uri(), reddit("reddit://user:password@app-id/app-secret/apprise").send(&c)).await.unwrap());
    assert_eq!(requests_to(&server, "POST", SUBMIT).await.len(), 1);
  }

  #[tokio::test]
  async fn test_rate_limit_headers() {
    let c = ctx("", "test", NotifyFormat::Markdown);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(AUTH)).respond_with(good()).mount(&server).await;
    Mock::given(method("POST"))
      .and(path(SUBMIT))
      .respond_with(good().insert_header("X-RateLimit-Remaining", "0").insert_header("X-RateLimit-Reset", (now + 1).to_string().as_str()))
      .mount(&server)
      .await;
    let r = reddit("reddit://user:password@app-id/app-secret/apprise");
    assert!(with_test_base(server.uri(), r.send(&c)).await.unwrap());
    {
      let session = r.session();
      assert_eq!(session.ratelimit_remaining, 0.0);
      assert_eq!(session.ratelimit_reset, UNIX_EPOCH + Duration::from_secs(now + 1));
    }

    // A response without the headers keeps the last values
    let server = server_with(good(), good()).await;
    r.session().ratelimit_remaining = 10.0;
    assert!(with_test_base(server.uri(), r.send(&c)).await.unwrap());
    assert_eq!(r.session().ratelimit_remaining, 10.0);
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_subreddits() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(AUTH)).respond_with(good()).mount(&server).await;
    let fail = ResponseTemplate::new(200).set_body_json(json!({"json": {"errors": [["KEY", "DESC", "INFO"]]}}));
    // b (sent first) succeeds; a fails once, then succeeds
    Mock::given(method("POST"))
      .and(path(SUBMIT))
      .and(wiremock::matchers::body_string_contains("sr=a"))
      .respond_with(fail)
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(SUBMIT)).respond_with(good()).mount(&server).await;

    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("reddit://user:password@app-id/app-secret/a/b?retry=1&wait=0"));
    let result = with_test_base(server.uri(), apprise.notify(&ctx("", "test", NotifyFormat::Markdown))).await;
    assert!(result.is_success(), "{:?}", result);
    let posts = requests_to(&server, "POST", SUBMIT).await;
    let srs: Vec<String> = posts.iter().filter_map(|p| field(p, "sr")).collect();
    assert_eq!(srs, vec!["b", "a", "a"]);
  }
}
