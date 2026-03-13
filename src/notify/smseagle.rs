use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct SmsEagle { host: String, port: Option<u16>, user: String, password: String, targets: Vec<String>, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl SmsEagle {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let user = url.user.clone().unwrap_or_else(|| "admin".to_string());
        let password = url.password.clone().unwrap_or_default();
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { host, port: url.port, user, password, targets, secure: url.schema == "smseagles", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SMSEagle", service_url: Some("https://smseagle.eu"), setup_url: None, protocols: vec!["smseagle", "smseagles"], description: "Send SMS via SMSEagle hardware gateway.", attachment_support: false } }
}
#[async_trait]
impl Notify for SmsEagle {
    fn schemas(&self) -> &[&str] { &["smseagle", "smseagles"] }
    fn service_name(&self) -> &str { "SMSEagle" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!([{"method": "sms.send_sms", "params": { "login": self.user, "pass": self.password, "to": target, "message": msg }}]);
            let url = format!("{}://{}{}/index.php/jsonrpc/sms", schema, self.host, port_str);
            let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
