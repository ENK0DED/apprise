use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct SmsManager { apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl SmsManager {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SmsManager", service_url: Some("https://smsmanager.cz"), setup_url: None, protocols: vec!["smsmanager"], description: "Send SMS via SmsManager (CZ).", attachment_support: false } }
}
#[async_trait]
impl Notify for SmsManager {
    fn schemas(&self) -> &[&str] { &["smsmanager"] }
    fn service_name(&self) -> &str { "SmsManager" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let url = format!("https://http.smsmanager.cz/send?apikey={}&number={}&message={}&type=promotional", self.apikey, urlencoding::encode(target), urlencoding::encode(&msg));
            let resp = client.get(&url).header("User-Agent", APP_ID).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
