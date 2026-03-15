use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct PushDeer { token: String, verify_certificate: bool, tags: Vec<String> }
impl PushDeer {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        if token.is_empty() { return None; }
        // Reject if token is all whitespace/non-alphanumeric after decoding
        let decoded = urlencoding::decode(&token).unwrap_or_default();
        if decoded.trim().is_empty() { return None; }
        if !decoded.chars().any(|c| c.is_ascii_alphanumeric() || c == '-') { return None; }
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "PushDeer", service_url: Some("https://www.pushdeer.com"), setup_url: None, protocols: vec!["pushdeer", "pushdeers"], description: "Send notifications via PushDeer.", attachment_support: false } }
}
#[async_trait]
impl Notify for PushDeer {
    fn schemas(&self) -> &[&str] { &["pushdeer", "pushdeers"] }
    fn service_name(&self) -> &str { "PushDeer" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
        let url = format!("https://api2.pushdeer.com/message/push?pushkey={}&text={}", self.token, urlencoding::encode(&text));
        let client = build_client(self.verify_certificate)?;
        let resp = client.get(&url).header("User-Agent", APP_ID).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let a8 = "a".repeat(8);
        let urls = vec![
            format!("pushdeer://localhost/{}", a8),
            format!("pushdeer://localhost:80/{}", a8),
            format!("pushdeer://{}", a8),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "pushdeer://",
            "pushdeers://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_basic_fields() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "pushdeer://mytoken",
        ).unwrap();
        let obj = PushDeer::from_url(&parsed).unwrap();
        assert_eq!(obj.token, "mytoken");
    }

    #[test]
    fn test_from_url_with_host() {
        // When host is specified, the token is the host (default API)
        let a8 = "a".repeat(8);
        let url = format!("pushdeer://{}", a8);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
        let obj = PushDeer::from_url(&parsed).unwrap();
        assert_eq!(obj.token, a8);
    }

    #[test]
    fn test_service_details() {
        let details = PushDeer::static_details();
        assert_eq!(details.service_name, "PushDeer");
        assert!(details.protocols.contains(&"pushdeer"));
        assert!(details.protocols.contains(&"pushdeers"));
        assert_eq!(details.service_url, Some("https://www.pushdeer.com"));
    }
}
