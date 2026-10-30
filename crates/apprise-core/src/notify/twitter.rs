use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};

const TWITTER_API: &str = "https://api.twitter.com";
const X_API: &str = "https://api.x.com";

/// Non-GIF images that can share one tweet.
const TWEET_IMAGE_BATCH: usize = 4;

/// Users looked up per `users/by` request.
const LOOKUP_BATCH: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  Dm,
  Tweet,
}

impl Mode {
  fn as_str(&self) -> &'static str {
    match self {
      Mode::Dm => "dm",
      Mode::Tweet => "tweet",
    }
  }
}

/// Lookup caches and rate limit state (class-level in upstream).
struct State {
  whoami: Option<Vec<(String, String)>>,
  /// username -> user id, in insertion order
  users: Vec<(String, String)>,
  ratelimit_remaining: i64,
  /// Unix timestamp at which the rate limit window resets.
  ratelimit_reset: i64,
}

enum Body<'a> {
  None,
  Json(Value),
  Media(&'a Attachment, &'static str),
}

/// An uploaded attachment.
struct Media {
  id: String,
  mime: String,
}

pub struct Twitter {
  ckey: String,
  csecret: String,
  akey: String,
  asecret: String,
  mode: Mode,
  /// `None` when every target given was invalid.
  targets: Option<Vec<String>>,
  cache: bool,
  batch: bool,
  verify_certificate: bool,
  tags: Vec<String>,
  state: Mutex<State>,
}

impl Twitter {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // {schema}://{ckey}/{csecret}/{akey}/{asecret}/{targets}
    let mut tokens = url.path_parts.iter().cloned();
    let ckey = url.host.as_deref().map(unquote).unwrap_or_default().trim().to_string();
    let csecret = tokens.next().unwrap_or_default().trim().to_string();
    let akey = tokens.next().unwrap_or_default().trim().to_string();
    let asecret = tokens.next().unwrap_or_default().trim().to_string();
    for (value, name) in [(&ckey, "Twitter Consumer Key"), (&csecret, "Twitter Consumer Secret"), (&akey, "Twitter Access Key"), (&asecret, "Access Secret")] {
      if value.is_empty() {
        tracing::warn!("An invalid {} was specified.", name);
        return None;
      }
    }

    // The mode is matched by prefix (t -> tweet), as upstream does
    let mode = match url.get("mode").filter(|m| !m.is_empty()).map(unquote) {
      Some(m) => match [Mode::Dm, Mode::Tweet].into_iter().find(|candidate| candidate.as_str().starts_with(m.as_str())) {
        Some(mode) => mode,
        None => {
          tracing::warn!("The Twitter message mode specified ({}) is invalid.", m);
          return None;
        }
      },
      None if url.schema.starts_with("tweet") => Mode::Tweet,
      None => Mode::Dm,
    };

