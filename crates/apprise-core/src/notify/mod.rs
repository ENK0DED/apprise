use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::asset::AppriseAsset;
use crate::error::NotifyError;
use crate::types::{NotifyFormat, NotifyType};

pub mod options;
pub mod registry;

pub use options::{ConfiguredService, ServiceOptions};

// ── Plugin modules ────────────────────────────────────────────────────────────
pub mod africas_talking;
pub mod apprise_api;
pub mod aprs;
pub mod bark;
pub mod bluesky;
pub mod brevo;
pub mod bulksms;
pub mod bulkvs;
pub mod burstsms;
pub mod chanify;
pub mod clickatell;
pub mod clicksend;
pub mod custom_form;
pub mod custom_json;
pub mod custom_xml;
pub mod d7networks;
pub mod dapnet;
pub mod dingtalk;
pub mod discord;
pub mod dot;
#[cfg(feature = "email")]
pub mod email;
pub mod emby;
pub mod enigma2;
pub mod fcm;
pub mod feishu;
pub mod flock;
pub mod fluxer;
pub mod fortysixelks;
pub mod freemobile;
pub mod google_chat;
pub mod gotify;
pub mod growl;
pub mod guilded;
pub mod home_assistant;
pub mod httpsms;
pub mod ifttt;
pub mod irc;
pub mod jellyfin;
pub mod join;
pub mod kavenegar;
pub mod kumulos;
pub mod lametric;
pub mod lark;
pub mod line;
pub mod mailgun;
pub mod mastodon;
pub mod matrix;
pub mod mattermost;
pub mod messagebird;
pub mod misskey;
#[cfg(feature = "mqtt")]
pub mod mqtt;
pub mod msg91;
pub mod msteams;
pub mod nextcloud;
pub mod nextcloudtalk;
pub mod notica;
pub mod notifiarr;
pub mod notificationapi;
pub mod notifico;
pub mod ntfy;
pub mod office365;
pub mod one_signal;
pub mod opsgenie;
pub mod pagerduty;
pub mod pagertree;
pub mod parseplatform;
pub mod plivo;
pub mod popcorn_notify;
pub mod prowl;
pub mod pushbullet;
pub mod pushdeer;
pub mod pushed;
pub mod pushjet;
pub mod pushme;
pub mod pushover;
pub mod pushplus;
pub mod pushsafer;
pub mod pushy;
pub mod qq;
pub mod reddit;
pub mod resend;
pub mod revolt;
pub mod rocketchat;
pub mod rsyslog;
pub mod ryver;
pub mod sendgrid;
pub mod sendpulse;
pub mod serverchan;
pub mod ses;
pub mod seven;
pub mod sfr;
pub mod signal_api;
pub mod signl4;
pub mod simplepush;
pub mod sinch;
pub mod slack;
pub mod smpp;
pub mod smseagle;
pub mod smsmanager;
pub mod smtp2go;
pub mod sns;
pub mod sparkpost;
pub mod spike;
pub mod splunk;
pub mod spugpush;
pub mod streamlabs;
pub mod synology;
pub mod syslog;
pub mod techuluspush;
pub mod telegram;
pub mod threema;
pub mod twilio;
pub mod twist;
pub mod twitter;
pub mod vapid;
pub mod viber;
pub mod voipms;
pub mod vonage;
pub mod webexteams;
pub mod wecombot;
pub mod whatsapp;
pub mod workflows;
pub mod wxpusher;
pub mod xbmc;
pub mod xmpp;
pub mod zulip;

// Platform-specific plugins
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub mod desktop;

// ── Overflow mode ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowMode {
  Upstream, // send as-is, let service handle
  Truncate, // hard truncate to body_maxlen
  Split,    // split into multiple messages
}

impl OverflowMode {
  pub fn as_str(&self) -> &'static str {
    match self {
      Self::Upstream => "upstream",
      Self::Truncate => "truncate",
      Self::Split => "split",
    }
  }
}

impl std::str::FromStr for OverflowMode {
  type Err = String;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s.trim().to_lowercase().as_str() {
      "upstream" => Ok(Self::Upstream),
      "truncate" => Ok(Self::Truncate),
      "split" => Ok(Self::Split),
      other => Err(format!("Unknown overflow mode: {}", other)),
    }
  }
}

// ── Core types ────────────────────────────────────────────────────────────────

/// An attachment (file path or URL)
#[derive(Debug, Clone)]
pub struct Attachment {
  pub name: String,
  pub data: Vec<u8>,
  pub mime_type: String,
}

