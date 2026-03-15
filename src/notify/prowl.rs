use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Prowl { apikey: String, priority: i32, verify_certificate: bool, tags: Vec<String> }
impl Prowl {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let priority = url.get("priority").and_then(|p| p.parse().ok()).unwrap_or(0);
        Some(Self { apikey, priority, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Prowl", service_url: Some("https://www.prowlapp.com"), setup_url: None, protocols: vec!["prowl"], description: "Send iOS push notifications via Prowl.", attachment_support: false } }
}
#[async_trait]
impl Notify for Prowl {
    fn schemas(&self) -> &[&str] { &["prowl"] }
    fn service_name(&self) -> &str { "Prowl" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let params = [("apikey", self.apikey.as_str()), ("application", "Apprise"), ("event", ctx.title.as_str()), ("description", ctx.body.as_str()), ("priority", &self.priority.to_string())];
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.prowlapp.com/publicapi/add").header("User-Agent", APP_ID).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "prowl://",
            "prowl://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
