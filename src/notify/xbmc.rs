use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Xbmc { host: String, port: u16, user: Option<String>, password: Option<String>, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Xbmc {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema.ends_with('s');
        let port = url.port.unwrap_or(if secure { 443 } else { 8080 });
        Some(Self { host, port, user: url.user.clone(), password: url.password.clone(), secure, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Kodi/XBMC", service_url: Some("https://kodi.tv"), setup_url: None, protocols: vec!["xbmc", "kodi", "kodis"], description: "Send notifications to Kodi/XBMC.", attachment_support: false } }
}
#[async_trait]
impl Notify for Xbmc {
    fn schemas(&self) -> &[&str] { &["xbmc", "kodi", "kodis"] }
    fn service_name(&self) -> &str { "Kodi/XBMC" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let schema = if self.secure { "https" } else { "http" };
        let payload = json!({ "jsonrpc": "2.0", "method": "GUI.ShowNotification", "params": { "title": ctx.title, "message": ctx.body, "displaytime": 5000 }, "id": 1 });
        let url = format!("{}://{}:{}/jsonrpc", schema, self.host, self.port);
        let mut req = client.post(&url).header("User-Agent", APP_ID).json(&payload);
        if let (Some(u), Some(p)) = (&self.user, &self.password) {
            req = req.basic_auth(u, Some(p));
        }
        let resp = req.send().await?;
        Ok(resp.status().is_success())
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
            "kodi://localhost",
            "kodi://192.168.4.1",
            "kodi://[2001:db8:002a:3256:adfe:05c0:0003:0006]",
            "kodi://[2001:db8:002a:3256:adfe:05c0:0003:0006]:8282",
            "kodi://user:pass@localhost",
            "kodi://localhost:8080",
            "kodi://user:pass@localhost:8080",
            "kodis://localhost",
            "kodis://user:pass@localhost",
            "kodis://localhost:8080/path/",
            "kodis://user:password@localhost:8080",
            "kodis://localhost:443",
            "kodi://user:pass@localhost:8083",
            "xbmc://localhost",
            "xbmc://localhost?duration=14",
            "xbmc://localhost?duration=invalid",
            "xbmc://localhost?duration=-1",
            "xbmc://user:pass@localhost",
            "xbmc://localhost:8080",
            "xbmc://user:pass@localhost:8080",
            "xbmc://user@localhost",
            "xbmc://user:pass@localhost:8083",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "kodi://",
            "kodis://",
            "kodi://:@/",
            "xbmc://",
            "xbmc://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    /// Helper: create an Xbmc instance pointing at the given mock server.
    fn xbmc_for_mock(server: &MockServer) -> Xbmc {
        let addr = server.address();
        let url_str = format!("kodi://{}:{}", addr.ip(), addr.port());
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        Xbmc::from_url(&parsed).unwrap()
    }

    fn xbmc_with_auth_for_mock(server: &MockServer, user: &str, pass: &str) -> Xbmc {
        let addr = server.address();
        let url_str = format!("kodi://{}:{}@{}:{}", user, pass, addr.ip(), addr.port());
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        Xbmc::from_url(&parsed).unwrap()
    }

    fn default_ctx() -> NotifyContext {
        NotifyContext {
            title: "Test Title".into(),
            body: "Test Body".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_send_basic_success() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let xbmc = xbmc_for_mock(&server);
        let result = xbmc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_json_rpc_payload() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "GUI.ShowNotification",
                "params": {
                    "title": "My Title",
                    "message": "My Body",
                    "displaytime": 5000
                },
                "id": 1
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let xbmc = xbmc_for_mock(&server);
        let ctx = NotifyContext {
            title: "My Title".into(),
            body: "My Body".into(),
            ..Default::default()
        };
        let result = xbmc.send(&ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_with_basic_auth() {
        use wiremock::matchers::header;

        let server = MockServer::start().await;

        // "user:pass" base64 = "dXNlcjpwYXNz"
        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .and(header("Authorization", "Basic dXNlcjpwYXNz"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let xbmc = xbmc_with_auth_for_mock(&server, "user", "pass");
        let result = xbmc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_server_error_returns_false() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let xbmc = xbmc_for_mock(&server);
        let result = xbmc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_send_bizarre_status_code() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .respond_with(ResponseTemplate::new(418))
            .expect(1)
            .mount(&server)
            .await;

        let xbmc = xbmc_for_mock(&server);
        let result = xbmc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_send_connection_failure() {
        let url_str = "kodi://127.0.0.1:1";
        let parsed = crate::utils::parse::ParsedUrl::parse(url_str).unwrap();
        let xbmc = Xbmc::from_url(&parsed).unwrap();

        let result = xbmc.send(&default_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_includes_user_agent() {
        use crate::notify::APP_ID;
        use wiremock::matchers::header;

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .and(header("User-Agent", APP_ID))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let xbmc = xbmc_for_mock(&server);
        let result = xbmc.send(&default_ctx()).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_default_ports() {
        // kodi:// defaults to 8080
        let parsed = crate::utils::parse::ParsedUrl::parse("kodi://localhost").unwrap();
        let x = Xbmc::from_url(&parsed).unwrap();
        assert_eq!(x.port, 8080);
        assert!(!x.secure);

        // kodis:// defaults to 443
        let parsed = crate::utils::parse::ParsedUrl::parse("kodis://localhost").unwrap();
        let x = Xbmc::from_url(&parsed).unwrap();
        assert_eq!(x.port, 443);
        assert!(x.secure);
    }

    #[test]
    fn test_custom_port() {
        let parsed = crate::utils::parse::ParsedUrl::parse("kodi://localhost:9090").unwrap();
        let x = Xbmc::from_url(&parsed).unwrap();
        assert_eq!(x.port, 9090);
    }

    #[test]
    fn test_xbmc_schema_also_works() {
        let parsed = crate::utils::parse::ParsedUrl::parse("xbmc://localhost").unwrap();
        let x = Xbmc::from_url(&parsed).unwrap();
        assert_eq!(x.host, "localhost");
    }

    #[tokio::test]
    async fn test_send_without_auth_no_auth_header() {
        let server = MockServer::start().await;

        // Mount a mock that does NOT require auth header
        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let xbmc = xbmc_for_mock(&server);
        assert!(xbmc.user.is_none());
        assert!(xbmc.password.is_none());

        let result = xbmc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }
}