/// Context passed to every notification send
#[derive(Debug, Clone)]
pub struct NotifyContext {
  pub body: String,
  pub title: String,
  pub notify_type: NotifyType,
  pub body_format: NotifyFormat,
  pub attachments: Vec<Attachment>,
  pub interpret_escapes: bool,
  pub interpret_emojis: bool,
  pub tags: Vec<String>,
  pub asset: AppriseAsset,
}

impl Default for NotifyContext {
  fn default() -> Self {
    Self {
      body: String::new(),
      title: String::new(),
      notify_type: NotifyType::Info,
      body_format: NotifyFormat::Text,
      attachments: Vec::new(),
      interpret_escapes: false,
      interpret_emojis: false,
      tags: Vec::new(),
      asset: AppriseAsset::default(),
    }
  }
}

/// Information about a notification service for --details / --schema output
#[derive(Debug, Clone)]
pub struct ServiceDetails {
  pub service_name: &'static str,
  pub service_url: Option<&'static str>,
  pub setup_url: Option<&'static str>,
  pub protocols: Vec<&'static str>,
  pub description: &'static str,
  pub attachment_support: bool,
}

impl ServiceDetails {
  pub fn to_json(&self) -> Value {
    json!({
        "service_name": self.service_name,
        "service_url": self.service_url,
        "setup_url": self.setup_url,
        "protocols": self.protocols,
        "description": self.description,
        "attachment_support": self.attachment_support,
    })
  }
}

// ── Notify trait ─────────────────────────────────────────────────────────────

/// A notification service plugin.
///
/// Every method added to this trait must also be forwarded in
/// `ConfiguredService` (`notify/options.rs`). `registry::from_url` wraps
/// every plugin in that type, so an unforwarded method silently falls back
/// to the trait default instead of the plugin's own implementation.
#[async_trait]
pub trait Notify: Send + Sync {
  /// URL schemes handled by this plugin (e.g., ["discord"])
  fn schemas(&self) -> &[&str];

  /// Human-readable service name
  fn service_name(&self) -> &str;

  /// Service details for --details / --schema output
  fn details(&self) -> ServiceDetails;

  /// Send a notification. Returns Ok(true) on success, Ok(false) on partial
  /// failure, Err on hard error.
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError>;

  /// Whether this plugin supports attachments
  fn attachment_support(&self) -> bool {
    false
  }

  /// Tags associated with this notification target
  fn tags(&self) -> Vec<String> {
    vec![]
  }

  /// The notification format this plugin expects (default: Text).
  /// The orchestrator converts the body from the user's input format
  /// to this format before calling send().
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Text
  }

  /// Maximum body length in characters (default: 32768, matching Python)
  fn body_maxlen(&self) -> usize {
    32768
  }

  /// Maximum title length in characters (default: 250, matching Python)
  /// Return 0 if the service doesn't support titles.
  fn title_maxlen(&self) -> usize {
    250
  }

  /// Minimum number of SECONDS between two consecutive requests from this
  /// service instance (upstream's misnamed `request_rate_per_sec`; default
  /// 5.5 as in upstream `NotifyBase`, `0` disables throttling).
  ///
  /// Plugins enforce it by calling [`throttle`] before each request. The
  /// last-request time lives with the configured service, so it carries over
  /// between pieces, retries and notifications.
  fn request_rate_per_sec(&self) -> f64 {
    DEFAULT_REQUEST_INTERVAL
  }

  /// How to handle messages that exceed body_maxlen (default: Upstream).
  fn overflow_mode(&self) -> OverflowMode {
    OverflowMode::Upstream
  }

  /// Maximum number of lines in the body (default: 0 = unlimited).
  /// When > 0, truncate to N lines BEFORE overflow handling.
  fn body_max_line_count(&self) -> usize {
    0
  }

  /// Convert one prepared body chunk into the service's own markup dialect
  /// (upstream `NotifyBase.dialect_convert`). Called after the generic format
  /// conversion. The orchestrator re-applies size limits to the result.
  /// Keep implementations stateless and return unsupported formats unchanged.
  fn dialect_convert(&self, body: &str, _body_format: &NotifyFormat) -> String {
    body.to_string()
  }

  /// Common URL options parsed for this instance (`?retry=`, `?format=`, ...).
  /// Only `ConfiguredService` returns `Some`.
  fn service_options(&self) -> Option<&ServiceOptions> {
    None
  }

  /// The URL this service was loaded from, exactly as given. It may hold
  /// secrets: mask it (`utils::cwe312::cwe312_loggable`) before showing it.
  /// Only `ConfiguredService` returns `Some`.
  fn source_url(&self) -> Option<&str> {
    None
  }

  /// When this instance last made a request, for [`throttle`]. Only
  /// `ConfiguredService` returns `Some`.
  fn throttle_clock(&self) -> Option<ThrottleClock> {
    None
  }

  /// The values that make this service distinct from a similar one, for
  /// its persistent storage namespace (upstream `url_identifier`). Leave
  /// targets out. `None` uses the URL's schema, credentials, host, port and
  /// path.
  fn url_identifier(&self) -> Option<Vec<String>> {
    None
  }
}

