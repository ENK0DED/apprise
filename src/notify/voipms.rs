use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct VoipMs { user: String, password: String, did: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl VoipMs {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        let did = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { user, password, did, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "VoIP.ms", service_url: Some("https://voip.ms"), setup_url: None, protocols: vec!["voipms"], description: "Send SMS via VoIP.ms.", attachment_support: false } }
}
#[async_trait]
impl Notify for VoipMs {
    fn schemas(&self) -> &[&str] { &["voipms"] }
    fn service_name(&self) -> &str { "VoIP.ms" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut all_ok = true;
        for target in &self.targets {
            let url = format!("https://voip.ms/api/v1/rest.php?api_username={}&api_password={}&method=sendSMS&did={}&dst={}&message={}",
                urlencoding::encode(&self.user), urlencoding::encode(&self.password), urlencoding::encode(&self.did), urlencoding::encode(target), urlencoding::encode(&msg));
            let resp = client.get(&url).header("User-Agent", APP_ID).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "voipms://",
            "voipms://@:",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
