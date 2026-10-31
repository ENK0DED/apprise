use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::error::NotifyError;
use crate::notify::{APP_ID, ClientDefaults, Notify, NotifyContext, ServiceDetails, build_client, client_builder, service_base};
use crate::utils::http::{is_secure_http_url, read_bounded_response};
use crate::utils::parse::ParsedUrl;

const DEFAULT_HOST: &str = "bsky.social";
const PUBLIC_API: &str = "https://public.api.bsky.app";
const PLC_DIRECTORY: &str = "https://plc.directory";

const XRPC_DID: &str = "/xrpc/com.atproto.identity.resolveHandle";
const XRPC_SESSION: &str = "/xrpc/com.atproto.server.createSession";
const XRPC_RECORD: &str = "/xrpc/com.atproto.repo.createRecord";
const XRPC_BLOB: &str = "/xrpc/com.atproto.repo.uploadBlob";

/// Replies to lookups and logins larger than this are discarded.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// How long a session token is kept (upstream `access_token_lifetime_sec`
/// minus its `clock_skew`).
const ACCESS_TOKEN_LIFETIME: Duration = Duration::from_secs(3600 - 10);

/// A string upstream keeps in the service's persistent store.
fn stored(key: &str) -> Option<String> {
  crate::storage::with_service_store(|s| s.get(key)).flatten().and_then(|v| v.as_str().map(String::from))
}

/// Keep `value` in the persistent store, when there is one.
fn store(key: &str, value: &str, expires: Option<Duration>) {
  crate::storage::with_service_store(|s| s.set(key, json!(value), expires, true));
}

/// The session token, resolved DIDs and PDS endpoints, and the rate limit
/// the server last reported. Tokens and identities also go to the
/// persistent store, as upstream keeps them; this copy serves the instance
/// when there is no store.
#[derive(Default)]
struct Session {
  access_token: Option<String>,
  /// handle -> (did, pds endpoint)
  identities: HashMap<String, (String, String)>,
  ratelimit_remaining: i64,
  /// Unix timestamp at which the rate limit window resets.
  ratelimit_reset: i64,
}

enum Payload<'a> {
  None,
  Json(String),
  Binary(&'a [u8], &'a str),
}

struct Request<'a> {
  url: String,
  method: reqwest::Method,
  payload: Payload<'a>,
  params: Option<(&'a str, &'a str)>,
  /// The payload carries our password: never follow a redirect.
  credentials: bool,
  /// Send our session token (false for lookups on other servers).
  auth: bool,
  bounded: bool,
}

pub struct BlueSky {
  /// The user as the URL gives it (user.example.ca), for the storage ID.
  handle: String,
  /// Handle without its host part.
  user: String,
  /// Host part of the handle (bsky.social unless the handle carries one).
  host: String,
  password: String,
  verify_certificate: bool,
  tags: Vec<String>,
  session: Mutex<Session>,
}

