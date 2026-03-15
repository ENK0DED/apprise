use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Seven { apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Seven {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        if apikey.is_empty() { return None; }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if targets.is_empty() { return None; }
        Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Seven (seven.io)", service_url: Some("https://seven.io"), setup_url: None, protocols: vec!["seven"], description: "Send SMS via seven.io.", attachment_support: false } }
}
#[async_trait]
impl Notify for Seven {
    fn schemas(&self) -> &[&str] { &["seven"] }
    fn service_name(&self) -> &str { "Seven" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let payload = json!({ "to": self.targets.join(","), "text": msg });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://gateway.seven.io/api/sms").header("User-Agent", APP_ID).header("X-Api-Key", self.apikey.as_str()).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let apikey = "a".repeat(25);
        let urls = vec![
            format!("seven://{}/15551232000", apikey),
            format!("seven://{}/?to=15551232000", apikey),
            format!("seven://{}/15551232000?from=apprise", "3".repeat(14)),
            format!("seven://{}/15551232000?source=apprise", "3".repeat(14)),
            format!("seven://{}/15551232000?from=apprise&flash=true", "3".repeat(14)),
            format!("seven://{}/15551232000?source=apprise&flash=true", "3".repeat(14)),
            format!("seven://{}/15551232000?source=AR&flash=1&label=123", "3".repeat(14)),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "seven://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_fields() {
        let apikey = "a".repeat(25);
        let url_str = format!("seven://{}/15551232000", apikey);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let s = Seven::from_url(&parsed).unwrap();
        assert_eq!(s.apikey, apikey);
        assert_eq!(s.targets, vec!["15551232000"]);
    }

    #[test]
    fn test_from_url_to_param() {
        let apikey = "a".repeat(25);
        let url_str = format!("seven://{}/?to=15551232000", apikey);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let s = Seven::from_url(&parsed).unwrap();
        assert_eq!(s.apikey, apikey);
        assert!(s.targets.contains(&"15551232000".to_string()));
    }

    #[test]
    fn test_no_targets_fails() {
        let apikey = "a".repeat(25);
        let url_str = format!("seven://{}/", apikey);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        assert!(Seven::from_url(&parsed).is_none());
    }

    #[test]
    fn test_service_details() {
        let d = Seven::static_details();
        assert_eq!(d.service_name, "Seven (seven.io)");
        assert!(d.protocols.contains(&"seven"));
        assert!(!d.attachment_support);
    }
}
