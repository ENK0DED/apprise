use crate::error::ConfigError;
use crate::notify::Notify;

/// Load config from an HTTP(S) URL
pub async fn load_from_http(url: &str, recursion_depth: u32) -> Result<Vec<Box<dyn Notify>>, ConfigError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| ConfigError::Other(e.to_string()))?;
    let resp = client.get(url).send().await.map_err(|e| ConfigError::Other(e.to_string()))?;
    let content = resp.text().await.map_err(|e| ConfigError::Other(e.to_string()))?;
    let lower = url.to_lowercase();
    if lower.contains(".yaml") || lower.contains(".yml") {
        #[cfg(feature = "yaml")]
        return super::yaml::parse_yaml(&content, recursion_depth).await;
        #[cfg(not(feature = "yaml"))]
        return Err(ConfigError::InvalidFormat(
            "YAML support not compiled in (rebuild with --features yaml)".into(),
        ));
    }
    super::text::parse_text(&content, recursion_depth).await
}
