use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct BulkSms { user: String, password: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl BulkSms {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone().unwrap_or_default();
        let password = url.password.clone().unwrap_or_default();
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        Some(Self { user, password, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "BulkSMS", service_url: Some("https://bulksms.com"), setup_url: None, protocols: vec!["bulksms"], description: "Send SMS via BulkSMS.", attachment_support: false } }
}
#[async_trait]
impl Notify for BulkSms {
    fn schemas(&self) -> &[&str] { &["bulksms"] }
    fn service_name(&self) -> &str { "BulkSMS" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let recipients: Vec<_> = self.targets.iter().map(|t| json!({ "id": t })).collect();
        let payload = json!({ "to": recipients, "body": msg });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.bulksms.com/v1/messages").header("User-Agent", APP_ID).basic_auth(&self.user, Some(&self.password)).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 201 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "bulksms://",
            "bulksms://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

}
