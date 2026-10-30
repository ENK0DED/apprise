//! Matrix (upstream `apprise/plugins/matrix/`).
//!
//! Two families of delivery:
//!
//! - **Webhooks** (`?mode=matrix|slack|t2bot|hookshot`): one POST to a
//!   bridge.
//! - **Server** (`?mode=off`, the default with credentials or targets): log
//!   in (or register, or use an access token), join each room or open a
//!   direct-message room for each `@user`, and send the message, an optional
//!   icon (API v2) and attachments. Rooms the server reports as encrypted
//!   get Olm/Megolm encrypted events (the `matrix-e2ee` feature), and
//!   `?autoverify=yes` answers a SAS verification from another device of
//!   the same user.
//!
//! Login state, joined rooms, discovery results and the encryption device
//! are kept in the service's persistent store, as upstream keeps them; a
//! private in-memory cache stands in when no store is available.
//!
//! Deliberate differences from upstream, each a bug upstream still has:
//!
//! - Every event gets its own transaction ID. Upstream reuses one UUID for
//!   every event of a raw-token send (and across sends), so Synapse drops
//!   the repeats as retransmissions (apprise-go #87, #91).
//! - A room whose encryption state cannot be determined is not sent to;
//!   upstream treats a failed lookup as "not encrypted" and caches it, which
//!   sends plaintext into an encrypted room (apprise-go 04d2c71).
//! - Without the `matrix-e2ee` feature an encrypted room fails closed
//!   instead of receiving plaintext.
//! - A rotated Megolm session is shared before an attachment uses it.
//! - Upstream logs out of an in-memory login when the object is destroyed;
//!   `Drop` cannot make that request here, so the device stays logged in.

mod sizing;

#[cfg(feature = "matrix-e2ee")]
mod e2ee;
#[cfg(feature = "matrix-e2ee")]
mod encrypted;
#[cfg(feature = "matrix-e2ee")]
mod sas;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::asset::NotifyImageSize;
use crate::conversion::{html_to_text, markdown_to_html_with};
use crate::delivery::{is_delivered, mark_delivered, recall, remember};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Attachment, ClientDefaults, Notify, NotifyContext, ServiceDetails, client_builder, service_base, throttle, throttle_for};
use crate::types::NotifyFormat;
use crate::utils::parse::{ParsedUrl, parse_bool, parse_list, quote};

const MATRIX_V1_WEBHOOK_PATH: &str = "/api/v1/matrix/hook";
const MATRIX_V2_API_PATH: &str = "/_matrix/client/r0";
const MATRIX_V3_API_PATH: &str = "/_matrix/client/v3";
const MATRIX_V3_MEDIA_PATH: &str = "/_matrix/media/v3";
const MATRIX_V2_MEDIA_PATH: &str = "/_matrix/media/r0";
const T2BOT_BASE: &str = "https://webhooks.t2bot.io";

/// Body limits outside content-aware preparation and for webhooks.
const BODY_MAXLEN_DEFAULT: usize = 65000;
/// Encrypted events use a lower fallback because encryption grows them.
const BODY_MAXLEN_E2EE: usize = 40000;
const TITLE_MAXLEN: usize = 250;

/// Attempts made when the server asks us to throttle back (HTTP 429).
const DEFAULT_RETRIES: u32 = 2;
/// Wait used for a 429 that does not say how long to wait.
const DEFAULT_WAIT_MS: f64 = 1000.0;

/// Lifetime of cached login, device and encryption state (20 days).
const CACHE_EXPIRY: Duration = Duration::from_secs(60 * 60 * 24 * 20);

const DISCOVERY_BASE_KEY: &str = "__discovery_base";
const DISCOVERY_IDENTITY_KEY: &str = "__discovery_identity";
const DISCOVERY_CACHE: Duration = Duration::from_secs(86400);

/// Discovery and login answers are small; bound what is read back.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Whether this build can encrypt (the `matrix-e2ee` feature).
const E2EE_SUPPORT: bool = cfg!(feature = "matrix-e2ee");

const MEGOLM_ALGORITHM: &str = "m.megolm.v1.aes-sha2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  Off,
  Matrix,
  Slack,
  T2Bot,
  Hookshot,
}

impl Mode {
  fn parse(s: &str) -> Option<Self> {
    match s.to_lowercase().as_str() {
      "off" => Some(Self::Off),
      "matrix" => Some(Self::Matrix),
      "slack" => Some(Self::Slack),
      "t2bot" => Some(Self::T2Bot),
      "hookshot" => Some(Self::Hookshot),
      _ => None,
    }
  }

  fn as_str(self) -> &'static str {
    match self {
      Self::Off => "off",
      Self::Matrix => "matrix",
      Self::Slack => "slack",
      Self::T2Bot => "t2bot",
      Self::Hookshot => "hookshot",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Version {
  V2,
  V3,
}

/// Upstream `MatrixMessageType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MsgType {
  Text,
  Notice,
}

/// How the next room event is identified (`/send/{type}/{txnId}`).
#[derive(Debug, Clone, PartialEq)]
enum Txn {
  /// A counter persisted with the login, as upstream keeps it.
  Counter(u64),
  /// A fresh UUID for every event (raw access tokens).
  Fresh,
}

/// Login state and caches, shared by every send of this service.
struct Session {
  /// The persisted state was read.
  restored: bool,
  home_server: Option<String>,
  user_id: Option<String>,
  access_token: Option<String>,
  device_id: Option<String>,
  txn: Txn,
  /// Rooms found through `/joined_rooms` when no target was given.
  joined_rooms: Vec<String>,
  #[cfg(feature = "matrix-e2ee")]
  account: Option<e2ee::OlmAccount>,
  /// Stand-in for the persistent store when none is available (upstream
  /// always has at least an in-memory store).
  local: HashMap<String, (Value, Option<Instant>)>,
}

impl Default for Session {
  fn default() -> Self {
    Self {
      restored: false,
      home_server: None,
      user_id: None,
      access_token: None,
      device_id: None,
      txn: Txn::Counter(0),
      joined_rooms: Vec::new(),
      #[cfg(feature = "matrix-e2ee")]
      account: None,
      local: HashMap::new(),
    }
  }
}

impl Session {
  fn get(&mut self, key: &str) -> Option<Value> {
    if let Some(value) = crate::storage::with_service_store(|s| s.get(key)) {
      return value.filter(|v| !v.is_null());
    }
    match self.local.get(key) {
      Some((_, Some(expiry))) if *expiry <= Instant::now() => {
        self.local.remove(key);
        None
      }
      Some((value, _)) => Some(value.clone()).filter(|v| !v.is_null()),
      None => None,
    }
  }

  fn get_str(&mut self, key: &str) -> Option<String> {
    self.get(key).and_then(|v| v.as_str().map(str::to_string)).filter(|s| !s.is_empty())
  }

  fn set(&mut self, key: &str, value: Value, expires: Option<Duration>) -> bool {
    if let Some(ok) = crate::storage::with_service_store(|s| s.set(key, value.clone(), expires, true)) {
      return ok;
    }
    self.local.insert(key.to_string(), (value, expires.map(|e| Instant::now() + e)));
    true
  }

  fn clear(&mut self, keys: &[&str]) {
    if crate::storage::with_service_store(|s| s.clear(keys)).is_none() {
      for key in keys {
        self.local.remove(*key);
      }
    }
  }

