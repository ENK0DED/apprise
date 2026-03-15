use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct FeiShu { token: String, verify_certificate: bool, tags: Vec<String> }
impl FeiShu {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()
            .filter(|h| !h.is_empty())
            .or_else(|| url.get("token").map(|s| s.to_string()))?;
        if token.trim().is_empty() || token.contains('%') { return None; }
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "FeiShu", service_url: Some("https://open.feishu.cn"), setup_url: None, protocols: vec!["feishu"], description: "Send via FeiShu (Lark) bot webhook.", attachment_support: false } }
}
#[async_trait]
impl Notify for FeiShu {
    fn schemas(&self) -> &[&str] { &["feishu"] }
    fn service_name(&self) -> &str { "FeiShu" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://open.feishu.cn/open-apis/bot/v2/hook/{}", self.token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
        let payload = json!({ "msg_type": "text", "content": { "text": text } });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
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
            "feishu://abc123",
            "feishu://?token=abc123",
            "feishu://token",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "feishu://",
            "feishu://:@/",
            "feishu://%badtoken%",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    fn parse_feishu(url: &str) -> FeiShu {
        let parsed = crate::utils::parse::ParsedUrl::parse(url).unwrap();
        FeiShu::from_url(&parsed).unwrap()
    }

    #[test]
    fn test_from_url_token_from_host() {
        let f = parse_feishu("feishu://abc123");
        assert_eq!(f.token, "abc123");
    }

    #[test]
    fn test_from_url_token_from_query() {
        let f = parse_feishu("feishu://?token=abc123");
        assert_eq!(f.token, "abc123");
    }

    #[test]
    fn test_from_url_bad_token_with_percent_returns_none() {
        let parsed = crate::utils::parse::ParsedUrl::parse("feishu://%badtoken%").unwrap();
        assert!(FeiShu::from_url(&parsed).is_none());
    }

    #[test]
    fn test_from_url_empty_returns_none() {
        let parsed = crate::utils::parse::ParsedUrl::parse("feishu://").unwrap();
        assert!(FeiShu::from_url(&parsed).is_none());
    }

    #[test]
    fn test_service_details() {
        let details = FeiShu::static_details();
        assert_eq!(details.service_name, "FeiShu");
        assert_eq!(details.protocols, vec!["feishu"]);
        assert!(!details.attachment_support);
    }
}