    let mut raw_targets: Vec<String> = url.user.iter().cloned().collect();
    raw_targets.extend(tokens);
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.push(to.to_string());
    }
    let mut targets = Vec::new();
    let mut has_error = false;
    for target in parse_list(&raw_targets) {
      let user = target.trim_start().strip_prefix('@').unwrap_or(target.trim_start());
      if !user.is_empty() && user.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        targets.push(user.to_string());
      } else {
        has_error = true;
        tracing::warn!("Dropped invalid Twitter user ({}) specified.", target);
      }
    }

    Some(Self {
      ckey,
      csecret,
      akey,
      asecret,
      mode,
      targets: if has_error && targets.is_empty() { None } else { Some(targets) },
      cache: url.get("cache").filter(|c| !c.is_empty()).map(parse_bool).unwrap_or(true),
      batch: url.get("batch").map(parse_bool).unwrap_or(true),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      state: Mutex::new(State { whoami: None, users: Vec::new(), ratelimit_remaining: 1, ratelimit_reset: 0 }),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Twitter",
      service_url: Some("https://twitter.com/"),
      setup_url: Some("https://appriseit.com/services/twitter/"),
      protocols: vec!["x", "twitter", "tweet"],
      description: "Send tweets or direct messages through the X API v2.",
      attachment_support: true,
    }
  }

  fn state(&self) -> std::sync::MutexGuard<'_, State> {
    self.state.lock().unwrap_or_else(|e| e.into_inner())
  }

  fn oauth1_header(&self, method: &str, url: &str) -> String {
    let nonce: String = (0..32)
      .map(|_| {
        let idx = rand::random::<u8>() % 62;
        (match idx {
          0..=9 => b'0' + idx,
          10..=35 => b'a' + idx - 10,
          _ => b'A' + idx - 36,
        }) as char
      })
      .collect();
    self.oauth1_header_with(method, url, &nonce, &chrono::Utc::now().timestamp().to_string())
  }

  /// OAuth 1.0a HMAC-SHA1 `Authorization` header, as `requests_oauthlib`
  /// builds it. Query parameters are signed; JSON and multipart bodies are not.
  fn oauth1_header_with(&self, method: &str, url: &str, nonce: &str, timestamp: &str) -> String {
    use base64::Engine;
    use hmac::{Hmac, KeyInit, Mac};
    use sha1::Sha1;

    let pct = |s: &str| urlencoding::encode(s).into_owned();

    let (base_uri, query): (String, Vec<(String, String)>) = match url::Url::parse(url) {
      Ok(u) => {
        let port = u.port().map(|p| format!(":{}", p)).unwrap_or_default();
        let path = if u.path().is_empty() { "/" } else { u.path() };
        (
          format!("{}://{}{}{}", u.scheme(), u.host_str().unwrap_or_default().to_lowercase(), port, path),
          u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect(),
        )
      }
      Err(_) => (url.to_string(), Vec::new()),
    };

    let oauth = [
      ("oauth_nonce", nonce),
      ("oauth_timestamp", timestamp),
      ("oauth_version", "1.0"),
      ("oauth_signature_method", "HMAC-SHA1"),
      ("oauth_consumer_key", self.ckey.as_str()),
      ("oauth_token", self.akey.as_str()),
    ];
    let mut params: Vec<(String, String)> = query.iter().map(|(k, v)| (pct(k), pct(v))).collect();
    params.extend(oauth.iter().map(|(k, v)| (pct(k), pct(v))));
    params.sort();
    let param_string = params.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("&");
    let base_string = format!("{}&{}&{}", method.to_uppercase(), pct(&base_uri), pct(&param_string));
    let signing_key = format!("{}&{}", pct(&self.csecret), pct(&self.asecret));

    let mut mac = Hmac::<Sha1>::new_from_slice(signing_key.as_bytes()).expect("HMAC accepts any key size");
    mac.update(base_string.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    let mut parts: Vec<String> = oauth.iter().map(|(k, v)| format!("{}=\"{}\"", k, pct(v))).collect();
    parts.push(format!("oauth_signature=\"{}\"", pct(&signature)));
    format!("OAuth {}", parts.join(", "))
  }

  /// Perform one API request. Returns whether it succeeded (2xx) and the
  /// decoded JSON reply (`{}` when the reply is not JSON).
  async fn fetch(&self, client: &reqwest::Client, method: reqwest::Method, url: &str, body: Body<'_>) -> (bool, Value) {
    let wait = {
      let s = self.state();
      let now = chrono::Utc::now().timestamp_millis();
      let reset = s.ratelimit_reset * 1000;
      (s.ratelimit_remaining == 0 && now < reset).then(|| Duration::from_millis((reset - now) as u64 + 500))
    };
    if let Some(wait) = wait {
      tracing::debug!("Twitter rate limit reached; waiting {:.2}s", wait.as_secs_f64());
      tokio::time::sleep(wait).await;
    }

    tracing::debug!("Twitter {} URL: {} (cert_verify={})", method, url, self.verify_certificate);
    let mut builder = client.request(method.clone(), url).header("User-Agent", APP_ID).header("Authorization", self.oauth1_header(method.as_str(), url));
    builder = match body {
      Body::None => builder,
      Body::Json(v) => builder.header("Content-Type", "application/json").body(v.to_string()),
      Body::Media(attachment, category) => {
        let part = reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(attachment.name.clone());
        builder.multipart(reqwest::multipart::Form::new().text("media_category", category).part("media", part))
      }
    };

    let resp = match builder.send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("Exception received when sending Twitter {} to {}: ", method, url);
        tracing::debug!("Socket Exception: {}", e);
        return (false, json!({}));
      }
    };
    let status = resp.status();
    let headers = resp.headers().clone();
    let raw = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    let content = serde_json::from_slice::<Value>(&raw).unwrap_or_else(|_| {
      tracing::debug!("Failed to parse Twitter JSON response; body: {}", String::from_utf8_lossy(&raw[..raw.len().min(2000)]));
      json!({})
    });

    if !status.is_success() {
      let reason = status.canonical_reason().unwrap_or("");
      tracing::warn!("Failed to send Twitter {} to {}: {}{}error={}.", method, url, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      tracing::debug!("Response Details:\r\n{}", String::from_utf8_lossy(&raw[..raw.len().min(2000)]));
      return (false, content);
    }

    // Track the rate limit the server reports
    let header_int = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<i64>().ok());
    if let Some(remaining) = header_int("x-rate-limit-remaining") {
      let mut s = self.state();
      s.ratelimit_remaining = remaining;
      if let Some(reset) = header_int("x-rate-limit-reset") {
        s.ratelimit_reset = reset;
      }
    }
    (true, content)
  }

  /// The authenticated user, as `[(username, id)]`.
  async fn whoami(&self, client: &reqwest::Client) -> Vec<(String, String)> {
    if self.cache
      && let Some(cached) = self.state().whoami.clone()
    {
      return cached;
    }
    let url = format!("{}/2/users/me", service_base(TWITTER_API));
    let (ok, response) = self.fetch(client, reqwest::Method::GET, &url, Body::None).await;
    if !ok {
      return Vec::new();
    }
    let data = &response["data"];
    let (Some(username), Some(id)) = (data.get("username").and_then(Value::as_str), data.get("id").and_then(id_string)) else {
      return Vec::new();
    };
    let result = vec![(username.to_string(), id)];
    let mut s = self.state();
    s.whoami = Some(result.clone());
    merge_users(&mut s.users, &result);
    result
  }

  /// Resolve usernames to user ids, as `[(username, id)]`.
  async fn user_lookup(&self, usernames: &[String], client: &reqwest::Client) -> Vec<(String, String)> {
    let mut names = parse_list(usernames);
    let mut results: Vec<(String, String)> = Vec::new();
    if self.cache {
      let s = self.state();
      results = s.users.iter().filter(|(k, _)| names.contains(k)).cloned().collect();
      names.retain(|n| !results.iter().any(|(k, _)| k == n));
    }

    for batch in names.chunks(LOOKUP_BATCH) {
      let query = batch.iter().map(|n| urlencoding::encode(n).into_owned()).collect::<Vec<_>>().join(",");
      let url = format!("{}/2/users/by?usernames={}", service_base(TWITTER_API), query);
      let (ok, response) = self.fetch(client, reqwest::Method::GET, &url, Body::None).await;
      if !ok {
        continue;
      }
      for entry in response.get("data").and_then(Value::as_array).into_iter().flatten() {
        if let (Some(username), Some(id)) = (entry.get("username").and_then(Value::as_str), entry.get("id").and_then(id_string)) {
          merge_users(&mut results, &[(username.to_string(), id)]);
        }
      }
    }

    merge_users(&mut self.state().users, &results);
    results
  }

  async fn send_tweet(&self, client: &reqwest::Client, body: &str, media: &[Media]) -> bool {
    let payloads: Vec<Value> = if media.is_empty() {
      vec![json!({ "text": body })]
    } else {
      // PNG and JPEG images share a tweet (up to 4); anything else, such
      // as a GIF, goes out on its own.
      let batch_size = if self.batch { TWEET_IMAGE_BATCH } else { 1 };
      let mut batches: Vec<Vec<String>> = Vec::new();
      let mut batch: Vec<String> = Vec::new();
      for m in media {
        if !is_png_or_jpeg(&m.mime) {
          if !batch.is_empty() {
            batches.push(std::mem::take(&mut batch));
          }
          batches.push(vec![m.id.clone()]);
        } else {
          batch.push(m.id.clone());
          if batch.len() >= batch_size {
            batches.push(std::mem::take(&mut batch));
          }
        }
      }
      if !batch.is_empty() {
        batches.push(batch);
      }
      let total = batches.len();
      batches
        .into_iter()
        .enumerate()
        .map(|(no, media_ids)| {
          let text = if no > 0 || body.is_empty() { format!("{:02}/{:02}", no + 1, total) } else { body.to_string() };
          json!({ "text": text, "media": { "media_ids": media_ids } })
        })
        .collect()
    };

    let url = format!("{}/2/tweets", service_base(TWITTER_API));
    let total = payloads.len();
    let mut has_error = false;
    for (no, payload) in payloads.into_iter().enumerate() {
      let (ok, response) = self.fetch(client, reqwest::Method::POST, &url, Body::Json(payload)).await;
      if !ok {
        has_error = true;
        for e in response.get("errors").and_then(Value::as_array).into_iter().flatten() {
          let title = e.get("title").or_else(|| e.get("code")).map(display).unwrap_or_else(|| "unk".into());
          let detail = e.get("detail").or_else(|| e.get("message")).map(display).unwrap_or_default();
          tracing::debug!("Tweet [{:02}/{:02}] Details: Error {}: {}", no + 1, total, title, detail);
        }
        continue;
      }
      let link = response["data"].get("id").and_then(id_string).map(|id| format!("https://x.com/i/web/status/{}", id)).unwrap_or_else(|| "unknown".into());
      tracing::debug!("Tweet [{:02}/{:02}] Details: {}", no + 1, total, link);
      tracing::info!("Sent [{:02}/{:02}] Twitter notification as public tweet.", no + 1, total);
    }
    !has_error
  }

  async fn send_dm(&self, client: &reqwest::Client, targets: &[String], body: &str, media: &[Media]) -> bool {
    let users = if targets.is_empty() { self.whoami(client).await } else { self.user_lookup(targets, client).await };
    if users.is_empty() {
      tracing::warn!("Failed to acquire user(s) to Direct Message via Twitter");
      return false;
    }

    // One message per attachment; messages after the first are numbered
    let payloads: Vec<Value> = if media.is_empty() {
      vec![json!({ "text": body })]
    } else {
      media
        .iter()
        .enumerate()
        .map(|(no, m)| {
          let text = if no == 0 && !body.is_empty() { body.to_string() } else { format!("{:02}/{:02}", no + 1, media.len()) };
          json!({ "text": text, "attachments": [{ "media_id": m.id }] })
        })
        .collect()
    };

    let mut has_error = false;
    for (no, payload) in payloads.iter().enumerate() {
      for (screen_name, user_id) in &users {
        let url = format!("{}/2/dm_conversations/with/{}/messages", service_base(TWITTER_API), user_id);
        if !self.fetch(client, reqwest::Method::POST, &url, Body::Json(payload.clone())).await.0 {
          has_error = true;
          continue;
        }
        tracing::info!("Sent [{:02}/{:02}] Twitter DM notification to @{}.", no + 1, payloads.len(), screen_name);
      }
    }
    !has_error
  }
}

