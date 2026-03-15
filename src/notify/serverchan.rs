use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct ServerChan { token: String, verify_certificate: bool, tags: Vec<String> }
impl ServerChan {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric()) { return None; }
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "ServerChan", service_url: Some("https://sct.ftqq.com"), setup_url: None, protocols: vec!["schan"], description: "Send notifications via ServerChan (WeChat).", attachment_support: false } }
}
#[async_trait]
impl Notify for ServerChan {
    fn schemas(&self) -> &[&str] { &["schan"] }
    fn service_name(&self) -> &str { "ServerChan" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://sctapi.ftqq.com/{}.send", self.token);
        let params = [("title", ctx.title.as_str()), ("desp", ctx.body.as_str())];
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "schan://12345678",
            "schan://aaaaaaaa",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "schan://",
            "schan://a_bd_/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
