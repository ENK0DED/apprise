use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct ParsePlatform { host: String, port: Option<u16>, app_id: String, master_key: String, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl ParsePlatform {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let app_id = url.user.clone()?;
        let master_key = url.password.clone()?;
        // Validate device param if provided
        if let Some(device) = url.get("device") {
            match device.to_lowercase().as_str() {
                "ios" | "android" | "" => {}
                _ => return None,
            }
        }
        Some(Self { host, port: url.port, app_id, master_key, secure: url.schema == "parseps", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Parse Platform", service_url: Some("https://parseplatform.org"), setup_url: None, protocols: vec!["parsep", "parseps"], description: "Send push via Parse Platform.", attachment_support: false } }
}
#[async_trait]
impl Notify for ParsePlatform {
    fn schemas(&self) -> &[&str] { &["parsep", "parseps"] }
    fn service_name(&self) -> &str { "Parse Platform" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/parse/push/", schema, self.host, port_str);
        let payload = json!({ "where": {}, "data": { "title": ctx.title, "alert": ctx.body } });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).header("X-Parse-Application-Id", self.app_id.as_str()).header("X-Parse-Master-Key", self.master_key.as_str()).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "parsep://app_id:master_key@localhost:8080?device=ios",
            "parseps://app_id:master_key@localhost",
            "parseps://app_id:master_key@localhost",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "parsep://",
            "parsep://:@/",
            "parsep://app_id:master_key@localhost?device=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
