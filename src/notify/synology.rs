use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Synology {
    host: String, port: u16, token: String, secure: bool,
    user: Option<String>, password: Option<String>,
    verify_certificate: bool, tags: Vec<String>,
}

impl Synology {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        if host.is_empty() { return None; }
        // Token from first path part or ?token= query param
        let token = url.path_parts.first().cloned()
            .or_else(|| url.get("token").map(|s| s.to_string()))?;
        if token.trim().is_empty() { return None; }
        let secure = url.schema.ends_with('s');
        let port = url.port.unwrap_or(if secure { 5001 } else { 5000 });
        Some(Self {
            host, port, token, secure,
            user: url.user.clone(), password: url.password.clone(),
            verify_certificate: url.verify_certificate(), tags: url.tags(),
        })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Synology Chat", service_url: Some("https://www.synology.com"), setup_url: None, protocols: vec!["synology", "synologys"], description: "Send via Synology Chat.", attachment_support: false } }
}

#[async_trait]
impl Notify for Synology {
    fn schemas(&self) -> &[&str] { &["synology", "synologys"] }
    fn service_name(&self) -> &str { "Synology Chat" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let schema = if self.secure { "https" } else { "http" };
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };

        let payload_str = serde_json::to_string(&json!({ "text": text })).unwrap();
        let params = [
            ("api", "SYNO.Chat.External"),
            ("method", "incoming"),
            ("version", "2"),
            ("token", self.token.as_str()),
        ];
        let url = format!("{}://{}:{}/webapi/entry.cgi", schema, self.host, self.port);

        let mut req = client.post(&url)
            .header("User-Agent", APP_ID)
            .query(&params)
            .body(format!("payload={}", urlencoding::encode(&payload_str)));

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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "synology://localhost/token",
            "synology://localhost/token?file_url=http://reddit.com/test.jpg",
            "synology://user:pass@localhost/token",
            "synology://user@localhost/token",
            "synology://localhost:8080/token",
            "synology://user:pass@localhost:8080/token",
            "synologys://localhost/token",
            "synologys://localhost/?token=mytoken",
            "synologys://user:pass@localhost/token",
            "synologys://localhost:8080/token/path/",
            "synologys://user:password@localhost:8080/token",
            "synology://localhost:8080/path?+HeaderKey=HeaderValue",
            "synology://user:pass@localhost:8083/token",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "synology://:@/",
            "synology://",
            "synologys://",
            "synology://user@localhost",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    /// Helper: create a Synology instance pointing at the given mock server.
    fn synology_for_mock(server: &MockServer, token: &str) -> Synology {
        let addr = server.address();
        let url_str = format!("synology://{}:{}/{}", addr.ip(), addr.port(), token);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        Synology::from_url(&parsed).unwrap()
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
            .and(path("/webapi/entry.cgi"))
            .and(query_param("api", "SYNO.Chat.External"))
            .and(query_param("method", "incoming"))
            .and(query_param("version", "2"))
            .and(query_param("token", "mytoken"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let synology = synology_for_mock(&server, "mytoken");
        let result = synology.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_title_and_body_concatenation() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/webapi/entry.cgi"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let synology = synology_for_mock(&server, "tok");
        let ctx = NotifyContext {
            title: "My Title".into(),
            body: "My Body".into(),
            ..Default::default()
        };
        let result = synology.send(&ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_body_only_when_no_title() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/webapi/entry.cgi"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let synology = synology_for_mock(&server, "tok");
        let ctx = NotifyContext {
            title: "".into(),
            body: "Body Only".into(),
            ..Default::default()
        };
        let result = synology.send(&ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_server_error_returns_false() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/webapi/entry.cgi"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let synology = synology_for_mock(&server, "tok");
        let result = synology.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_send_bizarre_status_code() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/webapi/entry.cgi"))
            .respond_with(ResponseTemplate::new(418))
            .expect(1)
            .mount(&server)
            .await;

        let synology = synology_for_mock(&server, "tok");
        let result = synology.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_send_connection_failure() {
        let url_str = "synology://127.0.0.1:1/tok";
        let parsed = crate::utils::parse::ParsedUrl::parse(url_str).unwrap();
        let synology = Synology::from_url(&parsed).unwrap();

        let result = synology.send(&default_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_includes_user_agent() {
        use crate::notify::APP_ID;
        use wiremock::matchers::header;

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/webapi/entry.cgi"))
            .and(header("User-Agent", APP_ID))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let synology = synology_for_mock(&server, "tok");
        let result = synology.send(&default_ctx()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_with_basic_auth() {
        use wiremock::matchers::header;

        let server = MockServer::start().await;

        // Create a Synology with user:pass
        let addr = server.address();
        let url_str = format!("synology://user:pass@{}:{}/tok", addr.ip(), addr.port());
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let synology = Synology::from_url(&parsed).unwrap();

        Mock::given(method("POST"))
            .and(path("/webapi/entry.cgi"))
            .and(header("Authorization", "Basic dXNlcjpwYXNz"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let result = synology.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[test]
    fn test_default_ports() {
        // Insecure defaults to 5000
        let parsed = crate::utils::parse::ParsedUrl::parse("synology://localhost/tok").unwrap();
        let s = Synology::from_url(&parsed).unwrap();
        assert_eq!(s.port, 5000);
        assert!(!s.secure);

        // Secure defaults to 5001
        let parsed = crate::utils::parse::ParsedUrl::parse("synologys://localhost/tok").unwrap();
        let s = Synology::from_url(&parsed).unwrap();
        assert_eq!(s.port, 5001);
        assert!(s.secure);
    }

    #[test]
    fn test_custom_port() {
        let parsed = crate::utils::parse::ParsedUrl::parse("synology://localhost:8080/tok").unwrap();
        let s = Synology::from_url(&parsed).unwrap();
        assert_eq!(s.port, 8080);
    }
}
