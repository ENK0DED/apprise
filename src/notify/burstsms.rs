use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct BurstSms { apikey: String, api_secret: String, from_phone: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl BurstSms {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.get("key").map(|s| s.to_string()).or_else(|| url.user.clone())?;
        let api_secret = url.get("secret").map(|s| s.to_string()).or_else(|| url.password.clone())?;
        let from_phone = url.get("from").or_else(|| url.get("source"))
            .map(|s| s.to_string())
            .or_else(|| {
                let h = url.host.clone().unwrap_or_default();
                let h = urlencoding::decode(&h).unwrap_or_default().trim().to_string();
                if h.is_empty() || h == "_" { None } else { Some(h) }
            }).unwrap_or_default();
        let from_phone = from_phone.trim().to_string();
        if from_phone.is_empty() { return None; }
        // Validate country if provided
        if let Some(country) = url.get("country") {
            if country.len() != 2 || !country.chars().all(|c| c.is_ascii_alphabetic()) {
                return None;
            }
        }
        // Validate validity if provided
        if let Some(validity) = url.get("validity") {
            if validity.parse::<u32>().is_err() { return None; }
        }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        Some(Self { apikey, api_secret, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Burst SMS", service_url: Some("https://burstsms.com.au"), setup_url: None, protocols: vec!["burstsms"], description: "Send SMS via Burst SMS.", attachment_support: false } }
}
#[async_trait]
impl Notify for BurstSms {
    fn schemas(&self) -> &[&str] { &["burstsms"] }
    fn service_name(&self) -> &str { "Burst SMS" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let params = [("to", target.as_str()), ("from", self.from_phone.as_str()), ("message", msg.as_str())];
            let resp = client.post("https://api.transmitsms.com/send-sms.json").header("User-Agent", APP_ID).basic_auth(&self.apikey, Some(&self.api_secret)).form(&params).send().await?;
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
            "burstsms://ffffffff:gggggggggggggggg@33333333333/123/999999999999999/abcd/",
            "burstsms://hhhhhhhh:iiiiiiiiiiiiiiii@55555555555",
            "burstsms://_?key=aaaaaaaa&secret=bbbbbbbbbbbbbbbb&from=55555555555&to=66666666666",
            "burstsms://_?key=aaaaaaaa&secret=bbbbbbbbbbbbbbbb&from=55555555555&to=66666666666&batch=y",
            "burstsms://_?key=aaaaaaaa&secret=bbbbbbbbbbbbbbbb&source=55555555555&to=66666666666&country=us",
            "burstsms://_?key=aaaaaaaa&secret=bbbbbbbbbbbbbbbb&source=55555555555&to=66666666666&validity=10",
            "burstsms://_?key=aaaaaaaa&secret=bbbbbbbbbbbbbbbb&from=55555555555&to=77777777777",
            "burstsms://aaaaaaaa:bbbbbbbbbbbbbbbb@66666666666/77777777777",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "burstsms://",
            "burstsms://:@/",
            "burstsms://aaaaaaaa@12345678",
            "burstsms://dddddddd:eeeeeeeeeeeeeeee@%20",
            "burstsms://_?key=aaaaaaaa&secret=bbbbbbbbbbbbbbbb&source=55555555555&to=66666666666&country=invalid",
            "burstsms://_?key=aaaaaaaa&secret=bbbbbbbbbbbbbbbb&source=55555555555&to=66666666666&validity=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
