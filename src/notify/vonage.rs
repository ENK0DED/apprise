use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Vonage {
    apikey: String,
    api_secret: String,
    from_phone: String,
    targets: Vec<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Vonage {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // vonage://apikey:secret@from_phone/to1/to2
        let apikey = url.user.clone()?;
        let api_secret = url.password.clone()?;
        let from_phone = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { apikey, api_secret, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Vonage (Nexmo)", service_url: Some("https://vonage.com"), setup_url: None, protocols: vec!["vonage", "nexmo"], description: "Send SMS via Vonage/Nexmo.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Vonage {
    fn schemas(&self) -> &[&str] { &["vonage", "nexmo"] }
    fn service_name(&self) -> &str { "Vonage (Nexmo)" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut all_ok = true;
        for target in &self.targets {
            let params = [("api_key", self.apikey.as_str()), ("api_secret", self.api_secret.as_str()), ("from", self.from_phone.as_str()), ("to", target.as_str()), ("text", msg.as_str())];
            let resp = client.post("https://rest.nexmo.com/sms/json").header("User-Agent", APP_ID).form(&params).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
