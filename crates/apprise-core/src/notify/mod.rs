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
/// to the trait default instead of the plugin's own implementation. The
/// `EveryOverride` test in that file fails until a new method is forwarded.
///
/// # What decides each setting
///
/// Before `send()` runs, the orchestrator (`prepare.rs`) resolves:
///
/// - **Format**: a plugin with one format in
///   [`notify_formats`](Self::notify_formats) gets `?format=` when the URL
///   sets it, else that format. A plugin with several gets `?format=` when
///   it is one of them, else the input format when it is one of them, else
///   the first listed. [`notify_format_overridable`](Self::notify_format_overridable)
///   returning false ignores `?format=`.
/// - **Overflow**: `?overflow=`, else [`overflow_mode`](Self::overflow_mode).
///   [`overflow_mode_overridable`](Self::overflow_mode_overridable)
///   returning false ignores `?overflow=`.
/// - **Emojis**: `?emojis=`, else `NotifyContext::interpret_emojis`.
/// - **Title**: when [`title_maxlen`](Self::title_maxlen) is 0 the title is
///   moved into the body (`title\r\nbody`, an HTML `<b>` line, or a
///   Markdown `#` heading) and `ctx.title` is empty.
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

  /// Every format the plugin can send, the default first (upstream's
  /// `notify_format` tuple). A multi-format plugin overrides this and keeps
  /// [`notify_format`](Self::notify_format) returning the first entry.
  ///
  /// The orchestrator picks one per send, as upstream `resolve_format` does:
  /// a single-format plugin always gets its one format (`?format=` replaces
  /// it); a multi-format plugin gets `?format=` when it is listed, else the
  /// input format when it is listed, else the first entry. The choice is in
  /// `ctx.body_format` when `send()` runs.
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![self.notify_format()]
  }

  /// Whether `?format=` may change the format (default true). Return false
  /// to keep [`notify_formats`](Self::notify_formats) whatever the URL says.
  fn notify_format_overridable(&self) -> bool {
    true
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

  /// Request rate limit in requests per second (default: 1.0).
  /// The orchestrator sleeps between sends to respect this.
  /// Python default is 5.5 but we only throttle in sequential mode.
  fn request_rate_per_sec(&self) -> f64 {
    0.0
  }

  /// How to handle messages that exceed body_maxlen (default: Upstream).
  /// `?overflow=` on the URL takes precedence unless
  /// [`overflow_mode_overridable`](Self::overflow_mode_overridable) is false.
  fn overflow_mode(&self) -> OverflowMode {
    OverflowMode::Upstream
  }

  /// Whether `?overflow=` may change the overflow mode (default true).
  /// Return false to pin [`overflow_mode`](Self::overflow_mode), as upstream
  /// LFG Alarm pins `upstream` so one message never rings twice.
  fn overflow_mode_overridable(&self) -> bool {
    true
  }

  /// Maximum number of lines in the body (default: 0 = unlimited).
  /// When > 0, truncate to N lines BEFORE overflow handling.
  fn body_max_line_count(&self) -> usize {
    0
  }

  /// Whether the title counts against [`body_maxlen`](Self::body_maxlen)
  /// (upstream `overflow_amalgamate_title`): the service shows title and
  /// body together, so `len(title) + len(body)` must fit the body limit.
  fn overflow_amalgamate_title(&self) -> bool {
    false
  }

  /// Convert one prepared body chunk into the service's own markup dialect
  /// (upstream `NotifyBase.dialect_convert`). Called after the generic format
  /// conversion. The orchestrator re-applies size limits to the result.
  /// Keep implementations stateless and return unsupported formats unchanged.
  ///
  /// For a plugin with a dialect ([`has_dialect`](Self::has_dialect)), the
  /// orchestrator re-splits (`overflow=split`) or re-truncates
  /// (`overflow=truncate`) each piece so the *converted* text fits
  /// [`body_maxlen`](Self::body_maxlen), using
  /// [`split_dialect_chunk`](crate::conversion::split_dialect_chunk) and
  /// [`truncate_dialect_chunk`](crate::conversion::truncate_dialect_chunk).
  fn dialect_convert(&self, body: &str, _body_format: &NotifyFormat) -> String {
    body.to_string()
  }

  /// Whether this plugin overrides [`dialect_convert`](Self::dialect_convert)
  /// (upstream checks for the override itself). Return true whenever you
  /// override it: a piece is then re-fitted to the limit even when the
  /// conversion leaves it unchanged, as upstream does. Without it, a piece
  /// the conversion leaves unchanged is sent exactly as prepared.
  fn has_dialect(&self) -> bool {
    false
  }

  /// Common URL options parsed for this instance (`?retry=`, `?format=`, ...).
  /// Only `ConfiguredService` returns `Some`.
  fn service_options(&self) -> Option<&ServiceOptions> {
    None
  }
}

// ── Helper: build a reqwest client ───────────────────────────────────────────

pub fn build_client(verify_cert: bool) -> Result<reqwest::Client, NotifyError> {
  crate::ensure_crypto_provider();
  let builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).danger_accept_invalid_certs(!verify_cert);
  builder.build().map_err(NotifyError::Http)
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
