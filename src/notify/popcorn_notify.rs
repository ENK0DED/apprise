use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct PopcornNotify { apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl PopcornNotify {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        if apikey.is_empty() { return None; }
        // Reject if apikey contains only underscores/non-alphanumeric
        if !apikey.chars().any(|c| c.is_ascii_alphanumeric()) { return None; }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if targets.is_empty() { return None; }
        Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Popcorn Notify", service_url: Some("https://popcornnotify.com"), setup_url: None, protocols: vec!["popcorn"], description: "Send notifications via Popcorn Notify.", attachment_support: false } }
}
#[async_trait]
impl Notify for PopcornNotify {
    fn schemas(&self) -> &[&str] { &["popcorn"] }
    fn service_name(&self) -> &str { "Popcorn Notify" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let params = [("email", target.as_str()), ("message", ctx.body.as_str()), ("subject", ctx.title.as_str())];
            let resp = client.post("https://popcornnotify.com/notify").header("User-Agent", APP_ID).basic_auth(&self.apikey, Option::<&str>::None).form(&params).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            format!("popcorn://{}/15551232000/user@example.com", "c".repeat(9)),
            format!("popcorn://{}/15551232000/user@example.com?batch=yes", "w".repeat(9)),
            format!("popcorn://{}/?to=15551232000", "w".repeat(9)),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "popcorn://".to_string(),
            // invalid apikey (underscores only)
            format!("popcorn://{}/18001231234", "_".repeat(9)),
            // no targets
            format!("popcorn://{}/", "a".repeat(9)),
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_basic_fields() {
        let apikey = "c".repeat(9);
        let url = format!("popcorn://{}/15551232000/user@example.com", apikey);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
        let obj = PopcornNotify::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, apikey);
        assert_eq!(obj.targets.len(), 2);
        assert!(obj.targets.contains(&"15551232000".to_string()));
        assert!(obj.targets.contains(&"user@example.com".to_string()));
    }

    #[test]
    fn test_from_url_to_param() {
        let apikey = "w".repeat(9);
        let url = format!("popcorn://{}/?to=15551232000", apikey);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
        let obj = PopcornNotify::from_url(&parsed).unwrap();
        assert!(obj.targets.contains(&"15551232000".to_string()));
    }

    #[test]
    fn test_service_details() {
        let details = PopcornNotify::static_details();
        assert_eq!(details.service_name, "Popcorn Notify");
        assert!(details.protocols.contains(&"popcorn"));
        assert_eq!(details.service_url, Some("https://popcornnotify.com"));
    }
}
