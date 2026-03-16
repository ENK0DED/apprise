use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::asset::AppriseAsset;
use crate::error::NotifyError;
use crate::types::{NotifyFormat, NotifyType};

pub mod registry;

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

pub mod rsyslog_mod {
  pub use super::rsyslog::*;
}

// ── Overflow mode ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OverflowMode {
  Upstream, // send as-is, let service handle
  Truncate, // hard truncate to body_maxlen
  Split,    // split into multiple messages
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

  /// Request rate limit in requests per second (default: 1.0).
  /// The orchestrator sleeps between sends to respect this.
  /// Python default is 5.5 but we only throttle in sequential mode.
  fn request_rate_per_sec(&self) -> f64 {
    0.0
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
}

// ── Helper: build a reqwest client ───────────────────────────────────────────

pub fn build_client(verify_cert: bool) -> Result<reqwest::Client, NotifyError> {
  let builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).danger_accept_invalid_certs(!verify_cert);
  builder.build().map_err(NotifyError::Http)
}

pub const APP_ID: &str = concat!("Apprise/", env!("CARGO_PKG_VERSION"));
