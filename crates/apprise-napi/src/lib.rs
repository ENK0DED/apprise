#![deny(clippy::all)]

use std::sync::{Arc, Mutex, MutexGuard};

use apprise_core::{Apprise as CoreApprise, NotifyContext, NotifyFormat, NotifyType, ServiceDetails, TagFilter};
use napi_derive::napi;

// ── Data types for JS interop ────────────────────────────────────────────────

/// Options for sending a notification.
#[napi(object)]
pub struct SendOptions {
  /// Notification service URL(s), separated by commas, semicolons or whitespace.
  pub urls: Option<String>,
  /// Path or URL of a configuration file.
  pub config: Option<String>,
  /// Message body (required).
  pub body: String,
  /// Message title (optional).
  pub title: Option<String>,
  /// Notification type: "info", "success", "warning", "failure".
  pub notification_type: Option<String>,
  /// Body format: "text", "html", "markdown". Without one the body is sent
  /// as written, unless the configuration's asset sets a format.
  pub body_format: Option<String>,
  /// One tag filter group: its comma-separated tags must all match (like one
  /// CLI `--tag`).
  pub tag: Option<String>,
  /// Tag filter groups; a service matching any group is notified (like
  /// repeating `--tag`). Without `tag` or `tags`, every service is notified.
  pub tags: Option<Vec<String>>,
  /// Attachments: file paths or `file://` / `http(s)://` URLs.
  pub attach: Option<Vec<String>>,
  /// Interpret backslash escapes such as `\n` in the title and body.
  pub interpret_escapes: Option<bool>,
  /// Interpret `:emoji:` shortcodes in the title and body.
  pub interpret_emojis: Option<bool>,
}

/// Result of a notification dispatch.
#[napi(object)]
pub struct SendResult {
  /// Total number of services targeted.
  pub total: i32,
  /// Number that succeeded.
  pub succeeded: i32,
  /// Number that failed.
  pub failed: i32,
  /// Overall outcome: "success", "failure", "partial", "nomatch" or "timeout".
  pub status: String,
}

/// Information about a notification service.
#[napi(object)]
pub struct ServiceInfo {
  /// Human-readable service name.
  pub name: String,
  /// URL scheme(s) (e.g., ["ntfy", "ntfys"]).
  pub protocols: Vec<String>,
  /// Short description.
  pub description: String,
  /// Whether the service supports file attachments.
  pub attachment_support: bool,
}