  /// The home server from `@user:home_server`.
  fn home_server_from_user_id(&self) -> Option<String> {
    self.user_id.as_deref().and_then(|u| u.split_once(':')).map(|(_, hs)| hs.to_string()).filter(|hs| !hs.is_empty())
  }
}

pub struct Matrix {
  host: String,
  port: Option<u16>,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  /// The t2bot webhook ID.
  t2bot_token: Option<String>,
  rooms: Vec<String>,
  users: Vec<String>,
  mode: Mode,
  version: Version,
  msgtype: MsgType,
  include_image: bool,
  discovery: bool,
  hsreq: bool,
  webhook_path: String,
  e2ee: bool,
  #[cfg_attr(not(feature = "matrix-e2ee"), allow(dead_code))]
  autoverify: bool,
  /// How long an `?autoverify=yes` bootstrap waits for a verification.
  #[cfg_attr(not(feature = "matrix-e2ee"), allow(dead_code))]
  autoverify_timeout: Duration,
  verify_certificate: bool,
  tags: Vec<String>,
  session: tokio::sync::Mutex<Session>,
}

/// One `(prefix)name(:home_server)` identifier, upstream's `IS_ROOM_ID`,
/// `IS_ROOM_ALIAS` and `IS_USER` patterns.
fn match_identifier(value: &str, prefixes: &[&str], prefix_required: bool, name_char: fn(char) -> bool) -> Option<(String, Option<String>)> {
  let s = value.trim();
  let rest = match prefixes.iter().find(|p| s.len() >= p.len() && s.is_char_boundary(p.len()) && s[..p.len()].eq_ignore_ascii_case(p)) {
    Some(p) => &s[p.len()..],
    None if prefix_required => return None,
    None => s,
  };
  let end = rest.find(|c: char| !name_char(c)).unwrap_or(rest.len());
  let (name, tail) = rest.split_at(end);
  if name.is_empty() {
    return None;
  }
  if tail.is_empty() {
    return Some((name.to_string(), None));
  }
  let hs = if let Some(hs) = tail.strip_prefix(':') {
    hs
  } else if tail.len() >= 3 && tail.is_char_boundary(3) && tail[..3].eq_ignore_ascii_case("%3A") {
    &tail[3..]
  } else {
    return None;
  };
  (!hs.is_empty() && hs.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))).then(|| (name.to_string(), Some(hs.to_string())))
}

fn room_char(c: char) -> bool {
  c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '=' | '-')
}

fn user_char(c: char) -> bool {
  c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '=' | '+' | '/' | '-')
}

fn parse_room_id(room: &str) -> Option<(String, Option<String>)> {
  match_identifier(room, &["!", "&#33;", "%21"], true, room_char)
}

fn parse_room_alias(room: &str) -> Option<(String, Option<String>)> {
  match_identifier(room, &["#", "%23"], false, room_char)
}

fn parse_user(user: &str) -> Option<(String, Option<String>)> {
  match_identifier(user, &["@", "%40"], true, user_char)
}

/// Upstream `URLBase.escape_html()`.
pub(super) fn escape_html(s: &str, convert_new_lines: bool, whitespace: bool) -> String {
  let mut out = String::with_capacity(s.len());
  for c in s.chars() {
    match c {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '\'' => out.push_str("&apos;"),
      '"' => out.push_str("&quot;"),
      '\t' if whitespace => out.push_str("&emsp;"),
      ' ' if whitespace => out.push_str("&nbsp;"),
      '\n' if convert_new_lines => out.push_str("<br/>"),
      c => out.push(c),
    }
  }
  out
}

fn is_rich(format: &NotifyFormat) -> bool {
  matches!(format, NotifyFormat::Html | NotifyFormat::Markdown)
}

/// Upstream `MATRIX_HTTP_ERROR_MAP`, else the standard reason.
fn status_message(status: u16) -> String {
  match status {
    403 => "Unauthorized - Invalid Token.".to_string(),
    429 => "Rate limit imposed; wait 2s and try again".to_string(),
    s => reqwest::StatusCode::from_u16(s).ok().and_then(|c| c.canonical_reason()).unwrap_or("").to_string(),
  }
}

fn uuid4() -> String {
  crate::notify::apprise_api::uuid4()
}

