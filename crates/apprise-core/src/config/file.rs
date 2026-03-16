use crate::asset::AppriseAsset;
use crate::error::ConfigError;
use crate::notify::Notify;
use tokio::fs;

/// Load config from a local file.
/// Supports `?format=text|yaml` query parameter to override auto-detection.
pub async fn load_from_file(path: &str, recursion_depth: u32) -> Result<(Vec<Box<dyn Notify>>, Option<AppriseAsset>), ConfigError> {
  // Strip query params from the path for reading the actual file
  let file_path = path.split('?').next().unwrap_or(path);
  let content = fs::read_to_string(file_path).await.map_err(|e| ConfigError::Other(e.to_string()))?;

  match super::detect_format(path) {
    super::ConfigFormat::Yaml => {
      #[cfg(feature = "yaml")]
      return super::yaml::parse_yaml(&content, recursion_depth).await;
      #[cfg(not(feature = "yaml"))]
      return Err(ConfigError::InvalidFormat("YAML support not compiled in (rebuild with --features yaml)".into()));
    }
    super::ConfigFormat::Text => {
      let services = super::text::parse_text(&content, recursion_depth).await?;
      Ok((services, None))
    }
  }
}
