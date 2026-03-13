use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct BurstSms { apikey: String, api_secret: String, from_phone: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl BurstSms {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.user.clone()?;
        let api_secret = url.password.clone()?;
        let from_phone = url.host.clone().unwrap_or_default();
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { apikey, api_secret, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Burst SMS", service_url: Some("https://burstsms.com.au"), setup_url: None, protocols: vec!["burstsms"], description: "Send SMS via Burst SMS.", attachment_support: false } }
}
#[async_trait]
impl Notify for BurstSms {
    fn schemas(&self) -> &[&str] { &["burstsms"] }
    fn service_name(&self) -> &str { "Burst SMS" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let params = [("to", target.as_str()), ("from", self.from_phone.as_str()), ("message", msg.as_str())];
            let resp = client.post("https://api.transmitsms.com/send-sms.json").header("User-Agent", APP_ID).basic_auth(&self.apikey, Some(&self.api_secret)).form(&params).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
