use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Kavenegar { apikey: String, targets: Vec<String>, sender: Option<String>, verify_certificate: bool, tags: Vec<String> }
impl Kavenegar {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        if apikey.is_empty() || !apikey.chars().all(|c| c.is_ascii_alphanumeric()) { return None; }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if targets.is_empty() { return None; }
        // Source from user field or ?from= query param; must be valid phone (10-14 digits)
        let sender = url.get("from").map(|s| s.to_string()).or_else(|| url.user.clone());
        if let Some(ref s) = sender {
            let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() < 10 || digits.len() > 14 { return None; }
        }
        Some(Self { apikey, targets, sender, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Kavenegar", service_url: Some("https://kavenegar.com"), setup_url: None, protocols: vec!["kavenegar"], description: "Send SMS via Kavenegar.", attachment_support: false } }
}
#[async_trait]
impl Notify for Kavenegar {
    fn schemas(&self) -> &[&str] { &["kavenegar"] }
    fn service_name(&self) -> &str { "Kavenegar" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let receptor = self.targets.join(",");
        let mut url = format!("https://api.kavenegar.com/v1/{}/sms/send.json?receptor={}&message={}", self.apikey, urlencoding::encode(&receptor), urlencoding::encode(&msg));
        if let Some(ref s) = self.sender { url.push_str(&format!("&sender={}", urlencoding::encode(s))); }
        let client = build_client(self.verify_certificate)?;
        let resp = client.get(&url).header("User-Agent", APP_ID).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;
    use crate::utils::parse::ParsedUrl;

    fn parse_kav(url: &str) -> Option<Kavenegar> {
        ParsedUrl::parse(url).and_then(|p| Kavenegar::from_url(&p))
    }

    #[test]
    fn test_invalid_urls() {
        let alpha_from = format!("kavenegar://{}@{}/{}", "a".repeat(14), "b".repeat(24), "3".repeat(14));
        let short_from = format!("kavenegar://{}@{}/{}", "3".repeat(4), "b".repeat(24), "3".repeat(14));
        let urls: Vec<&str> = vec![
            "kavenegar://",
            "kavenegar://:@/",
            // invalid from number (alpha chars)
            &alpha_from,
            // invalid from number (too short)
            &short_from,
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            format!("kavenegar://{}/{}", "a".repeat(24), "3".repeat(14)),
            format!("kavenegar://{}?to={}", "a".repeat(24), "3".repeat(14)),
            format!("kavenegar://{}@{}/{}", "1".repeat(14), "b".repeat(24), "3".repeat(14)),
            format!("kavenegar://{}/{}?from={}", "b".repeat(24), "3".repeat(14), "1".repeat(14)),
        ];
        for url in &urls {
            assert!(from_url(&url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_targets_not_valid_phones_still_parsed() {
        // kavenegar://apikey/target1/non_phone_target -- non-phone targets are still stored
        // The Python test shows this URL parses but notify_response is False
        let url = format!("kavenegar://{}/{}/{}", "1".repeat(10), "2".repeat(15), "a".repeat(13));
        let obj = parse_kav(&url);
        // Targets include non-digit strings; from_url doesn't validate target format
        assert!(obj.is_some());
    }

    #[test]
    fn test_from_url_fields() {
        let apikey = "a".repeat(24);
        let target = "3".repeat(14);
        let obj = parse_kav(&format!("kavenegar://{}/{}", apikey, target)).unwrap();
        assert_eq!(obj.apikey, apikey);
        assert!(obj.targets.contains(&target));
        assert!(obj.sender.is_none());
    }

    #[test]
    fn test_from_url_with_sender() {
        let sender = "1".repeat(14);
        let apikey = "b".repeat(24);
        let target = "3".repeat(14);
        let obj = parse_kav(&format!("kavenegar://{}@{}/{}", sender, apikey, target)).unwrap();
        assert_eq!(obj.sender.as_deref(), Some(sender.as_str()));
    }

    #[test]
    fn test_from_url_sender_via_query() {
        let sender = "1".repeat(14);
        let apikey = "b".repeat(24);
        let target = "3".repeat(14);
        let obj = parse_kav(&format!("kavenegar://{}/{}?from={}", apikey, target, sender)).unwrap();
        assert_eq!(obj.sender.as_deref(), Some(sender.as_str()));
    }

    #[test]
    fn test_to_query_param() {
        let apikey = "a".repeat(24);
        let target = "3".repeat(14);
        let obj = parse_kav(&format!("kavenegar://{}?to={}", apikey, target)).unwrap();
        assert!(obj.targets.contains(&target));
    }

    #[test]
    fn test_service_details() {
        let details = Kavenegar::static_details();
        assert_eq!(details.service_name, "Kavenegar");
        assert_eq!(details.protocols, vec!["kavenegar"]);
    }
}
