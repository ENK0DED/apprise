use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Brevo { apikey: String, from_email: String, to: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Brevo {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let from_email = url.path_parts.first()?.clone();
        let to: Vec<String> = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
        if to.is_empty() { return None; }
        Some(Self { apikey, from_email, to, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Brevo (Sendinblue)", service_url: Some("https://brevo.com"), setup_url: None, protocols: vec!["brevo"], description: "Send email via Brevo (formerly Sendinblue).", attachment_support: false } }
}
#[async_trait]
impl Notify for Brevo {
    fn schemas(&self) -> &[&str] { &["brevo"] }
    fn service_name(&self) -> &str { "Brevo (Sendinblue)" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let to_list: Vec<_> = self.to.iter().map(|e| json!({ "email": e })).collect();
        let payload = json!({ "sender": { "email": self.from_email }, "to": to_list, "subject": ctx.title, "textContent": ctx.body });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.brevo.com/v3/smtp/email").header("User-Agent", APP_ID).header("api-key", self.apikey.as_str()).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 201 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
