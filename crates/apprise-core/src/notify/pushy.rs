use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Pushy { apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Pushy {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        if apikey.is_empty() { return None; }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Pushy", service_url: Some("https://pushy.me"), setup_url: None, protocols: vec!["pushy"], description: "Send push notifications via Pushy.", attachment_support: false } }
}
#[async_trait]
impl Notify for Pushy {
    fn schemas(&self) -> &[&str] { &["pushy"] }
    fn service_name(&self) -> &str { "Pushy" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://api.pushy.me/push?api_key={}", self.apikey);
        let payload = json!({ "to": self.targets, "notification": { "title": ctx.title, "body": ctx.body } });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;
    use crate::utils::parse::ParsedUrl;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "pushy://apikey",
            "pushy://apikey/topic",
            "pushy://apikey/%20(",
            "pushy://apikey/@device",
            "pushy://apikey/device/?sound=alarm.aiff",
            "pushy://apikey/device/?badge=100",
            "pushy://apikey/device/?badge=invalid",
            "pushy://apikey/device/?badge=-12",
            "pushy://_/@device/#topic?key=apikey",
            "pushy://apikey/?to=@device",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "pushy://",
            "pushy://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_fields_basic() {
        let parsed = ParsedUrl::parse("pushy://apikey/@device").unwrap();
        let p = Pushy::from_url(&parsed).unwrap();
        assert_eq!(p.apikey, "apikey");
        assert!(p.targets.contains(&"@device".to_string()));
    }

    #[test]
    fn test_from_url_targets_via_to_param() {
        let parsed = ParsedUrl::parse("pushy://apikey/?to=@device").unwrap();
        let p = Pushy::from_url(&parsed).unwrap();
        assert_eq!(p.apikey, "apikey");
        assert!(p.targets.contains(&"@device".to_string()));
    }

    #[test]
    fn test_from_url_device_and_topic() {
        let parsed = ParsedUrl::parse("pushy://_/@device/#topic?key=apikey").unwrap();
        let p = Pushy::from_url(&parsed).unwrap();
        assert!(p.targets.contains(&"@device".to_string()));
        assert!(p.targets.contains(&"#topic".to_string()));
    }

    #[test]
    fn test_from_url_no_targets() {
        let parsed = ParsedUrl::parse("pushy://apikey").unwrap();
        let p = Pushy::from_url(&parsed).unwrap();
        assert_eq!(p.apikey, "apikey");
        assert!(p.targets.is_empty());
    }

    #[test]
    fn test_static_details() {
        let details = Pushy::static_details();
        assert_eq!(details.service_name, "Pushy");
        assert_eq!(details.service_url, Some("https://pushy.me"));
        assert!(details.protocols.contains(&"pushy"));
        assert!(!details.attachment_support);
    }
}
