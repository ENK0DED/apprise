use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct PopcornNotify { apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl PopcornNotify {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let targets = url.path_parts.clone();
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
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "popcorn://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
