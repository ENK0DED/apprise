use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct HttpSms { apikey: String, from_phone: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl HttpSms {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.get("key").map(|s| s.to_string()).or_else(|| url.user.clone())?;
        if apikey.is_empty() { return None; }
        // If ?from= is set, host becomes a target; otherwise host is from_phone
        let (from_phone, mut targets) = if let Some(from) = url.get("from").or_else(|| url.get("source")) {
            let mut t = Vec::new();
            if let Some(h) = url.host.as_deref() {
                if !h.is_empty() && h != "_" { t.push(h.to_string()); }
            }
            (from.to_string(), t)
        } else {
            (url.host.clone().unwrap_or_default(), Vec::new())
        };
        if from_phone.is_empty() || from_phone == "_" { return None; }
        // Validate from_phone (must be 10-14 digits)
        let digits: String = from_phone.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() < 10 || digits.len() > 14 { return None; }
        targets.extend(url.path_parts.clone());
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
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
    fn test_valid_urls() {
        let urls = vec![
            "httpsms://pppppppppp@1111111111/55",
            "httpsms://pppppppppp@2222222222",
            "httpsms://bbbbbbbbbb@9876543210/33333333333/abcd/",
            "httpsms://cccccccccc@44444444444",
            "httpsms://bbbbbbbbbb@55555555555",
            "httpsms://?key=yyyyyyyyyy&from=55555555555",
            "httpsms://?key=bbbbbbbbbb&from=55555555555&to=7777777777777",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "httpsms://",
            "httpsms://:@/",
            "httpsms://uuuuuuuuuu:pppppppppp@33333",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