// ── Helper: throttle requests ────────────────────────────────────────────────

/// Default seconds between requests of one service (upstream
/// `NotifyBase.request_rate_per_sec`).
pub const DEFAULT_REQUEST_INTERVAL: f64 = 5.5;

/// When a service instance last made (or reserved) a request (upstream
/// `URLBase._last_io_datetime`). Clones share the same clock.
#[derive(Debug, Clone, Default)]
pub struct ThrottleClock(std::sync::Arc<std::sync::Mutex<Option<tokio::time::Instant>>>);

#[derive(Debug, Clone)]
struct Throttle {
  interval: f64,
  clock: ThrottleClock,
}

tokio::task_local! {
  static THROTTLE: Throttle;
}

#[cfg(test)]
tokio::task_local! {
  static THROTTLE_IN_TESTS: ();
}

/// Run `fut` with [`throttle`] using `interval` and `clock`. The
/// orchestrator does this around every `send()`.
pub(crate) async fn with_throttle<F: std::future::Future>(interval: f64, clock: ThrottleClock, fut: F) -> F::Output {
  THROTTLE.scope(Throttle { interval, clock }, fut).await
}

/// Let [`throttle`] sleep inside `fut`. Unit tests skip throttling unless
/// they opt in with this, as upstream's test fixtures zero the rate.
#[cfg(test)]
pub(crate) async fn with_throttling_in_tests<F: std::future::Future>(fut: F) -> F::Output {
  THROTTLE_IN_TESTS.scope((), fut).await
}

/// Wait until this service may make its next request (upstream
/// `URLBase.throttle()`). Call it before every request a plugin makes.
///
/// The first request of a service instance never waits. Later ones wait
/// until [`Notify::request_rate_per_sec`] seconds have passed since the
/// previous one. Outside a dispatch (a plugin called directly) this does
/// nothing.
pub async fn throttle() {
  throttle_for(None).await
}

/// [`throttle`], but sleep exactly `wait` instead of the computed interval
/// when one is given (upstream `throttle(wait=...)`, used to honour a
/// service's rate-limit reset time). The first request still never waits.
pub async fn throttle_for(wait: Option<std::time::Duration>) {
  #[cfg(test)]
  if THROTTLE_IN_TESTS.try_with(|_| ()).is_err() {
    return;
  }
  let Ok(state) = THROTTLE.try_with(Throttle::clone) else { return };
  let sleep = {
    let mut last = state.clock.0.lock().unwrap_or_else(|e| e.into_inner());
    let now = tokio::time::Instant::now();
    let Some(previous) = *last else {
      *last = Some(now);
      return;
    };
    let interval = if state.interval.is_finite() && state.interval > 0.0 { state.interval } else { 0.0 };
    if interval <= 0.0 && wait.is_none_or(|w| w.is_zero()) {
      return;
    }
    let sleep = match wait {
      Some(wait) => wait,
      None => (previous + std::time::Duration::from_secs_f64(interval)).saturating_duration_since(now),
    };
    // Reserve the slot so a concurrent send of the same service queues
    // behind this one.
    *last = Some(now + sleep);
    sleep
  };
  if !sleep.is_zero() {
    tracing::debug!("Throttling for {:.3}s...", sleep.as_secs_f64());
    tokio::time::sleep(sleep).await;
  }
}

// ── Helper: build a reqwest client ───────────────────────────────────────────

/// Socket connect timeout used when a URL sets no `?cto=`, in seconds
/// (upstream `URLBase.socket_connect_timeout`).
pub const DEFAULT_CONNECT_TIMEOUT: f64 = 4.0;

/// Socket read timeout used when a URL sets no `?rto=`, in seconds
/// (upstream `URLBase.socket_read_timeout`).
pub const DEFAULT_READ_TIMEOUT: f64 = 4.0;

/// Redirects followed before giving up (the `requests` default).
const MAX_REDIRECTS: usize = 30;

