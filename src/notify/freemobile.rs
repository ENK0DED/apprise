use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct FreeMobile { user: String, password: String, verify_certificate: bool, tags: Vec<String> }
impl FreeMobile {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        Some(Self { user, password, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Free Mobile", service_url: Some("https://mobile.free.fr"), setup_url: None, protocols: vec!["freemobile"], description: "Send SMS via Free Mobile (France).", attachment_support: false } }
}
#[async_trait]
impl Notify for FreeMobile {
    fn schemas(&self) -> &[&str] { &["freemobile"] }
    fn service_name(&self) -> &str { "Free Mobile" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let payload = json!({ "user": self.user, "pass": self.password, "msg": msg });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://smsapi.free-mobile.fr/sendmsg").header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
