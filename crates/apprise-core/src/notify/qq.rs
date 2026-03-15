use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Qq { token: String, verify_certificate: bool, tags: Vec<String> }
impl Qq {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.get("token").map(|s| s.to_string())
            .or_else(|| {
                if url.schema == "https" || url.schema == "http" {
                    url.path_parts.last().cloned()
                } else {
                    url.host.clone().filter(|h| !h.is_empty())
                }
            })?;
        if token.contains('!') || token.contains('%') || token.trim().is_empty() { return None; }
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "QQ (Qmsg)", service_url: Some("https://qmsg.zendee.cn"), setup_url: None, protocols: vec!["qq"], description: "Send notifications via QQ Qmsg.", attachment_support: false } }
}
#[async_trait]
impl Notify for Qq {
    fn schemas(&self) -> &[&str] { &["qq"] }
    fn service_name(&self) -> &str { "QQ (Qmsg)" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
        let url = format!("https://qmsg.zendee.cn/send/{}", self.token);
        let params = [("msg", msg.as_str())];
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).form(&params).send().await?;
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
            "qq://abc123def456ghi789jkl012mno345pq",
            "qq://?token=abc123def456ghi789jkl012mno345pq",
            "https://qmsg.zendee.cn/send/abc123def456ghi789jkl012mno345pq",
            "qq://ffffffffffffffffffffffffffffffff",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "qq://",
            "qq://invalid!",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_token_from_host() {
        let parsed = ParsedUrl::parse("qq://abc123def456ghi789jkl012mno345pq").unwrap();
        let q = Qq::from_url(&parsed).unwrap();
        assert_eq!(q.token, "abc123def456ghi789jkl012mno345pq");
    }

    #[test]
    fn test_from_url_token_from_param() {
        let parsed = ParsedUrl::parse("qq://?token=abc123def456ghi789jkl012mno345pq").unwrap();
        let q = Qq::from_url(&parsed).unwrap();
        assert_eq!(q.token, "abc123def456ghi789jkl012mno345pq");
    }

    #[test]
    fn test_from_url_https_format() {
        let parsed = ParsedUrl::parse(
            "https://qmsg.zendee.cn/send/abc123def456ghi789jkl012mno345pq"
        ).unwrap();
        let q = Qq::from_url(&parsed).unwrap();
        assert_eq!(q.token, "abc123def456ghi789jkl012mno345pq");
    }

    #[test]
    fn test_static_details() {
        let details = Qq::static_details();
        assert_eq!(details.service_name, "QQ (Qmsg)");
        assert_eq!(details.service_url, Some("https://qmsg.zendee.cn"));
        assert!(details.protocols.contains(&"qq"));
        assert!(!details.attachment_support);
    }
}