impl BlueSky {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // bluesky://{user}@{password}
    let Some(user) = url.user.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
      tracing::warn!("A BlueSky UserID/Handle must be specified.");
      return None;
    };
    let password = url.password.clone().or_else(|| url.host.clone()).unwrap_or_default();
    let handle = url.user.clone().unwrap_or_default();

    // A handle such as user.example.ca targets example.ca
    let (user, host) = match user.split_once('.') {
      Some((handle, rest)) if !handle.is_empty() && !rest.trim_start_matches('.').is_empty() => {
        (handle.trim().to_string(), rest.trim_start_matches('.').trim().to_string())
      }
      _ => (user.to_string(), DEFAULT_HOST.to_string()),
    };

    Some(Self {
      handle,
      user,
      host,
      password,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      session: Mutex::new(Session { ratelimit_remaining: 1, ..Default::default() }),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "BlueSky",
      service_url: Some("https://bluesky.us/"),
      setup_url: Some("https://appriseit.com/services/bluesky/"),
      protocols: vec!["bsky", "bluesky"],
      description: "Post to BlueSky.",
      attachment_support: true,
    }
  }

  fn session(&self) -> std::sync::MutexGuard<'_, Session> {
    self.session.lock().unwrap_or_else(|e| e.into_inner())
  }

  /// Perform one API request. Returns whether it succeeded (HTTP 200) and
  /// the decoded JSON reply (`{}` when the reply is not JSON).
  async fn fetch(&self, client: &reqwest::Client, nofollow: &reqwest::Client, req: Request<'_>) -> (bool, Value) {
    let content_type = match &req.payload {
      Payload::Binary(_, mime) => mime.to_string(),
      _ if req.method == reqwest::Method::GET => "application/x-www-form-urlencoded; charset=utf-8".to_string(),
      _ => "application/json".to_string(),
    };

    let (token, wait) = {
      let s = self.session();
      let now = chrono::Utc::now().timestamp_millis();
      let reset = s.ratelimit_reset * 1000;
      let wait = (s.ratelimit_remaining == 0 && now < reset).then(|| Duration::from_millis((reset - now) as u64 + 300));
      (s.access_token.clone(), wait)
    };
    if let Some(wait) = wait {
      tracing::debug!("BlueSky rate limit reached; waiting {:.2}s", wait.as_secs_f64());
    }
    crate::notify::throttle_for(wait).await;

    tracing::debug!("BlueSky {} URL: {} (cert_verify={})", req.method, req.url, self.verify_certificate);
    let c = if req.credentials { nofollow } else { client };
    let mut builder = c.request(req.method.clone(), &req.url).header("User-Agent", APP_ID).header("Content-Type", content_type);
    if req.auth
      && let Some(token) = token
    {
      builder = builder.header("Authorization", format!("Bearer {}", token));
    }
    if let Some(params) = req.params {
      builder = builder.query(&[params]);
    }
    builder = match req.payload {
      Payload::None => builder,
      Payload::Json(s) => builder.body(s),
      Payload::Binary(data, _) => builder.body(data.to_vec()),
    };

    let resp = match builder.send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("Exception received when sending BlueSky {} to {}: ", req.method, req.url);
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return (false, json!({}));
      }
    };

    let status = resp.status();
    let headers = resp.headers().clone();
    let body = if req.bounded {
      match read_bounded_response(resp, MAX_RESPONSE_BYTES).await {
        Ok(Some(b)) => b,
        Ok(None) => {
          tracing::warn!("BlueSky reply from {} passed {} bytes; discarding it.", req.url, MAX_RESPONSE_BYTES);
          return (false, json!({}));
        }
        Err(e) => {
          tracing::warn!("Exception received when sending BlueSky {} to {}: ", req.method, req.url);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          return (false, json!({}));
        }
      }
    } else {
      resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default()
    };

    let content = serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| {
      tracing::debug!("Failed to parse BlueSky JSON response");
      json!({})
    });

    // Track the rate limit the server reports
    let header_int = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<i64>().ok());
    if let Some(remaining) = header_int("ratelimit-remaining") {
      let mut s = self.session();
      s.ratelimit_remaining = remaining;
      if let Some(reset) = header_int("ratelimit-reset") {
        s.ratelimit_reset = reset;
      }
    }

    if status.as_u16() != 200 {
      let reason = status.canonical_reason().unwrap_or("");
      tracing::warn!("Failed to send BlueSky {} to {}: {}{}error={}.", req.method, req.url, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      if !req.credentials {
        tracing::debug!("Response Details:\r\n{}", String::from_utf8_lossy(&body[..body.len().min(2000)]));
      }
      return (false, content);
    }
    (true, content)
  }

  /// Resolve a handle to its DID and PDS endpoint (cached per instance).
  async fn get_identifier(&self, client: &reqwest::Client, nofollow: &reqwest::Client) -> Option<(String, String)> {
    let user = if self.user.contains('.') { self.user.clone() } else { format!("{}.{}", self.user, self.host) };
    if let Some(found) = self.session().identities.get(&user) {
      return Some(found.clone());
    }
    let (did_key, endpoint_key) = (format!("did.{}", user), format!("endpoint.{}", user));
    if let (Some(did), Some(endpoint)) = (stored(&did_key), stored(&endpoint_key)) {
      self.session().identities.insert(user, (did.clone(), endpoint.clone()));
      return Some((did, endpoint));
    }

    let url = format!("{}{}", service_base(PUBLIC_API), XRPC_DID);
    let req =
      Request { url, method: reqwest::Method::GET, payload: Payload::None, params: Some(("handle", &user)), credentials: false, auth: false, bounded: true };
    let (ok, response) = self.fetch(client, nofollow, req).await;
    if !ok || !response.is_object() {
      return None;
    }

    let Some(did) = response.get("did").and_then(Value::as_str).map(String::from) else {
      tracing::warn!("An unparseable BlueSky DID was returned");
      return None;
    };

    let did_url = if did.starts_with("did:plc:") {
      format!("{}/{}", service_base(PLC_DIRECTORY), did)
    } else if let Some(web) = did.strip_prefix("did:web:") {
      format!("{}/.well-known/did.json", service_base(&format!("https://{}", web)))
    } else {
      tracing::warn!("Unknown BlueSky DID scheme detected in {}", did);
      return None;
    };

    let req =
      Request { url: did_url.clone(), method: reqwest::Method::GET, payload: Payload::None, params: None, credentials: false, auth: false, bounded: true };
    let (ok, document) = self.fetch(client, nofollow, req).await;
    if !ok || !document.is_object() {
      tracing::warn!("Could not fetch the DID document for {}; ensure {} is available.", did, did_url);
      return None;
    }

    let endpoint = document
      .get("service")
      .and_then(Value::as_array)
      .into_iter()
      .flatten()
      .find(|s| s.get("type").and_then(Value::as_str) == Some("AtprotoPersonalDataServer"))
      .and_then(|s| s.get("serviceEndpoint"))
      .and_then(Value::as_str)
      .map(String::from);

    let Some(endpoint) = endpoint.filter(|e| is_secure_http_url(e) && !e.contains('?')) else {
      tracing::warn!("Failed to resolve a usable BlueSky PDS endpoint");
      return None;
    };

    store(&did_key, &did, None);
    store(&endpoint_key, &endpoint, None);
    self.session().identities.insert(user, (did.clone(), endpoint.clone()));
    Some((did, endpoint))
  }

  async fn login(&self, client: &reqwest::Client, nofollow: &reqwest::Client) -> bool {
    let Some((did, endpoint)) = self.get_identifier(client, nofollow).await else {
      return false;
    };

    let payload = json!({ "identifier": did, "password": self.password }).to_string();
    let url = format!("{}{}", service_base(&endpoint), XRPC_SESSION);
    let req = Request { url, method: reqwest::Method::POST, payload: Payload::Json(payload), params: None, credentials: true, auth: true, bounded: true };
    let (ok, response) = self.fetch(client, nofollow, req).await;
    if !ok || response.as_object().is_none_or(|o| o.is_empty()) {
      return false;
    }

    let token = response.get("accessJwt").and_then(Value::as_str).map(String::from);
    if let Some(token) = &token {
      store("access_token", token, Some(ACCESS_TOKEN_LIFETIME));
    }
    if let Some(refresh) = response.get("refreshJwt").and_then(Value::as_str) {
      store("refresh_token", refresh, Some(ACCESS_TOKEN_LIFETIME));
    }
    store("endpoint", &endpoint, None);
    self.session().access_token = token;
    tracing::info!("Authenticated to BlueSky as {}.{}", self.user, self.host);
    true
  }
}

