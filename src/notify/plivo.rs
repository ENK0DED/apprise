use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Plivo { auth_id: String, auth_token: String, from_phone: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Plivo {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let auth_id = url.user.clone()?;
        let auth_token = url.password.clone()?;
        let from_phone = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { auth_id, auth_token, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Plivo", service_url: Some("https://plivo.com"), setup_url: None, protocols: vec!["plivo"], description: "Send SMS via Plivo.", attachment_support: false } }
}
#[async_trait]
impl Notify for Plivo {
    fn schemas(&self) -> &[&str] { &["plivo"] }
    fn service_name(&self) -> &str { "Plivo" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let url = format!("https://api.plivo.com/v1/Account/{}/Message/", self.auth_id);
        let payload = json!({ "src": self.from_phone, "recipients": self.targets.join(","), "text": msg });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).basic_auth(&self.auth_id, Some(&self.auth_token)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "plivo://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
