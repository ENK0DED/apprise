use crate::asset::AppriseAsset;
use crate::error::ConfigError;
use crate::notify::Notify;
use std::path::PathBuf;

/// Return the cache directory for config HTTP responses.
fn cache_dir() -> PathBuf {
  dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("apprise").join("config_cache")
}

/// Hash a URL to produce a deterministic cache filename.
fn cache_key(url: &str) -> String {
  use sha2::{Digest, Sha256};
  let mut hasher = Sha256::new();
  hasher.update(url.as_bytes());
  format!("{:x}", hasher.finalize())
}

/// Parse the `cache` query parameter.
/// Returns `None` if caching is disabled, or `Some(ttl_secs)`.
/// `cache=yes` => default TTL of 600 s (10 min).
/// `cache=no`  => disabled.
/// `cache=120` => 120 s TTL.
fn parse_cache_param(url: &str) -> Option<u64> {
  let val = super::extract_query_param(url, "cache")?;
  match val.to_lowercase().as_str() {
    "no" | "false" | "0" => None,
    "yes" | "true" => Some(600),
    other => other.parse::<u64>().ok(),
  }
}

/// Strip `cache` and `format` params from a URL so they don't get sent to the
/// remote server.
fn strip_control_params(url: &str) -> String {
  if let Some(qmark) = url.find('?') {
    let base = &url[..qmark];
    let query = &url[qmark + 1..];
    let filtered: Vec<&str> = query
      .split('&')
      .filter(|pair| {
        let key = pair.split('=').next().unwrap_or("");
        key != "cache" && key != "format"
      })
      .collect();
    if filtered.is_empty() { base.to_string() } else { format!("{}?{}", base, filtered.join("&")) }
  } else {
    url.to_string()
  }
}

/// Try reading a cached response. Returns `Some(content)` if the cache file
/// exists and is within TTL.
async fn read_cache(url: &str, ttl_secs: u64) -> Option<String> {
  let path = cache_dir().join(cache_key(url));
  let meta = tokio::fs::metadata(&path).await.ok()?;
  let modified = meta.modified().ok()?;
  let age = modified.elapsed().ok()?;
  if age.as_secs() > ttl_secs {
    return None;
  }
  tokio::fs::read_to_string(&path).await.ok()
}

/// Write content to the cache.
async fn write_cache(url: &str, content: &str) {
  let dir = cache_dir();
  if let Err(e) = tokio::fs::create_dir_all(&dir).await {
    tracing::debug!("Could not create cache dir {:?}: {}", dir, e);
    return;
  }
  let path = dir.join(cache_key(url));
  if let Err(e) = tokio::fs::write(&path, content).await {
    tracing::debug!("Could not write cache file {:?}: {}", path, e);
  }
}

/// Load config from an HTTP(S) URL.
/// Supports `?format=text|yaml` and `?cache=yes|no|SECONDS` query parameters.
pub async fn load_from_http(url: &str, recursion_depth: u32) -> Result<(Vec<Box<dyn Notify>>, Option<AppriseAsset>), ConfigError> {
  let cache_ttl = parse_cache_param(url);
  let fetch_url = strip_control_params(url);

  // Try cache first
  let content = if let Some(ttl) = cache_ttl {
    if let Some(cached) = read_cache(&fetch_url, ttl).await {
      tracing::debug!("Using cached config for {}", fetch_url);
      cached
    } else {
      let body = fetch_http(&fetch_url).await?;
      write_cache(&fetch_url, &body).await;
      body
    }
  } else {
    fetch_http(&fetch_url).await?
  };

  match super::detect_format(url) {
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

async fn fetch_http(url: &str) -> Result<String, ConfigError> {
  let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().map_err(|e| ConfigError::Other(e.to_string()))?;
  let resp = client.get(url).send().await.map_err(|e| ConfigError::Other(e.to_string()))?;
  resp.text().await.map_err(|e| ConfigError::Other(e.to_string()))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_cache_param() {
    assert_eq!(parse_cache_param("https://x.com/cfg?cache=yes"), Some(600));
    assert_eq!(parse_cache_param("https://x.com/cfg?cache=no"), None);
    assert_eq!(parse_cache_param("https://x.com/cfg?cache=120"), Some(120));
    assert_eq!(parse_cache_param("https://x.com/cfg?cache=true"), Some(600));
    assert_eq!(parse_cache_param("https://x.com/cfg?cache=false"), None);
    assert_eq!(parse_cache_param("https://x.com/cfg"), None);
  }

  #[test]
  fn test_strip_control_params() {
    assert_eq!(strip_control_params("https://x.com/cfg?cache=yes&format=yaml"), "https://x.com/cfg");
    assert_eq!(strip_control_params("https://x.com/cfg?a=1&cache=yes"), "https://x.com/cfg?a=1");
    assert_eq!(strip_control_params("https://x.com/cfg?a=1"), "https://x.com/cfg?a=1");
    assert_eq!(strip_control_params("https://x.com/cfg"), "https://x.com/cfg");
  }

  #[test]
  fn test_cache_key_deterministic() {
    let k1 = cache_key("https://example.com/config");
    let k2 = cache_key("https://example.com/config");
    assert_eq!(k1, k2);
    let k3 = cache_key("https://example.com/other");
    assert_ne!(k1, k3);
  }
}