#[async_trait]
impl Notify for BlueSky {
  fn schemas(&self) -> &[&str] {
    &["bsky", "bluesky"]
  }
  fn service_name(&self) -> &str {
    "BlueSky"
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
    280
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn request_rate_per_sec(&self) -> f64 {
    // The server's own rate limit headers decide when to wait
    0.0
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    // Upstream opens its store, fixing the ID, before it splits the host
    // off the handle, so the ID uses the handle as written
    Some(vec!["bsky".to_string(), self.handle.clone(), self.password.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    // Never redirect a request carrying our password
    let nofollow =
      client_builder(self.verify_certificate, ClientDefaults::default()).redirect(reqwest::redirect::Policy::none()).build().map_err(NotifyError::Http)?;

    // A session from an earlier run may still be good
    if self.session().access_token.is_none()
      && let Some(token) = stored("access_token")
    {
      self.session().access_token = Some(token);
    }
    if self.session().access_token.is_none() && !self.login(&client, &nofollow).await {
      return Ok(false);
    }

    let Some((did, endpoint)) = self.get_identifier(&client, &nofollow).await else {
      return Ok(false);
    };
    let base = service_base(&endpoint);

    // Upload each image attachment as a blob
    let mut blobs: Vec<(Value, String)> = Vec::new();
    for (no, attachment) in ctx.attachments.iter().enumerate() {
      if !attachment.mime_type.to_lowercase().starts_with("image/") {
        tracing::warn!("Ignoring unsupported BlueSky attachment {}.", attachment.name);
        continue;
      }
      tracing::debug!("Preparing BlueSky attachment {}", attachment.name);
      let req = Request {
        url: format!("{}{}", base, XRPC_BLOB),
        method: reqwest::Method::POST,
        payload: Payload::Binary(&attachment.data, &attachment.mime_type),
        params: None,
        credentials: false,
        auth: true,
        bounded: false,
      };
      let (ok, response) = self.fetch(&client, &nofollow, req).await;
      if !ok {
        return Ok(false);
      }
      let filename = if attachment.name.is_empty() { format!("file{:03}.dat", no + 1) } else { attachment.name.clone() };
      match response.get("blob").filter(|b| is_truthy(b)) {
        Some(blob) => blobs.push((blob.clone(), filename)),
        None => tracing::debug!("Could not attach the file to BlueSky: {} (mime={})", filename, attachment.mime_type),
      }
    }

    let created_at = || chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let post = |text: &str, embed: Option<Value>| {
      let mut record = json!({ "text": text, "createdAt": created_at(), "$type": "app.bsky.feed.post" });
      if let Some(embed) = embed {
        record["embed"] = embed;
      }
      json!({ "collection": "app.bsky.feed.post", "repo": did, "record": record })
    };

    // One post per image; posts after the first are numbered 02/NN, ...
    let payloads: Vec<Value> = if blobs.is_empty() {
      vec![post(&ctx.body, None)]
    } else {
      let total = blobs.len();
      blobs
        .into_iter()
        .enumerate()
        .map(|(i, (blob, alt))| {
          let text = if i == 0 { ctx.body.clone() } else { format!("{:02}/{:02}", i + 1, total) };
          post(&text, Some(json!({ "images": [{ "image": blob, "alt": alt }], "$type": "app.bsky.embed.images" })))
        })
        .collect()
    };

    for (no, payload) in payloads.into_iter().enumerate() {
      // Skip a post that was already created so a retry does not publish
      // it twice
      let key = (no + 1).to_string();
      if crate::delivery::is_delivered(&key, false) {
        continue;
      }
      let req = Request {
        url: format!("{}{}", base, XRPC_RECORD),
        method: reqwest::Method::POST,
        payload: Payload::Json(payload.to_string()),
        params: None,
        credentials: false,
        auth: true,
        bounded: false,
      };
      if !self.fetch(&client, &nofollow, req).await.0 {
        return Ok(false);
      }
      crate::delivery::mark_delivered(&key, false);
    }
    Ok(true)
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
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, Request as MockRequest, ResponseTemplate};

  fn parse_bsky(url: &str) -> BlueSky {
    BlueSky::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  /// Mount the handle lookup, PLC document and session responses.
  async fn mount_login(server: &MockServer, endpoint: &str) {
    Mock::given(method("GET"))
      .and(path(XRPC_DID))
      .and(query_param("handle", "handle.bsky.social"))
      .and(header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"did": "did:plc:123"})))
      .mount(server)
      .await;
    Mock::given(method("GET"))
      .and(path("/did:plc:123"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"service": [{"type": "AtprotoPersonalDataServer", "serviceEndpoint": endpoint}]})))
      .mount(server)
      .await;
    Mock::given(method("POST"))
      .and(path(XRPC_SESSION))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"accessJwt": "jwt-token", "refreshJwt": "refresh"})))
      .mount(server)
      .await;
  }

  fn requests_to<'a>(reqs: &'a [MockRequest], p: &str) -> Vec<&'a MockRequest> {
    reqs.iter().filter(|r| r.url.path() == p).collect()
  }

  #[test]
  fn test_storage_id_matches_upstream() {
    // uid: lines of upstream's apprise --dry-run; a missing value hashes as "None"
    // Upstream fixes the ID before it splits the host off the handle
    crate::notify::custom_json::assert_upstream_uids(&[
      ("bluesky://user.example.com:pass@bsky.social", "8db9828b"),
      ("bluesky://user.example.ca@app-pw3", "30756a26"),
      ("bluesky://user@app-pw", "88258130"),
    ]);
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "bluesky://user@app-pw",
      "bluesky://user@app-pw1?cache=no",
      "bluesky://user@app-pw2?cache=no",
      "bluesky://user@app-pw3",
      "bluesky://user.example.ca@app-pw3",
      "bsky://user@pass",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["bluesky://", "bluesky://:@/", "bluesky://app-pw"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse_bsky("bluesky://myhandle@my-app-password");
    assert_eq!(obj.user, "myhandle");
    assert_eq!(obj.host, "bsky.social");
    assert_eq!(obj.password, "my-app-password");

    // The handle carries its own host
    let obj = parse_bsky("bluesky://user.example.ca@app-pw3");
    assert_eq!(obj.user, "user");
    assert_eq!(obj.host, "example.ca");
    assert_eq!(obj.password, "app-pw3");

    let obj = parse_bsky("bluesky://user:secret@");
    assert_eq!(obj.password, "secret");
  }

  #[test]
  fn test_is_secure_http_url() {
    assert!(is_secure_http_url("https://example.pds.io"));
    assert!(is_secure_http_url("https://example.pds.io:8443/"));
    assert!(is_secure_http_url("https://10.0.0.1"));
    assert!(is_secure_http_url("https://my_pds.example.com"));
    assert!(!is_secure_http_url("http://evil.example.com"));
    assert!(!is_secure_http_url("https://attacker:secret@evil.example.com"));
    assert!(!is_secure_http_url("https://@evil.example.com"));
    assert!(!is_secure_http_url("https://-nope-.example.com"));
    assert!(!is_secure_http_url("https://example.com/#frag"));
    assert!(!is_secure_http_url("https://example.com:0"));
    assert!(!is_secure_http_url("not a url"));
  }

  #[tokio::test]
  async fn test_send_full_flow() {
    let server = MockServer::start().await;
    mount_login(&server, "https://example.pds.io").await;
    Mock::given(method("POST"))
      .and(path(XRPC_RECORD))
      .and(header("Authorization", "Bearer jwt-token"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"uri": "at://x"})))
      .expect(2)
      .mount(&server)
      .await;

    let svc = from_url("bsky://handle@token").unwrap();
    let ctx = NotifyContext { body: "hello from python".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    // A second notification reuses the session and resolved identity
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(requests_to(&reqs, XRPC_DID).len(), 1);
    assert_eq!(requests_to(&reqs, "/did:plc:123").len(), 1);

    // Lookups never carry a session token; the login carries the password
    for r in requests_to(&reqs, XRPC_DID).into_iter().chain(requests_to(&reqs, "/did:plc:123")) {
      assert!(!r.headers.contains_key("authorization"));
    }
    let login = requests_to(&reqs, XRPC_SESSION);
    assert_eq!(login.len(), 1);
    assert_eq!(serde_json::from_slice::<Value>(&login[0].body).unwrap(), json!({"identifier": "did:plc:123", "password": "token"}));
    assert_eq!(login[0].headers.get("content-type").unwrap(), "application/json");

    let record: Value = serde_json::from_slice(&requests_to(&reqs, XRPC_RECORD)[0].body).unwrap();
    assert_eq!(record["collection"], "app.bsky.feed.post");
    assert_eq!(record["repo"], "did:plc:123");
    assert_eq!(record["record"]["text"], "hello from python");
    assert_eq!(record["record"]["$type"], "app.bsky.feed.post");
    let created = record["record"]["createdAt"].as_str().unwrap();
    assert!(chrono::NaiveDateTime::parse_from_str(created, "%Y-%m-%dT%H:%M:%SZ").is_ok(), "{}", created);
  }

  #[tokio::test]
  async fn test_send_did_web() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path(XRPC_DID))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"did": "did:web:example.com"})))
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path("/.well-known/did.json"))
      .respond_with(
        ResponseTemplate::new(200).set_body_json(json!({"service": [{"type": "AtprotoPersonalDataServer", "serviceEndpoint": "https://example.com"}]})),
      )
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(XRPC_SESSION)).respond_with(ResponseTemplate::new(200).set_body_json(json!({"accessJwt": "a"}))).mount(&server).await;
    Mock::given(method("POST")).and(path(XRPC_RECORD)).respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).expect(1).mount(&server).await;

    let svc = from_url("bsky://handle@token").unwrap();
    let ctx = NotifyContext { body: "Resolved WEB Flow".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_attachments() {
    let server = MockServer::start().await;
    mount_login(&server, "https://example.pds.io").await;
    Mock::given(method("POST"))
      .and(path(XRPC_BLOB))
      .and(header("Content-Type", "image/png"))
      .and(header("Authorization", "Bearer jwt-token"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"blob": {"$type": "blob", "ref": {"$link": "bafy"}, "mimeType": "image/png", "size": 3}})))
      .expect(2)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(XRPC_RECORD)).respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).expect(2).mount(&server).await;

    let svc = from_url("bsky://handle@token").unwrap();
    let mut ctx = NotifyContext { body: "body".into(), ..Default::default() };
    ctx.attachments.push(Attachment { name: "a.png".into(), data: b"PNG".to_vec(), mime_type: "image/png".into() });
    // Unsupported attachments are skipped
    ctx.attachments.push(Attachment { name: "v.mp4".into(), data: b"MP4".to_vec(), mime_type: "video/mp4".into() });
    ctx.attachments.push(Attachment { name: String::new(), data: b"PNG".to_vec(), mime_type: "image/png".into() });
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    let records: Vec<Value> = requests_to(&reqs, XRPC_RECORD).iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
    assert_eq!(records[0]["record"]["text"], "body");
    assert_eq!(
      records[0]["record"]["embed"],
      json!({"images": [{"image": {"$type": "blob", "ref": {"$link": "bafy"}, "mimeType": "image/png", "size": 3}, "alt": "a.png"}], "$type": "app.bsky.embed.images"})
    );
    assert_eq!(records[1]["record"]["text"], "02/02");
    assert_eq!(records[1]["record"]["embed"]["images"][0]["alt"], "file003.dat");
  }

  #[tokio::test]
  async fn test_send_upload_failure_stops() {
    let server = MockServer::start().await;
    mount_login(&server, "https://example.pds.io").await;
    Mock::given(method("POST")).and(path(XRPC_BLOB)).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    Mock::given(method("POST")).and(path(XRPC_RECORD)).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;

    let svc = from_url("bsky://handle@token").unwrap();
    let mut ctx = NotifyContext { body: "body".into(), ..Default::default() };
    ctx.attachments.push(Attachment { name: "a.png".into(), data: b"PNG".to_vec(), mime_type: "image/png".into() });
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_auth_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path(XRPC_DID)).respond_with(ResponseTemplate::new(200).set_body_json(json!({"did": "did:plc:123"}))).mount(&server).await;
    Mock::given(method("GET"))
      .and(path("/did:plc:123"))
      .respond_with(
        ResponseTemplate::new(200).set_body_json(json!({"service": [{"type": "AtprotoPersonalDataServer", "serviceEndpoint": "https://example.pds.io"}]})),
      )
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(XRPC_SESSION)).respond_with(ResponseTemplate::new(401)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path(XRPC_RECORD)).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;

    let svc = from_url("bsky://handle@token").unwrap();
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_login_does_not_follow_redirects() {
    let server = MockServer::start().await;
    mount_login(&server, "https://example.pds.io").await;
    // Replace the session reply with a redirect
    server.reset().await;
    Mock::given(method("GET")).and(path(XRPC_DID)).respond_with(ResponseTemplate::new(200).set_body_json(json!({"did": "did:plc:123"}))).mount(&server).await;
    Mock::given(method("GET"))
      .and(path("/did:plc:123"))
      .respond_with(
        ResponseTemplate::new(200).set_body_json(json!({"service": [{"type": "AtprotoPersonalDataServer", "serviceEndpoint": "https://example.pds.io"}]})),
      )
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(XRPC_SESSION))
      .respond_with(ResponseTemplate::new(307).insert_header("Location", format!("{}/elsewhere", server.uri())))
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/elsewhere"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"accessJwt": "a"})))
      .expect(0)
      .mount(&server)
      .await;

    let svc = from_url("bsky://handle@token").unwrap();
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_resolution_failures() {
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    let cases: Vec<(Value, Option<Value>)> = vec![
      // Not an object / DID not a string / unknown DID scheme
      (json!(["not", "an", "object"]), None),
      (json!({"did": {"nested": "object"}}), None),
      (json!({"did": "did:unsupported:scheme"}), None),
      // Malformed or unsafe service entries
      (json!({"did": "did:plc:123"}), Some(json!({"service": "AtprotoPersonalDataServer"}))),
      (json!({"did": "did:plc:123"}), Some(json!({"service": ["string", 42, null]}))),
      (json!({"did": "did:plc:123"}), Some(json!({"service": [{"serviceEndpoint": "https://example.pds.io"}]}))),
      (json!({"did": "did:plc:123"}), Some(json!({"service": [{"type": "AtprotoPersonalDataServer"}]}))),
      (json!({"did": "did:plc:123"}), Some(json!({"service": [{"type": "AtprotoPersonalDataServer", "serviceEndpoint": "http://evil.example.com"}]}))),
      (json!({"did": "did:plc:123"}), Some(json!({"service": [{"type": "AtprotoPersonalDataServer", "serviceEndpoint": "https://a:b@evil.example.com"}]}))),
      (json!({"did": "did:plc:123"}), Some(json!({"service": [{"type": "AtprotoPersonalDataServer", "serviceEndpoint": "https://example.pds.io/?x=1"}]}))),
    ];
    for (identity, document) in cases {
      let server = MockServer::start().await;
      Mock::given(method("GET")).and(path(XRPC_DID)).respond_with(ResponseTemplate::new(200).set_body_json(identity.clone())).mount(&server).await;
      if let Some(document) = document {
        Mock::given(method("GET")).and(path("/did:plc:123")).respond_with(ResponseTemplate::new(200).set_body_json(document)).mount(&server).await;
      }
      // Our login details are never sent anywhere
      Mock::given(method("POST")).and(path(XRPC_SESSION)).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
      let svc = from_url("bsky://handle@token").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "{:?}", identity);
    }
  }

  #[tokio::test]
  async fn test_oversized_lookup_is_discarded() {
    let server = MockServer::start().await;
    let huge = format!("{{\"did\": \"did:plc:123\", \"pad\": \"{}\"}}", "x".repeat(MAX_RESPONSE_BYTES));
    Mock::given(method("GET")).and(path(XRPC_DID)).respond_with(ResponseTemplate::new(200).set_body_string(huge)).mount(&server).await;
    Mock::given(method("GET")).and(path("/did:plc:123")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
    let svc = from_url("bsky://handle@token").unwrap();
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_ratelimit_headers_tracked() {
    let server = MockServer::start().await;
    mount_login(&server, "https://example.pds.io").await;
    let reset = chrono::Utc::now().timestamp() - 1;
    Mock::given(method("POST"))
      .and(path(XRPC_RECORD))
      .respond_with(ResponseTemplate::new(200).insert_header("ratelimit-remaining", "0").insert_header("ratelimit-reset", reset.to_string().as_str()))
      .mount(&server)
      .await;
    let svc = parse_bsky("bsky://handle@token");
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(svc.session().ratelimit_remaining, 0);
    assert_eq!(svc.session().ratelimit_reset, reset);
    // A reset time in the past does not block the next post
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_session_and_identity_come_from_the_store() {
    let server = MockServer::start().await;
    // One lookup and one login across both runs
    Mock::given(method("GET"))
      .and(path(XRPC_DID))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"did": "did:plc:123"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path("/did:plc:123"))
      .respond_with(
        ResponseTemplate::new(200).set_body_json(json!({"service": [{"type": "AtprotoPersonalDataServer", "serviceEndpoint": "https://example.pds.io"}]})),
      )
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(XRPC_SESSION))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"accessJwt": "jwt-token", "refreshJwt": "refresh"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(XRPC_RECORD))
      .and(header("Authorization", "Bearer jwt-token"))
      .respond_with(ResponseTemplate::new(200))
      .expect(2)
      .mount(&server)
      .await;

    let tmp = tempfile::tempdir().unwrap();
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    for _ in 0..2 {
      // A fresh instance each time, as a new process would have
      let asset = crate::AppriseAsset { storage_path: Some(tmp.path().to_path_buf()), ..Default::default() };
      let mut apprise = crate::Apprise::with_asset(asset);
      assert!(apprise.add("bsky://handle@token"));
      assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx)).await.is_success());
      apprise.flush_stores();
    }
  }

  #[tokio::test]
  async fn test_retry_skips_posts_already_created() {
    let server = MockServer::start().await;
    mount_login(&server, "https://example.pds.io").await;
    Mock::given(method("POST")).and(path(XRPC_BLOB)).respond_with(ResponseTemplate::new(200).set_body_json(json!({"blob": {"ref": "x"}}))).mount(&server).await;
    let record =
      |text: &str| Mock::given(method("POST")).and(path(XRPC_RECORD)).and(wiremock::matchers::body_string_contains(format!("\"text\":\"{}\"", text)));
    record("body").respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    // The second post fails once, then goes through on the retry
    record("02/02").respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    record("02/02").respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let mut ctx = NotifyContext { body: "body".into(), ..Default::default() };
    for name in ["a.png", "b.png"] {
      ctx.attachments.push(Attachment { name: name.into(), data: b"PNG".to_vec(), mime_type: "image/png".into() });
    }
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("bsky://handle@token?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx)).await.is_success());
  }

  #[tokio::test]
  async fn test_waits_out_the_rate_limit() {
    let server = MockServer::start().await;
    mount_login(&server, "https://example.pds.io").await;
    // The window resets about a second from now
    let reset = chrono::Utc::now().timestamp() + 1;
    Mock::given(method("POST"))
      .and(path(XRPC_RECORD))
      .respond_with(ResponseTemplate::new(200).insert_header("ratelimit-remaining", "0").insert_header("ratelimit-reset", reset.to_string().as_str()))
      .expect(2)
      .mount(&server)
      .await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("bsky://handle@token"));
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    crate::notify::with_throttling_in_tests(crate::notify::with_test_base(server.uri(), async {
      assert!(apprise.notify(&ctx).await.is_success());
      let started = std::time::Instant::now();
      assert!(apprise.notify(&ctx).await.is_success());
      // Upstream waits for the reset plus 0.3s of grace
      assert!(started.elapsed() >= Duration::from_millis(300), "{:?}", started.elapsed());
    }))
    .await;
  }

  #[test]
  fn test_service_details() {
    let details = BlueSky::static_details();
    assert_eq!(details.service_name, "BlueSky");
    assert_eq!(details.protocols, vec!["bsky", "bluesky"]);
    assert!(details.attachment_support);
  }
}
