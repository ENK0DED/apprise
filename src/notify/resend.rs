use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Resend { apikey: String, from_email: String, to: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Resend {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let from_email = url.path_parts.first()?.clone();
        let to: Vec<String> = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
        if to.is_empty() { return None; }
        Some(Self { apikey, from_email, to, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Resend", service_url: Some("https://resend.com"), setup_url: None, protocols: vec!["resend"], description: "Send email via Resend.", attachment_support: false } }
}
#[async_trait]
impl Notify for Resend {
    fn schemas(&self) -> &[&str] { &["resend"] }
    fn service_name(&self) -> &str { "Resend" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let payload = json!({ "from": self.from_email, "to": self.to, "subject": ctx.title, "text": ctx.body });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.resend.com/emails").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.apikey)).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 201 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