impl Matrix {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream parse_url().
    let host = url.host.clone()?;
    let mut targets: Vec<String> = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.extend(parse_list(to));
    }
    let include_image = url.get("image").map(parse_bool).unwrap_or(false);
    // Unrecognized values read as false, as upstream's parse_bool() does.
    let mut discovery = url.get("discovery").map(parse_bool).unwrap_or(true);
    let hsreq = url.get("hsreq").map(parse_bool).unwrap_or(true);
    let e2ee_arg = url.get("e2ee").map(parse_bool);
    let autoverify = url.get("autoverify").map(parse_bool).unwrap_or(false);

    let mut user = url.user.clone();
    let mut password = url.password.clone();
    let mut mode_arg = url.get("mode").map(str::to_string);
    if mode_arg.is_none() && password.is_none() && targets.is_empty() {
      mode_arg = Some("t2bot".into());
    }
    if mode_arg.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("t2bot")) {
      password = Some(host.clone());
    }
    if let Some(token) = url.get("token").filter(|t| !t.is_empty()) {
      password = Some(token.to_string());
    } else if password.is_none() && user.is_some() {
      password = user.take();
    }
    let version = match url.get("version").filter(|v| !v.is_empty()).or_else(|| url.get("v").filter(|v| !v.is_empty())) {
      None | Some("3") => Version::V3,
      Some("2") => Version::V2,
      Some(v) => {
        tracing::warn!("The version specified ({}) is invalid.", v);
        return None;
      }
    };

    // Upstream NotifyMatrix.__init__().
    let mut rooms = Vec::new();
    let mut users = Vec::new();
    for target in parse_list(&targets.join(",")) {
      if parse_user(&target).is_some() { users.push(target) } else { rooms.push(target) }
    }

    let webhook_path = match url.get("path").map(str::trim).filter(|p| !p.is_empty()) {
      Some(p) => {
        let p = if p.starts_with('/') { p.to_string() } else { format!("/{p}") };
        let p = p.trim_end_matches('/');
        if p.is_empty() { "/".to_string() } else { p.to_string() }
      }
      None => "/webhook".to_string(),
    };

    // Requesting autoverify implies e2ee unless e2ee= was given.
    let e2ee = e2ee_arg.unwrap_or(true);

    let mode = match mode_arg.as_deref() {
      None => Mode::Off,
      Some(m) => match Mode::parse(m) {
        Some(mode) => mode,
        None => {
          tracing::warn!("The mode specified ({}) is invalid.", m);
          return None;
        }
      },
    };

    let msgtype = match url.get("msgtype").filter(|m| !m.is_empty()).map(str::to_lowercase).as_deref() {
      None | Some("text") => MsgType::Text,
      Some("notice") => MsgType::Notice,
      Some(m) => {
        tracing::warn!("The msgtype specified ({}) is invalid.", m);
        return None;
      }
    };

    let mut t2bot_token = None;
    if mode == Mode::T2Bot {
      match password.as_deref().filter(|p| p.len() == 64 && p.chars().all(|c| c.is_ascii_alphanumeric())) {
        Some(token) => t2bot_token = Some(token.to_string()),
        None => {
          tracing::warn!("An invalid T2Bot/Matrix Webhook ID was specified.");
          return None;
        }
      }
    } else if !crate::utils::validate::is_hostname(&host) {
      tracing::warn!("An invalid Matrix Hostname ({}) was specified", host);
      return None;
    } else if url.port == Some(0) {
      tracing::warn!("An invalid Matrix Port (0) was specified");
      return None;
    }

    if mode != Mode::Off {
      // Discovery only applies to server mode.
      discovery = false;
    }

    Some(Self {
      host,
      port: url.port,
      secure: url.schema == "matrixs",
      user,
      password,
      t2bot_token,
      rooms,
      users,
      mode,
      version,
      msgtype,
      include_image,
      discovery,
      hsreq,
      webhook_path,
      e2ee,
      autoverify,
      autoverify_timeout: Duration::from_secs(120),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      session: tokio::sync::Mutex::new(Session::default()),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Matrix",
      service_url: Some("https://matrix.org/"),
      setup_url: Some("https://appriseit.com/services/matrix/"),
      protocols: vec!["matrix", "matrixs"],
      description: "Send via Matrix room messages, direct messages and webhooks.",
      attachment_support: true,
    }
  }

  /// `http(s)://host[:port]` (a mock server in tests).
  fn host_base(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
    service_base(&format!("{schema}://{}{port}", self.host))
  }

  /// Raw access-token URLs (`matrix://token@host`) skip login.
  fn token_auth(&self) -> bool {
    self.user.is_none() && self.password.is_some()
  }

  /// Whether encryption is possible at all for this URL (upstream
  /// `e2ee and secure and MATRIX_E2EE_SUPPORT`).
  fn e2ee_capable_config(&self) -> bool {
    self.e2ee && self.secure && E2EE_SUPPORT
  }

  fn api_path(&self) -> &'static str {
    match self.version {
      Version::V3 => MATRIX_V3_API_PATH,
      Version::V2 => MATRIX_V2_API_PATH,
    }
  }

  fn media_path(&self) -> &'static str {
    match self.version {
      Version::V3 => MATRIX_V3_MEDIA_PATH,
      Version::V2 => MATRIX_V2_MEDIA_PATH,
    }
  }

  // ── Webhooks ───────────────────────────────────────────────────────────────

  fn webhook_url(&self) -> String {
    match self.mode {
      Mode::T2Bot => format!("{}{}/{}", service_base(T2BOT_BASE), MATRIX_V1_WEBHOOK_PATH, self.t2bot_token.as_deref().unwrap_or_default()),
      mode => {
        let token = self.password.as_deref().or(self.user.as_deref()).unwrap_or_default();
        let path = if mode == Mode::Hookshot { self.webhook_path.trim_end_matches('/') } else { MATRIX_V1_WEBHOOK_PATH };
        format!("{}{path}/{token}", self.host_base())
      }
    }
  }

  /// `<h1>title</h1>` for webhook HTML, with a break unless rendered
  /// Markdown already starts on its own line.
  fn webhook_html(&self, ctx: &NotifyContext) -> String {
    let title = if ctx.title.is_empty() {
      String::new()
    } else {
      format!("<h1>{}</h1>{}", escape_html(&ctx.title, false, true), if ctx.body_format == NotifyFormat::Markdown { "" } else { "<br/>" })
    };
    format!("{title}{}", ctx.body)
  }

  fn plain_with_title(title: &str, body: &str) -> String {
    if title.is_empty() { body.to_string() } else { format!("{title}\r\n{body}") }
  }

  fn matrix_webhook_payload(&self, ctx: &NotifyContext) -> Value {
    let display_name = self.user.clone().unwrap_or_else(|| ctx.asset.app_id.clone());
    if is_rich(&ctx.body_format) {
      json!({"displayName": display_name, "format": "html", "text": self.webhook_html(ctx)})
    } else {
      json!({"displayName": display_name, "format": "plain", "text": Self::plain_with_title(&ctx.title, &ctx.body)})
    }
  }

  fn slack_webhook_payload(&self, ctx: &NotifyContext) -> Value {
    let escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    json!({
      "username": self.user.clone().unwrap_or_else(|| ctx.asset.app_id.clone()),
      // Slack renders the CommonMark body itself.
      "mrkdwn": ctx.body_format == NotifyFormat::Markdown,
      "attachments": [{
        "title": escape(&ctx.title),
        "text": escape(&ctx.body),
        "color": ctx.asset.color(&ctx.notify_type),
        "ts": ts,
        "footer": ctx.asset.app_id,
      }],
    })
  }

  fn hookshot_webhook_payload(&self, ctx: &NotifyContext) -> Value {
    let username = self.user.clone().unwrap_or_else(|| ctx.asset.app_id.clone());
    if is_rich(&ctx.body_format) {
      // Keep confirmed markup out of the plain-text fallback.
      let plain = html_to_text(&ctx.body);
      json!({"username": username, "text": Self::plain_with_title(&ctx.title, &plain), "html": self.webhook_html(ctx)})
    } else {
      let text = Self::plain_with_title(&ctx.title, &ctx.body);
      let html = escape_html(&text, true, false);
      json!({"username": username, "text": text, "html": html})
    }
  }

  async fn send_webhook(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = self.webhook_url();
    let payload = match self.mode {
      Mode::Slack => self.slack_webhook_payload(ctx),
      Mode::Hookshot => self.hookshot_webhook_payload(ctx),
      Mode::T2Bot => {
        let mut payload = self.matrix_webhook_payload(ctx);
        if self.include_image
          && let Some(image) = ctx.image_url(NotifyImageSize::Xy32)
        {
          payload["avatarUrl"] = json!(image);
        }
        payload
      }
      _ => self.matrix_webhook_payload(ctx),
    };
    tracing::debug!("Matrix POST URL: {} (cert_verify={})", crate::utils::cwe312::cwe312_loggable(&url, true), self.verify_certificate);
    throttle().await;
    let client = client_builder(self.verify_certificate, ClientDefaults::default()).build()?;
    let response = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(payload.to_string()).send().await;
    match response {
      Ok(r) if matches!(r.status().as_u16(), 200 | 202) => {
        tracing::info!("Sent Matrix notification.");
        Ok(true)
      }
      Ok(r) => {
        let status = r.status().as_u16();
        let reason = status_message(status);
        tracing::warn!("Failed to send Matrix notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status);
        Ok(false)
      }
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Matrix notification.");
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        Ok(false)
      }
    }
  }
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

/// One Matrix request (upstream `_fetch()` arguments).
struct Req<'a> {
  method: reqwest::Method,
  path: String,
  payload: Option<Value>,
  params: Vec<(String, String)>,
  /// Raw upload body and its content type; sent to the media `/upload`.
  upload: Option<(&'a [u8], &'a str)>,
  /// Statuses the caller expects, returned without a warning.
  ok_status: &'a [u16],
  timeout: Option<Duration>,
  max_retry_wait: Option<Duration>,
  max_response_bytes: Option<usize>,
  /// The payload holds a password: hide it and refuse redirects.
  credentials: bool,
  /// Send our access token.
  auth: bool,
}

impl<'a> Req<'a> {
  fn new(method: reqwest::Method, path: impl Into<String>) -> Self {
    Self {
      method,
      path: path.into(),
      payload: None,
      params: Vec::new(),
      upload: None,
      ok_status: &[],
      timeout: None,
      max_retry_wait: None,
      max_response_bytes: None,
      credentials: false,
      auth: true,
    }
  }

  fn post(path: impl Into<String>, payload: Value) -> Self {
    Self { payload: Some(payload), ..Self::new(reqwest::Method::POST, path) }
  }

  fn put(path: impl Into<String>, payload: Value) -> Self {
    Self { payload: Some(payload), ..Self::new(reqwest::Method::PUT, path) }
  }

  fn get(path: impl Into<String>) -> Self {
    Self::new(reqwest::Method::GET, path)
  }

