//! Apprise — Push notifications that work with everything.
//!
//! This is the core library providing 127 notification service plugins,
//! configuration parsing, attachment handling, and notification dispatch.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use apprise_core::{Apprise, NotifyContext, NotifyType, NotifyFormat};
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut apprise = Apprise::new();
//!     apprise.add("ntfy://mytopic");
//!     apprise.add("slack://TokenA/TokenB/TokenC");
//!
//!     let ctx = NotifyContext {
//!         body: "Hello from Rust!".into(),
//!         title: "Test".into(),
//!         ..Default::default()
//!     };
//!
//!     let result = apprise.notify(&ctx).await;
//!     println!("{}: sent to {}/{} services", result.status, result.succeeded, result.total);
//! }
//! ```

pub mod asset;
pub mod attachment;
pub mod config;
pub mod conversion;
mod dispatch;
pub mod error;
pub mod notify;
mod prepare;
pub mod result;
pub mod storage;
pub mod tag;
pub mod types;
pub mod utils;

// Public re-exports for convenience
pub use asset::{AppriseAsset, NotifyImageSize};
pub use error::{AttachError, ConfigError, NotifyError};
pub use notify::registry;
pub use notify::{Attachment, Notify, NotifyContext, OverflowMode, ServiceDetails};
pub use result::{NotifyAttempt, ResultStatus, SendResult, ServiceResult};
pub use tag::{AppriseTag, TagFilter};
pub use types::{NotifyFormat, NotifyType, StorageMode};

use utils::parse::{parse_list, parse_urls};

/// Delivery tracking for plugins that send one notification to several
/// targets: with retries enabled, a retry can skip targets that already
/// accepted the message, and reuse work (uploads, tokens) from an earlier
/// attempt.
pub mod delivery {
  pub use crate::dispatch::{is_delivered, mark_delivered, recall, remember};
}

