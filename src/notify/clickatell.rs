use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Clickatell { apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Clickatell {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.get("apikey").map(|s| s.to_string()).or_else(|| url.host.clone())?;
        if apikey.is_empty() || apikey == "_" { return None; }
        // Validate source phone if provided (must be 10-14 digits)
        let source = url.get("from").map(|s| s.to_string()).or_else(|| url.user.clone());
        if let Some(ref s) = source {
            let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() || digits.len() < 10 || digits.len() > 14 { return None; }
        }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Clickatell", service_url: Some("https://clickatell.com"), setup_url: None, protocols: vec!["clickatell"], description: "Send SMS via Clickatell.", attachment_support: false } }
}
#[async_trait]
impl Notify for Clickatell {
    fn schemas(&self) -> &[&str] { &["clickatell"] }
    fn service_name(&self) -> &str { "Clickatell" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let url = format!("https://platform.clickatell.com/messages/http/send?apiKey={}&to={}&content={}", self.apikey, urlencoding::encode(target), urlencoding::encode(&msg));
            let resp = client.get(&url).header("User-Agent", APP_ID).send().await?;
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
            "clickatell://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
            "clickatell://1111111111@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
            "clickatell://1111111111@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/123/333333333333333/abcd",
            "clickatell://1111111111/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "clickatell://1111111111@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/1111111111",
            "clickatell://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/1111111111",
            "clickatell://_?apikey=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&from=1111111111&to=1111111111,1111111111",
            "clickatell://_?apikey=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "clickatell://_?apikey=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&from=1111111111",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "clickatell://",
            "clickatell:///",
            "clickatell://@/",
            // no apikey (only 10-digit number as host, no path)
            "clickatell://1111111111@/",
            // invalid from/source (too short)
            "clickatell://111@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_struct_fields() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "clickatell://1111111111@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/2222222222"
        ).unwrap();
        let obj = Clickatell::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(obj.targets, vec!["2222222222"]);
    }

    #[test]
    fn test_from_url_query_params() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "clickatell://_?apikey=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&from=1111111111&to=2222222222,3333333333"
        ).unwrap();
        let obj = Clickatell::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(obj.targets.len(), 2);
        assert!(obj.targets.contains(&"2222222222".to_string()));
        assert!(obj.targets.contains(&"3333333333".to_string()));
    }

    #[test]
    fn test_from_url_no_source() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "clickatell://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/1111111111"
        ).unwrap();
        let obj = Clickatell::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(obj.targets, vec!["1111111111"]);
    }

    #[test]
    fn test_service_details() {
        let details = Clickatell::static_details();
        assert_eq!(details.service_name, "Clickatell");
        assert_eq!(details.protocols, vec!["clickatell"]);
        assert!(!details.attachment_support);
    }
}