  /// A discovery probe: bounded, and without our access token.
  fn probe() -> Self {
    Self { max_response_bytes: Some(MAX_RESPONSE_BYTES), auth: false, ..Self::new(reqwest::Method::GET, "") }
  }
}

/// A normalized reply: success, the JSON body (`{}` when there is none)
/// and the HTTP status (500 when no response arrived).
struct Reply {
  ok: bool,
  body: Value,
  status: u16,
}

impl Reply {
  fn failed(status: u16) -> Self {
    Self { ok: false, body: json!({}), status }
  }

  fn str(&self, key: &str) -> Option<String> {
    self.body.get(key).and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty())
  }

  fn errcode(&self) -> Option<&str> {
    self.body.get("errcode").and_then(Value::as_str)
  }
}

/// One server-mode send: the service, its session and the notification.
struct Run<'a> {
  m: &'a Matrix,
  s: &'a mut Session,
  ctx: &'a NotifyContext,
}

impl Run<'_> {
  /// Send a request to the homeserver (upstream `_fetch()`).
  async fn fetch(&mut self, req: Req<'_>) -> Reply {
    let Ok(base) = self.base_url().await else { return Reply::failed(500) };
    let url = if req.upload.is_some() { format!("{base}{}/upload", self.m.media_path()) } else { format!("{base}{}{}", self.m.api_path(), req.path) };
    let token = if req.auth { self.s.access_token.clone() } else { None };
    self.m.request(&url, &req, token.as_deref()).await
  }

  /// Upstream `base_url`: the discovered homeserver, or the URL's host.
  async fn base_url(&mut self) -> Result<String, ()> {
    match self.server_discovery().await {
      Ok(base) if !base.is_empty() => Ok(base),
      Ok(_) => Ok(self.m.host_base()),
      Err(()) => {
        self.s.clear(&[DISCOVERY_BASE_KEY, DISCOVERY_IDENTITY_KEY]);
        Err(())
      }
    }
  }

  /// Home server discovery through `/.well-known/matrix/client` (upstream
  /// `server_discovery()`). `Ok("")` means "use the URL's host".
  async fn server_discovery(&mut self) -> Result<String, ()> {
    if !(self.m.discovery && self.m.secure) {
      return Ok(String::new());
    }
    let (base, identity) = (self.s.get(DISCOVERY_BASE_KEY), self.s.get(DISCOVERY_IDENTITY_KEY));
    if base.is_some() || identity.is_some() {
      return Ok(base.and_then(|b| b.as_str().map(str::to_string)).unwrap_or_default());
    }
    let verify_url = format!("{}/.well-known/matrix/client", self.m.host_base());
    let reply = self.m.request(&verify_url, &Req::probe(), None).await;
    let none_found = |s: &mut Session| {
      s.set(DISCOVERY_BASE_KEY, json!(""), Some(DISCOVERY_CACHE));
      s.set(DISCOVERY_IDENTITY_KEY, json!(""), Some(DISCOVERY_CACHE));
    };
    if reply.status == 404 {
      tracing::debug!("Matrix Well-Known Base URI not found at {}", verify_url);
      none_found(self.s);
      return Ok(String::new());
    }
    if reply.status != 200 {
      tracing::warn!("Matrix Well-Known Base URI Discovery Failed - {} returned error code: {}", verify_url, reply.status);
      return Err(());
    }
    if reply.body.as_object().is_none_or(|o| o.is_empty()) {
      tracing::debug!("Matrix Well-Known Base URI not defined {}", verify_url);
      none_found(self.s);
      return Ok(String::new());
    }

    // Discovery decides where the access token goes: accept only an HTTPS
    // address without credentials or a query.
    let checked = |v: Option<&Value>| -> Option<String> {
      let url = v?.get("base_url")?.as_str()?.trim_end_matches('/').to_string();
      (crate::utils::http::is_secure_http_url(&url) && !url.contains('?')).then_some(url)
    };
    let Some(base_url) = checked(reply.body.get("m.homeserver")) else {
      tracing::warn!("Matrix Well-Known Base URI Discovery Failed - m.homeserver payload is missing or invalid");
      return Err(());
    };
    let versions = format!("{base_url}/_matrix/client/versions");
    let status = self.m.request(&versions, &Req::probe(), None).await.status;
    if status != 200 {
      tracing::warn!("Matrix Well-Known Base URI Discovery Verification Failed - {} returned error code: {}", versions, status);
      return Err(());
    }

    // The identity server is optional, but must check out when given.
    let identity_expiry = Some(DISCOVERY_CACHE + Duration::from_secs(2));
    if let Some(identity) = reply.body.get("m.identity_server") {
      let Some(identity_url) = checked(Some(identity)) else {
        tracing::warn!("Matrix Well-Known Identity URI Discovery Failed - m.identity_server payload is missing or invalid");
        return Err(());
      };
      let probe = format!("{identity_url}/_matrix/identity/v2");
      let status = self.m.request(&probe, &Req::probe(), None).await.status;
      if status != 200 {
        tracing::warn!("Matrix Well-Known Identity URI Discovery Failed - {} returned error code: {}", probe, status);
        return Err(());
      }
      self.s.set(DISCOVERY_IDENTITY_KEY, json!(identity_url), identity_expiry);
    } else {
      self.s.set(DISCOVERY_IDENTITY_KEY, json!(""), identity_expiry);
    }
    self.s.set(DISCOVERY_BASE_KEY, json!(base_url), Some(DISCOVERY_CACHE));
    Ok(base_url)
  }

  // ── Transactions ──────────────────────────────────────────────────────────

  /// The transaction ID for the next event.
  fn txn(&self) -> String {
    match self.s.txn {
      Txn::Counter(n) => n.to_string(),
      Txn::Fresh => uuid4(),
    }
  }

  /// Move past the transaction ID just used, so the next event is not
  /// taken for a retransmission.
  fn advance_txn(&mut self) {
    if let Txn::Counter(n) = self.s.txn {
      self.s.txn = Txn::Counter(n + 1);
      self.s.set("transaction_id", json!(n + 1), Some(CACHE_EXPIRY));
    }
  }

  // ── Identity ──────────────────────────────────────────────────────────────

  /// Read the state upstream loads in `__init__`.
  fn restore(&mut self) {
    if self.s.restored {
      return;
    }
    self.s.restored = true;
    self.s.home_server = self.s.get_str("home_server");
    self.s.user_id = self.s.get_str("user_id");
    self.s.access_token = self.s.get_str("access_token");
    self.s.device_id = self.s.get_str("device_id");
    if self.s.home_server.is_none() {
      // Older entries may lack home_server; recover it from the user ID.
      self.s.home_server = self.s.home_server_from_user_id();
    }
    let counter = if self.s.access_token.is_some() { self.s.get("transaction_id").and_then(|v| v.as_u64()).unwrap_or(0) } else { 0 };
    self.s.txn = Txn::Counter(counter);
    #[cfg(feature = "matrix-e2ee")]
    if self.m.e2ee {
      self.s.account = self.s.get("e2ee_account").and_then(|v| e2ee::OlmAccount::from_value(&v));
    }
  }

  /// Remember the identity a login, registration or whoami returned.
  fn store_identity(&mut self, with_token: bool) {
    if with_token {
      let token = self.s.access_token.clone();
      self.s.set("access_token", json!(token), Some(CACHE_EXPIRY));
    }
    if let Some(hs) = self.s.home_server.clone() {
      self.s.set("home_server", json!(hs), Some(CACHE_EXPIRY));
    }
    if with_token || self.s.user_id.is_some() {
      let user_id = self.s.user_id.clone();
      self.s.set("user_id", json!(user_id), Some(CACHE_EXPIRY));
    }
    if let Some(device_id) = self.s.device_id.clone() {
      self.s.set("device_id", json!(device_id), Some(CACHE_EXPIRY));
    }
  }

  fn adopt_identity(&mut self, reply: &Reply) {
    self.s.access_token = reply.str("access_token");
    self.s.user_id = reply.str("user_id");
    self.s.device_id = reply.str("device_id");
    // home_server left modern responses; derive it from the user ID.
    if let Some(hs) = reply.str("home_server") {
      self.s.home_server = Some(hs);
    } else if self.s.home_server.is_none() {
      self.s.home_server = self.s.home_server_from_user_id();
    }
  }

  /// Register with the server if it allows it (upstream `_register()`).
  async fn register(&mut self) -> bool {
    let mut payload = json!({"kind": "user", "auth": {"type": "m.login.dummy"}});
    if let Some(user) = &self.m.user {
      payload["username"] = json!(user);
    }
    if let Some(password) = &self.m.password {
      payload["password"] = json!(password);
    }
    // Reuse a known device so Matrix keeps one identity for this notifier.
    match &self.s.device_id {
      Some(device_id) => payload["device_id"] = json!(device_id),
      None => payload["initial_device_display_name"] = json!(self.ctx.asset.app_id),
    }
    let mut req = Req::post("/register", payload);
    req.params = vec![("kind".into(), "user".into())];
    // The payload may hold a password; keep it out of logs.
    req.credentials = self.m.password.is_some();
    let reply = self.fetch(req).await;
    if !(reply.ok && reply.body.is_object()) {
      return false;
    }
    self.adopt_identity(&reply);
    self.store_identity(true);
    if self.s.access_token.is_some() {
      tracing::debug!("Registered successfully with Matrix server.");
      return true;
    }
    false
  }

  /// Log in with the URL's user and password (upstream `_login()`).
  async fn login(&mut self) -> bool {
    if self.s.access_token.is_some() {
      return true;
    }
    let (Some(user), Some(password)) = (&self.m.user, &self.m.password) else {
      tracing::warn!("Failed to login to Matrix server: token or user/pass combo is missing.");
      return false;
    };
    let mut payload = match self.m.version {
      Version::V3 => json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": user}, "password": password}),
      Version::V2 => json!({"type": "m.login.password", "user": user, "password": password}),
    };
    // Reuse our last device to avoid creating a new one on every login.
    match &self.s.device_id {
      Some(device_id) => payload["device_id"] = json!(device_id),
      None => payload["initial_device_display_name"] = json!(self.ctx.asset.app_id),
    }
    let mut req = Req::post("/login", payload);
    req.credentials = true;
    req.max_response_bytes = Some(MAX_RESPONSE_BYTES);
    let reply = self.fetch(req).await;
    if !(reply.ok && reply.body.is_object()) {
      return false;
    }
    self.adopt_identity(&reply);
    if self.s.access_token.is_none() {
      return false;
    }
    tracing::debug!("Authenticated successfully with Matrix server.");
    self.store_identity(true);
    true
  }

  /// Fill in the user and device a raw token belongs to (upstream
  /// `_whoami()`).
  async fn whoami(&mut self) -> bool {
    let reply = self.fetch(Req::get("/account/whoami")).await;
    if !(reply.ok && reply.body.is_object()) {
      return false;
    }
    if let Some(user_id) = reply.str("user_id") {
      self.s.user_id = Some(user_id);
    }
    if let Some(device_id) = reply.str("device_id") {
      self.s.device_id = Some(device_id);
    }
    if self.s.home_server.is_none() {
      self.s.home_server = self.s.home_server_from_user_id();
    }
    self.store_identity(false);
    true
  }

  // ── Rooms ─────────────────────────────────────────────────────────────────

  fn cached_room(&mut self, key: &str) -> Option<String> {
    self.s.get(key).and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
  }

  fn alias_home_server(&self, explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| self.s.home_server.clone()).or_else(|| self.s.home_server_from_user_id())
  }

  /// Join a room (creating an alias that does not exist) and return its
  /// room ID (upstream `_room_join()`).
  async fn room_join(&mut self, room: &str) -> Option<String> {
    self.s.access_token.as_ref()?;
    if let Some((token, explicit)) = parse_room_id(room) {
      let home_server = explicit.clone().or_else(|| self.s.home_server.clone());
      let cache_key = format!("!{token}:{}", home_server.as_deref().unwrap_or("None"));
      // hsreq (the default) always adds the home server to a bare room ID.
      let room_id = if explicit.is_some() || self.m.hsreq { cache_key.clone() } else { format!("!{token}") };
      if let Some(id) = self.cached_room(&cache_key) {
        return Some(id);
      }
      let reply = self.fetch(Req::post(format!("/join/{}", quote(&room_id, "/")), json!({}))).await;
      if !reply.ok {
        return None;
      }
      let joined = reply.str("room_id").unwrap_or(room_id);
      self.s.set(&cache_key, json!({"id": joined, "home_server": home_server}), None);
      return Some(joined);
    }

    let Some((name, explicit)) = parse_room_alias(room) else {
      tracing::warn!("Ignoring illegally formed room {} from Matrix server list.", room);
      return None;
    };
    let Some(home_server) = self.alias_home_server(explicit) else {
      tracing::warn!("Could not resolve a homeserver for Matrix room alias {}.", room);
      return None;
    };
    let alias = format!("#{name}:{home_server}");
    if let Some(id) = self.cached_room(&alias) {
      return Some(id);
    }
    let reply = self.fetch(Req::post(format!("/join/{}", quote(&alias, "/")), json!({}))).await;
    if reply.ok {
      let room_id = reply.str("room_id");
      self.s.set(&alias, json!({"id": room_id, "home_server": home_server}), None);
      return room_id;
    }
    // Only create the room when the alias clearly does not exist; a join
    // also fails for invite-only rooms, bad tokens or permissions.
    if reply.status == 404 || reply.errcode() == Some("M_NOT_FOUND") {
      return self.room_create(&alias).await;
    }
    tracing::warn!(
      "Could not join Matrix room alias {} (error={}). If this is a private room, ensure the user is invited or already joined, or specify the room_id (!...).",
      alias,
      reply.status
    );
    None
  }

  /// The `initial_state` that makes a room Apprise creates encrypted.
  fn encrypted_initial_state(&self) -> Option<Value> {
    self.m.e2ee_capable_config().then(|| json!([{"type": "m.room.encryption", "state_key": "", "content": {"algorithm": MEGOLM_ALGORITHM}}]))
  }

  /// Create a private room for an alias (upstream `_room_create()`).
  async fn room_create(&mut self, room: &str) -> Option<String> {
    self.s.access_token.as_ref()?;
    let (name, explicit) = parse_room_alias(room)?;
    let home_server = self.alias_home_server(explicit)?;
    let alias = format!("#{name}:{home_server}");
    let mut payload = json!({
      "room_alias_name": name,
      "name": format!("#{name} - {}", self.ctx.asset.app_desc),
      // Hidden by default; the user can open it up.
      "visibility": "private",
      "preset": "trusted_private_chat",
    });
    if let Some(state) = self.encrypted_initial_state() {
      payload["initial_state"] = state;
    }
    let reply = self.fetch(Req::post("/createRoom", payload)).await;
    if !reply.ok {
      if reply.errcode() == Some("M_ROOM_IN_USE") {
        return self.room_id(&alias).await;
      }
      return None;
    }
    let room_id = reply.str("room_id");
    if let Some(room_alias) = reply.str("room_alias") {
      self.s.set(&room_alias, json!({"id": room_id, "home_server": home_server}), None);
    }
    if let Some(id) = &room_id
      && self.m.e2ee_capable_config()
    {
      // We just enabled encryption; skip the state lookup.
      self.s.set(&room_encryption_key(id), json!(true), Some(CACHE_EXPIRY));
    }
    room_id
  }

  /// The rooms this user already joined (upstream `_joined_rooms()`).
  async fn joined_rooms(&mut self) -> Vec<String> {
    if self.s.access_token.is_none() {
      return Vec::new();
    }
    let reply = self.fetch(Req::get("/joined_rooms")).await;
    if !reply.ok {
      return Vec::new();
    }
    reply.body.get("joined_rooms").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
  }

  /// Resolve an alias to its room ID (upstream `_room_id()`).
  async fn room_id(&mut self, room: &str) -> Option<String> {
    self.s.access_token.as_ref()?;
    let (name, explicit) = parse_room_alias(room)?;
    let home_server = self.alias_home_server(explicit)?;
    let alias = format!("#{name}:{home_server}");
    let reply = self.fetch(Req::get(format!("/directory/room/{}", quote(&alias, "/")))).await;
    if reply.ok { reply.str("room_id") } else { None }
  }

  /// Whether the server says `room_id` is encrypted, or `None` when that is
  /// unknown. Only a 404 confirms a plaintext room, and only a confirmed
  /// answer is cached.
  async fn room_encrypted(&mut self, room_id: &str) -> Option<bool> {
    let key = room_encryption_key(room_id);
    if let Some(Value::Bool(cached)) = self.s.get(&key) {
      return Some(cached);
    }
    let mut req = Req::get(format!("/rooms/{}/state/m.room.encryption", quote(room_id, "/")));
    // No encryption state event means a plaintext room; no warning.
    req.ok_status = &[404];
    let reply = self.fetch(req).await;
    let encrypted = if reply.status == 404 {
      false
    } else if reply.ok && reply.body.as_object().is_some_and(|o| !o.is_empty()) {
      true
    } else {
      return None;
    };
    self.s.set(&key, json!(encrypted), Some(CACHE_EXPIRY));
    Some(encrypted)
  }

  /// Find or create the direct-message room for `@user` (upstream
  /// `_dm_room_find_or_create()`).
  async fn dm_room_find_or_create(&mut self, user: &str) -> Option<String> {
    let Some((name, explicit)) = parse_user(user) else {
      tracing::warn!("Matrix DM: invalid user identifier {}.", user);
      return None;
    };
    let home_server = explicit.or_else(|| self.s.home_server.clone()).unwrap_or_else(|| "None".to_string());
    let user_id = format!("@{name}:{home_server}");
    let cache_key = format!("dm_room_{user_id}");
    if let Some(room_id) = self.s.get_str(&cache_key) {
      return Some(room_id);
    }

    // An existing DM room is listed in our m.direct account data.
    let mut mdirect = json!({});
    if let Some(own) = self.s.user_id.clone() {
      let reply = self.fetch(Req::get(format!("/user/{}/account_data/m.direct", quote(&own, "/")))).await;
      if reply.ok && reply.body.is_object() {
        mdirect = reply.body;
        if let Some(room_id) = mdirect.get(&user_id).and_then(Value::as_array).and_then(|r| r.first()).and_then(Value::as_str).map(str::to_string) {
          self.s.set(&cache_key, json!(room_id), Some(CACHE_EXPIRY));
          return Some(room_id);
        }
      }
    }

    let mut payload = json!({"is_direct": true, "preset": "trusted_private_chat", "invite": [user_id]});
    if let Some(state) = self.encrypted_initial_state() {
      payload["initial_state"] = state;
    }
    let reply = self.fetch(Req::post("/createRoom", payload)).await;
    if !(reply.ok && reply.body.is_object()) {
      tracing::warn!("Matrix DM: failed to create room for {}.", user_id);
      return None;
    }
    let room_id = reply.str("room_id")?;
    self.s.set(&cache_key, json!(room_id), Some(CACHE_EXPIRY));
    if self.m.e2ee_capable_config() {
      self.s.set(&room_encryption_key(&room_id), json!(true), Some(CACHE_EXPIRY));
    }

    // Record the room in m.direct so other clients show it as a DM.
    if let Some(own) = self.s.user_id.clone() {
      let mut rooms = mdirect.get(&user_id).and_then(Value::as_array).cloned().unwrap_or_default();
      rooms.push(json!(room_id));
      mdirect[user_id.as_str()] = Value::Array(rooms);
      self.fetch(Req::put(format!("/user/{}/account_data/m.direct", quote(&own, "/")), mdirect)).await;
    }
    Some(room_id)
  }

  // ── Content ───────────────────────────────────────────────────────────────

  /// The `m.room.message` content of the notification: a plain body, plus
  /// a formatted body for HTML and rendered Markdown.
  fn message_content(&self) -> Value {
    let ctx = self.ctx;
    let rich = is_rich(&ctx.body_format);
    // Clients that cannot show HTML fall back to this text.
    let plain = if rich { html_to_text(&ctx.body) } else { ctx.body.clone() };
    let title = if ctx.title.is_empty() { String::new() } else { format!("# {}\r\n", ctx.title) };
    let mut content = json!({
      "msgtype": match self.m.msgtype { MsgType::Text => "m.text", MsgType::Notice => "m.notice" },
      "body": format!("{title}{plain}"),
    });
    if rich {
      let mut title_html = if ctx.title.is_empty() { String::new() } else { format!("<h1>{}</h1>", escape_html(&ctx.title, false, false)) };
      // Rendered Markdown already starts on its own line.
      if !title_html.is_empty() && ctx.body_format != NotifyFormat::Markdown {
        title_html.push_str("<br/>");
      }
      content["format"] = json!("org.matrix.custom.html");
      content["formatted_body"] = json!(format!("{title_html}{}", ctx.body));
    }
    content
  }

  /// Upload the attachments for unencrypted rooms and build the events
  /// that reference them (upstream `_send_attachments()`). `None` when an
  /// upload failed.
  async fn upload_attachments(&mut self, attachments: &[Attachment]) -> Option<Vec<Value>> {
    let mut payloads = Vec::new();
    for attachment in attachments {
      let is_image = attachment.mime_type.to_lowercase().starts_with("image/");
      if !is_image && self.m.version == Version::V2 {
        // Only images can be posted through the v2 API.
        continue;
      }
      let mut req = Req::new(reqwest::Method::POST, "/upload");
      req.upload = Some((&attachment.data, &attachment.mime_type));
      req.params = vec![("filename".into(), attachment.name.clone())];
      let reply = self.fetch(req).await;
      if !(reply.ok && reply.body.is_object()) {
        return None;
      }
      let content_uri = reply.body.get("content_uri").cloned().unwrap_or(Value::Null);
      payloads.push(match self.m.version {
        Version::V3 => {
          let mut payload = json!({
            "body": attachment.name,
            "info": {"mimetype": attachment.mime_type, "size": attachment.data.len()},
            "msgtype": if is_image { "m.image" } else { "m.file" },
            "url": content_uri,
          });
          if !is_image {
            payload["filename"] = json!(attachment.name);
          }
          payload
        }
        Version::V2 => json!({"info": {"mimetype": attachment.mime_type}, "msgtype": "m.image", "body": "tta.webp", "url": content_uri}),
      });
    }
    Some(payloads)
  }

  fn message_path(&self, room_id: &str, event_type: &str) -> String {
    format!("/rooms/{}/send/{event_type}/{}", quote(room_id, "/"), self.txn())
  }

  /// Deliver to every target (upstream `_send_server_notification()`).
  async fn send(&mut self) -> bool {
    self.restore();
    if self.m.token_auth() {
      if self.s.access_token.is_none() {
        self.s.access_token = self.m.password.clone();
      }
      self.s.txn = Txn::Fresh;
    }
    if self.s.access_token.is_none() && !self.login().await && !self.register().await {
      return false;
    }
    // A raw token or incomplete login leaves the user ID unknown; without
    // it a DM lookup could create duplicate rooms.
    if self.s.user_id.is_none() {
      self.whoami().await;
    }
    if self.s.home_server.is_none() {
      self.s.home_server = Some(self.m.host.clone());
    }

    let mut rooms: Vec<String> = self.m.rooms.clone();
    if rooms.is_empty() && self.m.users.is_empty() {
      if self.s.joined_rooms.is_empty() {
        self.s.joined_rooms = self.joined_rooms().await;
      }
      if self.s.joined_rooms.is_empty() {
        tracing::warn!("There were no Matrix rooms specified to notify.");
        return false;
      }
      rooms = self.s.joined_rooms.clone();
    }
    // Pair each room with the target it was configured as.
    let mut targets: Vec<(String, String)> = rooms.into_iter().map(|r| (format!("room:{r}"), r)).collect();
    let mut has_error = false;

    let m = self.m;
    for user in &m.users {
      let key = format!("user:{user}");
      if is_delivered(&key, false) {
        continue;
      }
      match self.dm_room_find_or_create(user).await {
        Some(room_id) => targets.push((key, room_id)),
        None => {
          tracing::warn!("Could not find or create a DM room for Matrix user {}.", user);
          has_error = true;
        }
      }
    }

    // Encryption is set up once per send; each room still decides whether
    // it needs it.
    let wants_e2ee = self.m.e2ee && self.m.secure;
    #[cfg(feature = "matrix-e2ee")]
    let e2ee_capable = if wants_e2ee {
      let ok = self.e2ee_setup().await;
      if !ok {
        tracing::warn!("Matrix E2EE setup failed; messages will be sent unencrypted.");
      }
      ok
    } else {
      false
    };
    #[cfg(not(feature = "matrix-e2ee"))]
    let e2ee_capable = false;

    // Verification is best-effort and never decides delivery.
    #[cfg(feature = "matrix-e2ee")]
    if e2ee_capable && self.m.autoverify && !self.auto_verify().await {
      tracing::warn!("Matrix E2EE automatic device verification did not complete; continuing without confirming device trust.");
    }

    // Uploads are shared by every unencrypted room, so a purely encrypted
    // send never uploads plaintext.
    let ctx = self.ctx;
    let mut uploaded: Option<Vec<Value>> = None;

    for (delivery_key, room) in targets {
      if is_delivered(&delivery_key, false) {
        continue;
      }
      let mut target_error = false;
      let message_key = format!("message:{delivery_key}");

      let Some(room_id) = self.room_join(&room).await else {
        tracing::warn!("Could not join Matrix room {}.", room);
        has_error = true;
        continue;
      };

      // Settle the room's encryption before sending it anything. Without
      // encryption support an encrypted room fails closed.
      let check = e2ee_capable || (wants_e2ee && !E2EE_SUPPORT);
      let encrypted = if check {
        match self.room_encrypted(&room_id).await {
          Some(encrypted) => encrypted,
          None => {
            tracing::warn!("Could not determine whether Matrix room {} is encrypted; nothing was sent to it.", room);
            has_error = true;
            continue;
          }
        }
      } else {
        false
      };

      if encrypted {
        #[cfg(feature = "matrix-e2ee")]
        {
          if !is_delivered(&message_key, false) {
            if !self.e2ee_send_to_room(&room_id).await {
              has_error = true;
              continue;
            }
            mark_delivered(&message_key, false);
          }
          for (number, attachment) in ctx.attachments.iter().enumerate() {
            let key = format!("attachment:{delivery_key}:{}", number + 1);
            if is_delivered(&key, false) {
              continue;
            }
            if !self.e2ee_send_attachment(attachment, &room_id).await {
              has_error = true;
              target_error = true;
              continue;
            }
            mark_delivered(&key, false);
          }
          if !target_error {
            mark_delivered(&delivery_key, false);
          }
        }
        #[cfg(not(feature = "matrix-e2ee"))]
        {
          tracing::warn!(
            "Matrix room {} is end-to-end encrypted, but this build has no encryption support (the matrix-e2ee feature); nothing was sent to it.",
            room
          );
          has_error = true;
        }
        continue;
      }

      // ── Unencrypted room ──
      if !ctx.attachments.is_empty() && uploaded.is_none() {
        // A retry reuses what an earlier attempt uploaded.
        let earlier = recall("uploaded", true).and_then(|v| v.as_array().cloned());
        let payloads = match earlier {
          Some(p) => p,
          None => match self.upload_attachments(&ctx.attachments).await {
            Some(p) => {
              remember("uploaded", json!(p), true);
              p
            }
            None => return false,
          },
        };
        uploaded = Some(payloads);
      }

      if self.m.version == Version::V2
        && self.m.include_image
        && let Some(image_url) = self.ctx.image_url(NotifyImageSize::Xy32)
      {
        // The icon belongs to the whole notification: posted once.
        let image_key = format!("image:{delivery_key}");
        if !is_delivered(&image_key, true) {
          let body = if self.ctx.title.is_empty() { self.ctx.notify_type.as_str().to_string() } else { self.ctx.title.clone() };
          let payload = json!({"msgtype": "m.image", "url": image_url, "body": body});
          let path = self.message_path(&room_id, "m.room.message");
          if !self.fetch(Req::put(path, payload)).await.ok {
            has_error = true;
            continue;
          }
          mark_delivered(&image_key, true);
          self.advance_txn();
        }
      }

      for (number, attachment) in uploaded.clone().unwrap_or_default().into_iter().enumerate() {
        let key = format!("attachment:{delivery_key}:{}", number + 1);
        if is_delivered(&key, false) {
          continue;
        }
        let mut event = attachment;
        event["room_id"] = json!(room_id);
        event["type"] = json!("m.room.message");
        let path = self.message_path(&room_id, "m.room.message");
        let ok = self.fetch(Req::put(path, event)).await.ok;
        self.advance_txn();
        if !ok {
          has_error = true;
          target_error = true;
          continue;
        }
        mark_delivered(&key, false);
      }

      if !is_delivered(&message_key, false) {
        let path = self.message_path(&room_id, "m.room.message");
        let ok = self.fetch(Req::put(path, self.message_content())).await.ok;
        self.advance_txn();
        if !ok {
          tracing::warn!("Could not send notification Matrix room {}.", room);
          has_error = true;
          continue;
        }
        mark_delivered(&message_key, false);
      }
      if !target_error {
        mark_delivered(&delivery_key, false);
      }
    }

    #[cfg(feature = "matrix-e2ee")]
    if !has_error && e2ee_capable && self.m.autoverify && !self.refresh_verified_state() {
      tracing::warn!("Matrix E2EE verified device state could not be refreshed.");
    }
    !has_error
  }
}

