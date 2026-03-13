use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct FortySixElks { user: String, password: String, from_phone: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl FortySixElks {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        let from_phone = url.host.clone().unwrap_or_else(|| "Apprise".to_string());
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { user, password, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "46elks", service_url: Some("https://46elks.com"), setup_url: None, protocols: vec!["46elks", "elks"], description: "Send SMS via 46elks.", attachment_support: false } }
}
#[async_trait]
impl Notify for FortySixElks {
    fn schemas(&self) -> &[&str] { &["46elks", "elks"] }
    fn service_name(&self) -> &str { "46elks" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let params = [("to", target.as_str()), ("from", self.from_phone.as_str()), ("message", msg.as_str())];
            let resp = client.post("https://api.46elks.com/a1/sms").header("User-Agent", APP_ID).basic_auth(&self.user, Some(&self.password)).form(&params).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