/// Install the ring crypto provider for rustls. Safe to call multiple times.
pub fn ensure_crypto_provider() {
  let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The main Apprise orchestrator.
///
/// Collects notification services from URLs and config files, then dispatches
/// notifications to all matching services with format conversion, overflow
/// handling, and tag filtering.
pub struct Apprise {
  services: Vec<Box<dyn Notify>>,
  asset: AppriseAsset,
  tag_filter: TagFilter,
  timeout: Option<std::time::Duration>,
  stores: storage::StoreRegistry,
  location: attachment::ContentLocation,
  config: ConfigState,
}

/// Configuration and template settings of an [`Apprise`] instance.
#[derive(Default)]
struct ConfigState {
  /// YAML entries still waiting for template values.
  pending: Vec<config::PendingService>,
  /// Values for `${NAME}` template variables.
  template_values: std::collections::HashMap<String, String>,
  /// Ignore YAML `template:` sections (upstream `allow_templates=False`).
  templates_disabled: bool,
  /// Let any configuration include a local file.
  insecure_includes: bool,
}

impl Default for Apprise {
  fn default() -> Self {
    Self::new()
  }
}

impl Apprise {
  /// Create a new empty Apprise instance.
  pub fn new() -> Self {
    Self::with_asset(AppriseAsset::default())
  }

  /// Create with a custom asset (branding, delivery and storage settings).
  pub fn with_asset(asset: AppriseAsset) -> Self {
    ensure_crypto_provider();
    Self {
      services: Vec::new(),
      asset,
      tag_filter: TagFilter::All,
      timeout: None,
      stores: storage::StoreRegistry::default(),
      location: attachment::ContentLocation::Local,
      config: ConfigState::default(),
    }
  }

  /// Set the asset (branding/theming) for this instance.
  pub fn set_asset(&mut self, asset: AppriseAsset) {
    self.asset = asset;
  }

  /// Add the notification services in `urls`: one URL, or several
  /// separated by commas, semicolons and/or whitespace (upstream
  /// `Apprise.add()` with `parse_urls()`). Returns `true` only when every
  /// URL loaded; the ones that did load are kept either way.
  pub fn add(&mut self, urls: &str) -> bool {
    self.add_with_tags(urls, &[])
  }

  /// [`Apprise::add`], giving each new service `tags` (raw strings; a
  /// `priority:tag` form is kept as written). Tags are never read from the
  /// URL itself: `?tag=`/`?tags=` belong to the plugin.
  pub fn add_with_tags(&mut self, urls: &str, tags: &[&str]) -> bool {
    let entries = parse_urls(urls);
    if entries.is_empty() {
      return false;
    }
    let tags: Vec<String> = parse_list(&tags.join(","));
    let mut all = true;
    for url in entries {
      let service = registry::parse_service_url(&url).and_then(|mut parsed| {
        parsed.assigned_tags = Some(tags.clone());
        registry::from_parsed(&parsed)
      });
      match service {
        Some(svc) => self.services.push(svc),
        None => all = false,
      }
    }
    all
  }

  /// Add every URL in a list string (see [`Apprise::add`]). Returns the
  /// number of services added.
  pub fn add_urls(&mut self, raw: &str) -> usize {
    let before = self.services.len();
    self.add(raw);
    self.services.len() - before
  }

  /// Load services from a configuration source: a local path, `file://`,
  /// `http://` or `https://` URL. `recursion` is how many levels of
  /// `include` to follow (zero ignores them).
  ///
  /// Returns the number of services loaded. YAML entries still waiting for
  /// a template value are kept aside (see [`Apprise::pending_templates`])
  /// and load as soon as [`Apprise::set_template_values`] supplies one.
  pub async fn add_config(&mut self, source: &str, recursion: u32) -> Result<usize, ConfigError> {
    let loaded = config::load_config(source, &self.load_options(recursion)).await?;
    Ok(self.take_loaded(loaded))
  }

  /// Load services from configuration text (upstream
  /// `AppriseConfig.add_config()`). The format is detected when `None`.
  pub async fn add_config_text(&mut self, content: &str, format: Option<config::ConfigFormat>, recursion: u32) -> Result<usize, ConfigError> {
    let loaded = config::load_config_text(content, format, &self.load_options(recursion)).await?;
    Ok(self.take_loaded(loaded))
  }

  /// Load every default configuration file that exists (upstream CLI
  /// `DEFAULT_CONFIG_PATHS`). Returns the number of services loaded.
  pub async fn load_default_configs(&mut self, recursion: u32) -> usize {
    let mut count = 0;
    for path in default_config_paths() {
      if path.is_file() {
        match self.add_config(&path.to_string_lossy(), recursion).await {
          Ok(n) => count += n,
          Err(e) => tracing::warn!("Could not load configuration {}: {}", path.display(), e),
        }
      }
    }
    count
  }

  /// Supply values for `${NAME}` template variables (names are
  /// case-insensitive). Pending YAML entries that now have every value are
  /// loaded; returns how many were. A value not given here falls back to
  /// the configuration default, then to `APPRISE_TEMPLATE_<NAME>`.
  pub fn set_template_values(&mut self, values: std::collections::HashMap<String, String>) -> Result<usize, ConfigError> {
    config::template::validate_overrides(&values).map_err(|e| ConfigError::Other(e.to_string()))?;
    self.config.template_values = values;
    let pending = std::mem::take(&mut self.config.pending);
    let before = self.services.len();
    for entry in pending {
      self.resolve_pending(entry);
    }
    Ok(self.services.len() - before)
  }

  /// Number of loaded services the current tag filter selects.
  pub fn filtered_len(&self) -> usize {
    self.find().count()
  }

  /// YAML entries waiting for a template value.
  pub fn pending_templates(&self) -> &[config::PendingService] {
    &self.config.pending
  }

  /// The entries waiting for a template value that `filter` selects. A
  /// notification counts each of them as a failed service, as upstream does
  /// for an entry it cannot resolve.
  pub fn pending_matching<'a>(&'a self, filter: &'a TagFilter) -> impl Iterator<Item = &'a config::PendingService> + 'a {
    self.config.pending.iter().filter(move |p| filter.matches(&tag::parse_service_tags(&p.tags())))
  }

  /// Turn YAML `template:` sections on (the default) or off. With them off,
  /// `${NAME}` text is kept literally.
  pub fn set_allow_templates(&mut self, allow: bool) {
    self.config.templates_disabled = !allow;
  }

  /// Let configuration from any source include local files. Only for
  /// trusted configuration.
  pub fn set_insecure_includes(&mut self, insecure: bool) {
    self.config.insecure_includes = insecure;
  }

  fn load_options(&self, recursion: u32) -> config::LoadOptions {
    config::LoadOptions {
      recursion,
      insecure_includes: self.config.insecure_includes,
      allow_templates: !self.config.templates_disabled,
      asset: self.asset.clone(),
    }
  }

  /// Keep what a configuration produced; returns the services added.
  fn take_loaded(&mut self, loaded: config::Loaded) -> usize {
    if let Some(asset) = loaded.asset {
      self.asset = asset;
    }
    let before = self.services.len();
    for item in loaded.items {
      match item {
        config::ConfigItem::Service(svc) => self.services.push(svc),
        config::ConfigItem::Pending(entry) => self.resolve_pending(*entry),
      }
    }
    self.services.len() - before
  }

  /// Build a pending entry now if its values are available, else keep it.
  fn resolve_pending(&mut self, entry: config::PendingService) {
    match entry.resolve(&self.config.template_values, &config::template::process_env) {
      Some(svc) => self.services.push(svc),
      None => self.config.pending.push(entry),
    }
  }

  /// Set tag filter groups.
  /// Each inner Vec is an AND group (all tags must match); outer Vec is OR'd.
  /// An empty list selects every service.
  pub fn set_tags(&mut self, tag_groups: Vec<Vec<String>>) {
    self.tag_filter = if tag_groups.is_empty() { TagFilter::All } else { TagFilter::Groups(tag_groups) };
  }

  /// Parse tag strings from CLI format: each string is one AND group
  /// (tokens separated by commas or spaces) and the strings are OR'd. Tokens
  /// may carry a priority (`2:tag`) and a retry count (`tag:3`). An empty
  /// list selects every service.
  pub fn set_tag_strings(&mut self, tags: &[String]) {
    self.tag_filter = if tags.is_empty() { TagFilter::All } else { TagFilter::from_cli(tags) };
  }

  /// Set which services notifications go to.
  pub fn set_tag_filter(&mut self, filter: TagFilter) {
    self.tag_filter = filter;
  }

  /// The current tag filter.
  pub fn tag_filter(&self) -> &TagFilter {
    &self.tag_filter
  }

  /// Limit every notification call to `seconds` (upstream
  /// `notify(timeout=)`); unfinished services report a timeout. `0` removes
  /// the limit, leaving only the asset's per-service limit. Negative or
  /// non-finite values are rejected.
  pub fn set_timeout(&mut self, seconds: f64) -> Result<(), String> {
    self.timeout = dispatch::validate_timeout(seconds)?;
    Ok(())
  }

  /// The services the current tag filter selects, in load order.
  pub fn find(&self) -> impl Iterator<Item = &dyn Notify> {
    self.services.iter().filter(|svc| self.tag_filter.matches(&tag::parse_service_tags(&svc.tags()))).map(|svc| svc.as_ref())
  }

  /// Every loaded service, in load order.
  pub fn services(&self) -> impl Iterator<Item = &dyn Notify> {
    self.services.iter().map(|svc| svc.as_ref())
  }

  /// Get the current number of loaded services.
  pub fn len(&self) -> usize {
    self.services.len()
  }

  /// Check if no services are loaded.
  pub fn is_empty(&self) -> bool {
    self.services.is_empty()
  }

  /// Clear all loaded services.
  pub fn clear(&mut self) {
    self.services.clear();
  }

  /// Get details of all loaded services.
  pub fn details(&self) -> Vec<ServiceDetails> {
    self.services.iter().map(|s| s.details()).collect()
  }

  /// Get details of all supported services (static, doesn't require instances).
  pub fn all_service_details() -> Vec<ServiceDetails> {
    registry::all_service_details()
  }

  /// Every supported service, the asset and the version, as upstream's
  /// `Apprise.details()` reports them.
  pub fn details_json(&self) -> serde_json::Value {
    serde_json::json!({
      "version": env!("CARGO_PKG_VERSION"),
      "schemas": Self::all_service_details().iter().map(ServiceDetails::to_json).collect::<Vec<_>>(),
      "asset": self.asset.details(),
    })
  }

  /// [`details_json`](Self::details_json) as compact JSON (upstream
  /// `Apprise.json()`).
  pub fn json(&self) -> String {
    self.details_json().to_string()
  }

  /// Get the current asset.
  pub fn asset(&self) -> &AppriseAsset {
    &self.asset
  }

  /// Send a notification to the services the tag filter selects.
  ///
  /// Services are notified concurrently (unless the asset disables async
  /// mode), each with its own retries (`?retry=`, `?wait=`) and time limit
  /// (the asset's `service_timeout`, and the call limit from
  /// [`set_timeout`](Self::set_timeout)). Without an explicit priority in
  /// the filter, services are tried by tag priority and lower-priority
  /// groups only run if a more urgent one failed. Plugins see the asset of
  /// this instance in `ctx.asset`.
  pub async fn notify(&self, ctx: &NotifyContext) -> SendResult {
    self.dispatch(ctx, &self.dispatch_options(), &self.tag_filter).await
  }

  /// [`notify_with_attachments`](Self::notify_with_attachments) for the
  /// services `filter` selects instead of the instance's tag filter
  /// (upstream `notify(tag=...)`). Pass no `sources` to send without
  /// attachments.
  pub async fn notify_tagged(&self, ctx: &NotifyContext, filter: &TagFilter, sources: &[String]) -> SendResult {
    self.notify_attached(ctx, sources, filter).await
  }

  /// [`notify`](Self::notify) with attachments loaded from `sources` (paths
  /// or `file://`/`http(s)://` URLs) added to `ctx.attachments`.
  ///
  /// As upstream: a source that can never load (unknown schema, invalid
  /// `?mime=`, or not allowed from this instance's location) fails the whole
  /// call; one that cannot be read now (missing file, HTTP error, too large)
  /// fails only the services that take attachments, and the others send the
  /// message without it.
  pub async fn notify_with_attachments(&self, ctx: &NotifyContext, sources: &[String]) -> SendResult {
    self.notify_attached(ctx, sources, &self.tag_filter).await
  }

  async fn notify_attached(&self, ctx: &NotifyContext, sources: &[String], filter: &TagFilter) -> SendResult {
    let mut opts = self.dispatch_options();
    let mut ctx = ctx.clone();
    if !self.services.is_empty() {
      let mut parsed = Vec::with_capacity(sources.len());
      for source in sources {
        match attachment::parse_attachment_from(source, self.location) {
          Ok(p) => parsed.push(p),
          Err(_) => {
            tracing::warn!("Could not prepare the notification: One or more attachments could not be added.");
            return SendResult::new(ResultStatus::Failure, Vec::new(), std::time::Duration::ZERO);
          }
        }
      }
      for source in parsed {
        tracing::debug!("Loading attachment: {}", source.loggable());
        match source.load().await {
          Ok(att) => ctx.attachments.push(att),
          Err(_) => opts.attachment_errors.push(source.loggable().to_string()),
        }
      }
    }
    self.dispatch(&ctx, &opts, filter).await
  }

  /// Where this instance runs, which limits where attachments may come
  /// from (a hosted instance only accepts web content).
  pub fn set_location(&mut self, location: attachment::ContentLocation) {
    self.location = location;
  }

  /// [`notify`](Self::notify), but one service at a time.
  pub async fn notify_sequential(&self, ctx: &NotifyContext) -> SendResult {
    let mut opts = self.dispatch_options();
    opts.sequential = true;
    self.dispatch(ctx, &opts, &self.tag_filter).await
  }

  fn dispatch_options(&self) -> dispatch::DispatchOptions {
    let mut opts = dispatch::DispatchOptions::from_asset(&self.asset);
    opts.call_timeout = self.timeout;
    opts.stores = self.stores.clone();
    opts
  }

  /// Write the services' pending persistent storage to disk. Stores also
  /// flush when the instance is dropped; call this before exiting the
  /// process without unwinding.
  pub fn flush_stores(&self) {
    self.stores.flush();
  }

  /// The persistent storage namespace (URL ID) of a loaded service, or
  /// `None` when it has no URL.
  pub fn url_id(&self, svc: &dyn Notify) -> Option<String> {
    notify::options::url_id(svc, &self.asset.storage_salt, self.asset.storage_idlen)
  }

  async fn dispatch(&self, ctx: &NotifyContext, opts: &dispatch::DispatchOptions, filter: &TagFilter) -> SendResult {
    let mut ctx = ctx.clone();
    ctx.asset = self.asset.clone();
    // Names reach MIME and multipart headers; attachments built by a
    // library caller have not been through the loaders' cleanup.
    for attachment in &mut ctx.attachments {
      attachment.name = attachment::safe_name(&attachment.name);
    }
    let result = dispatch::notify(&self.services, filter, &ctx, opts).await;
    self.with_unresolved(result, filter)
  }

  /// Add a failed result for every selected entry still waiting for a
  /// template value: a run with one is partial at best, as upstream.
  fn with_unresolved(&self, result: SendResult, filter: &TagFilter) -> SendResult {
    let pending: Vec<&config::PendingService> = self.pending_matching(filter).collect();
    if pending.is_empty() {
      return result;
    }
    let SendResult { mut results, elapsed, .. } = result;
    for entry in pending {
      let optional = notify::ServiceOptions::from_url(&entry.parsed).ok().and_then(|o| o.optional).unwrap_or(false);
      let mut tags: Vec<String> = tag::parse_service_tags(&entry.tags()).iter().map(|t| t.name().to_string()).collect();
      tags.sort();
      results.push(result::ServiceResult::new(
        registry::service_name(entry.schema()),
        utils::cwe312::cwe312_loggable(&entry.display_url(), true),
        tags,
        optional,
        1,
        vec![result::NotifyAttempt { status: ResultStatus::Failure, elapsed: std::time::Duration::ZERO }],
        std::time::Duration::ZERO,
      ));
    }
    let status = result::aggregate_status(false, &results);
    SendResult::new(status, results, elapsed)
  }

  /// Load an attachment from a file path or URL.
  pub async fn load_attachment(source: &str) -> Result<Attachment, AttachError> {
    attachment::load_attachment(source).await
  }
}

