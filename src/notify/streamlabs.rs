use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Streamlabs { access_token: String, verify_certificate: bool, tags: Vec<String> }
impl Streamlabs {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let access_token = url.host.clone()?;
        // Token must be 40 alphanumeric characters
        if access_token.len() != 40 || !access_token.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        // Validate currency if provided (must be 3-letter code)
        if let Some(currency) = url.get("currency") {
            if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_alphabetic()) {
                return None;
            }
        }
        // Validate name if provided (must be 2-25 chars, start with non-whitespace)
        if let Some(name) = url.get("name") {
            if name.len() < 2 || name.len() > 25 || name.starts_with(' ') { return None; }
        }
        // Validate identifier if provided (regex: ^[^\s].{1,24}$)
        if let Some(ident) = url.get("identifier") {
            if ident.len() < 2 || ident.len() > 25 || ident.starts_with(' ') { return None; }
        }
        // Validate call if provided
        if let Some(call) = url.get("call") {
            match call.to_uppercase().as_str() {
                "ALERTS" | "DONATIONS" | "" => {}
                _ => return None,
            }
        }
        // Validate alert_type if provided
        if let Some(at) = url.get("alert_type") {
            match at.to_lowercase().as_str() {
                "donation" | "follow" | "subscription" | "host" | "bits" | "raid" | "" => {}
                _ => return None,
            }
        }
        Some(Self { access_token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Streamlabs", service_url: Some("https://streamlabs.com"), setup_url: None, protocols: vec!["strmlabs"], description: "Send alerts via Streamlabs.", attachment_support: false } }
}
#[async_trait]
impl Notify for Streamlabs {
    fn schemas(&self) -> &[&str] { &["strmlabs"] }
    fn service_name(&self) -> &str { "Streamlabs" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({ "access_token": self.access_token, "type": "donation", "message": ctx.body, "name": ctx.title, "identifier": "apprise" });
        let resp = client.post("https://streamlabs.com/api/v1.0/alerts").header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso",
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?name=tt&identifier=pyt&amount=20&currency=USD&call=donations",
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?image_href=https://example.org/rms.jpg&sound_href=https://example.org/rms.mp3",
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?duration=1000&image_href=&sound_href=&alert_type=donation&special_text_color=crimson",
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?call=alerts",
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?call=donations",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "strmlabs://",
            "strmlabs://a_bd_/",
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?currency=ABCD",
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?name=tt&identifier=pyt&amount=20&currency=USD&call=rms",
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?name=tt&identifier=pyt&amount=20&currency=USD&alert_type=rms",
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?name=t",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_fields() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso"
        ).unwrap();
        let sl = Streamlabs::from_url(&parsed).unwrap();
        assert_eq!(sl.access_token, "IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso");
    }

    #[test]
    fn test_token_must_be_40_chars_alphanumeric() {
        // Too short
        let parsed = crate::utils::parse::ParsedUrl::parse("strmlabs://short").unwrap();
        assert!(Streamlabs::from_url(&parsed).is_none());

        // Contains underscore
        let parsed = crate::utils::parse::ParsedUrl::parse("strmlabs://a_bd_/").unwrap();
        assert!(Streamlabs::from_url(&parsed).is_none());
    }

    #[test]
    fn test_currency_validation() {
        // Valid 3-letter currency
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?currency=USD"
        ).unwrap();
        assert!(Streamlabs::from_url(&parsed).is_some());

        // Invalid 4-letter currency
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?currency=ABCD"
        ).unwrap();
        assert!(Streamlabs::from_url(&parsed).is_none());
    }

    #[test]
    fn test_call_validation() {
        for call in &["alerts", "donations", "ALERTS", "DONATIONS"] {
            let url = format!(
                "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?call={}", call
            );
            let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
            assert!(Streamlabs::from_url(&parsed).is_some(), "call={} should be valid", call);
        }
        // Invalid call
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?call=rms"
        ).unwrap();
        assert!(Streamlabs::from_url(&parsed).is_none());
    }

    #[test]
    fn test_alert_type_validation() {
        let valid_types = vec!["donation", "follow", "subscription", "host", "bits", "raid"];
        for at in &valid_types {
            let url = format!(
                "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?alert_type={}", at
            );
            let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
            assert!(Streamlabs::from_url(&parsed).is_some(), "alert_type={} should be valid", at);
        }
        // Invalid alert_type
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?alert_type=rms"
        ).unwrap();
        assert!(Streamlabs::from_url(&parsed).is_none());
    }

    #[test]
    fn test_name_validation() {
        // Name too short (1 char)
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?name=t"
        ).unwrap();
        assert!(Streamlabs::from_url(&parsed).is_none());

        // Valid name (2 chars)
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "strmlabs://IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso/?name=tt"
        ).unwrap();
        assert!(Streamlabs::from_url(&parsed).is_some());
    }

    #[test]
    fn test_static_details() {
        let details = Streamlabs::static_details();
        assert_eq!(details.service_name, "Streamlabs");
        assert_eq!(details.service_url, Some("https://streamlabs.com"));
        assert!(details.protocols.contains(&"strmlabs"));
        assert!(!details.attachment_support);
    }
}
