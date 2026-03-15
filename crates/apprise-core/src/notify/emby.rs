use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Emby {
    host: String, port: u16, user: String, password: String,
    secure: bool, modal: bool, verify_certificate: bool, tags: Vec<String>,
}

impl Emby {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        let secure = url.schema.ends_with('s');
        let port = url.port.unwrap_or(if secure { 8920 } else { 8096 });
        let modal = url.get("modal").map(crate::utils::parse::parse_bool).unwrap_or(false);
        Some(Self { host, port, user, password, secure, modal, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Emby", service_url: Some("https://emby.media"), setup_url: None, protocols: vec!["emby", "embys"], description: "Send notifications to Emby.", attachment_support: false } }

    fn base_url(&self) -> String {
        let schema = if self.secure { "https" } else { "http" };
        format!("{}://{}:{}", schema, self.host, self.port)
    }
}

#[async_trait]
impl Notify for Emby {
    fn schemas(&self) -> &[&str] { &["emby", "embys"] }
    fn service_name(&self) -> &str { "Emby" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let base = self.base_url();

        // Step 1: Authenticate to get access token and session IDs
        let auth_url = format!("{}/emby/Users/AuthenticateByName", base);
        let auth_header = format!(
            "MediaBrowser Client=\"Apprise\", Device=\"Apprise\", DeviceId=\"apprise\", Version=\"{}\"",
            env!("CARGO_PKG_VERSION")
        );
        let auth_payload = json!({ "Username": self.user, "Pw": self.password });
        let auth_resp = client.post(&auth_url)
            .header("User-Agent", APP_ID)
            .header("X-Emby-Authorization", &auth_header)
            .header("Content-Type", "application/json")
            .json(&auth_payload)
            .send().await?;

        if !auth_resp.status().is_success() {
            return Err(NotifyError::Auth("Emby authentication failed".into()));
        }

        let auth_json: serde_json::Value = auth_resp.json().await?;
        let access_token = auth_json["AccessToken"].as_str()
            .ok_or_else(|| NotifyError::Other("No Emby access token".into()))?;

        // Step 2: Get sessions to send to
        let sessions_url = format!("{}/emby/Sessions?api_key={}", base, access_token);
        let sessions_resp = client.get(&sessions_url)
            .header("User-Agent", APP_ID)
            .send().await?;

        let sessions: Vec<serde_json::Value> = sessions_resp.json().await
            .unwrap_or_default();

        // Step 3: Send message to each session
        let payload = json!({
            "Header": if ctx.title.is_empty() { "Apprise Notification" } else { ctx.title.as_str() },
            "Text": ctx.body,
            "TimeoutMs": 60000_u64,
        });

        let mut all_ok = true;
        for session in &sessions {
            if let Some(session_id) = session["Id"].as_str() {
                let msg_url = format!("{}/emby/Sessions/{}/Message?api_key={}", base, session_id, access_token);
                let resp = client.post(&msg_url)
                    .header("User-Agent", APP_ID)
                    .json(&payload)
                    .send().await?;
                if !resp.status().is_success() { all_ok = false; }
            }
        }

        // Step 4: Logout
        let _ = client.post(format!("{}/emby/Sessions/Logout?api_key={}", base, access_token))
            .header("User-Agent", APP_ID)
            .send().await;

        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;
    use crate::notify::{Notify, NotifyContext};
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "emby://",
            "embys://",
            "emby://localhost",
            "emby://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    fn emby_for_mock(server: &MockServer) -> super::Emby {
        let addr = server.address();
        let url_str = format!("emby://l2g:l2gpass@{}:{}", addr.ip(), addr.port());
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        super::Emby::from_url(&parsed).unwrap()
    }

    fn default_ctx() -> NotifyContext {
        NotifyContext {
            title: "Test Title".into(),
            body: "Test Body".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_send_success() {
        let server = MockServer::start().await;

        // Mock auth endpoint
        Mock::given(method("POST"))
            .and(path_regex("/emby/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "AccessToken": "test-token-123",
                "User": { "Id": "user-abc" },
                "Id": "session-xyz"
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Mock sessions endpoint
        Mock::given(method("GET"))
            .and(path_regex("/emby/Sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "Id": "sess1" },
                { "Id": "sess2" }
            ])))
            .expect(1)
            .mount(&server)
            .await;

        // Mock message sending to sessions
        Mock::given(method("POST"))
            .and(path_regex("/emby/Sessions/.*/Message"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;

        // Mock logout
        Mock::given(method("POST"))
            .and(path_regex("/emby/Sessions/Logout"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let emby = emby_for_mock(&server);
        let ctx = default_ctx();
        let result = emby.send(&ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_auth_failure() {
        let server = MockServer::start().await;

        // Auth returns 401
        Mock::given(method("POST"))
            .and(path_regex("/emby/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let emby = emby_for_mock(&server);
        let ctx = default_ctx();
        let result = emby.send(&ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_server_error() {
        let server = MockServer::start().await;

        // Auth returns 500
        Mock::given(method("POST"))
            .and(path_regex("/emby/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let emby = emby_for_mock(&server);
        let ctx = default_ctx();
        let result = emby.send(&ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_no_sessions() {
        let server = MockServer::start().await;

        // Auth succeeds
        Mock::given(method("POST"))
            .and(path_regex("/emby/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "AccessToken": "test-token",
                "User": { "Id": "user1" }
            })))
            .mount(&server)
            .await;

        // Sessions returns empty array
        Mock::given(method("GET"))
            .and(path_regex("/emby/Sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        // Logout
        Mock::given(method("POST"))
            .and(path_regex("/emby/Sessions/Logout"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let emby = emby_for_mock(&server);
        let ctx = default_ctx();
        let result = emby.send(&ctx).await;
        assert!(result.is_ok());
        // No sessions means no messages sent, but still succeeds
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_message_failure() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path_regex("/emby/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "AccessToken": "test-token",
                "User": { "Id": "user1" }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/emby/Sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "Id": "sess1" }
            ])))
            .mount(&server)
            .await;

        // Message send fails
        Mock::given(method("POST"))
            .and(path_regex("/emby/Sessions/.*/Message"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex("/emby/Sessions/Logout"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let emby = emby_for_mock(&server);
        let ctx = default_ctx();
        let result = emby.send(&ctx).await;
        assert!(result.is_ok());
        // Message failed, so all_ok = false
        assert_eq!(result.unwrap(), false);
    }

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "emby://l2g:pass@localhost",
            "embys://l2g:password@localhost",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_default_ports() {
        let parsed = crate::utils::parse::ParsedUrl::parse("emby://l2g:pass@localhost").unwrap();
        let emby = super::Emby::from_url(&parsed).unwrap();
        assert_eq!(emby.port, 8096);

        let parsed = crate::utils::parse::ParsedUrl::parse("embys://l2g:pass@localhost").unwrap();
        let emby = super::Emby::from_url(&parsed).unwrap();
        assert_eq!(emby.port, 8920);
    }

    #[test]
    fn test_custom_port() {
        let parsed = crate::utils::parse::ParsedUrl::parse("emby://l2g:pass@localhost:1234").unwrap();
        let emby = super::Emby::from_url(&parsed).unwrap();
        assert_eq!(emby.port, 1234);
    }
}
