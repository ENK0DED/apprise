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
}

impl Default for AppriseAsset {
    fn default() -> Self {
        Self {
            app_id: "Apprise".to_string(),
            app_desc: "Apprise Notifications".to_string(),
            app_url: "https://github.com/caronc/apprise".to_string(),
            image_url_mask: None,
            image_url_logo: None,
        }
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
    }

    #[test]
    fn test_custom_asset() {
        let asset = AppriseAsset {
            app_id: "MyApp".to_string(),
            app_desc: "My Application".to_string(),
            app_url: "https://example.com".to_string(),
            image_url_mask: Some("https://example.com/mask.png".to_string()),
            image_url_logo: Some("https://example.com/logo.png".to_string()),
        };
        assert_eq!(asset.app_id, "MyApp");
        assert!(asset.image_url_mask.is_some());
    }
}