/// The HTTP settings of the service currently being sent, taken from its
/// URL (`?cto=`, `?rto=`, `?redirect=`, `?verify=`) and the asset. The
/// orchestrator sets them around every `send()`, so [`build_client`] picks
/// them up without the plugin passing them along.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HttpOptions {
  /// Connect timeout in seconds; `None` uses the plugin or global default.
  pub connect_timeout: Option<f64>,
  /// Read timeout in seconds; `None` uses the plugin or global default.
  pub read_timeout: Option<f64>,
  /// Follow 3xx redirects; `None` follows them.
  pub redirect: Option<bool>,
  /// Verify TLS certificates; `None` leaves it to the plugin.
  pub verify: Option<bool>,
}

tokio::task_local! {
  static HTTP_OPTIONS: HttpOptions;
}

/// Run `fut` with [`build_client`] honouring `options`.
pub async fn with_http_options<F: std::future::Future>(options: HttpOptions, fut: F) -> F::Output {
  HTTP_OPTIONS.scope(options, fut).await
}

/// The HTTP settings of the service being sent, if the orchestrator set any.
pub fn current_http_options() -> HttpOptions {
  HTTP_OPTIONS.try_with(|o| o.clone()).unwrap_or_default()
}

/// Defaults a plugin can give [`build_client_with`] when upstream's class
/// uses other timeouts than the global ones (for example Apprise API reads
/// for 30 s). A URL's `?cto=` and `?rto=` still win.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClientDefaults {
  pub connect_timeout: f64,
  pub read_timeout: f64,
}

impl Default for ClientDefaults {
  fn default() -> Self {
    Self { connect_timeout: DEFAULT_CONNECT_TIMEOUT, read_timeout: DEFAULT_READ_TIMEOUT }
  }
}

/// Build an HTTP client for the service being sent.
///
/// Timeouts, redirect handling and certificate checks follow the service's
/// URL options (`?cto=`, `?rto=`, `?redirect=`, `?verify=`), defaulting to
/// upstream's 4 s connect and 4 s read timeouts. `verify_cert` is the
/// plugin's own choice; either it or `?verify=no` turns verification off.
pub fn build_client(verify_cert: bool) -> Result<reqwest::Client, NotifyError> {
  build_client_with(verify_cert, ClientDefaults::default())
}

/// [`build_client`] with plugin-specific default timeouts.
pub fn build_client_with(verify_cert: bool, defaults: ClientDefaults) -> Result<reqwest::Client, NotifyError> {
  client_builder(verify_cert, defaults).build().map_err(NotifyError::Http)
}

/// The builder behind [`build_client_with`], for plugins that need to add
/// settings of their own before building.
pub fn client_builder(verify_cert: bool, defaults: ClientDefaults) -> reqwest::ClientBuilder {
  crate::ensure_crypto_provider();
  let options = current_http_options();
  let connect = options.connect_timeout.filter(|t| t.is_finite() && *t > 0.0).unwrap_or(defaults.connect_timeout);
  let read = options.read_timeout.filter(|t| t.is_finite() && *t > 0.0).unwrap_or(defaults.read_timeout);
  let verify = verify_cert && options.verify.unwrap_or(true);
  let redirect = if options.redirect.unwrap_or(true) { reqwest::redirect::Policy::limited(MAX_REDIRECTS) } else { reqwest::redirect::Policy::none() };
  reqwest::Client::builder()
    .connect_timeout(std::time::Duration::from_secs_f64(connect))
    .read_timeout(std::time::Duration::from_secs_f64(read))
    .redirect(redirect)
    .danger_accept_invalid_certs(!verify)
}

pub const APP_ID: &str = concat!("Apprise/", env!("CARGO_PKG_VERSION"));

// ── Helper: service base URLs that tests can redirect ────────────────────────

#[cfg(test)]
tokio::task_local! {
  static TEST_SERVICE_BASE: String;
}

/// Return `default` (a fixed service base URL such as
/// `https://api.telegram.org`). In unit tests, a call made inside
/// [`with_test_base`] returns the mock server's base URL instead, so plugins
/// with hard-coded hosts can be exercised against `wiremock`.
pub(crate) fn service_base(default: &str) -> String {
  #[cfg(test)]
  if let Ok(base) = TEST_SERVICE_BASE.try_with(|b| b.clone()) {
    return base;
  }
  default.to_string()
}

/// Run `fut` with every [`service_base`] call returning `base`.
#[cfg(test)]
pub(crate) async fn with_test_base<F: std::future::Future>(base: impl Into<String>, fut: F) -> F::Output {
  TEST_SERVICE_BASE.scope(base.into(), fut).await
}
