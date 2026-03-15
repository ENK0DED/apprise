use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct LaMetric { apikey: String, app_id: Option<String>, verify_certificate: bool, tags: Vec<String> }
impl LaMetric {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let app_id = url.path_parts.first().cloned();
        Some(Self { apikey, app_id, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "LaMetric", service_url: Some("https://lametric.com"), setup_url: None, protocols: vec!["lametric", "lametrics"], description: "Send notifications to LaMetric devices.", attachment_support: false } }
}
#[async_trait]
impl Notify for LaMetric {
    fn schemas(&self) -> &[&str] { &["lametric", "lametrics"] }
    fn service_name(&self) -> &str { "LaMetric" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        let payload = json!({ "frames": [{ "index": 0, "text": text, "icon": "i555" }] });
        let url = if let Some(ref aid) = self.app_id { format!("https://developer.lametric.com/api/v1/dev/widget/update/com.lametric.{}/1", aid) } else { "https://developer.lametric.com/api/v1/dev/widget/update/1".to_string() };
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).header("X-Access-Token", self.apikey.as_str()).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "lametric://192.168.2.8/?mode=device&apikey=abc123",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "lametric://",
            "lametric://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