/// Options for the notify method on the Apprise class.
#[napi(object)]
pub struct NotifyOptions {
  pub title: Option<String>,
  pub notification_type: Option<String>,
  pub body_format: Option<String>,
  /// One tag filter group (see `SendOptions.tag`).
  pub tag: Option<String>,
  /// Tag filter groups (see `SendOptions.tags`).
  pub tags: Option<Vec<String>>,
  /// Attachments: file paths or `file://` / `http(s)://` URLs.
  pub attach: Option<Vec<String>>,
  pub interpret_escapes: Option<bool>,
  pub interpret_emojis: Option<bool>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

struct Message {
  ctx: NotifyContext,
  filter: TagFilter,
  attach: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn message(
  body: String,
  title: Option<String>,
  notification_type: Option<String>,
  body_format: Option<String>,
  tag: Option<String>,
  tags: Option<Vec<String>>,
  attach: Option<Vec<String>>,
  interpret_escapes: Option<bool>,
  interpret_emojis: Option<bool>,
) -> napi::Result<Message> {
  let notify_type = match notification_type {
    Some(t) => t.parse::<NotifyType>().map_err(napi::Error::from_reason)?,
    None => NotifyType::Info,
  };
  // No format: the body passes through unconverted (upstream body_format=None)
  let body_passthrough = body_format.is_none();
  let body_format = match body_format {
    Some(f) => f.parse::<NotifyFormat>().map_err(napi::Error::from_reason)?,
    None => NotifyFormat::Text,
  };
  let mut groups = tags.unwrap_or_default();
  groups.extend(tag);
  let filter = if groups.is_empty() { TagFilter::All } else { TagFilter::from_cli(&groups) };
  let ctx = NotifyContext {
    body,
    title: title.unwrap_or_default(),
    notify_type,
    body_format,
    body_passthrough,
    interpret_escapes: interpret_escapes.unwrap_or(false),
    interpret_emojis: interpret_emojis.unwrap_or(false),
    ..Default::default()
  };
  Ok(Message { ctx, filter, attach: attach.unwrap_or_default() })
}

fn to_js(result: apprise_core::SendResult) -> SendResult {
  SendResult { total: result.total as i32, succeeded: result.succeeded as i32, failed: result.failed as i32, status: result.status.name().to_lowercase() }
}

fn service_info(d: ServiceDetails) -> ServiceInfo {
  ServiceInfo {
    name: d.service_name.to_string(),
    protocols: d.protocols.iter().map(|s| s.to_string()).collect(),
    description: d.description.to_string(),
    attachment_support: d.attachment_support,
  }
}

// ── Standalone functions ─────────────────────────────────────────────────────

/// Send a one-shot notification. Creates an Apprise instance, adds URLs/config,
/// and sends the notification.
#[napi]
pub async fn send(options: SendOptions) -> napi::Result<SendResult> {
  let msg = message(
    options.body,
    options.title,
    options.notification_type,
    options.body_format,
    options.tag,
    options.tags,
    options.attach,
    options.interpret_escapes,
    options.interpret_emojis,
  )?;
  let mut apprise = CoreApprise::new();

  if let Some(urls) = &options.urls {
    apprise.add_urls(urls);
  }
  if let Some(config) = &options.config {
    apprise.add_config(config, 1).await.map_err(|e| napi::Error::from_reason(format!("Config error: {}", e)))?;
  }

  if apprise.is_empty() {
    return Err(napi::Error::from_reason("No notification services specified"));
  }

  Ok(to_js(apprise.notify_tagged(&msg.ctx, &msg.filter, &msg.attach).await))
}

/// Parse a notification URL and return service info, or null if invalid.
#[napi]
pub fn parse_url(url: String) -> Option<ServiceInfo> {
  apprise_core::registry::from_url(&url).map(|svc| service_info(svc.details()))
}

/// List all supported notification services.
#[napi]
pub fn list_services() -> Vec<ServiceInfo> {
  CoreApprise::all_service_details().into_iter().map(service_info).collect()
}

// ── Apprise class for stateful usage ─────────────────────────────────────────

/// Stateful notification manager.
///
/// ```js
/// import { Apprise } from '@enk0ded/apprise';
/// const a = new Apprise();
/// a.add('ntfy://mytopic');
/// await a.addConfig('/etc/apprise.yml');
/// await a.notify('Hello!', { title: 'Test', tags: ['devops'] });
/// ```
#[napi]
pub struct Apprise {
  // A notification works on a snapshot of the services and holds no lock
  // while it sends, so the synchronous methods never wait for one (a
  // change made meanwhile copies the shared, cheaply cloned instance).
  inner: Arc<Mutex<Arc<CoreApprise>>>,
}

impl Default for Apprise {
  fn default() -> Self {
    Self { inner: Arc::new(Mutex::new(Arc::new(CoreApprise::new()))) }
  }
}

impl Apprise {
  fn current(&self) -> MutexGuard<'_, Arc<CoreApprise>> {
    self.inner.lock().unwrap_or_else(|e| e.into_inner())
  }
}

#[napi]
impl Apprise {
  /// Create a new Apprise instance.
  #[napi(constructor)]
  pub fn new() -> Self {
    Self::default()
  }

  /// Add notification service URL(s). Returns true if every URL was parsed
  /// and added.
  #[napi]
  pub fn add(&self, url: String) -> bool {
    Arc::make_mut(&mut self.current()).add(&url)
  }

  /// Load services from a configuration file or URL. Resolves to the number
  /// of services loaded.
  #[napi]
  pub async fn add_config(&self, path: String) -> napi::Result<i32> {
    // Load without holding the instance, then move the result in.
    let mut loaded = CoreApprise::with_asset(self.current().asset().clone());
    let count = loaded.add_config(&path, 1).await.map_err(|e| napi::Error::from_reason(format!("Config error: {}", e)))?;
    Arc::make_mut(&mut self.current()).absorb(loaded);
    Ok(count as i32)
  }

  /// Get the number of loaded entries: services, and configuration
  /// entries with template variables whether or not their values are known.
  #[napi]
  pub fn len(&self) -> i32 {
    self.current().len() as i32
  }

  /// Check if nothing is loaded.
  #[napi]
  pub fn is_empty(&self) -> bool {
    self.current().is_empty()
  }

  /// Send a notification to the loaded services the tag filter selects
  /// (every service when no tags are given).
  #[napi]
  pub async fn notify(&self, body: String, options: Option<NotifyOptions>) -> napi::Result<SendResult> {
    let o = options.unwrap_or(NotifyOptions {
      title: None,
      notification_type: None,
      body_format: None,
      tag: None,
      tags: None,
      attach: None,
      interpret_escapes: None,
      interpret_emojis: None,
    });
    let msg = message(body, o.title, o.notification_type, o.body_format, o.tag, o.tags, o.attach, o.interpret_escapes, o.interpret_emojis)?;
    let snapshot = self.current().clone();
    Ok(to_js(snapshot.notify_tagged(&msg.ctx, &msg.filter, &msg.attach).await))
  }

  /// Get details of all loaded services.
  #[napi]
  pub fn details(&self) -> Vec<ServiceInfo> {
    self.current().details().into_iter().map(service_info).collect()
  }

  /// Remove every loaded service and configuration entry.
  #[napi]
  pub fn clear(&self) {
    Arc::make_mut(&mut self.current()).clear();
  }
}