/// Default configuration files, in the order upstream's CLI checks them.
pub fn default_config_paths() -> Vec<std::path::PathBuf> {
  use std::path::PathBuf;
  if cfg!(windows) {
    let mut out = Vec::new();
    for var in ["APPDATA", "LOCALAPPDATA", "ALLUSERSPROFILE", "PROGRAMFILES", "COMMONPROGRAMFILES"] {
      if let Ok(base) = std::env::var(var) {
        for name in ["apprise", "apprise.conf", "apprise.yml", "apprise.yaml"] {
          out.push(PathBuf::from(&base).join("Apprise").join(name));
        }
      }
    }
    return out;
  }

  let home = dirs::home_dir().unwrap_or_default();
  let mut out = Vec::new();
  for dir in [".apprise", ".config/apprise"] {
    for ext in ["", ".conf", ".yml", ".yaml"] {
      out.push(home.join(format!("{}{}", dir, ext)));
    }
  }
  for dir in [".apprise", ".config/apprise"] {
    for name in ["apprise", "apprise.conf", "apprise.yml", "apprise.yaml"] {
      out.push(home.join(dir).join(name));
    }
  }
  for path in [
    "/etc/apprise",
    "/etc/apprise.yml",
    "/etc/apprise.yaml",
    "/etc/apprise/apprise",
    "/etc/apprise/apprise.conf",
    "/etc/apprise/apprise.yml",
    "/etc/apprise/apprise.yaml",
  ] {
    out.push(PathBuf::from(path));
  }
  out
}

