use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct FreeMobile { user: String, password: String, verify_certificate: bool, tags: Vec<String> }
impl FreeMobile {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // freemobile://user:pass or freemobile://user@password or freemobile://?user=x&pass=y
        let user = url.user.clone()
            .or_else(|| url.get("user").map(|s| s.to_string()))?;
        let password = url.password.clone()
            .or_else(|| url.host.clone().filter(|h| !h.is_empty()))
            .or_else(|| url.get("pass").map(|s| s.to_string()))?;
        Some(Self { user, password, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Free Mobile", service_url: Some("https://mobile.free.fr"), setup_url: None, protocols: vec!["freemobile"], description: "Send SMS via Free Mobile (France).", attachment_support: false } }
}
#[async_trait]
impl Notify for FreeMobile {
    fn schemas(&self) -> &[&str] { &["freemobile"] }
    fn service_name(&self) -> &str { "Free Mobile" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let payload = json!({ "user": self.user, "pass": self.password, "msg": msg });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://smsapi.free-mobile.fr/sendmsg").header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "freemobile://user@password",
            "freemobile://?user=user&pass=password",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "freemobile://",
            "freemobile://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    fn parse_freemobile(url: &str) -> FreeMobile {
        let parsed = crate::utils::parse::ParsedUrl::parse(url).unwrap();
        FreeMobile::from_url(&parsed).unwrap()
    }

    #[test]
    fn test_from_url_user_at_password() {
        // freemobile://user@password -> user=user, host=password (used as password)
        let f = parse_freemobile("freemobile://user@password");
        assert_eq!(f.user, "user");
        assert_eq!(f.password, "password");
    }

    #[test]
    fn test_from_url_query_params() {
        let f = parse_freemobile("freemobile://?user=user&pass=password");
        assert_eq!(f.user, "user");
        assert_eq!(f.password, "password");
    }

    #[test]
    fn test_from_url_no_user_returns_none() {
        let parsed = crate::utils::parse::ParsedUrl::parse("freemobile://").unwrap();
        assert!(FreeMobile::from_url(&parsed).is_none());
    }

    #[test]
    fn test_from_url_no_password_returns_none() {
        let parsed = crate::utils::parse::ParsedUrl::parse("freemobile://:@/").unwrap();
        assert!(FreeMobile::from_url(&parsed).is_none());
    }

    #[test]
    fn test_service_details() {
        let details = FreeMobile::static_details();
        assert_eq!(details.service_name, "Free Mobile");
        assert_eq!(details.protocols, vec!["freemobile"]);
        assert!(!details.attachment_support);
    }
}
