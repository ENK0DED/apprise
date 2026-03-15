use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct HttpSms { apikey: String, from_phone: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl HttpSms {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.user.clone()?;
        let from_phone = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { apikey, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "HttpSMS", service_url: Some("https://httpsms.com"), setup_url: None, protocols: vec!["httpsms"], description: "Send SMS via HttpSMS.", attachment_support: false } }
}
#[async_trait]
impl Notify for HttpSms {
    fn schemas(&self) -> &[&str] { &["httpsms"] }
    fn service_name(&self) -> &str { "HttpSMS" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!({ "content": msg, "from": self.from_phone, "to": target });
            let resp = client.post("https://api.httpsms.com/v1/messages/send").header("User-Agent", APP_ID).header("x-api-key", self.apikey.as_str()).json(&payload).send().await?;
            if !resp.status().is_success() && resp.status().as_u16() != 200 && resp.status().as_u16() != 202 { all_ok = false; }
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
            "httpsms://",
            "httpsms://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
