use async_trait::async_trait;
use serde_json::{json, Value};
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct SendPulse { client_id: String, client_secret: String, from_email: String, to: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl SendPulse {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let client_id = url.user.clone()?;
        let client_secret = url.password.clone()?;
        let from_email = url.host.clone().map(|h| format!("noreply@{}", h)).unwrap_or_else(|| "noreply@example.com".to_string());
        let to: Vec<String> = url.path_parts.clone();
        if to.is_empty() { return None; }
        Some(Self { client_id, client_secret, from_email, to, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SendPulse", service_url: Some("https://sendpulse.com"), setup_url: None, protocols: vec!["sendpulse"], description: "Send email via SendPulse.", attachment_support: false } }
}
#[async_trait]
impl Notify for SendPulse {
    fn schemas(&self) -> &[&str] { &["sendpulse"] }
    fn service_name(&self) -> &str { "SendPulse" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let token_params = [("grant_type", "client_credentials"), ("client_id", self.client_id.as_str()), ("client_secret", self.client_secret.as_str())];
        let token_resp: Value = client.post("https://api.sendpulse.com/oauth/access_token").header("User-Agent", APP_ID).form(&token_params).send().await?.json().await.map_err(|e| NotifyError::Auth(e.to_string()))?;
        let access_token = token_resp["access_token"].as_str().ok_or_else(|| NotifyError::Auth("No token".into()))?;
        let to_list: Vec<_> = self.to.iter().map(|e| json!({ "email": e, "name": e })).collect();
        let payload = json!({ "html": ctx.body, "text": ctx.body, "subject": ctx.title, "from": { "name": "Apprise", "email": self.from_email }, "to": to_list });
        let resp = client.post("https://api.sendpulse.com/smtp/emails").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", access_token)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
