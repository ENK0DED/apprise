use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct AfricasTalking { apikey: String, user: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl AfricasTalking {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Validate mode if provided
        if let Some(mode) = url.get("mode") {
            let m = mode.to_lowercase();
            let valid_modes = ["bulksms", "premium", "sandbox"];
            if !valid_modes.iter().any(|v| v.starts_with(&m)) {
                return None;
            }
        }
        // Support ?apikey= query param (host becomes a target in that case)
        let (apikey, mut targets) = if let Some(ak) = url.get("apikey").or_else(|| url.get("key")) {
            let mut t = Vec::new();
            if let Some(h) = url.host.as_deref() {
                if !h.is_empty() && h != "_" { t.push(h.to_string()); }
            }
            (ak.to_string(), t)
        } else {
            (url.host.clone()?, Vec::new())
        };
        if apikey.is_empty() { return None; }
        let user = url.get("user").map(|s| s.to_string())
            .or_else(|| url.user.clone())
            .unwrap_or_else(|| "sandbox".to_string());
        targets.extend(url.path_parts.clone());
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if targets.is_empty() { return None; }
        Some(Self { apikey, user, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Africa's Talking", service_url: Some("https://africastalking.com"), setup_url: None, protocols: vec!["atalk"], description: "Send SMS via Africa's Talking.", attachment_support: false } }
}
#[async_trait]
impl Notify for AfricasTalking {
    fn schemas(&self) -> &[&str] { &["atalk"] }
    fn service_name(&self) -> &str { "Africa's Talking" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let to = self.targets.join(",");
        let params = [("username", self.user.as_str()), ("to", to.as_str()), ("message", msg.as_str())];
        let resp = client.post("https://api.africastalking.com/version1/messaging").header("User-Agent", APP_ID).header("apiKey", self.apikey.as_str()).header("Accept", "application/json").form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "atalk://",
            "atalk://:@/",
            "atalk://user@^/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