/// Cache key of a room's confirmed encryption state. A new name, so entries
/// upstream cached from failed lookups are not trusted.
fn room_encryption_key(room_id: &str) -> String {
  format!("e2ee_room_enc_v2_{room_id}")
}

impl Matrix {
  /// Send one request and normalize its answer (the HTTP half of upstream
  /// `_fetch()`): JSON in, `(ok, body, status)` out, 429s retried within the
  /// shared wait budget.
  async fn request(&self, url: &str, req: &Req<'_>, token: Option<&str>) -> Reply {
    let mut builder = client_builder(self.verify_certificate, ClientDefaults::default());
    if req.credentials {
      // Never let a redirect carry the password elsewhere.
      builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    if let Some(timeout) = req.timeout {
      builder = builder.connect_timeout(timeout).read_timeout(timeout);
    }
    let client = match builder.build() {
      Ok(c) => c,
      Err(e) => {
        tracing::warn!("A Connection error occurred while registering with Matrix server.");
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return Reply::failed(500);
      }
    };

    throttle().await;
    let mut status = 500;
    let deadline = req.max_retry_wait.map(|w| Instant::now() + w);
    let mut retries = DEFAULT_RETRIES.max(1);
    while retries > 0 {
      retries -= 1;
      tracing::debug!("Matrix {} URL: {} (cert_verify={})", req.method, crate::utils::cwe312::cwe312_loggable(url, true), self.verify_certificate);
      let mut builder = client.request(req.method.clone(), url).header("User-Agent", APP_ID).header("Accept", "application/json");
      if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
      }
      if !req.params.is_empty() {
        builder = builder.query(&req.params);
      }
      builder = match req.upload {
        Some((data, mime)) => builder.header("Content-Type", mime).body(data.to_vec()),
        None => {
          // A request without a payload still sends JSON `null`, as upstream.
          let payload = req.payload.as_ref().map(|p| p.to_string()).unwrap_or_else(|| "null".into());
          if !req.credentials {
            tracing::debug!("Matrix Payload: {}", payload);
          }
          builder.header("Content-Type", "application/json").body(payload)
        }
      };

      let response = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred while registering with Matrix server.");
          tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
          return Reply::failed(status);
        }
      };
      status = response.status().as_u16();
      let content = match req.max_response_bytes {
        Some(max) => match crate::utils::http::read_bounded_response(response, max).await {
          Ok(Some(c)) => c,
          Ok(None) => {
            tracing::warn!("Matrix response exceeded {} bytes; discarding it.", max);
            return Reply::failed(status);
          }
          Err(e) => {
            tracing::warn!("A Connection error occurred while registering with Matrix server.");
            tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
            return Reply::failed(status);
          }
        },
        None => match response.bytes().await {
          Ok(c) => c.to_vec(),
          Err(e) => {
            tracing::warn!("A Connection error occurred while registering with Matrix server.");
            tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
            return Reply::failed(status);
          }
        },
      };
      let loggable = || if req.credentials { "<hidden>".to_string() } else { String::from_utf8_lossy(&content[..content.len().min(2000)]).into_owned() };
      tracing::debug!("Matrix Response: code={}, {}", status, loggable());
      let Ok(body) = serde_json::from_slice::<Value>(&content) else {
        tracing::warn!("Invalid response from Matrix server.");
        tracing::debug!("Response Details:\r\n{}", loggable());
        return Reply::failed(status);
      };

