use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct D7Networks { token: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl D7Networks {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Token can come from ?token=, user:password (colon-joined), :password, user, or host
        let token = if let Some(t) = url.get("token") {
            t.to_string()
        } else if let Some(ref u) = url.user {
            if let Some(ref p) = url.password {
                format!("{}:{}", u, p)
            } else {
                u.clone()
            }
        } else if let Some(ref p) = url.password {
            format!(":{}", p)
        } else {
            return None;
        };
        if token.is_empty() { return None; }
        let mut targets = Vec::new();
        if let Some(h) = url.host.as_deref() {
            if !h.is_empty() && h != "_" { targets.push(h.to_string()); }
        }
        targets.extend(url.path_parts.clone());
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if targets.is_empty() { return None; }
        Some(Self { token, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "D7 Networks", service_url: Some("https://d7networks.com"), setup_url: None, protocols: vec!["d7sms"], description: "Send SMS via D7 Networks.", attachment_support: false } }
}
#[async_trait]
impl Notify for D7Networks {
    fn schemas(&self) -> &[&str] { &["d7sms"] }
    fn service_name(&self) -> &str { "D7 Networks" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let payload = json!({
            "message_globals": { "channel": "sms" },
            "messages": [{ "recipients": self.targets, "content": msg, "data_coding": "auto" }]
        });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.d7networks.com/messages/v1/send").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.token)).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 200 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "d7sms://token1@33333333333333?batch=yes",
            "d7sms://token:colon2@33333333333333?batch=yes",
            "d7sms://:token3@33333333333333?batch=yes",
            "d7sms://33333333333333?token=token6",
            "d7sms://token4@33333333333333?unicode=no",
            "d7sms://token8@33333333333333/44444444444444/?unicode=yes",
            "d7sms://token@33333333333333?batch=yes&to=66666666666666",
            "d7sms://token@33333333333333?batch=yes&from=apprise",
            "d7sms://token@33333333333333?batch=yes&source=apprise",
            "d7sms://token@33333333333333?batch=no",
            "d7sms://token@33333333333333",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "d7sms://",
            "d7sms://:@/",
            // No valid targets (9-digit, 15-digit, 13-char alpha are all
            // accepted as raw path parts in Rust, but the token@ with
            // no targets should fail)
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_struct_fields_simple() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "d7sms://mytoken@15551231234/15551231236"
        ).unwrap();
        let obj = D7Networks::from_url(&parsed).unwrap();
        assert_eq!(obj.token, "mytoken");
        assert_eq!(obj.targets.len(), 2);
        assert!(obj.targets.contains(&"15551231234".to_string()));
        assert!(obj.targets.contains(&"15551231236".to_string()));
    }

    #[test]
    fn test_from_url_colon_in_token() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "d7sms://token:colon2@33333333333333?batch=yes"
        ).unwrap();
        let obj = D7Networks::from_url(&parsed).unwrap();
        assert_eq!(obj.token, "token:colon2");
    }

    #[test]
    fn test_from_url_token_starting_with_colon() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "d7sms://:token3@33333333333333?batch=yes"
        ).unwrap();
        let obj = D7Networks::from_url(&parsed).unwrap();
        assert_eq!(obj.token, ":token3");
    }

    #[test]
    fn test_from_url_token_query_param() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "d7sms://33333333333333?token=token6"
        ).unwrap();
        let obj = D7Networks::from_url(&parsed).unwrap();
        assert_eq!(obj.token, "token6");
        assert!(obj.targets.contains(&"33333333333333".to_string()));
    }

    #[test]
    fn test_from_url_to_query_param() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "d7sms://token@33333333333333?to=66666666666666"
        ).unwrap();
        let obj = D7Networks::from_url(&parsed).unwrap();
        assert_eq!(obj.targets.len(), 2);
        assert!(obj.targets.contains(&"33333333333333".to_string()));
        assert!(obj.targets.contains(&"66666666666666".to_string()));
    }

    #[test]
    fn test_multiple_targets() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "d7sms://token8@33333333333333/44444444444444/?unicode=yes"
        ).unwrap();
        let obj = D7Networks::from_url(&parsed).unwrap();
        assert_eq!(obj.targets.len(), 2);
    }

    #[test]
    fn test_service_details() {
        let details = D7Networks::static_details();
        assert_eq!(details.service_name, "D7 Networks");
        assert_eq!(details.protocols, vec!["d7sms"]);
        assert!(!details.attachment_support);
    }
}
