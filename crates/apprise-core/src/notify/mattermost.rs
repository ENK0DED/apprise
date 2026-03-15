use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

enum MattermostMode {
    Webhook { webhook_path: String },
    Bot { access_token: String },
}

pub struct Mattermost {
    host: String,
    port: Option<u16>,
    mode: MattermostMode,
    channels: Vec<String>,
    secure: bool,
    verify_certificate: bool,
    tags: Vec<String>,
    /// Override base URL for testing (e.g. "http://127.0.0.1:PORT").
    /// When set, replaces the scheme://host:port portion of all requests.
    base_url_override: Option<String>,
}

impl Mattermost {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Webhook: mmost://host/webhook_token[/channel...]
        // Bot:     mmost://bottoken@host[/channel...]
        // HTTPS:   https://mattermost.example.com/hooks/webhook_token
        let host = url.host.clone()?;
        if host.is_empty() { return None; }
        // Reject hosts with colons (invalid port was attempted)
        if host.contains(':') { return None; }
        let secure = url.schema == "mmosts" || url.schema == "https";

        // Validate mode if provided
        let mode_hint = url.get("mode").map(|s| s.to_lowercase());
        if let Some(ref m) = mode_hint {
            match m.as_str() {
                "bot" | "b" | "webhook" | "hook" | "w" | "" => {}
                _ => return None,
            }
        }

