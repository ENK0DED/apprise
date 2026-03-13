use crate::error::ConfigError;
use crate::notify::Notify;
use crate::notify::registry::from_url;

/// Parse text config format (one URL per line, # and ; for comments)
pub async fn parse_text(content: &str, _recursion_depth: u32) -> Result<Vec<Box<dyn Notify>>, ConfigError> {
    let urls = crate::utils::parse::extract_urls(content);
    let services = urls.iter().filter_map(|u| from_url(u)).collect();
    Ok(services)
}
