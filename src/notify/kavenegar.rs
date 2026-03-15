use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Kavenegar { apikey: String, targets: Vec<String>, sender: Option<String>, verify_certificate: bool, tags: Vec<String> }
impl Kavenegar {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        let sender = url.get("from").map(|s| s.to_string());
        Some(Self { apikey, targets, sender, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Kavenegar", service_url: Some("https://kavenegar.com"), setup_url: None, protocols: vec!["kavenegar"], description: "Send SMS via Kavenegar.", attachment_support: false } }
}
#[async_trait]
impl Notify for Kavenegar {
    fn schemas(&self) -> &[&str] { &["kavenegar"] }
    fn service_name(&self) -> &str { "Kavenegar" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let receptor = self.targets.join(",");
        let mut url = format!("https://api.kavenegar.com/v1/{}/sms/send.json?receptor={}&message={}", self.apikey, urlencoding::encode(&receptor), urlencoding::encode(&msg));
        if let Some(ref s) = self.sender { url.push_str(&format!("&sender={}", urlencoding::encode(s))); }
        let client = build_client(self.verify_certificate)?;
        let resp = client.get(&url).header("User-Agent", APP_ID).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "kavenegar://",
            "kavenegar://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
