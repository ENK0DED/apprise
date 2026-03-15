use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Sinch { service_plan_id: String, api_token: String, from_phone: String, targets: Vec<String>, region: String, verify_certificate: bool, tags: Vec<String> }
impl Sinch {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let service_plan_id = url.get("spi").map(|s| s.to_string()).or_else(|| url.user.clone())?;
        let api_token = url.get("token").map(|s| s.to_string()).or_else(|| url.password.clone())?;
        let from_phone = url.get("from").or_else(|| url.get("source"))
            .map(|s| s.to_string())
            .or_else(|| {
                let h = url.host.clone().unwrap_or_default();
                if h.is_empty() || h == "_" { None } else { Some(h) }
            })?;
        // Validate from_phone: must be 5-6 digits (short code) or 11-14 digits (full phone)
        let digits: String = from_phone.chars().filter(|c| c.is_ascii_digit()).collect();
        let valid_phone = (digits.len() >= 11 && digits.len() <= 14) || digits.len() == 5 || digits.len() == 6;
        if !valid_phone { return None; }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        let region = url.get("region").unwrap_or("us").to_string();
        // Validate region: must be 2 lowercase alpha chars
        if !region.chars().all(|c| c.is_ascii_alphabetic()) || region.len() != 2 { return None; }
        let valid_regions = ["us", "eu"];
        if !valid_regions.contains(&region.to_lowercase().as_str()) { return None; }
        Some(Self { service_plan_id, api_token, from_phone, targets, region, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Sinch", service_url: Some("https://sinch.com"), setup_url: None, protocols: vec!["sinch"], description: "Send SMS via Sinch.", attachment_support: false } }
}
#[async_trait]
impl Notify for Sinch {
    fn schemas(&self) -> &[&str] { &["sinch"] }
    fn service_name(&self) -> &str { "Sinch" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let url = format!("https://{}.sms.api.sinch.com/xms/v1/{}/batches", self.region, self.service_plan_id);
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!({ "from": self.from_phone, "to": [target], "body": msg });
            let resp = client.post(&url).header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.api_token)).json(&payload).send().await?;
            if !resp.status().is_success() && resp.status().as_u16() != 201 { all_ok = false; }
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
            "sinch://",
            "sinch://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