#[async_trait]
impl Notify for Twitter {
  fn schemas(&self) -> &[&str] {
    &["x", "twitter", "tweet"]
  }
  fn service_name(&self) -> &str {
    "Twitter"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn attachment_support(&self) -> bool {
    true
  }
  fn body_maxlen(&self) -> usize {
    // Direct messages allow far more than a public tweet
    if self.mode == Mode::Dm { 10000 } else { 280 }
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let Some(targets) = &self.targets else {
      tracing::warn!("No valid Twitter targets to notify.");
      return Ok(false);
    };
    let client = build_client(self.verify_certificate)?;

    let mut media = Vec::new();
    let category = if self.mode == Mode::Tweet { "tweet_image" } else { "dm_image" };
    for (no, attachment) in ctx.attachments.iter().enumerate() {
      if !attachment.mime_type.to_lowercase().starts_with("image/") {
        tracing::warn!("Ignoring unsupported Twitter attachment {}.", attachment.name);
        continue;
      }
      tracing::debug!("Preparing Twitter attachment {}", attachment.name);
      let url = format!("{}/2/media/upload", service_base(X_API));
      let (ok, response) = self.fetch(&client, reqwest::Method::POST, &url, Body::Media(attachment, category)).await;
      if !ok {
        return Ok(false);
      }
      match response["data"].get("id").filter(|v| v.as_u64() != Some(0)).and_then(id_string).filter(|id| !id.is_empty()) {
        Some(id) => media.push(Media { id, mime: attachment.mime_type.clone() }),
        None => {
          let filename = if attachment.name.is_empty() { format!("file{:03}.dat", no + 1) } else { attachment.name.clone() };
          tracing::debug!("Could not attach the file to Twitter: {} (mime={})", filename, attachment.mime_type);
        }
      }
    }

    Ok(match self.mode {
      Mode::Tweet => self.send_tweet(&client, &ctx.body, &media).await,
      Mode::Dm => self.send_dm(&client, targets, &ctx.body, &media).await,
    })
  }
}

fn is_png_or_jpeg(mime: &str) -> bool {
  let m = mime.to_lowercase();
  m.starts_with("image/png") || m.starts_with("image/jpeg") || m.starts_with("image/jpg")
}

/// Ids arrive as strings, but accept numbers too.
fn id_string(v: &Value) -> Option<String> {
  match v {
    Value::String(s) => Some(s.clone()),
    Value::Number(n) => Some(n.to_string()),
    _ => None,
  }
}

fn display(v: &Value) -> String {
  match v {
    Value::String(s) => s.clone(),
    other => other.to_string(),
  }
}

/// Insert or update `(username, id)` pairs, keeping first-seen order.
fn merge_users(into: &mut Vec<(String, String)>, from: &[(String, String)]) {
  let mut index: HashMap<String, usize> = into.iter().enumerate().map(|(i, (k, _))| (k.clone(), i)).collect();
  for (k, v) in from {
    match index.get(k) {
      Some(&i) => into[i].1 = v.clone(),
      None => {
        index.insert(k.clone(), into.len());
        into.push((k.clone(), v.clone()));
      }
    }
  }
}

fn unquote(s: &str) -> String {
  urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}

/// Upstream `parse_list()`: split on `[]`, `;`, `,` and whitespace, then
/// de-duplicate and sort.
fn parse_list(items: &[String]) -> Vec<String> {
  let mut out: Vec<String> = items
    .iter()
    .flat_map(|i| i.split(|c: char| c == '[' || c == ']' || c == ';' || c == ',' || c.is_whitespace()))
    .filter(|t| !t.is_empty())
    .map(String::from)
    .collect();
  out.sort();
  out.dedup();
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header_exists, method, path, query_param};
  use wiremock::{Mock, MockServer, Request as MockRequest, ResponseTemplate};

