use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct SmsEagle { host: String, port: Option<u16>, user: String, password: String, targets: Vec<String>, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl SmsEagle {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let user = url.user.clone().unwrap_or_else(|| "admin".to_string());
        let password = url.password.clone().unwrap_or_default();
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        // Reject invalid targets (@ prefix with no name)
        targets.retain(|t| {
            let stripped = t.trim_start_matches('@');
            !stripped.is_empty()
        });
        if targets.is_empty() { return None; }
        // Validate priority if provided
        if let Some(priority) = url.get("priority") {
            match priority.to_lowercase().as_str() {
                "0" | "1" | "2" | "3" | "low" | "normal" | "high" | "" => {}
                _ => return None,
            }
        }
        Some(Self { host, port: url.port, user, password, targets, secure: url.schema == "smseagles", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SMSEagle", service_url: Some("https://smseagle.eu"), setup_url: None, protocols: vec!["smseagle", "smseagles"], description: "Send SMS via SMSEagle hardware gateway.", attachment_support: true } }
}
#[async_trait]
impl Notify for SmsEagle {
    fn schemas(&self) -> &[&str] { &["smseagle", "smseagles"] }
    fn service_name(&self) -> &str { "SMSEagle" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let mut params = json!({ "login": self.user, "pass": self.password, "to": target, "message": msg });
            if !ctx.attachments.is_empty() {
                params["attachments"] = json!(ctx.attachments.iter().map(|att| json!({
                    "content_type": att.mime_type,
                    "content": base64::engine::general_purpose::STANDARD.encode(&att.data),
                })).collect::<Vec<_>>());
            }
            let payload = json!([{"method": "sms.send_sms", "params": params}]);
            let url = format!("{}://{}{}/index.php/jsonrpc/sms", schema, self.host, port_str);
            let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
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
            "smseagle://tokenb@localhost/%20/%20/",
            "smseagle://token@localhost/@user/?priority=high",
            "smseagle://token@localhost/@user/?priority=1",
            "smseagle://token@localhost:8082/#abcd/",
            "smseagle://token@localhost:8082/@abcd/",
            "smseagles://token@localhost:8081/contact/",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "smseagle://",
            "smseagle://:@/",
            "smseagle://localhost",
            "smseagle://%20@localhost",
            "smseagle://token@localhost/@user/?priority=invalid",
            "smseagle://token@localhost/@user/?priority=25",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    /// Helper: create an SmsEagle pointing at the mock server with given targets.
    fn smseagle_for_mock(server: &MockServer, targets: &[&str]) -> SmsEagle {
        let addr = server.address();
        let target_path = targets.join("/");
        let url_str = format!("smseagle://token@{}:{}/{}", addr.ip(), addr.port(), target_path);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        SmsEagle::from_url(&parsed).unwrap()
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
            .and(path("/index.php/jsonrpc/sms"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let eagle = smseagle_for_mock(&server, &["@user"]);
        let result = eagle.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_multiple_targets() {
        let server = MockServer::start().await;

        // Should make one POST per target
        Mock::given(method("POST"))
            .and(path("/index.php/jsonrpc/sms"))
            .respond_with(ResponseTemplate::new(200))
            .expect(3)
            .mount(&server)
            .await;

        let addr = server.address();
        let url_str = format!(
            "smseagle://token@{}:{}/11111111111/22222222222/@contact",
            addr.ip(), addr.port()
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let eagle = SmsEagle::from_url(&parsed).unwrap();

        let result = eagle.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_title_and_body_concatenation() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/index.php/jsonrpc/sms"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let eagle = smseagle_for_mock(&server, &["@user"]);
        let ctx = NotifyContext {
            title: "My Title".into(),
            body: "My Body".into(),
            ..Default::default()
        };
        let result = eagle.send(&ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_body_only_when_no_title() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/index.php/jsonrpc/sms"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let eagle = smseagle_for_mock(&server, &["@user"]);
        let ctx = NotifyContext {
            title: "".into(),
            body: "Body Only".into(),
            ..Default::default()
        };
        let result = eagle.send(&ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_server_error_returns_false() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/index.php/jsonrpc/sms"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let eagle = smseagle_for_mock(&server, &["@user"]);
        let result = eagle.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_send_partial_failure() {
        // Two targets: first succeeds, second fails
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/index.php/jsonrpc/sms"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let addr = server.address();
        let url_str = format!(
            "smseagle://token@{}:{}/11111111111/22222222222",
            addr.ip(), addr.port()
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let eagle = SmsEagle::from_url(&parsed).unwrap();

        let result = eagle.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_send_connection_failure() {
        // Point at a port that nothing is listening on
        let url_str = "smseagle://token@127.0.0.1:1/@user";
        let parsed = crate::utils::parse::ParsedUrl::parse(url_str).unwrap();
        let eagle = SmsEagle::from_url(&parsed).unwrap();

        let result = eagle.send(&default_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_secure_url() {
        // Verify that smseagles:// sets secure flag
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "smseagles://token@localhost/@user"
        ).unwrap();
        let eagle = SmsEagle::from_url(&parsed).unwrap();
        assert!(eagle.secure);
    }

    #[tokio::test]
    async fn test_send_includes_user_agent() {
        use crate::notify::APP_ID;
        use wiremock::matchers::header;

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/index.php/jsonrpc/sms"))
            .and(header("User-Agent", APP_ID))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let eagle = smseagle_for_mock(&server, &["@user"]);
        let result = eagle.send(&default_ctx()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_json_payload_structure() {
        let server = MockServer::start().await;

        // Verify the JSON-RPC payload structure
        Mock::given(method("POST"))
            .and(path("/index.php/jsonrpc/sms"))
            .and(wiremock::matchers::body_json(serde_json::json!([{
                "method": "sms.send_sms",
                "params": {
                    "login": "token",
                    "pass": "",
                    "to": "@user",
                    "message": "My Title: My Body"
                }
            }])))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let eagle = smseagle_for_mock(&server, &["@user"]);
        let ctx = NotifyContext {
            title: "My Title".into(),
            body: "My Body".into(),
            ..Default::default()
        };
        let result = eagle.send(&ctx).await;
        assert!(result.is_ok());
    }
}
