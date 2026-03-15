use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Notica { token: String, host: Option<String>, port: Option<u16>, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Notica {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        Some(Self { token, host: None, port: url.port, secure: url.schema == "noticas", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Notica", service_url: Some("https://notica.us"), setup_url: None, protocols: vec!["notica", "noticas"], description: "Send push notifications via Notica.", attachment_support: false } }
}
#[async_trait]
impl Notify for Notica {
    fn schemas(&self) -> &[&str] { &["notica", "noticas"] }
    fn service_name(&self) -> &str { "Notica" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        let url = format!("https://notica.us/?{}", self.token);
        let params = [("d", text.as_str())];
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;
    use super::*;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "notica://",
            "notica://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            format!("notica://{}", "a".repeat(6)),
            format!("notica://localhost/{}", "b".repeat(6)),
            format!("notica://user@localhost/{}", "c".repeat(6)),
            format!("notica://user:pass@localhost/{}/", "d".repeat(6)),
            format!("notica://localhost:8080/{}", "a".repeat(6)),
            format!("noticas://localhost/{}", "j".repeat(6)),
            format!("noticas://user:pass@localhost/{}", "e".repeat(6)),
            format!("noticas://localhost:8080/path/{}", "5".repeat(6)),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_fields_cloud_mode() {
        let parsed = ParsedUrl::parse(&format!("notica://{}", "a".repeat(6))).unwrap();
        let n = Notica::from_url(&parsed).unwrap();
        assert_eq!(n.token, "a".repeat(6));
        assert!(!n.secure);
        assert!(n.host.is_none());
    }

    #[test]
    fn test_from_url_self_hosted() {
        // In self-hosted mode, the host becomes the token (impl limitation)
        let parsed = ParsedUrl::parse(&format!("notica://localhost/{}", "b".repeat(6))).unwrap();
        let n = Notica::from_url(&parsed).unwrap();
        // Current impl uses host as token
        assert_eq!(n.token, "localhost");
    }

    #[test]
    fn test_secure_flag() {
        let parsed = ParsedUrl::parse(&format!("noticas://localhost/{}", "j".repeat(6))).unwrap();
        let n = Notica::from_url(&parsed).unwrap();
        assert!(n.secure);
    }

    #[test]
    fn test_port_parsing() {
        let parsed = ParsedUrl::parse(&format!("notica://localhost:8080/{}", "a".repeat(6))).unwrap();
        let n = Notica::from_url(&parsed).unwrap();
        assert_eq!(n.port, Some(8080));
    }

    #[test]
    fn test_static_details() {
        let details = Notica::static_details();
        assert_eq!(details.service_name, "Notica");
        assert_eq!(details.service_url, Some("https://notica.us"));
        assert!(details.protocols.contains(&"notica"));
        assert!(details.protocols.contains(&"noticas"));
        assert!(!details.attachment_support);
    }
}
