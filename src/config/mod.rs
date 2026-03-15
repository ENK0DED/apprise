pub mod file;
pub mod http;
pub mod text;
#[cfg(feature = "yaml")]
pub mod yaml;

use crate::error::ConfigError;
use crate::notify::Notify;
use crate::notify::registry::from_url;

/// Controls whether one config file may include another of a different protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossIncludeMode {
    /// Includes are completely disabled.
    Never,
    /// Only same-protocol includes allowed (file->file, http->http).
    /// This is the default for `file://` sources.
    Strict,
    /// Any protocol can include any other.
    Always,
}

impl Default for CrossIncludeMode {
    fn default() -> Self {
        Self::Strict
    }
}

/// Return the protocol bucket for a source string ("file" or "http").
fn source_protocol(source: &str) -> &'static str {
    if source.starts_with("http://") || source.starts_with("https://") {
        "http"
    } else {
        "file"
    }
}

/// Check whether `child` is allowed to be included from `parent` under
/// the given cross-include mode.
fn cross_include_allowed(parent: &str, child: &str, mode: CrossIncludeMode) -> bool {
    match mode {
        CrossIncludeMode::Always => true,
        CrossIncludeMode::Never => false,
        CrossIncludeMode::Strict => source_protocol(parent) == source_protocol(child),
    }
}

/// Detect config format from a source path/URL, supporting `?format=text|yaml`
/// query parameter override.
pub enum ConfigFormat {
    Text,
    Yaml,
}

/// Determine format from source string.
/// 1. Check for `?format=text` or `?format=yaml` (or `&format=...`) query param.
/// 2. Fall back to file extension.
pub fn detect_format(source: &str) -> ConfigFormat {
    // Check for format query parameter
    if let Some(format_val) = extract_query_param(source, "format") {
        match format_val.to_lowercase().as_str() {
            "yaml" | "yml" => return ConfigFormat::Yaml,
            "text" | "txt" => return ConfigFormat::Text,
            _ => {} // fall through to extension detection
        }
    }

    // Extension-based detection
    let lower = source.to_lowercase();
    // Strip query string for extension check
    let path_part = lower.split('?').next().unwrap_or(&lower);
    if path_part.ends_with(".yaml") || path_part.ends_with(".yml")
        || path_part.contains(".yaml") || path_part.contains(".yml")
    {
        ConfigFormat::Yaml
    } else {
        ConfigFormat::Text
    }
}

/// Extract a query parameter value from a URL-like string.
pub(crate) fn extract_query_param(source: &str, key: &str) -> Option<String> {
    let query_start = source.find('?')?;
    let query = &source[query_start + 1..];
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Load notification services from a config file path or URL.
/// Uses `Box::pin` to support recursive async calls from include directives.
pub fn load_config(source: &str, recursion_depth: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Box<dyn Notify>>, ConfigError>> + Send + '_>> {
    load_config_with_mode(source, recursion_depth, None, CrossIncludeMode::default())
}

/// Load notification services with cross-include mode enforcement.
/// `parent_source` is `Some(...)` when this is a recursive include.
pub fn load_config_with_mode<'a>(
    source: &'a str,
    recursion_depth: u32,
    parent_source: Option<&'a str>,
    cross_include_mode: CrossIncludeMode,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Box<dyn Notify>>, ConfigError>> + Send + 'a>> {
    Box::pin(async move {
        if recursion_depth == 0 {
            return Err(ConfigError::RecursionDepth);
        }

        // Enforce cross-include policy
        if let Some(parent) = parent_source {
            if !cross_include_allowed(parent, source, cross_include_mode) {
                tracing::warn!(
                    "Cross-include blocked: {} -> {} (mode={:?})",
                    parent,
                    source,
                    cross_include_mode,
                );
                return Ok(Vec::new());
            }
        }

        if source.starts_with("http://") || source.starts_with("https://") {
            http::load_from_http(source, recursion_depth).await
        } else {
            file::load_from_file(source, recursion_depth).await
        }
    })
}

/// Parse a list of URL strings into notification services
pub fn parse_urls(urls: &[String]) -> Vec<Box<dyn Notify>> {
    urls.iter().filter_map(|u| from_url(u)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_format_extension_yaml() {
        assert!(matches!(detect_format("/etc/apprise.yml"), ConfigFormat::Yaml));
        assert!(matches!(detect_format("/etc/apprise.yaml"), ConfigFormat::Yaml));
    }

    #[test]
    fn test_detect_format_extension_text() {
        assert!(matches!(detect_format("/etc/apprise.conf"), ConfigFormat::Text));
        assert!(matches!(detect_format("/etc/apprise"), ConfigFormat::Text));
    }

    #[test]
    fn test_detect_format_query_override() {
        assert!(matches!(detect_format("/etc/apprise.conf?format=yaml"), ConfigFormat::Yaml));
        assert!(matches!(detect_format("https://example.com/config.yaml?format=text"), ConfigFormat::Text));
        assert!(matches!(detect_format("https://example.com/config?foo=bar&format=yml"), ConfigFormat::Yaml));
    }

    #[test]
    fn test_cross_include_strict() {
        assert!(cross_include_allowed("/etc/apprise.conf", "/etc/other.conf", CrossIncludeMode::Strict));
        assert!(cross_include_allowed("https://a.com/cfg", "https://b.com/cfg", CrossIncludeMode::Strict));
        assert!(!cross_include_allowed("/etc/apprise.conf", "https://evil.com/cfg", CrossIncludeMode::Strict));
        assert!(!cross_include_allowed("https://a.com/cfg", "/etc/secret", CrossIncludeMode::Strict));
    }

    #[test]
    fn test_cross_include_never() {
        assert!(!cross_include_allowed("/a", "/b", CrossIncludeMode::Never));
        assert!(!cross_include_allowed("https://a.com", "https://b.com", CrossIncludeMode::Never));
    }

    #[test]
    fn test_cross_include_always() {
        assert!(cross_include_allowed("/a", "https://b.com", CrossIncludeMode::Always));
        assert!(cross_include_allowed("https://a.com", "/b", CrossIncludeMode::Always));
    }

    #[test]
    fn test_extract_query_param() {
        assert_eq!(extract_query_param("http://x.com?format=yaml", "format"), Some("yaml".into()));
        assert_eq!(extract_query_param("http://x.com?a=1&format=text", "format"), Some("text".into()));
        assert_eq!(extract_query_param("http://x.com?a=1", "format"), None);
        assert_eq!(extract_query_param("/path/file", "format"), None);
    }
}
