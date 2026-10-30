/// Branding / asset information propagated to plugins.
///
/// Mirrors Python's `AppriseAsset` class.  Plugins can use these fields for
/// app identification, logos, etc.
#[derive(Debug, Clone)]
pub struct AppriseAsset {
  pub app_id: String,
  pub app_desc: String,
  pub app_url: String,
  pub image_url_mask: Option<String>,
  pub image_url_logo: Option<String>,
  pub theme: String,
  pub body_format: Option<String>,
  pub secure_logging: bool,
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
    }
  }
}

/// Upstream's default `AppriseAsset.image_url_mask`, used when the asset
/// does not set one.
pub const DEFAULT_IMAGE_URL_MASK: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/{THEME}/apprise-{TYPE}-{XY}{EXTENSION}";

impl AppriseAsset {
  /// The image URL for a notification type at `image_size` (such as
  /// `"72x72"`), as upstream `AppriseAsset.image_url()` builds it.
  pub fn image_url(&self, notify_type: &crate::types::NotifyType, image_size: &str) -> Option<String> {
    let mask = self.image_url_mask.as_deref().unwrap_or(DEFAULT_IMAGE_URL_MASK);
    if mask.is_empty() {
      return None;
    }
    Some(mask.replace("{THEME}", &self.theme).replace("{TYPE}", notify_type.as_str()).replace("{XY}", image_size).replace("{EXTENSION}", ".png"))
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
    };
    assert_eq!(asset.app_id, "MyApp");
    assert!(asset.image_url_mask.is_some());
    assert_eq!(asset.theme, "dark");
    assert_eq!(asset.body_format, Some("html".to_string()));
    assert!(!asset.secure_logging);
  }

  #[test]
  fn test_image_url() {
    let asset = AppriseAsset::default();
    assert_eq!(
      asset.image_url(&crate::types::NotifyType::Warning, "72x72").as_deref(),
      Some("https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-warning-72x72.png")
    );
    let custom = AppriseAsset { image_url_mask: Some("https://x/{TYPE}-{XY}.png".into()), ..Default::default() };
    assert_eq!(custom.image_url(&crate::types::NotifyType::Info, "128x128").as_deref(), Some("https://x/info-128x128.png"));
  }
}
