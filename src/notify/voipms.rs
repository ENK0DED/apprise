use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct VoipMs { user: String, password: String, did: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl VoipMs {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // voipms://password:user@host/from_did/to1/to2
        // or voipms://password:user@host/?from=DID&to=PHONE
        let password = url.user.clone()?;
        let user = url.password.clone()?;
        if user.is_empty() || password.is_empty() { return None; }
        // DID (from) comes from first path part or ?from= query param
        let did = url.get("from").or_else(|| url.get("source")).map(|s| s.to_string())
            .or_else(|| url.path_parts.first().cloned())?;
        if did.is_empty() { return None; }
        // Validate DID has at least 11 digits
        let did_digits: String = did.chars().filter(|c| c.is_ascii_digit()).collect();
        if did_digits.len() < 11 { return None; }
        // Targets
        let mut targets: Vec<String> = if url.get("from").is_some() || url.get("source").is_some() {
            url.path_parts.clone()
        } else {
            url.path_parts.get(1..).unwrap_or(&[]).to_vec()
        };
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        // Validate target phone numbers (at least 11 digits each)
        for t in &targets {
            let digits: String = t.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() < 11 { return None; }
            // Reject international format starting with 011
            if digits.starts_with("011") { return None; }
        }
        if targets.is_empty() { return None; }
        Some(Self { user, password, did, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "VoIP.ms", service_url: Some("https://voip.ms"), setup_url: None, protocols: vec!["voipms"], description: "Send SMS via VoIP.ms.", attachment_support: false } }
}
#[async_trait]
impl Notify for VoipMs {
    fn schemas(&self) -> &[&str] { &["voipms"] }
    fn service_name(&self) -> &str { "VoIP.ms" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut all_ok = true;
        for target in &self.targets {
            let url = format!("https://voip.ms/api/v1/rest.php?api_username={}&api_password={}&method=sendSMS&did={}&dst={}&message={}",
                urlencoding::encode(&self.user), urlencoding::encode(&self.password), urlencoding::encode(&self.did), urlencoding::encode(target), urlencoding::encode(&msg));
            let resp = client.get(&url).header("User-Agent", APP_ID).send().await?;
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
        let urls = vec![
            "voipms://",
            "voipms://@:",
            // No password
            "voipms://user@example.com/11111111111",
            // No email (just password)
            "voipms://:password",
            // Invalid email (user@ without domain)
            "voipms://user@:pass/11111111111",
            // Invalid short phone number
            "voipms://password:user@example.com/1613",
            // International format starting with 011
            "voipms://password:user@example.com/01133122446688",
            // International format target starting with 011
            "voipms://password:user@example.com/16134448888/01133122446688",
        ];
        for url in urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            // Valid with from and target
            "voipms://password:user@example.com/16138884444/16134442222",
            // Valid with multiple targets
            "voipms://password:user@example.com/16138884444/16134442222/16134443333",
            // Valid with from= and to= query params
            "voipms://password:user@example.com/?from=16138884444&to=16134448888",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_fields() {
        // voipms://password:user@example.com/DID/TARGET
        // URL parsing: user=password, password=user, host=example.com
        let parsed = ParsedUrl::parse(
            "voipms://password:user@example.com/16138884444/16134442222"
        ).unwrap();
        let v = VoipMs::from_url(&parsed).unwrap();
        assert_eq!(v.password, "password");
        // password field from URL parser is "user" (not user@host)
        assert_eq!(v.user, "user");
        assert_eq!(v.did, "16138884444");
        assert_eq!(v.targets.len(), 1);
        assert_eq!(v.targets[0], "16134442222");
    }

    #[test]
    fn test_from_url_query_params() {
        let parsed = ParsedUrl::parse(
            "voipms://password:user@example.com/?from=16138884444&to=16134448888"
        ).unwrap();
        let v = VoipMs::from_url(&parsed).unwrap();
        assert_eq!(v.did, "16138884444");
        assert!(v.targets.contains(&"16134448888".to_string()));
    }

    #[test]
    fn test_from_url_multiple_targets() {
        let parsed = ParsedUrl::parse(
            "voipms://password:user@example.com/16138884444/16134442222/16134443333"
        ).unwrap();
        let v = VoipMs::from_url(&parsed).unwrap();
        assert_eq!(v.targets.len(), 2);
        assert_eq!(v.targets[0], "16134442222");
        assert_eq!(v.targets[1], "16134443333");
    }

    #[test]
    fn test_service_details() {
        let details = VoipMs::static_details();
        assert_eq!(details.service_name, "VoIP.ms");
        assert_eq!(details.service_url, Some("https://voip.ms"));
        assert!(details.protocols.contains(&"voipms"));
        assert!(!details.attachment_support);
    }
}
