use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct RocketChat {
    host: String,
    port: Option<u16>,
    webhook_token: Option<String>,
    user: Option<String>,
    password: Option<String>,
    targets: Vec<String>,
    secure: bool,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl RocketChat {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let secure = url.schema == "rockets";

        // Validate mode if provided
        if let Some(mode) = url.get("mode") {
            match mode.to_lowercase().as_str() {
                "basic" | "webhook" | "token" | "bot" | "slack" | "" => {}
                _ => return None,
            }
        }

        let mode_hint = url.get("mode").map(|s| s.to_lowercase());

        // Handle the case where webhook token contains '/' and the url crate
        // misparses it: rockets://web/token@localhost/@user/#channel/roomid
        // becomes host=web, path_parts=["token@localhost", "@user", "#channel", "roomid"]
        let (host, webhook_token, user, password, raw_targets) =
            if url.user.is_none() && url.password.is_none() {
                // Check if first path_part contains '@' — misparse of credentials
                if let Some(first_part) = url.path_parts.first() {
                    if first_part.contains('@') {
                        // Split on last '@' to get webhook_token_suffix and real host
                        if let Some(at_pos) = first_part.rfind('@') {
                            let wh_suffix = &first_part[..at_pos];
                            let real_host = &first_part[at_pos + 1..];
                            let parsed_host = url.host.clone()?;
                            let webhook_tok = format!("{}/{}", parsed_host, wh_suffix);
                            let targets = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
                            (real_host.to_string(), Some(webhook_tok), None, None, targets)
                        } else {
                            let host = url.host.clone()?;
                            (host, None, None, None, url.path_parts.clone())
                        }
                    } else {
                        let host = url.host.clone()?;
                        // No auth — check for ?webhook= param
                        if let Some(wh) = url.get("webhook") {
                            (host, Some(wh.to_string()), None, None, url.path_parts.clone())
                        } else if mode_hint.as_deref() == Some("webhook") {
                            // Webhook mode but no token in URL — can't proceed
                            return None;
                        } else {
                            // No auth at all — invalid
                            return None;
                        }
                    }
                } else {
                    let host = url.host.clone()?;
                    if let Some(wh) = url.get("webhook") {
                        (host, Some(wh.to_string()), None, None, Vec::new())
                    } else {
                        return None;
                    }
                }
            } else {
                let host = url.host.clone()?;
                match (&url.user, &url.password) {
                    (Some(u), Some(p)) => {
                        if mode_hint.as_deref() == Some("webhook") || p.contains('/') {
                            // Webhook mode with token in password or user:password
                            let wh = if u.contains('/') {
                                format!("{}", u)
                            } else {
                                format!("{}/{}", u, p)
                            };
                            (host, Some(wh), None, None, url.path_parts.clone())
                        } else {
                            // Basic auth mode
                            (host, None, Some(u.clone()), Some(p.clone()), url.path_parts.clone())
                        }
                    }
                    (Some(u), None) => {
                        if u.contains('/') {
                            (host, Some(u.clone()), None, None, url.path_parts.clone())
                        } else {
                            return None;
                        }
                    }
                    _ => return None,
                }
            };

        if host.is_empty() { return None; }

        // Collect targets from raw_targets and ?to= param
        let mut targets: Vec<String> = raw_targets.iter()
            .filter(|s| !s.is_empty() && s.len() > 1)
            .cloned()
            .collect();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }

        // For basic auth mode (user:pass), must have at least one target
        // For webhook mode, empty targets means "use the webhook's default channel"
        if webhook_token.is_none() && targets.is_empty() { return None; }

        // Validate targets — reject single special chars
        targets.retain(|t| {
            let stripped = t.trim_start_matches(|c: char| c == '#' || c == '@');
            !stripped.is_empty()
        });

        Some(Self { host, port: url.port, webhook_token, user, password, targets, secure, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Rocket.Chat", service_url: Some("https://rocket.chat"), setup_url: None, protocols: vec!["rocket", "rockets"], description: "Send via Rocket.Chat webhooks.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for RocketChat {
    fn schemas(&self) -> &[&str] { &["rocket", "rockets"] }
    fn service_name(&self) -> &str { "Rocket.Chat" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let client = build_client(self.verify_certificate)?;

        let mut all_ok = true;
        for target in &self.targets {
            let channel = if target.starts_with('#') || target.starts_with('@') {
                target.clone()
            } else {
                format!("#{}", target)
            };

            if let Some(ref wh) = self.webhook_token {
                let url = format!("{}://{}{}/hooks/{}", schema, self.host, port_str, wh);
                let payload = json!({ "text": text, "channel": channel });
                let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
                if !resp.status().is_success() { all_ok = false; }
            } else if let (Some(u), Some(p)) = (&self.user, &self.password) {
                let url = format!("{}://{}{}/api/v1/chat.postMessage", schema, self.host, port_str);
                let payload = json!({ "text": text, "channel": channel });
                let resp = client.post(&url)
                    .header("User-Agent", APP_ID)
                    .basic_auth(u, Some(p))
                    .json(&payload)
                    .send().await?;
                if !resp.status().is_success() { all_ok = false; }
            }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;
    use crate::notify::NotifyContext;
    use wiremock::matchers::{method, path, header, body_json};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "rocket://user:pass@localhost:8080/room/",
            "rockets://user:pass@localhost?to=#channel",
            "rockets://user:pass@localhost/#channel",
            "rockets://user:token@localhost/#channel?mode=token",
            "rocket://user:pass@localhost/#channel1/#channel2/?avatar=Yes",
            "rocket://user:pass@localhost/room1/room2",
            "rocket://user:pass@localhost/room/#channel?mode=basic&avatar=Yes",
            "rockets://user:pass%2Fwithslash@localhost/#channel/?mode=basic",
            "rockets://web/token@localhost/@user/#channel/roomid",
            "rockets://user:web/token@localhost/@user/?mode=webhook",
            "rockets://user:web/token@localhost?to=@user2,#channel2",
            "rockets://web/token@localhost/?avatar=No",
            "rockets://localhost/@user/?mode=webhook&webhook=web/token",
            "rocket://user:pass@localhost:8083/#chan1/#chan2/room",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "rocket://",
            "rockets://",
            "rocket://:@/",
            "rocket://localhost",
            "rocket://user:pass@localhost",
            "rocket://user:pass@localhost/#/!/@",
            "rocket://user@localhost/room/",
            "rocket://localhost/room/",
            "rockets://user:web/token@localhost/@user/?mode=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    /// Build a RocketChat instance pointing at the mock server using webhook mode.
    fn rocketchat_webhook_for_mock(server: &MockServer, token: &str, targets: &[&str]) -> RocketChat {
        let addr = server.address();
        let target_path = targets.iter().map(|t| format!("/{}", t)).collect::<String>();
        let url_str = format!(
            "rocket://web/{}@{}:{}{}",
            token, addr.ip(), addr.port(), target_path
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        RocketChat::from_url(&parsed).unwrap()
    }

    /// Build a RocketChat instance pointing at the mock server using basic auth mode.
    fn rocketchat_basic_for_mock(server: &MockServer, user: &str, pass: &str, targets: &[&str]) -> RocketChat {
        let addr = server.address();
        let target_path = targets.iter().map(|t| format!("/{}", t)).collect::<String>();
        let url_str = format!(
            "rocket://{}:{}@{}:{}{}",
            user, pass, addr.ip(), addr.port(), target_path
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        RocketChat::from_url(&parsed).unwrap()
    }

    fn default_ctx() -> NotifyContext {
        NotifyContext {
            title: "Test Title".into(),
            body: "Test Body".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_webhook_post_success() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/hooks/web/mytoken"))
            .and(header("User-Agent", APP_ID))
            .and(body_json(serde_json::json!({
                "text": "**Test Title**\nTest Body",
                "channel": "#channel"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let rc = rocketchat_webhook_for_mock(&server, "mytoken", &["#channel"]);
        let result = rc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_webhook_channel_and_user_targets() {
        let server = MockServer::start().await;

        // Expect two POST requests: one for #general, one for @admin
        Mock::given(method("POST"))
            .and(path("/hooks/web/tok123"))
            .and(body_json(serde_json::json!({
                "text": "**Test Title**\nTest Body",
                "channel": "#general"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .named("channel target")
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/hooks/web/tok123"))
            .and(body_json(serde_json::json!({
                "text": "**Test Title**\nTest Body",
                "channel": "@admin"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .named("user target")
            .mount(&server)
            .await;

        let rc = rocketchat_webhook_for_mock(&server, "tok123", &["#general", "@admin"]);
        let result = rc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_webhook_room_target_gets_hash_prefix() {
        // A bare room name (no # or @) should get a # prefix
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/hooks/web/tok"))
            .and(body_json(serde_json::json!({
                "text": "**Test Title**\nTest Body",
                "channel": "#myroom"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let rc = rocketchat_webhook_for_mock(&server, "tok", &["myroom"]);
        let result = rc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_basic_auth_post_success() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/v1/chat.postMessage"))
            .and(header("User-Agent", APP_ID))
            .and(header("Authorization", "Basic dXNlcjpwYXNz")) // base64("user:pass")
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let rc = rocketchat_basic_for_mock(&server, "user", "pass", &["#channel"]);
        let result = rc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_webhook_error_500_returns_false() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/hooks/web/errtoken"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let rc = rocketchat_webhook_for_mock(&server, "errtoken", &["#channel"]);
        let result = rc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_basic_auth_error_500_returns_false() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/v1/chat.postMessage"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let rc = rocketchat_basic_for_mock(&server, "user", "pass", &["#chan"]);
        let result = rc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_multiple_targets_one_fails() {
        // If one target returns 500, overall result should be false
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/hooks/web/multi"))
            .and(body_json(serde_json::json!({
                "text": "**Test Title**\nTest Body",
                "channel": "#good"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .named("good channel")
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/hooks/web/multi"))
            .and(body_json(serde_json::json!({
                "text": "**Test Title**\nTest Body",
                "channel": "#bad"
            })))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .named("bad channel")
            .mount(&server)
            .await;

        let rc = rocketchat_webhook_for_mock(&server, "multi", &["#good", "#bad"]);
        let result = rc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_connection_failure() {
        // Point at a port nothing listens on
        let url_str = "rocket://web/tok@127.0.0.1:1/#chan";
        let parsed = crate::utils::parse::ParsedUrl::parse(url_str).unwrap();
        let rc = RocketChat::from_url(&parsed).unwrap();

        let result = rc.send(&default_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_title_body_formatting() {
        // When title is empty, text should be just the body
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/hooks/web/fmt"))
            .and(body_json(serde_json::json!({
                "text": "Just the body",
                "channel": "#room"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let rc = rocketchat_webhook_for_mock(&server, "fmt", &["#room"]);
        let ctx = NotifyContext {
            title: "".into(),
            body: "Just the body".into(),
            ..Default::default()
        };
        let result = rc.send(&ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }
}
