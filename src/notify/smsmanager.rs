use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct SmsManager { apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl SmsManager {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Validate gateway if provided
        if let Some(gw) = url.get("gateway") {
            let g = gw.to_lowercase();
            if !["economy", "low", "high", "standard"].contains(&g.as_str()) { return None; }
        }
        let apikey = url.get("key").map(|s| s.to_string()).or_else(|| url.user.clone())?;
        if apikey.is_empty() { return None; }
        let mut targets = Vec::new();
        if let Some(h) = url.host.as_deref() {
            if !h.is_empty() && h != "_" { targets.push(h.to_string()); }
        }
        targets.extend(url.path_parts.clone());
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if targets.is_empty() { return None; }
        Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SmsManager", service_url: Some("https://smsmanager.cz"), setup_url: None, protocols: vec!["smsmanager", "smsmgr"], description: "Send SMS via SmsManager (CZ).", attachment_support: false } }
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
            let params = [
                ("apikey", self.apikey.as_str()),
                ("number", target.as_str()),
                ("message", msg.as_str()),
                ("type", "promotional"),
            ];
            let resp = client.post("https://http-api.smsmanager.cz/Send")
                .header("User-Agent", APP_ID)
                .form(&params)
                .send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "smsmgr://bbbbbbbbbb@33333",
            "smsmgr://zzzzzzzzzz@123/33333333333/abcd/+44444444444",
            "smsmgr://bbbbb@44444444444?batch=y",
            "smsmgr://aaaaaaaaaa@11111111111?gateway=low",
            "smsmgr://11111111111?key=aaaaaaaaaa&from=user",
            "smsmgr://_?to=11111111111,22222222222&key=bbbbbbbbbb&sender=5555555555555",
            "smsmgr://aaaaaaaaaa@11111111111",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "smsmgr://",
            "smsmgr://:@/",
            "smsmgr://aaaaaaaaaa@11111111111?gateway=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