/// Default persistent storage path, as upstream: `~/.local/share/apprise/cache`
/// (`%APPDATA%/Apprise/cache` on Windows). `APPRISE_STORAGE_PATH` is not
/// consulted here.
pub fn default_storage_path() -> String {
  if cfg!(windows) {
    let appdata = std::env::var("APPDATA").ok().filter(|v| !v.is_empty());
    return appdata.map(|a| format!("{a}/Apprise/cache")).unwrap_or_else(|| "%APPDATA%/Apprise/cache".to_string());
  }
  dirs::home_dir()
    .map(|h| h.join(".local").join("share").join("apprise").join("cache").to_string_lossy().into_owned())
    .unwrap_or_else(|| "~/.local/share/apprise/cache".to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_details_json() {
    let a = Apprise::new();
    let d: serde_json::Value = serde_json::from_str(&a.json()).unwrap();
    assert_eq!(d["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(d["asset"]["app_id"], "Apprise");
    assert_eq!(d["asset"]["default_extension"], ".png");
    assert!(d["schemas"].as_array().unwrap().iter().any(|s| s["service_name"] == "Slack"));
  }

  #[tokio::test]
  async fn test_with_asset_installs_the_crypto_provider() {
    let a = Apprise::with_asset(AppriseAsset::default());
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    drop(a);
  }
}
