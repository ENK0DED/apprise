use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Chanify { token: String, verify_certificate: bool, tags: Vec<String> }
impl Chanify {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()
            .filter(|h| !h.is_empty())
            .or_else(|| url.get("token").map(|s| s.to_string()))?;
        // Reject tokens with invalid percent-encoding or whitespace-only
        if token.trim().is_empty() || token.contains('%') { return None; }
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Chanify", service_url: Some("https://chanify.net"), setup_url: None, protocols: vec!["chanify"], description: "Send notifications via Chanify.", attachment_support: false } }
}
#[async_trait]
impl Notify for Chanify {
    fn schemas(&self) -> &[&str] { &["chanify"] }
    fn service_name(&self) -> &str { "Chanify" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://api.chanify.net/v1/sender/{}", self.token);
        let params = [("title", ctx.title.as_str()), ("text", ctx.body.as_str())];
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "chanify://abc123",
            "chanify://?token=abc123",
            "chanify://token",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "chanify://",
            "chanify://:@/",
            "chanify://%badtoken%",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_struct_fields() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "chanify://my-secret-token"
        ).unwrap();
        let obj = Chanify::from_url(&parsed).unwrap();
        assert_eq!(obj.token, "my-secret-token");
    }

    #[test]
    fn test_from_url_token_query_param() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "chanify://?token=abc123"
        ).unwrap();
        let obj = Chanify::from_url(&parsed).unwrap();
        assert_eq!(obj.token, "abc123");
    }

    #[test]
    fn test_service_details() {
        let details = Chanify::static_details();
        assert_eq!(details.service_name, "Chanify");
        assert_eq!(details.protocols, vec!["chanify"]);
        assert!(!details.attachment_support);
    }
}
