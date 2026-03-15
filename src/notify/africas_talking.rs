use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct AfricasTalking { apikey: String, user: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl AfricasTalking {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Validate mode if provided
        if let Some(mode) = url.get("mode") {
            let m = mode.to_lowercase();
            let valid_modes = ["bulksms", "premium", "sandbox"];
            if !valid_modes.iter().any(|v| v.starts_with(&m)) {
                return None;
            }
        }
        // Support ?apikey= query param (host becomes a target in that case)
        let (apikey, mut targets) = if let Some(ak) = url.get("apikey").or_else(|| url.get("key")) {
            let mut t = Vec::new();
            if let Some(h) = url.host.as_deref() {
                if !h.is_empty() && h != "_" { t.push(h.to_string()); }
            }
            (ak.to_string(), t)
        } else {
            (url.host.clone()?, Vec::new())
        };
        if apikey.is_empty() { return None; }
        let user = url.get("user").map(|s| s.to_string())
            .or_else(|| url.user.clone())
            .unwrap_or_else(|| "sandbox".to_string());
        targets.extend(url.path_parts.clone());
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if targets.is_empty() { return None; }
        Some(Self { apikey, user, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Africa's Talking", service_url: Some("https://africastalking.com"), setup_url: None, protocols: vec!["atalk"], description: "Send SMS via Africa's Talking.", attachment_support: false } }
}
#[async_trait]
impl Notify for AfricasTalking {
    fn schemas(&self) -> &[&str] { &["atalk"] }
    fn service_name(&self) -> &str { "Africa's Talking" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let to = self.targets.join(",");
        let params = [("username", self.user.as_str()), ("to", to.as_str()), ("message", msg.as_str())];
        let resp = client.post("https://api.africastalking.com/version1/messaging").header("User-Agent", APP_ID).header("apiKey", self.apikey.as_str()).header("Accept", "application/json").form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "atalk://",
            "atalk://:@/",
            "atalk://user@^/",
            // invalid mode
            "atalk://user@apikey/+44444444444?mode=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "atalk://user@apikey/33333333333",
            "atalk://user@apikey/123/33333333333/abcd/+44444444444",
            "atalk://user@apikey/+44444444444?batch=y",
            "atalk://user@apikey/+44444444444?mode=s",
            "atalk://user@apikey/+44444444444?mode=PREM",
            "atalk://11111111111?apikey=key&user=user&from=FROMUSER",
            "atalk://_?user=user&to=11111111111,22222222222&key=bbbbbbbbbb&from=5555555555555",
            "atalk://user@apikey/11111111111/",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_struct_fields() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "atalk://myuser@myapikey/+15551231234/15555555555"
        ).unwrap();
        let obj = AfricasTalking::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, "myapikey");
        assert_eq!(obj.user, "myuser");
        assert_eq!(obj.targets.len(), 2);
        assert!(obj.targets.contains(&"+15551231234".to_string()));
        assert!(obj.targets.contains(&"15555555555".to_string()));
    }

    #[test]
    fn test_from_url_query_params() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "atalk://_?user=testuser&apikey=testapikey&to=11111111111,22222222222"
        ).unwrap();
        let obj = AfricasTalking::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, "testapikey");
        assert_eq!(obj.user, "testuser");
        assert_eq!(obj.targets.len(), 2);
    }

    #[test]
    fn test_from_url_key_alias() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "atalk://_?user=testuser&key=mykeyvalue&to=11111111111"
        ).unwrap();
        let obj = AfricasTalking::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, "mykeyvalue");
    }

    #[test]
    fn test_from_url_no_targets_returns_none() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "atalk://user@apikey"
        ).unwrap();
        assert!(AfricasTalking::from_url(&parsed).is_none());
    }

    #[test]
    fn test_mode_sandbox() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "atalk://user@apikey/+44444444444?mode=s"
        ).unwrap();
        assert!(AfricasTalking::from_url(&parsed).is_some());
    }

    #[test]
    fn test_mode_premium() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "atalk://user@apikey/+44444444444?mode=PREM"
        ).unwrap();
        assert!(AfricasTalking::from_url(&parsed).is_some());
    }

    #[test]
    fn test_service_details() {
        let details = AfricasTalking::static_details();
        assert_eq!(details.service_name, "Africa's Talking");
        assert_eq!(details.protocols, vec!["atalk"]);
        assert!(!details.attachment_support);
    }
}
