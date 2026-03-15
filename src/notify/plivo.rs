use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Plivo { auth_id: String, auth_token: String, from_phone: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Plivo {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // plivo://auth_id@auth_token/phone or plivo://?id=X&token=Y&from=Z&to=P
        let (auth_id, auth_token, from_phone) = if let Some(id) = url.get("id") {
            let token = url.get("token").map(|s| s.to_string())?;
            let from = url.get("from").or_else(|| url.get("source")).map(|s| s.to_string())
                .or_else(|| url.host.clone().filter(|h| !h.is_empty() && h != "_"))?;
            (id.to_string(), token, from)
        } else if url.password.is_some() {
            (url.user.clone()?, url.password.clone()?, url.host.clone()?)
        } else {
            // plivo://auth_id@auth_token/phone
            let auth_id = url.user.clone()?;
            let auth_token = url.host.clone()?;
            let from = url.get("from").or_else(|| url.get("source")).map(|s| s.to_string())
                .unwrap_or_default();
            (auth_id, auth_token, from)
        };
        if auth_id.is_empty() || auth_token.is_empty() { return None; }
        // Validate auth_id (20+ chars) and auth_token (30+ chars)
        if auth_id.len() < 20 { return None; }
        if auth_token.len() < 30 { return None; }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        // Validate targets - each must have at least 10 digits
        for t in &targets {
            let digits: String = t.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() < 10 { return None; }
        }
        Some(Self { auth_id, auth_token, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Plivo", service_url: Some("https://plivo.com"), setup_url: None, protocols: vec!["plivo"], description: "Send SMS via Plivo.", attachment_support: false } }
}
#[async_trait]
impl Notify for Plivo {
    fn schemas(&self) -> &[&str] { &["plivo"] }
    fn service_name(&self) -> &str { "Plivo" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let url = format!("https://api.plivo.com/v1/Account/{}/Message/", self.auth_id);
        let payload = json!({ "src": self.from_phone, "recipients": self.targets.join(","), "text": msg });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).basic_auth(&self.auth_id, Some(&self.auth_token)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "plivo://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
