use crate::error::ConfigError;
use crate::notify::Notify;
use tokio::fs;

/// Load config from a local file
pub async fn load_from_file(path: &str, recursion_depth: u32) -> Result<Vec<Box<dyn Notify>>, ConfigError> {
    let content = fs::read_to_string(path).await.map_err(|e| ConfigError::Other(e.to_string()))?;
    let lower = path.to_lowercase();
    if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        super::yaml::parse_yaml(&content, recursion_depth).await
    } else {
        super::text::parse_text(&content, recursion_depth).await
    }
}
