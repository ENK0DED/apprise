pub mod file;
pub mod http;
pub mod text;
#[cfg(feature = "yaml")]
pub mod yaml;

use crate::error::ConfigError;
use crate::notify::Notify;
use crate::notify::registry::from_url;

/// Load notification services from a config file path or URL.
/// Uses `Box::pin` to support recursive async calls from include directives.
pub fn load_config(source: &str, recursion_depth: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Box<dyn Notify>>, ConfigError>> + Send + '_>> {
    Box::pin(async move {
        if recursion_depth == 0 {
            return Err(ConfigError::RecursionDepth);
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
