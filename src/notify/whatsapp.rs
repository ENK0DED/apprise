use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct WhatsApp { token: String, phone_id: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl WhatsApp {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // whatsapp://token@phone[/to1/to2]
        // whatsapp://template:token@phone[/to1/to2]
        // whatsapp://_?token=T&from=F&to=T
        let (token, phone_id) = if let Some(tok) = url.get("token") {
            let phone = url.get("from").or_else(|| url.get("source")).map(|s| s.to_string())?;
            (tok.to_string(), phone)
        } else if url.password.is_some() {
            (url.password.clone()?, url.host.clone()?)
        } else {
            // whatsapp://token@phone
            let token = url.user.clone()?;
            let phone = url.host.clone()?;
            (token, phone)
        };
        if token.is_empty() || phone_id.is_empty() || phone_id == "_" { return None; }
        // Reject whitespace in token
        if token.chars().any(|c| c.is_whitespace()) { return None; }
        // If there's a user (template name), validate it's not all whitespace
        if let Some(ref user) = url.user {
            if user.trim().is_empty() { return None; }
        }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        // Validate lang if provided (must be XX_XX format, 5 chars)
        if let Some(lang) = url.get("lang") {
            if !lang.is_empty() && (lang.len() != 5 || !lang.contains('_')) { return None; }
        }
        // Validate template params (:N=value, :body=N, :type=N)
        let valid_param_keys = ["body", "type", "header"];
        let mut param_indices = std::collections::HashSet::new();
        for (key, val) in &url.qsd {
            if key.starts_with(':') {
                let param_key = &key[1..];
                if param_key.is_empty() { return None; }
                if val.is_empty() { return None; }
                // Check for numeric index or named key
                if let Ok(idx) = param_key.parse::<u32>() {
                    // Numeric param
                    if !param_indices.insert(idx) { return None; } // duplicate
                } else if valid_param_keys.contains(&param_key) {
                    // Named param - value should be numeric
                    if let Ok(idx) = val.parse::<u32>() {
                        if !param_indices.insert(idx) { return None; } // duplicate
                    }
                } else {
                    return None; // invalid param key
                }
            }
        }
        Some(Self { token, phone_id, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "WhatsApp", service_url: Some("https://www.whatsapp.com"), setup_url: None, protocols: vec!["whatsapp"], description: "Send messages via WhatsApp Cloud API.", attachment_support: false } }
}
#[async_trait]
impl Notify for WhatsApp {
    fn schemas(&self) -> &[&str] { &["whatsapp"] }
    fn service_name(&self) -> &str { "WhatsApp" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!({ "messaging_product": "whatsapp", "to": target, "type": "text", "text": { "body": msg } });
            let url = format!("https://graph.facebook.com/v17.0/{}/messages", self.phone_id);
            let resp = client.post(&url).header("User-Agent", APP_ID).bearer_auth(&self.token).json(&payload).send().await?;
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
            "whatsapp://",
            "whatsapp://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