      if status == 429 {
        let retry_after = body.get("retry_after_ms").or_else(|| body.get("error").and_then(|e| e.get("retry_after_ms")));
        let mut wait_ms = retry_after.and_then(Value::as_f64).unwrap_or(DEFAULT_WAIT_MS).max(0.0);
        if let Some(deadline) = deadline {
          // Every 429 of this request draws from one wait budget.
          let remaining = deadline.saturating_duration_since(Instant::now());
          if remaining.is_zero() {
            tracing::warn!("Matrix server requested we throttle back, but our retry budget is exhausted; giving up.");
            return Reply { ok: false, body, status };
          }
          wait_ms = wait_ms.min(remaining.as_secs_f64() * 1000.0);
        }
        tracing::warn!("Matrix server requested we throttle back {}ms; retries left {}.", wait_ms, retries);
        throttle_for(Some(Duration::from_secs_f64(wait_ms / 1000.0))).await;
        if deadline.is_some_and(|d| Instant::now() >= d) {
          return Reply { ok: false, body, status };
        }
        continue;
      }
      if !matches!(status, 200 | 202) {
        if req.ok_status.contains(&status) {
          return Reply { ok: false, body, status };
        }
        let reason = status_message(status);
        tracing::warn!("Failed to handshake with Matrix server: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status);
        tracing::debug!("Response Details:\r\n{}", loggable());
        return Reply { ok: false, body, status };
      }
      return Reply { ok: true, body, status };
    }
    Reply::failed(status)
  }
}

