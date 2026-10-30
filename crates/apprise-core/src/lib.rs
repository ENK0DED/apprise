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
//!     println!("Sent to {}/{} services", result.succeeded, result.total);
//! }
//! ```

pub mod asset;
pub mod attachment;
pub mod config;
mod dispatch;
pub mod error;
pub mod notify;
mod prepare;
pub mod storage;
pub mod tag;
pub mod types;
pub mod utils;

// Public re-exports for convenience
pub use asset::AppriseAsset;
pub use error::{AttachError, ConfigError, NotifyError};
pub use notify::registry;
pub use notify::{Attachment, Notify, NotifyContext, OverflowMode, ServiceDetails};
pub use types::{NotifyFormat, NotifyType, StorageMode};

use utils::parse::{parse_list, parse_urls};

/// Install the ring crypto provider for rustls. Safe to call multiple times.
pub fn ensure_crypto_provider() {
  let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Result of a notification dispatch.
#[derive(Debug, Clone)]
pub struct SendResult {
  /// Total number of services targeted.
  pub total: usize,
  /// Number of services that succeeded.
  pub succeeded: usize,
  /// Number of services that failed.
  pub failed: usize,
}

/// The main Apprise orchestrator.
///
/// Collects notification services from URLs and config files, then dispatches
/// notifications to all matching services with format conversion, overflow
/// handling, and tag filtering.
pub struct Apprise {
  services: Vec<Box<dyn Notify>>,
  asset: AppriseAsset,
  tag_groups: Vec<Vec<String>>,
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
    ensure_crypto_provider();
    Self { services: Vec::new(), asset: AppriseAsset::default(), tag_groups: Vec::new(), config: ConfigState::default() }
  }

  /// Create with a custom asset (branding/theming).
  pub fn with_asset(asset: AppriseAsset) -> Self {
    Self { services: Vec::new(), asset, tag_groups: Vec::new(), config: ConfigState::default() }
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
    self.filtered_indices().len()
  }

  /// YAML entries waiting for a template value.
  pub fn pending_templates(&self) -> &[config::PendingService] {
    &self.config.pending
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
  pub fn set_tags(&mut self, tag_groups: Vec<Vec<String>>) {
    self.tag_groups = tag_groups;
  }

  /// Parse tag strings from CLI format (each string is comma-separated AND group).
  pub fn set_tag_strings(&mut self, tags: &[String]) {
    self.tag_groups =
      tags.iter().map(|g| g.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect::<Vec<_>>()).filter(|g| !g.is_empty()).collect();
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

  /// Get the current asset.
  pub fn asset(&self) -> &AppriseAsset {
    &self.asset
  }

  /// Send notifications to all matching services.
  pub async fn notify(&self, ctx: &NotifyContext) -> SendResult {
    dispatch::notify(&self.services, &self.filtered_indices(), ctx).await
  }

  /// Send notifications one service at a time, honouring each service's
  /// request rate.
  pub async fn notify_sequential(&self, ctx: &NotifyContext) -> SendResult {
    dispatch::notify_sequential(&self.services, &self.filtered_indices(), ctx).await
  }

  /// Load an attachment from a file path or URL.
  pub async fn load_attachment(source: &str) -> Result<Attachment, AttachError> {
    attachment::load_attachment(source).await
  }

  // ── Internal helpers ──────────────────────────────────────────────

  /// Get indices of services matching the current tag filters.
  fn filtered_indices(&self) -> Vec<usize> {
    self.services.iter().enumerate().filter(|(_, svc)| tag::is_match(&svc.tags(), &self.tag_groups)).map(|(i, _)| i).collect()
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

/// Default storage path for persistent cache.
pub fn default_storage_path() -> String {
  dirs::data_local_dir().map(|p| p.join("apprise").join("cache").to_string_lossy().to_string()).unwrap_or_else(|| "/tmp/apprise/cache".to_string())
}
