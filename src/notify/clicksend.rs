use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct ClickSend { user: String, apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl ClickSend {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone()?;
        let apikey = url.password.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { user, apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "ClickSend", service_url: Some("https://clicksend.com"), setup_url: None, protocols: vec!["clicksend"], description: "Send SMS via ClickSend.", attachment_support: false } }
}
#[async_trait]
impl Notify for ClickSend {
    fn schemas(&self) -> &[&str] { &["clicksend"] }
    fn service_name(&self) -> &str { "ClickSend" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let msgs: Vec<_> = self.targets.iter().map(|t| json!({ "to": t, "body": msg, "source": "Apprise" })).collect();
        let payload = json!({ "messages": msgs });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://rest.clicksend.com/v3/sms/send").header("User-Agent", APP_ID).basic_auth(&self.user, Some(&self.apikey)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
