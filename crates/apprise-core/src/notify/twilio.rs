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
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls: Vec<String> = vec![
            "twilio://".into(),
            "twilio://:@/".into(),
            // Just SID, no token
            format!("twilio://AC{}@12345678", "a".repeat(32)),
            // SID and token but invalid from (_)
            format!("twilio://AC{}:{}@_", "a".repeat(32), "b".repeat(32)),
            // 9-digit from (not short code 5-6, not full 11+)
            format!("twilio://AC{}:{}@{}", "a".repeat(32), "b".repeat(32), "3".repeat(9)),
            // Invalid method
            format!("twilio://AC{}:{}@{}?method=mms", "a".repeat(32), "b".repeat(32), "5".repeat(11)),
            // w: prefix with call method - incompatible
            format!("twilio://AC{}:{}@{}?method=call", "a".repeat(32), "b".repeat(32), format!("w:{}", "5".repeat(11))),
            // Invalid short-code w: prefix
            format!("twilio://AC{}:{}@w:12345/{}/{}", "a".repeat(32), "b".repeat(32), "4".repeat(11), "5".repeat(11)),
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            // Short code (5 digits)
            format!("twilio://AC{}:{}@{}", "a".repeat(32), "b".repeat(32), "3".repeat(5)),
            // Short code (5 digits) with target
            format!("twilio://AC{}:{}@12345/{}", "a".repeat(32), "b".repeat(32), "4".repeat(11)),
            // Short code (6 digits)
            format!("twilio://AC{}:{}@123456/{}", "a".repeat(32), "b".repeat(32), "4".repeat(11)),
            // Full 11-digit from, targets with valid/invalid mixed
            format!("twilio://AC{}:{}@{}/123/{}/abcd/w:{}",
                "a".repeat(32), "b".repeat(32), "3".repeat(11), "9".repeat(15), 8u64 * 11),
            // Phone number as from, self-text
            format!("twilio://AC{}:{}@{}", "a".repeat(32), "b".repeat(32), "5".repeat(11)),
            // Explicit sms method
            format!("twilio://AC{}:{}@{}?method=sms", "a".repeat(32), "b".repeat(32), "5".repeat(11)),
            // Query param form
            format!("twilio://_?sid=AC{}&token={}&from={}", "a".repeat(32), "b".repeat(32), "5".repeat(11)),
            // Query param with source=
            format!("twilio://_?sid=AC{}&token={}&source={}", "a".repeat(32), "b".repeat(32), "5".repeat(11)),
            // Query param with to=
            format!("twilio://_?sid=AC{}&token={}&from={}&to={}",
                "a".repeat(32), "b".repeat(32), "5".repeat(11), "7".repeat(13)),
            // Whatsapp target
            format!("twilio://_?sid=AC{}&token={}&from={}&to=w:{}",
                "a".repeat(32), "b".repeat(32), "5".repeat(11), "6".repeat(11)),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_fields() {
        let sid = format!("AC{}", "a".repeat(32));
        let token = "b".repeat(32);
        let from = "15551233456";
        let target = "15559876543";
        let url_str = format!("twilio://{}:{}@{}/{}", sid, token, from, target);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let tw = Twilio::from_url(&parsed).unwrap();
        assert_eq!(tw.account_sid, sid);
        assert_eq!(tw.auth_token, token);
        assert_eq!(tw.from_phone, from);
        assert_eq!(tw.targets.len(), 1);
        assert_eq!(tw.targets[0], target);
    }

    #[test]
    fn test_from_url_query_params() {
        let sid = format!("AC{}", "c".repeat(32));
        let token = "d".repeat(32);
        let from = "15551112222";
        let to = "15553334444";
        let url_str = format!("twilio://_?sid={}&token={}&from={}&to={}", sid, token, from, to);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let tw = Twilio::from_url(&parsed).unwrap();
        assert_eq!(tw.account_sid, sid);
        assert_eq!(tw.auth_token, token);
        assert_eq!(tw.from_phone, from);
        assert!(tw.targets.contains(&to.to_string()));
    }

    #[test]
    fn test_service_details() {
        let details = Twilio::static_details();
        assert_eq!(details.service_name, "Twilio");
        assert_eq!(details.service_url, Some("https://twilio.com"));
        assert!(details.protocols.contains(&"twilio"));
        assert!(!details.attachment_support);
    }

    #[test]
    fn test_short_code_from() {
        // 5-digit short code
        let url_str = format!("twilio://AC{}:{}@33333/{}", "a".repeat(32), "b".repeat(32), "4".repeat(11));
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let tw = Twilio::from_url(&parsed).unwrap();
        assert_eq!(tw.from_phone, "33333");

        // 6-digit short code
        let url_str = format!("twilio://AC{}:{}@333333/{}", "a".repeat(32), "b".repeat(32), "4".repeat(11));
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let tw = Twilio::from_url(&parsed).unwrap();
        assert_eq!(tw.from_phone, "333333");
    }

    #[test]
    fn test_multiple_targets() {
        let url_str = format!("twilio://AC{}:{}@{}/{}/{}", "a".repeat(32), "b".repeat(32),
            "3".repeat(11), "4".repeat(11), "5".repeat(11));
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let tw = Twilio::from_url(&parsed).unwrap();
        assert_eq!(tw.targets.len(), 2);
    }
}
