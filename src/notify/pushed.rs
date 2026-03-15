use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Pushed { app_key: String, secret: String, verify_certificate: bool, tags: Vec<String> }
impl Pushed {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // pushed://appkey/appsecret[/#channel/@alias...] or pushed://user:pass@host
        let (app_key, secret) = if url.password.is_some() {
            (url.user.clone()?, url.password.clone()?)
        } else {
            let app_key = url.host.clone()?;
            let secret = url.path_parts.first()?.clone();
            if secret.is_empty() { return None; }
            (app_key, secret)
        };
        if app_key.is_empty() || secret.is_empty() { return None; }
        // Validate: remaining path parts after secret must be # or @ prefixed
        let extra = if url.password.is_some() { &url.path_parts[..] } else { url.path_parts.get(1..).unwrap_or(&[]) };
        for p in extra {
            if !p.starts_with('#') && !p.starts_with('@') && !p.starts_with("%23") {
                return None;
            }
        }
        Some(Self { app_key, secret, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Pushed", service_url: Some("https://pushed.co"), setup_url: None, protocols: vec!["pushed"], description: "Send push notifications via Pushed.", attachment_support: false } }
}
#[async_trait]
impl Notify for Pushed {
    fn schemas(&self) -> &[&str] { &["pushed"] }
    fn service_name(&self) -> &str { "Pushed" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        let payload = json!({ "app_key": self.app_key, "app_secret": self.secret, "target_type": "app", "content": text });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.pushed.co/1/push").header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "pushed://",
            "pushed://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