#[async_trait]
impl Notify for Matrix {
  fn schemas(&self) -> &[&str] {
    &["matrix", "matrixs"]
  }
  fn service_name(&self) -> &str {
    "Matrix"
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
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Text
  }
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![NotifyFormat::Text, NotifyFormat::Html, NotifyFormat::Markdown]
  }
  fn title_maxlen(&self) -> usize {
    TITLE_MAXLEN
  }
  fn body_maxlen(&self) -> usize {
    if self.mode == Mode::Off && self.e2ee_capable_config() { BODY_MAXLEN_E2EE } else { BODY_MAXLEN_DEFAULT }
  }
  fn body_maxlen_for(&self, title: &str, body: &str, body_format: &NotifyFormat) -> usize {
    if self.mode != Mode::Off {
      // Webhook payloads keep their flat limit.
      return BODY_MAXLEN_DEFAULT;
    }
    let title: String = crate::conversion::pystr::strip(title).chars().take(TITLE_MAXLEN).collect();
    let body = crate::conversion::pystr::rstrip(body);
    // The device ID is only known once a send has logged in.
    let device_id = self.session.try_lock().ok().and_then(|s| s.device_id.clone());
    sizing::body_char_limit(body, &title, body_format, self.e2ee_capable_config(), device_id.as_deref())
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.5
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    if self.mode == Mode::Slack {
      // Slack-style text is escaped before sending, so HTML would show up
      // literally; send it as plain text instead.
      return if *body_format == NotifyFormat::Html { html_to_text(body) } else { body.to_string() };
    }
    if *body_format != NotifyFormat::Markdown {
      return body.to_string();
    }
    // Matrix keeps plain Python-Markdown line handling: no extensions.
    markdown_to_html_with(body, &[])
  }
  fn has_dialect(&self) -> bool {
    true
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    let none = || "None".to_string();
    let t2bot = self.mode == Mode::T2Bot;
    Some(vec![
      if self.secure { "matrixs" } else { "matrix" }.to_string(),
      self.mode.as_str().to_string(),
      if t2bot { self.t2bot_token.clone().unwrap_or_else(none) } else { self.host.clone() },
      self.port.unwrap_or(if self.secure { 443 } else { 80 }).to_string(),
      if self.mode == Mode::Hookshot { self.webhook_path.clone() } else { none() },
      if t2bot { none() } else { self.user.clone().unwrap_or_else(none) },
      if t2bot { none() } else { self.password.clone().unwrap_or_else(none) },
    ])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.mode != Mode::Off {
      return self.send_webhook(ctx).await;
    }
    let mut session = self.session.lock().await;
    Ok(Run { m: self, s: &mut session, ctx }.send().await)
  }
}
