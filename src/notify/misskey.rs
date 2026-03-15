use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Misskey { host: String, port: Option<u16>, token: String, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Misskey {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let token = url.user.clone()
            .or_else(|| url.get("token").map(|s| s.to_string()))?;
        if token.is_empty() { return None; }
        // Validate visibility if provided
        if let Some(vis) = url.get("visibility") {
            match vis.to_lowercase().as_str() {
                "public" | "home" | "followers" | "specified" | "" => {}
                _ => return None,
            }
        }
        Some(Self { host, port: url.port, token, secure: url.schema == "misskeys", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Misskey", service_url: Some("https://misskey.io"), setup_url: None, protocols: vec!["misskey", "misskeys"], description: "Post to Misskey instances.", attachment_support: false } }
}
#[async_trait]
impl Notify for Misskey {
    fn schemas(&self) -> &[&str] { &["misskey", "misskeys"] }
    fn service_name(&self) -> &str { "Misskey" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/api/notes/create", schema, self.host, port_str);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let payload = json!({ "i": self.token, "text": text, "visibility": "public" });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "misskey://access_token@hostname",
            "misskeys://access_token@hostname",
            "misskey://hostname/?token=abcd123",
            "misskeys://access_token@hostname:8443",
            "misskeys://access_token@hostname?visibility=specified",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "misskey://",
            "misskey://:@/",
            "misskey://hostname",
            "misskey://access_token@hostname?visibility=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
