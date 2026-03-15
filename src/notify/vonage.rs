use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Vonage {
    apikey: String,
    api_secret: String,
    from_phone: String,
    targets: Vec<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Vonage {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // vonage://apikey:secret@from_phone[/to1/to2]
        // or vonage://_?key=K&secret=S&from=F&to=T
        let (apikey, api_secret, from_phone) = if let Some(key) = url.get("key") {
            let secret = url.get("secret").map(|s| s.to_string())?;
            let from = url.get("from").or_else(|| url.get("source")).map(|s| s.to_string())?;
            (key.to_string(), secret, from)
        } else {
            (url.user.clone()?, url.password.clone()?, url.host.clone()?)
        };
        if apikey.is_empty() || api_secret.is_empty() || from_phone.is_empty() { return None; }
        // Validate from_phone: must have at least 11 digits and be all digits
        let from_digits: String = from_phone.chars().filter(|c| c.is_ascii_digit()).collect();
        if from_digits.len() < 11 { return None; }
        // Reject non-digit characters in from phone (except +)
        if !from_phone.chars().all(|c| c.is_ascii_digit() || c == '+') { return None; }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        // Validate ttl if provided
        if let Some(ttl) = url.get("ttl") {
            let ttl_val: i64 = ttl.parse().ok()?;
            if ttl_val <= 0 { return None; }
        }
        Some(Self { apikey, api_secret, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Vonage (Nexmo)", service_url: Some("https://vonage.com"), setup_url: None, protocols: vec!["vonage", "nexmo"], description: "Send SMS via Vonage/Nexmo.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Vonage {
    fn schemas(&self) -> &[&str] { &["vonage", "nexmo"] }
    fn service_name(&self) -> &str { "Vonage (Nexmo)" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    fn body_maxlen(&self) -> usize { 160 }
    fn title_maxlen(&self) -> usize { 0 }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut all_ok = true;
        for target in &self.targets {
            let params = [("api_key", self.apikey.as_str()), ("api_secret", self.api_secret.as_str()), ("from", self.from_phone.as_str()), ("to", target.as_str()), ("text", msg.as_str())];
            let resp = client.post("https://rest.nexmo.com/sms/json").header("User-Agent", APP_ID).form(&params).send().await?;
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
            "vonage://ACffffffff:gggggggggggggggg@33333333333/123/999999999999999/abcd/",
            "vonage://AChhhhhhhh:iiiiiiiiiiiiiiii@55555555555",
            "vonage://_?key=ACaaaaaaaa&secret=bbbbbbbbbbbbbbbb&from=55555555555",
            "vonage://_?key=ACaaaaaaaa&secret=bbbbbbbbbbbbbbbb&source=55555555555",
            "vonage://_?key=ACaaaaaaaa&secret=bbbbbbbbbbbbbbbb&from=55555555555&to=7777777777777",
            "vonage://ACaaaaaaaa:bbbbbbbbbbbbbbbb@66666666666",
            "nexmo://ACffffffff:gggggggggggggggg@33333333333/123/999999999999999/abcd/",
            "nexmo://AChhhhhhhh:iiiiiiiiiiiiiiii@55555555555",
            "nexmo://_?key=ACaaaaaaaa&secret=bbbbbbbbbbbbbbbb&from=55555555555",
            "nexmo://_?key=ACaaaaaaaa&secret=bbbbbbbbbbbbbbbb&source=55555555555",
            "nexmo://_?key=ACaaaaaaaa&secret=bbbbbbbbbbbbbbbb&from=55555555555&to=7777777777777",
            "nexmo://ACaaaaaaaa:bbbbbbbbbbbbbbbb@66666666666",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "vonage://",
            "vonage://:@/",
            "vonage://ACaaaaaaaa@12345678",
            "vonage://ACaaaaaaaa:bbbbbbbbbbbbbbbb@333333333",
            "vonage://ACbbbbbbbb:cccccccccccccccc@33333333333/?ttl=0",
            "vonage://ACdddddddd:eeeeeeeeeeeeeeee@aaaaaaaaaaa",
            "nexmo://",
            "nexmo://:@/",
            "nexmo://ACaaaaaaaa@12345678",
            "nexmo://ACaaaaaaaa:bbbbbbbbbbbbbbbb@333333333",
            "nexmo://ACbbbbbbbb:cccccccccccccccc@33333333333/?ttl=0",
            "nexmo://ACdddddddd:eeeeeeeeeeeeeeee@aaaaaaaaaaa",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