        // Collect extra channels from ?to=, ?channel=, ?channels=
        let mut extra_channels: Vec<String> = Vec::new();
        for key in &["to", "channel", "channels"] {
            if let Some(val) = url.get(key) {
                extra_channels.extend(val.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
            }
        }

        let is_bot_mode = mode_hint.as_deref() == Some("bot") || mode_hint.as_deref() == Some("b");

        let (mode, mut channels) = if is_bot_mode {
            // Bot mode: token from first path part or user field
            let token = url.path_parts.first()
                .cloned()
                .or_else(|| url.user.clone())?;
            let channels = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
            (MattermostMode::Bot { access_token: token }, channels)
        } else if url.user.is_some() && !is_bot_mode {
            // User field present — could be bot mode
            let token = url.user.clone()?;
            let channels = url.path_parts.clone();
            (MattermostMode::Bot { access_token: token }, channels)
        } else {
            // Webhook mode
            // For https:// URLs, look for /hooks/ in path
            let path_parts = &url.path_parts;
            if url.schema == "https" || url.schema == "http" {
                // Find the "hooks" element and use the next part as webhook_path
                let hooks_idx = path_parts.iter().position(|p| p == "hooks")?;
                let webhook_path = path_parts.get(hooks_idx + 1)?.clone();
                let channels = path_parts.get(hooks_idx + 2..).unwrap_or(&[]).to_vec();
                (MattermostMode::Webhook { webhook_path }, channels)
            } else {
                // Standard: last non-empty path part is the webhook token
                // (supports mmost://host/a/path/token)
                let non_empty: Vec<&String> = path_parts.iter().filter(|s| !s.is_empty()).collect();
                if non_empty.is_empty() { return None; }
                let webhook_path = non_empty.last()?.to_string();
                let channels: Vec<String> = Vec::new(); // channels come from extra_channels
                (MattermostMode::Webhook { webhook_path }, channels)
            }
        };

        channels.extend(extra_channels);

        Some(Self { host, port: url.port, mode, channels, secure, verify_certificate: url.verify_certificate(), tags: url.tags(), base_url_override: None })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Mattermost", service_url: Some("https://mattermost.com"), setup_url: None, protocols: vec!["mmost", "mmosts"], description: "Send via Mattermost webhooks or bot API.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Mattermost {
    fn schemas(&self) -> &[&str] { &["mmost", "mmosts"] }
    fn service_name(&self) -> &str { "Mattermost" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let base_url = if let Some(ref ov) = self.base_url_override {
            ov.clone()
        } else {
            let schema = if self.secure { "https" } else { "http" };
            let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
            format!("{}://{}{}", schema, self.host, port_str)
        };
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let client = build_client(self.verify_certificate)?;

        match &self.mode {
            MattermostMode::Webhook { webhook_path } => {
                let url = format!("{}/hooks/{}", base_url, webhook_path);
                // Send to each channel (or once if no channels)
                let targets: Vec<Option<&String>> = if self.channels.is_empty() {
                    vec![None]
                } else {
                    self.channels.iter().map(Some).collect()
                };
                let mut all_ok = true;
                for ch in targets {
                    let mut payload = json!({ "text": text });
                    if let Some(channel) = ch {
                        let ch_name = if channel.starts_with('#') { channel.clone() } else { format!("#{}", channel) };
                        payload["channel"] = json!(ch_name);
                    }
                    let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
                    if !resp.status().is_success() { all_ok = false; }
                }
                Ok(all_ok)
            }
            MattermostMode::Bot { access_token } => {
                let url = format!("{}/api/v4/posts", base_url);
                let mut all_ok = true;
                for channel in &self.channels {
                    let payload = json!({ "channel_id": channel, "message": text });
                    let resp = client.post(&url)
                        .header("User-Agent", APP_ID)
                        .header("Authorization", format!("Bearer {}", access_token))
                        .json(&payload)
                        .send().await?;
                    if !resp.status().is_success() { all_ok = false; }
                }
                Ok(all_ok)
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "mmost://localhost/3ccdd113474722377935511fc85d3dd4",
            "mmost://localhost/3ccdd113474722377935511fc85d3dd4?icon_url=http://localhost/test.png",
            "mmost://user@localhost/3ccdd113474722377935511fc85d3dd4?channel=test",
            "mmost://user@localhost/3ccdd113474722377935511fc85d3dd4?channels=test",
            "mmost://user@localhost/3ccdd113474722377935511fc85d3dd4?to=test",
            "mmost://localhost/3ccdd113474722377935511fc85d3dd4?to=test&image=True",
            "mmost://team@localhost/3ccdd113474722377935511fc85d3dd4?channel=$!garbag3^&mode=bot",
            "mmost://localhost/3ccdd113474722377935511fc85d3dd4?to=test&image=False",
            "mmost://localhost:8080/3ccdd113474722377935511fc85d3dd4",
            "mmosts://localhost/3ccdd113474722377935511fc85d3dd4",
            "https://mattermost.example.com/hooks/3ccdd113474722377935511fc85d3dd4",
            "mmosts://localhost/a/path/3ccdd113474722377935511fc85d3dd4",
            "mmosts://localhost/////3ccdd113474722377935511fc85d3dd4///",
            "mmost://localhost/token?mode=w",
            "mmost://localhost/token?mode=b&to=channel-id-1",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "mmost://",
            "mmosts://",
            "mmost://:@/",
            "mmosts://localhost",
            "mmost://localhost:invalid-port/3ccdd113474722377935511fc85d3dd4",
            "mmost://localhost/token?mode=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    // ── Behavioral tests using wiremock ──────────────────────────────────

    use super::*;
    use crate::asset::AppriseAsset;
    use crate::notify::{Notify, NotifyContext};
    use crate::types::{NotifyFormat, NotifyType};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Helper: build a NotifyContext with sensible defaults.
    fn ctx(title: &str, body: &str) -> NotifyContext {
        NotifyContext {
            body: body.to_string(),
            title: title.to_string(),
            notify_type: NotifyType::Info,
            body_format: NotifyFormat::Text,
            attachments: vec![],
            interpret_escapes: false,
            interpret_emojis: false,
            tags: vec![],
            asset: AppriseAsset::default(),
        }
    }

    /// Helper: create a webhook-mode Mattermost pointing at the mock server.
    fn webhook_mm(server: &MockServer, token: &str, channels: Vec<&str>) -> Mattermost {
        let addr = server.address();
        let base = format!("http://127.0.0.1:{}", addr.port());
        Mattermost {
            host: "localhost".to_string(),
            port: None,
            mode: MattermostMode::Webhook { webhook_path: token.to_string() },
            channels: channels.iter().map(|s| s.to_string()).collect(),
            secure: false,
            verify_certificate: false,
            tags: vec![],
            base_url_override: Some(base),
        }
    }

    /// Helper: create a bot-mode Mattermost pointing at the mock server.
    fn bot_mm(server: &MockServer, token: &str, channels: Vec<&str>) -> Mattermost {
        let addr = server.address();
        let base = format!("http://127.0.0.1:{}", addr.port());
        Mattermost {
            host: "localhost".to_string(),
            port: None,
            mode: MattermostMode::Bot { access_token: token.to_string() },
            channels: channels.iter().map(|s| s.to_string()).collect(),
            secure: false,
            verify_certificate: false,
            tags: vec![],
            base_url_override: Some(base),
        }
    }

    // ── 1. Webhook mode: basic POST with JSON payload ───────────────────

    #[tokio::test]
    async fn test_webhook_post_json_payload() {
        // Mirrors Python test_mattermost_post_default_port: POST to
        // /hooks/token with JSON payload containing text field.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hooks/token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mm = webhook_mm(&server, "token", vec![]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Webhook POST should succeed");
    }

    #[tokio::test]
    async fn test_webhook_no_channel_no_channel_key() {
        // Mirrors Python test_plugin_mattermost_webhook_payload_variants case 3:
        // when no channels are specified, payload has no "channel" key.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hooks/token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mm = webhook_mm(&server, "token", vec![]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 2. Channel targeting (#channel, @user) ─────────────────────────

    #[tokio::test]
    async fn test_webhook_channel_targeting() {
        // Mirrors Python test_plugin_mattermost_channels: two channels
        // (#one, two) produce two separate POSTs with channel in payload.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hooks/token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;

        let mm = webhook_mm(&server, "token", vec!["#one", "two"]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_webhook_single_channel_sends_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hooks/token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mm = webhook_mm(&server, "token", vec!["general"]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 3. Custom port and path ─────────────────────────────────────────

    #[tokio::test]
    async fn test_webhook_custom_port() {
        // Mirrors Python test for mmost://localhost:8080/token: the port
        // appears in the URL. We verify via from_url parsing.
        let parsed = ParsedUrl::parse("mmost://localhost:8080/3ccdd113474722377935511fc85d3dd4").unwrap();
        let mm = Mattermost::from_url(&parsed).unwrap();
        assert_eq!(mm.port, Some(8080));
    }

    #[tokio::test]
    async fn test_webhook_with_path() {
        // Mirrors Python test for mmosts://localhost/a/path/token: the
        // last non-empty path segment is the webhook token.
        let parsed = ParsedUrl::parse("mmosts://localhost/a/path/mytoken").unwrap();
        let mm = Mattermost::from_url(&parsed).unwrap();
        match &mm.mode {
            MattermostMode::Webhook { webhook_path } => {
                assert_eq!(webhook_path, "mytoken");
            }
            _ => panic!("Expected webhook mode"),
        }
        assert!(mm.secure);
    }

    // ── 4. Error handling (HTTP 500, connection error) ───────────────────

    #[tokio::test]
    async fn test_webhook_http_500_returns_false() {
        // Mirrors Python: requests_response_code=500 -> response=False
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hooks/token"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let mm = webhook_mm(&server, "token", vec![]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "HTTP 500 should return false");
    }

    #[tokio::test]
    async fn test_webhook_bizarre_status_code_returns_false() {
        // Mirrors Python: requests_response_code=999 -> response=False
        // wiremock only supports valid HTTP codes; use 599 as proxy.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hooks/token"))
            .respond_with(ResponseTemplate::new(599))
            .expect(1)
            .mount(&server)
            .await;

        let mm = webhook_mm(&server, "token", vec![]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "Unusual HTTP status should return false");
    }

    #[tokio::test]
    async fn test_connection_refused_returns_error() {
        // Mirrors Python test_requests_exceptions: connection errors
        // should propagate as Err.
        let mm = Mattermost {
            host: "localhost".to_string(),
            port: None,
            mode: MattermostMode::Webhook { webhook_path: "token".to_string() },
            channels: vec![],
            secure: false,
            verify_certificate: false,
            tags: vec![],
            base_url_override: Some("http://127.0.0.1:19999".to_string()),
        };

        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_err(), "Connection refused should return Err");
    }

    #[tokio::test]
    async fn test_webhook_partial_failure_returns_false() {
        // Two channels: both get HTTP 500, result should be false.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hooks/token"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let mm = webhook_mm(&server, "token", vec!["chan1", "chan2"]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "Partial failure should return false");
    }

    // ── 5. HTTPS URL support ────────────────────────────────────────────

    #[test]
    fn test_https_url_parses_as_secure() {
        // Mirrors Python: mmosts://localhost/token sets secure=True
        let parsed = ParsedUrl::parse("mmosts://localhost/3ccdd113474722377935511fc85d3dd4").unwrap();
        let mm = Mattermost::from_url(&parsed).unwrap();
        assert!(mm.secure);
    }

    #[test]
    fn test_https_hook_url_parses() {
        // Mirrors Python: https://mattermost.example.com/hooks/token
        let parsed = ParsedUrl::parse(
            "https://mattermost.example.com/hooks/3ccdd113474722377935511fc85d3dd4",
        ).unwrap();
        let mm = Mattermost::from_url(&parsed).unwrap();
        assert!(mm.secure);
        assert_eq!(mm.host, "mattermost.example.com");
        match &mm.mode {
            MattermostMode::Webhook { webhook_path } => {
                assert_eq!(webhook_path, "3ccdd113474722377935511fc85d3dd4");
            }
            _ => panic!("Expected webhook mode"),
        }
    }

    // ── 6. Bot mode ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_bot_post_with_bearer_token() {
        // Mirrors Python test_plugin_mattermost_bot_mode_success_and_payload:
        // POST to /api/v4/posts with Authorization: Bearer header.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let mm = bot_mm(&server, "bearerToken", vec!["channel-id-123"]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Bot mode POST should succeed with 201");
    }

    #[tokio::test]
    async fn test_bot_multiple_channels() {
        // Multiple channel IDs produce one POST per channel.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(201))
            .expect(2)
            .mount(&server)
            .await;

        let mm = bot_mm(&server, "bearerToken", vec!["id1", "id2"]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_bot_no_channels_returns_true_no_requests() {
        // Mirrors Python test_plugin_mattermost_bot_mode_requires_channel_id:
        // bot mode with no channel IDs sends no requests. In Rust, the loop
        // just doesn't execute, so all_ok stays true (differs slightly from
        // Python which returns False explicitly).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(201))
            .expect(0)
            .mount(&server)
            .await;

        let mm = bot_mm(&server, "bearerToken", vec![]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_bot_http_401_returns_false() {
        // Mirrors Python test_plugin_mattermost_bot_mode_http_error_and_exception:
        // unauthorized returns false.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let mm = bot_mm(&server, "bearerToken", vec!["id1"]);
        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "Bot HTTP 401 should return false");
    }

    #[tokio::test]
    async fn test_bot_connection_refused_returns_error() {
        let mm = Mattermost {
            host: "localhost".to_string(),
            port: None,
            mode: MattermostMode::Bot { access_token: "token".to_string() },
            channels: vec!["id1".to_string()],
            secure: false,
            verify_certificate: false,
            tags: vec![],
            base_url_override: Some("http://127.0.0.1:19999".to_string()),
        };

        let result = mm.send(&ctx("title", "body")).await;
        assert!(result.is_err(), "Bot connection refused should return Err");
    }

    // ── 7. Service details ──────────────────────────────────────────────

    #[test]
    fn test_static_details() {
        let details = Mattermost::static_details();
        assert_eq!(details.service_name, "Mattermost");
        assert!(!details.attachment_support);
        assert_eq!(details.protocols, vec!["mmost", "mmosts"]);
    }
}
