use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Viber { token: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Viber {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()
            .filter(|h| !h.is_empty())
            .or_else(|| url.get("token").map(|s| s.to_string()))
            .unwrap_or_default();
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        Some(Self { token, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Viber", service_url: Some("https://www.viber.com"), setup_url: None, protocols: vec!["viber"], description: "Send messages via Viber Bot API.", attachment_support: false } }
}
#[async_trait]
impl Notify for Viber {
    fn schemas(&self) -> &[&str] { &["viber"] }
    fn service_name(&self) -> &str { "Viber" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!({ "auth_token": self.token, "receiver": target, "type": "text", "text": msg, "sender": { "name": "Apprise" } });
            let resp = client.post("https://chatapi.viber.com/pa/send_message").header("User-Agent", APP_ID).json(&payload).send().await?;
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
    fn test_valid_urls() {
        let urls = vec![
            "viber://",
            "viber://?token=tokenb",
            "viber://token/targetx",
            "viber://token/t1/t2?from=Viber%20Bot",
            "viber://t1/t2?token=token",
            "viber://?token=token&to=t5",
            "viber://token/t3?avatar=value",
            "viber://token/?to=abc,def",
            "viber://?token=token&to=hij,klm",
            "viber://?token=token&to=nop,qrs",
            "viber://?token=token&to=tuv,wxy",
            "viber://token/t10",
            "viber://token/targetY",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_token_from_host() {
        let parsed = ParsedUrl::parse("viber://mytoken/target1").unwrap();
        let v = Viber::from_url(&parsed).unwrap();
        assert_eq!(v.token, "mytoken");
        assert_eq!(v.targets, vec!["target1".to_string()]);
    }

    #[test]
    fn test_from_url_token_from_query() {
        let parsed = ParsedUrl::parse("viber://?token=mytoken&to=t1,t2").unwrap();
        let v = Viber::from_url(&parsed).unwrap();
        assert_eq!(v.token, "mytoken");
        assert!(v.targets.contains(&"t1".to_string()));
        assert!(v.targets.contains(&"t2".to_string()));
    }

    #[test]
    fn test_from_url_multiple_targets() {
        let parsed = ParsedUrl::parse("viber://token/t1/t2?from=Viber%20Bot").unwrap();
        let v = Viber::from_url(&parsed).unwrap();
        assert_eq!(v.token, "token");
        assert_eq!(v.targets.len(), 2);
        assert_eq!(v.targets[0], "t1");
        assert_eq!(v.targets[1], "t2");
    }

    #[test]
    fn test_from_url_to_param_combined() {
        let parsed = ParsedUrl::parse("viber://token/?to=abc,def").unwrap();
        let v = Viber::from_url(&parsed).unwrap();
        assert_eq!(v.token, "token");
        assert_eq!(v.targets.len(), 2);
        assert!(v.targets.contains(&"abc".to_string()));
        assert!(v.targets.contains(&"def".to_string()));
    }

    #[test]
    fn test_service_details() {
        let details = Viber::static_details();
        assert_eq!(details.service_name, "Viber");
        assert_eq!(details.service_url, Some("https://www.viber.com"));
        assert!(details.protocols.contains(&"viber"));
        assert!(!details.attachment_support);
    }
}
