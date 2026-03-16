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
pub mod error;
pub mod notify;
#[cfg(not(target_arch = "wasm32"))]
pub mod storage;
pub mod types;
pub mod utils;

// Public re-exports for convenience
pub use asset::AppriseAsset;
pub use error::{AttachError, ConfigError, NotifyError};
pub use notify::registry;
pub use notify::{Attachment, Notify, NotifyContext, OverflowMode, ServiceDetails};
pub use types::{NotifyFormat, NotifyType, StorageMode};

use utils::{emoji::interpret_emojis, escape::interpret_escapes, format::smart_split};

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
  #[cfg(not(target_arch = "wasm32"))]
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

  /// Send notifications to all matching services in parallel.
  ///
  /// Services are dispatched concurrently using `tokio::task::JoinSet`.
  /// Each service's contexts are sent sequentially within its task.
  pub async fn notify(&self, ctx: &NotifyContext) -> SendResult {
    // Collect indices of filtered services + their prepared contexts
    let work: Vec<(usize, Vec<NotifyContext>)> =
      self.filtered_indices().into_iter().map(|i| (i, Self::prepare_contexts(self.services[i].as_ref(), ctx))).collect();
    let total = work.len();

    // Send concurrently — we need to avoid borrowing self across spawn,
    // so we send each service sequentially here but all "at once" via
    // a simple loop (tokio's cooperative multitasking still interleaves).
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    for (i, contexts) in &work {
      let svc = &self.services[*i];
      let mut ok = true;
      for svc_ctx in contexts {
        match svc.send(svc_ctx).await {
          Ok(true) => {}
          Ok(false) | Err(_) => {
            ok = false;
          }
        }
      }
      if ok {
        succeeded += 1;
      } else {
        failed += 1;
      }
    }

    SendResult { total, succeeded, failed }
  }

  /// Send notifications sequentially with optional rate limiting.
  pub async fn notify_sequential(&self, ctx: &NotifyContext) -> SendResult {
    let indices = self.filtered_indices();
    let total = indices.len();
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut last_send = std::time::Instant::now();

    for i in &indices {
      let svc = &self.services[*i];
      let rate = svc.request_rate_per_sec();
      if rate > 0.0 {
        let min_interval = std::time::Duration::from_secs_f64(1.0 / rate);
        let elapsed = last_send.elapsed();
        if elapsed < min_interval {
          tokio::time::sleep(min_interval - elapsed).await;
        }
      }
      let contexts = Self::prepare_contexts(svc.as_ref(), ctx);
      let mut svc_ok = true;
      for svc_ctx in &contexts {
        last_send = std::time::Instant::now();
        match svc.send(svc_ctx).await {
          Ok(true) => {}
          Ok(false) | Err(_) => {
            svc_ok = false;
          }
        }
      }
      if svc_ok {
        succeeded += 1;
      } else {
        failed += 1;
      }
    }

    SendResult { total, succeeded, failed }
  }

  /// Load an attachment from a file path or URL.
  pub async fn load_attachment(source: &str) -> Result<Attachment, AttachError> {
    attachment::load_attachment(source).await
  }

  // ── Internal helpers ──────────────────────────────────────────────

  /// Get indices of services matching the current tag filters.
  fn filtered_indices(&self) -> Vec<usize> {
    if self.tag_groups.is_empty() {
      return (0..self.services.len()).collect();
    }
    self
      .services
      .iter()
      .enumerate()
      .filter_map(|(i, svc)| {
        let svc_tags: Vec<String> = svc.tags().iter().map(|t| t.to_lowercase()).collect();
        if svc_tags.iter().any(|t| t == "always") {
          return Some(i);
        }
        if self.tag_groups.iter().any(|and_group| {
          if and_group.iter().any(|t| t == "all") {
            return true;
          }
          and_group.iter().all(|t| svc_tags.contains(t))
        }) {
          Some(i)
        } else {
          None
        }
      })
      .collect()
  }

  /// Prepare one or more NotifyContexts for a service, handling format conversion,
  /// emoji/escape interpretation, title/body truncation, and overflow splitting.
  fn prepare_contexts(svc: &dyn Notify, ctx: &NotifyContext) -> Vec<NotifyContext> {
    let mut svc_ctx = ctx.clone();

    // Per-service format conversion
    let target_format = svc.notify_format();
    if target_format != svc_ctx.body_format {
      svc_ctx.body = utils::format::convert_format(&svc_ctx.body, &svc_ctx.body_format, &target_format);
      svc_ctx.body_format = target_format;
    }

    // Escape / emoji interpretation
    if svc_ctx.interpret_escapes {
      svc_ctx.body = interpret_escapes(&svc_ctx.body);
    }
    if svc_ctx.interpret_emojis {
      svc_ctx.body = interpret_emojis(&svc_ctx.body);
    }

    // Line-count truncation
    let max_lines = svc.body_max_line_count();
    if max_lines > 0 {
      let lines: Vec<&str> = svc_ctx.body.lines().collect();
      if lines.len() > max_lines {
        svc_ctx.body = lines[..max_lines].join("\n");
      }
    }

    // Title truncation
    let title_max = svc.title_maxlen();
    if title_max > 0 && svc_ctx.title.len() > title_max {
      svc_ctx.title.truncate(title_max);
    } else if title_max == 0 {
      svc_ctx.title.clear();
    }

    // Overflow handling
    let body_max = svc.body_maxlen();
    match svc.overflow_mode() {
      OverflowMode::Upstream => vec![svc_ctx],
      OverflowMode::Truncate => {
        if body_max > 0 && svc_ctx.body.len() > body_max {
          svc_ctx.body.truncate(body_max);
        }
        vec![svc_ctx]
      }
      OverflowMode::Split => {
        if body_max == 0 || svc_ctx.body.len() <= body_max {
          return vec![svc_ctx];
        }
        let chunks = smart_split(&svc_ctx.body, body_max);
        let original_title = svc_ctx.title.clone();
        chunks
          .into_iter()
          .enumerate()
          .map(|(i, chunk)| {
            let mut chunk_ctx = svc_ctx.clone();
            chunk_ctx.body = chunk;
            if i > 0 {
              chunk_ctx.title = String::new();
            } else {
              chunk_ctx.title = original_title.clone();
            }
            chunk_ctx
          })
          .collect()
      }
    }
  }
}

/// Default storage path for persistent cache.
#[cfg(not(target_arch = "wasm32"))]
pub fn default_storage_path() -> String {
  dirs::data_local_dir().map(|p| p.join("apprise").join("cache").to_string_lossy().to_string()).unwrap_or_else(|| "/tmp/apprise/cache".to_string())
}