  fn parse_twitter(url: &str) -> Twitter {
    Twitter::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn json_bodies(reqs: &[MockRequest], p: &str) -> Vec<Value> {
    reqs.iter().filter(|r| r.url.path() == p).map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  fn png(name: &str) -> Attachment {
    Attachment { name: name.into(), data: b"PNG".to_vec(), mime_type: "image/png".into() }
  }

  fn gif(name: &str) -> Attachment {
    Attachment { name: name.into(), data: b"GIF".to_vec(), mime_type: "image/gif".into() }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "twitter://consumer_key/consumer_secret/atoken2/access_secret",
      "twitter://consumer_key/consumer_secret/atoken3/access_secret?cache=no",
      "twitter://consumer_key/consumer_secret2/atoken6/access_secret",
      "twitter://user@consumer_key/csecret2/atoken7/access_secret/-/%25/",
      "twitter://user@consumer_key/csecret/atoken8/access_secret?cache=No&batch=No",
      "twitter://user@consumer_key/csecret/atoken9/access_secret",
      "tweet://ckey/csecret/atoken12/access_secret",
      "twitter://usera@consumer_key/consumer_secret/atoken14/access_secret/user/?to=userb",
      "twitter://ckey/csecret/atoken16/access_secret",
      "twitter://ckey/csecret/atoken17/access_secret?mode=tweet",
      "x://ckey/csecret/akey/asecret",
      "twitter://ckey/csecret/akey/asecret?mode=t",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "twitter://",
      "twitter://:@/",
      "twitter://consumer_key",
      "twitter://consumer_key/consumer_secret/",
      "twitter://consumer_key/consumer_secret/atoken1/",
      "twitter://user@ckey/csecret/atoken13/access_secret?mode=invalid",
      // Modes are matched case-sensitively
      "twitter://ckey/csecret/akey/asecret?mode=TWEET",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let t = parse_twitter("twitter://usera@ckey/csecret/akey/asecret/@userc/?to=userb&cache=no&batch=no");
    assert_eq!((t.ckey.as_str(), t.csecret.as_str(), t.akey.as_str(), t.asecret.as_str()), ("ckey", "csecret", "akey", "asecret"));
    assert_eq!(t.mode, Mode::Dm);
    assert_eq!(t.targets, Some(vec!["userc".to_string(), "usera".into(), "userb".into()]));
    assert!(!t.cache);
    assert!(!t.batch);

    assert_eq!(parse_twitter("tweet://ckey/csecret/akey/asecret").mode, Mode::Tweet);
    assert_eq!(parse_twitter("tweet://ckey/csecret/akey/asecret?mode=dm").mode, Mode::Dm);
    assert_eq!(parse_twitter("x://ckey/csecret/akey/asecret?mode=tw").mode, Mode::Tweet);

    // Only invalid targets: the URL loads but cannot notify
    assert_eq!(parse_twitter("twitter://ckey/csecret/akey/asecret/-/%25/").targets, None);
    // Invalid targets alongside a valid one are dropped
    assert_eq!(parse_twitter("twitter://user@ckey/csecret/akey/asecret/-/%25/").targets, Some(vec!["user".to_string()]));
  }

  #[test]
  fn test_oauth1_signature_matches_oauthlib() {
    let t = parse_twitter("twitter://ckey/csecret/akey/asecret");
    let expect = |sig: &str| {
      format!(
        "OAuth oauth_nonce=\"parity-nonce\", oauth_timestamp=\"1704067200\", oauth_version=\"1.0\", oauth_signature_method=\"HMAC-SHA1\", oauth_consumer_key=\"ckey\", oauth_token=\"akey\", oauth_signature=\"{}\"",
        sig
      )
    };
    assert_eq!(t.oauth1_header_with("POST", "https://api.twitter.com/2/tweets", "parity-nonce", "1704067200"), expect("LfDSuky9L%2BaqtaKdpUnli31Twrg%3D"));
    assert_eq!(t.oauth1_header_with("GET", "https://api.twitter.com/2/users/me", "parity-nonce", "1704067200"), expect("fF%2FbPvGSsKsYOUnfkXQZFvD7VpA%3D"));
    // Query parameters are part of the signature
    assert_eq!(
      t.oauth1_header_with("GET", "https://api.twitter.com/2/users/by?usernames=alice,bob", "parity-nonce", "1704067200"),
      expect("0vXjybHjkGq%2FypDZtBD8RH2hsaw%3D")
    );
    // A non-default port is kept in the base string
    assert_eq!(t.oauth1_header_with("POST", "http://127.0.0.1:8080/2/tweets", "parity-nonce", "1704067200"), expect("oWRCZUKcTPgZAGeZ9K4yT5%2FZq%2Bk%3D"));
  }

  #[tokio::test]
  async fn test_send_tweet() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/2/tweets"))
      .and(header_exists("Authorization"))
      .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"id": "1"}})))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("tweet://ckey/csecret/akey/asecret").unwrap();
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(json_bodies(&reqs, "/2/tweets"), vec![json!({"text": "hello"})]);
    assert_eq!(reqs[0].headers.get("content-type").unwrap(), "application/json");
    assert!(reqs[0].headers.get("authorization").unwrap().to_str().unwrap().starts_with("OAuth oauth_nonce=\""));
  }

  #[tokio::test]
  async fn test_send_tweet_with_media_batches() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/2/media/upload"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "m"}})))
      .expect(6)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/2/tweets"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "1"}})))
      .expect(3)
      .mount(&server)
      .await;

    let svc = from_url("tweet://ckey/csecret/akey/asecret").unwrap();
    let mut ctx = NotifyContext { body: "body".into(), ..Default::default() };
    // png png gif png png png + an unsupported file
    ctx.attachments = vec![png("a.png"), png("b.png"), gif("c.gif"), png("d.png"), png("e.png"), png("f.png")];
    ctx.attachments.push(Attachment { name: "v.mp4".into(), data: b"MP4".to_vec(), mime_type: "video/mp4".into() });
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    let upload = reqs.iter().find(|r| r.url.path() == "/2/media/upload").unwrap();
    let raw = String::from_utf8_lossy(&upload.body);
    assert!(upload.headers.get("content-type").unwrap().to_str().unwrap().starts_with("multipart/form-data"));
    assert!(raw.contains("name=\"media_category\"\r\n\r\ntweet_image"), "{}", raw);
    assert!(raw.contains("name=\"media\"; filename=\"a.png\""), "{}", raw);

    let tweets = json_bodies(&reqs, "/2/tweets");
    assert_eq!(tweets[0], json!({"text": "body", "media": {"media_ids": ["m", "m"]}}));
    assert_eq!(tweets[1], json!({"text": "02/03", "media": {"media_ids": ["m"]}}));
    assert_eq!(tweets[2], json!({"text": "03/03", "media": {"media_ids": ["m", "m", "m"]}}));
  }

  #[tokio::test]
  async fn test_send_tweet_without_batching() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/2/media/upload"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": 7}})))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/2/tweets")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;
    let svc = from_url("tweet://ckey/csecret/akey/asecret?batch=no").unwrap();
    let mut ctx = NotifyContext::default();
    ctx.attachments = vec![png("a.png"), png("b.png")];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let tweets = json_bodies(&server.received_requests().await.unwrap(), "/2/tweets");
    // No body: every tweet is numbered
    assert_eq!(tweets[0], json!({"text": "01/02", "media": {"media_ids": ["7"]}}));
    assert_eq!(tweets[1], json!({"text": "02/02", "media": {"media_ids": ["7"]}}));
  }

  #[tokio::test]
  async fn test_send_dm_to_self() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/2/users/me"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "123", "username": "me"}})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/2/dm_conversations/with/123/messages")).respond_with(ResponseTemplate::new(201)).expect(2).mount(&server).await;
    let svc = from_url("x://ckey/csecret/akey/asecret").unwrap();
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    // The second notification uses the cached identity
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(json_bodies(&server.received_requests().await.unwrap(), "/2/dm_conversations/with/123/messages")[0], json!({"text": "hello"}));
  }

  #[tokio::test]
  async fn test_send_dm_to_users_with_attachments() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/2/users/by"))
      .and(query_param("usernames", "alice,bob"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"username": "alice", "id": "11"}, {"username": "bob", "id": "22"}]})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/2/media/upload"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "m1"}})))
      .expect(2)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/2/dm_conversations/with/11/messages")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;
    Mock::given(method("POST")).and(path("/2/dm_conversations/with/22/messages")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;

    let svc = from_url("twitter://ckey/csecret/akey/asecret/@bob/alice").unwrap();
    let mut ctx = NotifyContext { body: "B".into(), ..Default::default() };
    ctx.attachments = vec![png("a.png"), gif("b.gif")];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    let upload = reqs.iter().find(|r| r.url.path() == "/2/media/upload").unwrap();
    assert!(String::from_utf8_lossy(&upload.body).contains("dm_image"));
    let dms = json_bodies(&reqs, "/2/dm_conversations/with/11/messages");
    assert_eq!(dms[0], json!({"text": "B", "attachments": [{"media_id": "m1"}]}));
    assert_eq!(dms[1], json!({"text": "02/02", "attachments": [{"media_id": "m1"}]}));
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };

    // Every target invalid
    let svc = from_url("twitter://ckey/csecret/akey/asecret/-/%25/").unwrap();
    assert!(!svc.send(&ctx).await.unwrap());

    // Identity lookup fails
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/2/users/me")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
    let svc = from_url("twitter://ckey/csecret/akey/asecret").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // Tweet rejected with an error body
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/2/tweets"))
      .respond_with(ResponseTemplate::new(403).set_body_json(json!({"errors": [{"title": "Forbidden", "detail": "nope"}]})))
      .mount(&server)
      .await;
    let svc = from_url("tweet://ckey/csecret/akey/asecret").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // A failed upload stops the notification
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/2/media/upload")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    Mock::given(method("POST")).and(path("/2/tweets")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
    let mut ctx2 = ctx.clone();
    ctx2.attachments = vec![png("a.png")];
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx2)).await.unwrap());
  }

  #[tokio::test]
  async fn test_rate_limit_headers() {
    let server = MockServer::start().await;
    let reset = chrono::Utc::now().timestamp() - 1;
    Mock::given(method("POST"))
      .and(path("/2/tweets"))
      .respond_with(ResponseTemplate::new(200).insert_header("x-rate-limit-remaining", "0").insert_header("x-rate-limit-reset", reset.to_string().as_str()))
      .mount(&server)
      .await;
    let svc = parse_twitter("tweet://ckey/csecret/akey/asecret");
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(svc.state().ratelimit_remaining, 0);
    assert_eq!(svc.state().ratelimit_reset, reset);
  }

  #[test]
  fn test_body_maxlen_by_mode() {
    assert_eq!(parse_twitter("x://ckey/csecret/akey/asecret").body_maxlen(), 10000);
    assert_eq!(parse_twitter("tweet://ckey/csecret/akey/asecret").body_maxlen(), 280);
  }

  #[test]
  fn test_service_details() {
    let details = Twitter::static_details();
    assert_eq!(details.service_name, "Twitter");
    assert_eq!(details.protocols, vec!["x", "twitter", "tweet"]);
    assert!(details.attachment_support);
  }
}
