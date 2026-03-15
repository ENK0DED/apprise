use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct PushJet { host: Option<String>, port: Option<u16>, secret: String, secure: bool, user: Option<String>, password: Option<String>, verify_certificate: bool, tags: Vec<String> }
impl PushJet {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // pjet://host/secret or pjet://user:pass@host?secret=X
        let (host, secret) = if let Some(sec) = url.get("secret") {
            (url.host.clone(), sec.to_string())
        } else if !url.path_parts.is_empty() {
            let sec = url.path_parts.last()?.clone();
            (url.host.clone(), sec)
        } else {
            return None;
        };
        if secret.is_empty() { return None; }
        // Secret must be at least 32 characters
        if secret.len() < 32 { return None; }
        Some(Self { host, port: url.port, secret, secure: url.schema == "pjets", user: url.user.clone(), password: url.password.clone(), verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Pushjet", service_url: Some("https://pushjet.io"), setup_url: None, protocols: vec!["pjet", "pjets"], description: "Send push notifications via Pushjet.", attachment_support: false } }
}
#[async_trait]
impl Notify for PushJet {
    fn schemas(&self) -> &[&str] { &["pjet", "pjets"] }
    fn service_name(&self) -> &str { "Pushjet" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let base = match &self.host {
            Some(h) => { let schema = if self.secure { "https" } else { "http" }; let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default(); format!("{}://{}{}", schema, h, port_str) },
            None => "https://api.pushjet.io".to_string(),
        };
        let url = format!("{}/message?secret={}&message={}&title={}", base, self.secret, urlencoding::encode(&ctx.body), urlencoding::encode(&ctx.title));
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;
    use crate::notify::NotifyContext;
    use crate::utils::parse::ParsedUrl;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_invalid_urls() {
        let urls: Vec<String> = vec![
            "pjet://".into(),
            "pjets://".into(),
            "pjet://:@/".into(),
            // Secret key too short (< 32 chars)
            format!("pjet://{}", "a".repeat(32)),
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_valid_urls() {
        let secret = "a".repeat(32);
        let urls = vec![
            format!("pjet://user:pass@localhost/{}", secret),
            format!("pjets://localhost/{}", secret),
            format!("pjet://user:pass@localhost?secret={}", secret),
            format!("pjets://localhost:8080/{}", secret),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_fields() {
        let secret = "a".repeat(32);
        let url_str = format!("pjet://user:pass@localhost/{}", secret);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let p = PushJet::from_url(&parsed).unwrap();
        assert_eq!(p.secret, secret);
        assert_eq!(p.host, Some("localhost".to_string()));
        assert_eq!(p.user, Some("user".to_string()));
        assert_eq!(p.password, Some("pass".to_string()));
        assert!(!p.secure);
    }

    #[test]
    fn test_from_url_secure() {
        let secret = "a".repeat(32);
        let url_str = format!("pjets://localhost:8080/{}", secret);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let p = PushJet::from_url(&parsed).unwrap();
        assert!(p.secure);
        assert_eq!(p.port, Some(8080));
    }

    #[test]
    fn test_from_url_secret_via_param() {
        let secret = "a".repeat(32);
        let url_str = format!("pjet://user:pass@localhost?secret={}", secret);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let p = PushJet::from_url(&parsed).unwrap();
        assert_eq!(p.secret, secret);
    }

    #[test]
    fn test_static_details() {
        let details = PushJet::static_details();
        assert_eq!(details.service_name, "Pushjet");
        assert_eq!(details.service_url, Some("https://pushjet.io"));
        assert!(details.protocols.contains(&"pjet"));
        assert!(details.protocols.contains(&"pjets"));
        assert!(!details.attachment_support);
    }

    fn default_ctx() -> NotifyContext {
        NotifyContext {
            title: "Test Title".into(),
            body: "Test Body".into(),
            ..Default::default()
        }
    }

    /// Helper: create a PushJet instance pointing at the mock server.
    fn pushjet_for_mock(server: &MockServer, secret: &str) -> PushJet {
        let addr = server.address();
        let url_str = format!("pjet://{}:{}/{}", addr.ip(), addr.port(), secret);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        PushJet::from_url(&parsed).unwrap()
    }

    #[tokio::test]
    async fn test_send_success() {
        let server = MockServer::start().await;
        let secret = "a".repeat(32);

        Mock::given(method("POST"))
            .and(path(format!("/message")))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let pj = pushjet_for_mock(&server, &secret);
        let result = pj.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_server_error() {
        let server = MockServer::start().await;
        let secret = "a".repeat(32);

        Mock::given(method("POST"))
            .and(path("/message"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Error"))
            .expect(1)
            .mount(&server)
            .await;

        let pj = pushjet_for_mock(&server, &secret);
        let result = pj.send(&default_ctx()).await;
        assert!(result.is_err());
    }
}
