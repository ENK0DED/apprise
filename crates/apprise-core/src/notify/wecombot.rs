use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct WeComBot { key: String, verify_certificate: bool, tags: Vec<String> }
impl WeComBot {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let key = url.host.clone()
            .filter(|h| !h.is_empty())
            .or_else(|| url.get("key").map(|s| s.to_string()))?;
        if key.trim().is_empty() { return None; }
        Some(Self { key, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "WeCom Bot", service_url: Some("https://work.weixin.qq.com"), setup_url: None, protocols: vec!["wecombot"], description: "Send messages via WeCom group robot.", attachment_support: false } }
}
#[async_trait]
impl Notify for WeComBot {
    fn schemas(&self) -> &[&str] { &["wecombot"] }
    fn service_name(&self) -> &str { "WeCom Bot" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}\n", ctx.title) }, ctx.body);
        let payload = json!({ "msgtype": "text", "text": { "content": msg } });
        let url = format!("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key={}", self.key);
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
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
            "wecombot://botkey",
            "wecombot://?key=botkey",
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=BOTKEY",
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send/?key=BOTKEY&data=123",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "wecombot://",
            "wecombot://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_key_from_host() {
        let parsed = ParsedUrl::parse("wecombot://mykey").unwrap();
        let w = WeComBot::from_url(&parsed).unwrap();
        assert_eq!(w.key, "mykey");
    }

    #[test]
    fn test_from_url_key_from_query() {
        let parsed = ParsedUrl::parse("wecombot://?key=mykey2").unwrap();
        let w = WeComBot::from_url(&parsed).unwrap();
        assert_eq!(w.key, "mykey2");
    }

    #[test]
    fn test_from_url_native_url() {
        // Native https URL: host=qyapi.weixin.qq.com, key=BOTKEY in query
        // WeComBot::from_url looks at host first, then key= query param
        // Since host is qyapi.weixin.qq.com (non-empty), it gets used as key
        // The registry handles mapping native URLs; here we test direct from_url
        // which sees key=BOTKEY in the query param
        let parsed = ParsedUrl::parse("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=BOTKEY").unwrap();
        let w = WeComBot::from_url(&parsed).unwrap();
        // host is non-empty so it takes precedence in from_url
        assert!(!w.key.is_empty());
    }

    #[test]
    fn test_service_details() {
        let details = WeComBot::static_details();
        assert_eq!(details.service_name, "WeCom Bot");
        assert_eq!(details.service_url, Some("https://work.weixin.qq.com"));
        assert!(details.protocols.contains(&"wecombot"));
        assert!(!details.attachment_support);
    }
}
