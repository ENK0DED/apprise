use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct SendGrid {
    apikey: String,
    from_email: String,
    to: Vec<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl SendGrid {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // sendgrid://apikey/from_email/to1/to2
        let apikey = url.host.clone()?;
        let from_email = url.path_parts.first()?.clone();
        let to: Vec<String> = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
        if to.is_empty() { return None; }
        Some(Self { apikey, from_email, to, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "SendGrid", service_url: Some("https://sendgrid.com"), setup_url: None, protocols: vec!["sendgrid"], description: "Send email via SendGrid.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for SendGrid {
    fn schemas(&self) -> &[&str] { &["sendgrid"] }
    fn service_name(&self) -> &str { "SendGrid" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let personalizations: Vec<_> = self.to.iter().map(|t| json!({ "to": [{ "email": t }] })).collect();
        let payload = json!({
            "personalizations": personalizations,
            "from": { "email": self.from_email },
            "subject": ctx.title,
            "content": [{ "type": "text/plain", "value": ctx.body }]
        });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.sendgrid.com/v3/mail/send").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.apikey)).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 202 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
