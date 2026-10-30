use std::path::PathBuf;

use crate::types::StorageMode;

/// Default budget for one service, retries and waits included, in seconds
/// (upstream `AppriseAsset._service_timeout`).
pub const DEFAULT_SERVICE_TIMEOUT: f64 = 60.0;

/// Branding and run-time settings shared by every service of an `Apprise`
/// instance.
///
/// Mirrors Python's `AppriseAsset` class. Plugins can use the branding fields
/// for app identification, logos, etc.; the orchestrator reads the delivery
/// settings.
#[derive(Debug, Clone)]
pub struct AppriseAsset {
  pub app_id: String,
  pub app_desc: String,
  pub app_url: String,
  pub image_url_mask: Option<String>,
  pub image_url_logo: Option<String>,
  pub theme: String,
  pub body_format: Option<String>,
  /// Mask secrets (CWE-312) in anything Apprise logs.
  pub secure_logging: bool,
  /// Notify services concurrently. `false` sends them one after another.
  pub async_mode: bool,
  /// Retries after a first failure for services without `?retry=`.
  pub default_service_retry: u32,
  /// Seconds between retries for services without `?wait=`.
  pub default_service_wait: f64,
  /// Stop the call as soon as one tag chain has exhausted its priority
  /// groups, instead of letting every chain finish.
  pub abort_on_chain_failure: bool,
  /// Follow HTTP 3xx redirects unless a URL sets `?redirect=`.
  pub http_redirects: bool,
  /// Seconds one service may take, retries and waits included; it reports
  /// a timeout after that. `0` disables the limit.
  pub service_timeout: f64,
  /// Where persistent storage lives. `None` keeps everything in memory.
  pub storage_path: Option<PathBuf>,
  /// How persistent storage writes to disk.
  pub storage_mode: StorageMode,
  /// Salt mixed into every storage namespace.
  pub storage_salt: Vec<u8>,
  /// Length of storage namespace names (URL IDs); `0` keeps the full hash.
  pub storage_idlen: usize,
}

impl Default for AppriseAsset {
  fn default() -> Self {
    Self {
      app_id: "Apprise".to_string(),
      app_desc: "Apprise Notifications".to_string(),
      app_url: "https://github.com/caronc/apprise".to_string(),
      image_url_mask: None,
      image_url_logo: None,
      theme: "default".to_string(),
      body_format: None,
      secure_logging: true,
      async_mode: true,
      default_service_retry: 0,
      default_service_wait: 0.5,
      abort_on_chain_failure: false,
      http_redirects: true,
      service_timeout: DEFAULT_SERVICE_TIMEOUT,
      storage_path: None,
      storage_mode: StorageMode::Auto,
      storage_salt: Vec::new(),
      storage_idlen: 8,
    }
  }
}

impl AppriseAsset {
  /// The per-service time limit, or `None` when disabled.
  pub fn service_timeout(&self) -> Option<std::time::Duration> {
    (self.service_timeout.is_finite() && self.service_timeout > 0.0).then(|| std::time::Duration::from_secs_f64(self.service_timeout))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_default_asset() {
    let asset = AppriseAsset::default();
    assert_eq!(asset.app_id, "Apprise");
    assert_eq!(asset.app_desc, "Apprise Notifications");
    assert_eq!(asset.app_url, "https://github.com/caronc/apprise");
    assert!(asset.image_url_mask.is_none());
    assert!(asset.image_url_logo.is_none());
    assert_eq!(asset.theme, "default");
    assert!(asset.body_format.is_none());
    assert!(asset.secure_logging);
    assert!(asset.async_mode);
    assert_eq!(asset.default_service_wait, 0.5);
    assert_eq!(asset.service_timeout(), Some(std::time::Duration::from_secs(60)));
    assert_eq!(asset.storage_idlen, 8);
  }

  #[test]
  fn test_service_timeout_zero_disables() {
    let asset = AppriseAsset { service_timeout: 0.0, ..Default::default() };
    assert_eq!(asset.service_timeout(), None);
  }

  #[test]
  fn test_custom_asset() {
    let asset = AppriseAsset {
      app_id: "MyApp".to_string(),
      app_desc: "My Application".to_string(),
      app_url: "https://example.com".to_string(),
      image_url_mask: Some("https://example.com/mask.png".to_string()),
      image_url_logo: Some("https://example.com/logo.png".to_string()),
      theme: "dark".to_string(),
      body_format: Some("html".to_string()),
      secure_logging: false,
      ..Default::default()
    };
    assert_eq!(asset.app_id, "MyApp");
    assert!(asset.image_url_mask.is_some());
    assert_eq!(asset.theme, "dark");
    assert_eq!(asset.body_format, Some("html".to_string()));
    assert!(!asset.secure_logging);
  }
}
