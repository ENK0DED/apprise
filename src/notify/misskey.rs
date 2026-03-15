use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Misskey { host: String, port: Option<u16>, token: String, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Misskey {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let token = url.user.clone()
            .or_else(|| url.get("token").map(|s| s.to_string()))?;
        if token.is_empty() { return None; }
        // Validate visibility if provided
        if let Some(vis) = url.get("visibility") {
            match vis.to_lowercase().as_str() {
                "public" | "home" | "followers" | "specified" | "" => {}
                _ => return None,
            }
        }
        Some(Self { host, port: url.port, token, secure: url.schema == "misskeys", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Misskey", service_url: Some("https://misskey.io"), setup_url: None, protocols: vec!["misskey", "misskeys"], description: "Post to Misskey instances.", attachment_support: false } }
}
#[async_trait]
impl Notify for Misskey {
    fn schemas(&self) -> &[&str] { &["misskey", "misskeys"] }
    fn service_name(&self) -> &str { "Misskey" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/api/notes/create", schema, self.host, port_str);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let payload = json!({ "i": self.token, "text": text, "visibility": "public" });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;
    use crate::notify::NotifyContext;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "misskey://access_token@hostname",
            "misskeys://access_token@hostname",
            "misskey://hostname/?token=abcd123",
            "misskeys://access_token@hostname:8443",
            "misskeys://access_token@hostname?visibility=specified",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "misskey://",
            "misskey://:@/",
            "misskey://hostname",
            "misskey://access_token@hostname?visibility=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    fn misskey_for_mock(server: &MockServer, token: &str) -> Misskey {
        let addr = server.address();
        let url_str = format!("misskey://{}@{}:{}", token, addr.ip(), addr.port());
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        Misskey::from_url(&parsed).unwrap()
    }

    #[tokio::test]
    async fn test_send_basic_success() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/notes/create"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mk = misskey_for_mock(&server, "my_access_token");
        let ctx = NotifyContext {
            body: "Test Body".into(),
            ..Default::default()
        };
        let result = mk.send(&ctx).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_send_with_title_and_body() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/notes/create"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "i": "my_token",
                "text": "**My Title**\nMy Body",
                "visibility": "public",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mk = misskey_for_mock(&server, "my_token");
        let ctx = NotifyContext {
            title: "My Title".into(),
            body: "My Body".into(),
            ..Default::default()
        };
        let result = mk.send(&ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_body_only_no_title() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/notes/create"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "i": "tok123",
                "text": "Just a body",
                "visibility": "public",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mk = misskey_for_mock(&server, "tok123");
        let ctx = NotifyContext {
            body: "Just a body".into(),
            ..Default::default()
        };
        let result = mk.send(&ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_token_in_payload() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/notes/create"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "i": "secret_token_abc",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mk = misskey_for_mock(&server, "secret_token_abc");
        let ctx = NotifyContext {
            body: "test".into(),
            ..Default::default()
        };
        let result = mk.send(&ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_server_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/notes/create"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let mk = misskey_for_mock(&server, "my_token");
        let ctx = NotifyContext {
            body: "test".into(),
            ..Default::default()
        };
        let result = mk.send(&ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_non_standard_error_code() {
        let server = MockServer::start().await;

        // Use 418 (I'm a teapot) as a non-standard error
        Mock::given(method("POST"))
            .and(path("/api/notes/create"))
            .respond_with(ResponseTemplate::new(418))
            .expect(1)
            .mount(&server)
            .await;

        let mk = misskey_for_mock(&server, "my_token");
        let ctx = NotifyContext {
            body: "test".into(),
            ..Default::default()
        };
        let result = mk.send(&ctx).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_visibility_settings() {
        // Valid visibility values
        for vis in &["public", "home", "followers", "specified"] {
            let url = format!("misskey://token@host?visibility={}", vis);
            assert!(from_url(&url).is_some(), "Should accept visibility={}", vis);
        }

        // Invalid visibility
        let url = "misskey://token@host?visibility=invalid";
        assert!(from_url(url).is_none());
    }

    #[test]
    fn test_secure_vs_insecure() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "misskey://token@host",
        ).unwrap();
        let mk = Misskey::from_url(&parsed).unwrap();
        assert!(!mk.secure);

        let parsed = crate::utils::parse::ParsedUrl::parse(
            "misskeys://token@host",
        ).unwrap();
        let mk = Misskey::from_url(&parsed).unwrap();
        assert!(mk.secure);
    }

    #[test]
    fn test_custom_port() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "misskeys://token@host:8443",
        ).unwrap();
        let mk = Misskey::from_url(&parsed).unwrap();
        assert_eq!(mk.port, Some(8443));
    }
}
