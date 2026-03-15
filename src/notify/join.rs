use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Join { apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Join {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let targets = url.path_parts.clone();
        Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Join", service_url: Some("https://joaoapps.com/join/"), setup_url: None, protocols: vec!["join"], description: "Send notifications via Join.", attachment_support: false } }
}
#[async_trait]
impl Notify for Join {
    fn schemas(&self) -> &[&str] { &["join"] }
    fn service_name(&self) -> &str { "Join" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let device_ids = if self.targets.is_empty() { "group.all".to_string() } else { self.targets.join(",") };
        let url = format!("https://joinjoaomgcd.appspot.com/_ah/api/messaging/v1/sendPush?apikey={}&deviceIds={}&title={}&text={}",
            self.apikey, urlencoding::encode(&device_ids), urlencoding::encode(&ctx.title), urlencoding::encode(&ctx.body));
        let client = build_client(self.verify_certificate)?;
        let resp = client.get(&url).header("User-Agent", APP_ID).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "join://",
            "join://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
