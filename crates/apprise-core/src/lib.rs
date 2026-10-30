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
    Self { services: Vec::new(), asset: AppriseAsset::default(), tag_groups: Vec::new() }
  }

  /// Create with a custom asset (branding/theming).
  pub fn with_asset(asset: AppriseAsset) -> Self {
    Self { services: Vec::new(), asset, tag_groups: Vec::new() }
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

/// Default storage path for persistent cache.
pub fn default_storage_path() -> String {
  dirs::data_local_dir().map(|p| p.join("apprise").join("cache").to_string_lossy().to_string()).unwrap_or_else(|| "/tmp/apprise/cache".to_string())
}
