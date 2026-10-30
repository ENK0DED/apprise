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
pub use asset::AppriseAsset;
pub use error::{AttachError, ConfigError, NotifyError};
pub use notify::registry;
pub use notify::{Attachment, Notify, NotifyContext, OverflowMode, ServiceDetails};
pub use result::{NotifyAttempt, ResultStatus, SendResult, ServiceResult};
pub use tag::{AppriseTag, TagFilter};
pub use types::{NotifyFormat, NotifyType, StorageMode};

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
    Self { services: Vec::new(), asset, tag_filter: TagFilter::All, timeout: None }
  }

  /// Set the asset (branding/theming) for this instance.
  pub fn set_asset(&mut self, asset: AppriseAsset) {
    self.asset = asset;
  }

  /// Add a notification service from a URL string.
  /// Returns `true` if the URL was successfully parsed and added.
  pub fn add(&mut self, url: &str) -> bool {
    if let Some(svc) = registry::from_url(url) {
      self.services.push(svc);
      true
    } else {
      false
    }
  }

  /// Add multiple URLs from a raw string (separated by `;`, `,`, or newlines).
  /// Returns the number of services successfully added.
  pub fn add_urls(&mut self, raw: &str) -> usize {
    let mut count = 0;
    for url in raw.split([';', ',', '\n', '\r']) {
      let url = url.trim();
      if url.is_empty() || !url.contains("://") {
        continue;
      }
      if self.add(url) {
        count += 1;
      }
    }
    count
  }

  /// Load services from a config file or URL.
  /// Returns the number of services loaded.
  pub async fn add_config(&mut self, source: &str, recursion_depth: u32) -> Result<usize, ConfigError> {
    let (mut svcs, parsed_asset) = config::load_config(source, recursion_depth).await?;
    let count = svcs.len();
    self.services.append(&mut svcs);
    if let Some(asset) = parsed_asset {
      self.asset = asset;
    }
    Ok(count)
  }

  /// Try loading from default config file paths (~/.apprise, /etc/apprise, etc.).
  /// Stops at the first path that yields services.
  pub async fn load_default_configs(&mut self, recursion_depth: u32) -> usize {
    let mut default_paths: Vec<std::path::PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
      for name in &["apprise", "apprise.conf", "apprise.yml", "apprise.yaml"] {
        default_paths.push(home.join(format!(".{}", name)));
      }
      for name in &["apprise", "apprise.conf", "apprise.yml", "apprise.yaml"] {
        default_paths.push(home.join(".apprise").join(name));
      }
    }
    if let Some(cfg) = dirs::config_dir() {
      for name in &["apprise", "apprise.conf", "apprise.yml", "apprise.yaml"] {
        default_paths.push(cfg.join("apprise").join(name));
      }
    }
    for name in &[
      "/etc/apprise",
      "/etc/apprise.conf",
      "/etc/apprise.yml",
      "/etc/apprise.yaml",
      "/etc/apprise/apprise",
      "/etc/apprise/apprise.conf",
      "/etc/apprise/apprise.yml",
      "/etc/apprise/apprise.yaml",
    ] {
      default_paths.push(std::path::PathBuf::from(name));
    }
    for path in &default_paths {
      if path.exists() {
        let path_str = path.to_string_lossy().to_string();
        if let Ok(count) = self.add_config(&path_str, recursion_depth).await {
          if count > 0 {
            return count;
          }
        }
      }
    }
    0
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
    let mut opts = dispatch::DispatchOptions::from_asset(&self.asset);
    opts.call_timeout = self.timeout;
    self.dispatch(ctx, &opts).await
  }

  /// [`notify`](Self::notify), but one service at a time.
  pub async fn notify_sequential(&self, ctx: &NotifyContext) -> SendResult {
    let mut opts = dispatch::DispatchOptions::from_asset(&self.asset);
    opts.call_timeout = self.timeout;
    opts.sequential = true;
    self.dispatch(ctx, &opts).await
  }

  async fn dispatch(&self, ctx: &NotifyContext, opts: &dispatch::DispatchOptions) -> SendResult {
    let mut ctx = ctx.clone();
    ctx.asset = self.asset.clone();
    dispatch::notify(&self.services, &self.tag_filter, &ctx, opts).await
  }

  /// Load an attachment from a file path or URL.
  pub async fn load_attachment(source: &str) -> Result<Attachment, AttachError> {
    attachment::load_attachment(source).await
  }
}

/// Default storage path for persistent cache.
pub fn default_storage_path() -> String {
  dirs::data_local_dir().map(|p| p.join("apprise").join("cache").to_string_lossy().to_string()).unwrap_or_else(|| "/tmp/apprise/cache".to_string())
}
