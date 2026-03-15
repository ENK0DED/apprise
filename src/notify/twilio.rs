use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Twilio {
    account_sid: String,
    auth_token: String,
    from_phone: String,
    targets: Vec<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Twilio {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // twilio://SID:token@from_phone[/to1/to2]
        // or twilio://_?sid=SID&token=TOKEN&from=PHONE&to=PHONE
        let (account_sid, auth_token, from_phone) = if let Some(sid) = url.get("sid") {
            let token = url.get("token").map(|s| s.to_string())?;
            let from = url.get("from").or_else(|| url.get("source")).map(|s| s.to_string())?;
            (sid.to_string(), token, from)
        } else {
            let sid = url.user.clone()?;
            let token = url.password.clone()?;
            let from = url.host.clone()?;
            (sid, token, from)
        };
        // Validate from_phone: reject colons (like w:12345), _
        if from_phone.contains(':') || from_phone == "_" { return None; }
        // Validate from_phone: short code (5-6 digits) or full number (11+ digits)
        let from_digits: String = from_phone.chars().filter(|c| c.is_ascii_digit()).collect();
        if from_digits.is_empty() { return None; }
        let len = from_digits.len();
        if !(len >= 5 && len <= 6) && len < 11 { return None; }
        // Validate method if provided
        if let Some(method) = url.get("method") {
            match method.to_lowercase().as_str() {
                "sms" | "call" | "" => {}
                _ => return None,
            }
            // w: prefix means whatsapp - call method requires non-whatsapp from
            if method.to_lowercase() == "call" && from_phone.starts_with("w:") { return None; }
        }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        Some(Self { account_sid, auth_token, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Twilio", service_url: Some("https://twilio.com"), setup_url: None, protocols: vec!["twilio"], description: "Send SMS via Twilio.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Twilio {
    fn schemas(&self) -> &[&str] { &["twilio"] }
    fn service_name(&self) -> &str { "Twilio" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    fn body_maxlen(&self) -> usize { 160 }
    fn title_maxlen(&self) -> usize { 0 }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json", self.account_sid);
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut all_ok = true;
        for target in &self.targets {
            let params = [("From", self.from_phone.as_str()), ("To", target.as_str()), ("Body", msg.as_str())];
            let resp = client.post(&url).header("User-Agent", APP_ID).basic_auth(&self.account_sid, Some(&self.auth_token)).form(&params).send().await?;
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
            "twilio://",
            "twilio://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
