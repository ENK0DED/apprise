use crate::error::ConfigError;
use crate::notify::Notify;
use crate::notify::registry::from_url;

/// Parse YAML config format
pub async fn parse_yaml(content: &str, _recursion_depth: u32) -> Result<Vec<Box<dyn Notify>>, ConfigError> {
    let doc: serde_yaml::Value = serde_yaml::from_str(content).map_err(|e| ConfigError::Other(e.to_string()))?;
    let mut services: Vec<Box<dyn Notify>> = Vec::new();

    // Handle "urls:" key
    if let Some(urls) = doc.get("urls") {
        if let Some(url_list) = urls.as_sequence() {
            for item in url_list {
                // Can be a plain string or a mapping with URL + options
                let url_str = if let Some(s) = item.as_str() {
                    Some(s.to_string())
                } else if let Some(m) = item.as_mapping() {
                    // First key of the mapping is the URL
                    m.keys().next().and_then(|k| k.as_str()).map(|s| s.to_string())
                } else {
                    None
                };
                if let Some(url) = url_str {
                    if let Some(svc) = from_url(&url) {
                        services.push(svc);
                    }
                }
            }
        }
    }
    Ok(services)
}
