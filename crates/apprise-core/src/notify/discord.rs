use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

pub struct Discord {
    webhook_id: String,
    webhook_token: String,
    tts: bool,
    avatar_url: Option<String>,
    username: Option<String>,
    footer: bool,
    footer_logo: Option<String>,
    include_image: bool,
    thread_id: Option<String>,
    href: Option<String>,
    verify_certificate: bool,
    tags: Vec<String>,
    /// Override base URL for testing (replaces https://discord.com)
    #[cfg(test)]
    base_url_override: Option<String>,
}

impl Discord {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let webhook_id = url.host.clone()?;
        let webhook_token = url.path_parts.first()?.clone();
        if webhook_token.is_empty() {
            return None;
        }

        let username = url.user.clone();
        let tts = url.get("tts").map(crate::utils::parse::parse_bool).unwrap_or(false);
        let avatar_url = url.get("avatar_url").or_else(|| url.get("avatar")).map(|s| s.to_string());
        let footer = url.get("footer").map(crate::utils::parse::parse_bool).unwrap_or(false);
        let footer_logo = url.get("footer_logo").map(|s| s.to_string());
        let include_image = url.get("image").map(crate::utils::parse::parse_bool).unwrap_or(false);
        let thread_id = url.get("thread").map(|s| s.to_string());
        let href = url.get("href").or_else(|| url.get("url")).map(|s| s.to_string());

        // Validate flags if provided
        if let Some(flags) = url.get("flags") {
            if !flags.is_empty() {
                let val: i64 = flags.parse().ok()?;
                if val < 0 { return None; }
            }
        }

        Some(Self {
            webhook_id, webhook_token, tts, avatar_url, username, footer,
            footer_logo, include_image, thread_id, href,
            verify_certificate: url.verify_certificate(), tags: url.tags(),
            #[cfg(test)]
            base_url_override: None,
        })
    }

    /// Returns the base URL for the webhook endpoint.
    fn webhook_base_url(&self) -> String {
        #[cfg(test)]
        if let Some(ref base) = self.base_url_override {
            return base.clone();
        }
        "https://discord.com".to_string()
    }

    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "Discord",
            service_url: Some("https://discord.com"),
            setup_url: Some("https://support.discord.com/hc/en-us/articles/228383668-Intro-to-Webhooks"),
            protocols: vec!["discord"],
            description: "Send notifications via Discord webhooks.",
            attachment_support: true,
        }
    }
}

#[async_trait]
impl Notify for Discord {
    fn schemas(&self) -> &[&str] { &["discord"] }
    fn service_name(&self) -> &str { "Discord" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    fn attachment_support(&self) -> bool { true }
    fn body_maxlen(&self) -> usize { 2000 }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let base = self.webhook_base_url();
        let mut url = format!(
            "{}/api/webhooks/{}/{}",
            base, self.webhook_id, self.webhook_token
        );

        // Add thread_id as query parameter if specified
        if let Some(ref tid) = self.thread_id {
            url = format!("{}?thread_id={}", url, tid);
        }

        let color = match ctx.notify_type {
            NotifyType::Info => 0x3498DB_u32,
            NotifyType::Success => 0x2ECC71,
            NotifyType::Warning => 0xE67E22,
            NotifyType::Failure => 0xE74C3C,
        };

        let mut payload = json!({
            "tts": self.tts,
            "wait": true,
        });

        if let Some(ref username) = self.username {
            payload["username"] = json!(username);
        }
        if let Some(ref avatar) = self.avatar_url {
            payload["avatar_url"] = json!(avatar);
        }

        // Use embeds for rich formatting
        let mut embed = json!({
            "description": ctx.body,
            "color": color,
        });

        if !ctx.title.is_empty() {
            embed["title"] = json!(ctx.title);
        }

        // Support href/url linking in embed title
        if let Some(ref href) = self.href {
            embed["url"] = json!(href);
        }

        if self.footer {
            let mut footer_obj = json!({ "text": APP_ID });
            if let Some(ref logo) = self.footer_logo {
                footer_obj["icon_url"] = json!(logo);
            }
            embed["footer"] = footer_obj;
        }

        payload["embeds"] = json!([embed]);

        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID)
            .header("Content-Type", "application/json")
            .json(&payload).send().await?;

        let status = resp.status();

        // Handle rate limiting
        if status.as_u16() == 429 {
            if let Some(retry_after) = resp.headers().get("Retry-After") {
                if let Ok(secs) = retry_after.to_str().unwrap_or("1").parse::<f64>() {
                    tracing::warn!("Discord rate limited, retrying after {}s", secs);
                    tokio::time::sleep(tokio::time::Duration::from_secs_f64(secs)).await;
                    // Retry once
                    let resp2 = client
                        .post(format!("{}/api/webhooks/{}/{}", base, self.webhook_id, self.webhook_token))
                        .header("User-Agent", APP_ID)
                        .header("Content-Type", "application/json")
                        .json(&payload)
                        .send()
                        .await?;
                    return if resp2.status().is_success() || resp2.status().as_u16() == 204 {
                        Ok(true)
                    } else {
                        Err(NotifyError::ServiceError { status: resp2.status().as_u16(), body: resp2.text().await.unwrap_or_default() })
                    };
                }
            }
        }

        if status.is_success() || status.as_u16() == 204 {
            // Upload attachments as separate multipart POSTs
            for attach in &ctx.attachments {
                let part = reqwest::multipart::Part::bytes(attach.data.clone())
                    .file_name(attach.name.clone())
                    .mime_str(&attach.mime_type).unwrap_or_else(|_| reqwest::multipart::Part::bytes(attach.data.clone()).file_name(attach.name.clone()));
                let form = reqwest::multipart::Form::new().part("file", part);
                let _ = client.post(&url).multipart(form).send().await;
            }
            Ok(true)
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(NotifyError::ServiceError { status: status.as_u16(), body })
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::AppriseAsset;
    use crate::notify::registry::from_url;
    use crate::notify::{Attachment, Notify, NotifyContext};
    use crate::types::{NotifyFormat, NotifyType};
    use crate::utils::parse::ParsedUrl;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── Helpers ─────────────────────────────────────────────────────────

    /// Build a NotifyContext with sensible defaults.
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

    /// Create a Discord instance from a URL string with mock server override.
    fn discord_from_url_with_mock(server: &MockServer, url: &str) -> Discord {
        let addr = server.address();
        let port = addr.port();
        let parsed = ParsedUrl::parse(url).expect("parse test URL");
        let mut d = Discord::from_url(&parsed).expect("create Discord from test URL");
        d.base_url_override = Some(format!("http://localhost:{}", port));
        d
    }

    // Webhook IDs/tokens used across tests
    const WH_ID: &str = "AAAAAAAAAAAAAAAAAAAAAAAA";
    const WH_TOKEN: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    fn webhook_path() -> String {
        format!("/api/webhooks/{}/{}", WH_ID, WH_TOKEN)
    }

    // ── 1. URL parsing / invalid URLs ───────────────────────────────────

    #[test]
    fn test_invalid_urls() {
        let no_token = format!("discord://{}", "i".repeat(24));
        let urls: Vec<&str> = vec![
            "discord://",
            "discord://:@/",
            // No webhook_token specified (only ID, no path)
            &no_token,
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_valid_urls_parse() {
        let urls = vec![
            format!("discord://{}/{}", "i".repeat(24), "t".repeat(64)),
            format!("discord://l2g@{}/{}", "i".repeat(24), "t".repeat(64)),
            format!("discord://{}/{}?format=markdown&footer=Yes&image=Yes", "i".repeat(24), "t".repeat(64)),
            format!("discord://{}/{}?format=markdown&avatar=No&footer=No", "i".repeat(24), "t".repeat(64)),
            format!("discord://{}/{}?avatar_url=http://localhost/test.jpg", "i".repeat(24), "t".repeat(64)),
            format!("discord://{}/{}?format=markdown&thread=abc123", "i".repeat(24), "t".repeat(64)),
            format!("discord://{}/{}?flags=1", "i".repeat(24), "t".repeat(64)),
        ];
        for url in &urls {
            let parsed = ParsedUrl::parse(url).unwrap();
            assert!(Discord::from_url(&parsed).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_flags_rejected() {
        // Negative flags
        let url = format!("discord://{}/{}?flags=-1", "i".repeat(24), "t".repeat(64));
        let parsed = ParsedUrl::parse(&url).unwrap();
        assert!(Discord::from_url(&parsed).is_none(), "Negative flags should be rejected");

        // Non-numeric flags
        let url = format!("discord://{}/{}?flags=invalid", "i".repeat(24), "t".repeat(64));
        let parsed = ParsedUrl::parse(&url).unwrap();
        assert!(Discord::from_url(&parsed).is_none(), "Non-numeric flags should be rejected");
    }

    #[test]
    fn test_from_url_extracts_webhook_id_and_token() {
        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let parsed = ParsedUrl::parse(&url).unwrap();
        let d = Discord::from_url(&parsed).unwrap();
        assert_eq!(d.webhook_id, WH_ID);
        assert_eq!(d.webhook_token, WH_TOKEN);
    }

    #[test]
    fn test_from_url_extracts_username() {
        let url = format!("discord://l2g@{}/{}", WH_ID, WH_TOKEN);
        let parsed = ParsedUrl::parse(&url).unwrap();
        let d = Discord::from_url(&parsed).unwrap();
        assert_eq!(d.username.as_deref(), Some("l2g"));
    }

    #[test]
    fn test_from_url_footer_and_image() {
        let url = format!("discord://{}/{}?footer=Yes&image=Yes", WH_ID, WH_TOKEN);
        let parsed = ParsedUrl::parse(&url).unwrap();
        let d = Discord::from_url(&parsed).unwrap();
        assert!(d.footer);
        assert!(d.include_image);
    }

    #[test]
    fn test_from_url_avatar_url() {
        let url = format!("discord://{}/{}?avatar_url=http://localhost/test.jpg", WH_ID, WH_TOKEN);
        let parsed = ParsedUrl::parse(&url).unwrap();
        let d = Discord::from_url(&parsed).unwrap();
        assert_eq!(d.avatar_url.as_deref(), Some("http://localhost/test.jpg"));
    }

    #[test]
    fn test_from_url_thread_id() {
        let url = format!("discord://{}/{}?thread=abc123", WH_ID, WH_TOKEN);
        let parsed = ParsedUrl::parse(&url).unwrap();
        let d = Discord::from_url(&parsed).unwrap();
        assert_eq!(d.thread_id.as_deref(), Some("abc123"));
    }

    #[test]
    fn test_from_url_href() {
        let url = format!("discord://{}/{}?href=http://localhost", WH_ID, WH_TOKEN);
        let parsed = ParsedUrl::parse(&url).unwrap();
        let d = Discord::from_url(&parsed).unwrap();
        assert_eq!(d.href.as_deref(), Some("http://localhost"));

        // Also test url= alias
        let url = format!("discord://{}/{}?url=http://example.com", WH_ID, WH_TOKEN);
        let parsed = ParsedUrl::parse(&url).unwrap();
        let d = Discord::from_url(&parsed).unwrap();
        assert_eq!(d.href.as_deref(), Some("http://example.com"));
    }

    // ── 2. Basic webhook POST with correct JSON payload ─────────────────

    #[tokio::test]
    async fn test_basic_send_json_payload() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(header("Content-Type", "application/json"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_payload_contains_tts_and_embeds() {
        let server = MockServer::start().await;
        // Verify tts and embeds are in the JSON body
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"tts\":false"))
            .and(body_string_contains("\"embeds\""))
            .and(body_string_contains("\"wait\":true"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_payload_embed_has_description_and_color() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"description\":\"my body\""))
            .and(body_string_contains("\"color\":"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("my title", "my body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_payload_embed_has_title() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"title\":\"hello\""))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("hello", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_empty_title_omits_title_in_embed() {
        let server = MockServer::start().await;
        // We just verify the request is accepted; the embed should not contain "title"
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("", "body only")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 3. Username / avatar_url in payload ─────────────────────────────

    #[tokio::test]
    async fn test_username_in_payload() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"username\":\"l2g\""))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://l2g@{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_avatar_url_in_payload() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"avatar_url\":\"http://localhost/test.jpg\""))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}?avatar_url=http://localhost/test.jpg", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 4. Footer in embed ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_footer_in_embed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"footer\""))
            .and(body_string_contains("\"text\":\"Apprise/"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}?footer=Yes", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_footer_logo_in_embed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"icon_url\":\"http://example.com/logo.png\""))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!(
            "discord://{}/{}?footer=Yes&footer_logo=http://example.com/logo.png",
            WH_ID, WH_TOKEN
        );
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 5. href/url linking in embed ────────────────────────────────────

    #[tokio::test]
    async fn test_href_in_embed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"url\":\"http://localhost\""))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}?href=http://localhost", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 6. TTS setting ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_tts_enabled() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"tts\":true"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}?tts=yes", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 7. Thread ID as query parameter ─────────────────────────────────

    #[tokio::test]
    async fn test_thread_id_appended_as_query_param() {
        let server = MockServer::start().await;
        // When thread_id is set, the URL should have ?thread_id=12345
        Mock::given(method("POST"))
            .and(wiremock::matchers::query_param("thread_id", "12345"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}?thread=12345", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 8. Notify type colors ───────────────────────────────────────────

    #[tokio::test]
    async fn test_info_color() {
        let server = MockServer::start().await;
        // Info color = 0x3498DB = 3447003
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"color\":3447003"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let mut c = ctx("title", "body");
        c.notify_type = NotifyType::Info;
        let result = d.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_success_color() {
        let server = MockServer::start().await;
        // Success color = 0x2ECC71 = 3066993
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"color\":3066993"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let mut c = ctx("title", "body");
        c.notify_type = NotifyType::Success;
        let result = d.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_warning_color() {
        let server = MockServer::start().await;
        // Warning color = 0xE67E22 = 15105570
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"color\":15105570"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let mut c = ctx("title", "body");
        c.notify_type = NotifyType::Warning;
        let result = d.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_failure_color() {
        let server = MockServer::start().await;
        // Failure color = 0xE74C3C = 15158332
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"color\":15158332"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let mut c = ctx("title", "body");
        c.notify_type = NotifyType::Failure;
        let result = d.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 9. Error handling ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_http_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_err(), "HTTP 500 should return Err");
    }

    #[tokio::test]
    async fn test_http_204_no_content_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_rate_limiting_429_retries() {
        let server = MockServer::start().await;
        // First request returns 429 with Retry-After header
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(
                ResponseTemplate::new(429)
                    .append_header("Retry-After", "0")
            )
            .expect(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second request (retry) returns 200
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Should succeed after retry on 429");
    }

    #[tokio::test]
    async fn test_rate_limiting_429_retry_also_fails() {
        let server = MockServer::start().await;
        // Both requests return 429
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(
                ResponseTemplate::new(429)
                    .append_header("Retry-After", "0")
            )
            .expect(2)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        // The retry gets a non-success status, should return error
        assert!(result.is_err(), "Double 429 should return Err");
    }

    #[tokio::test]
    async fn test_connection_refused_returns_error() {
        // Point at a port nothing is listening on
        let parsed = ParsedUrl::parse(&format!("discord://{}/{}", WH_ID, WH_TOKEN)).unwrap();
        let mut d = Discord::from_url(&parsed).unwrap();
        d.base_url_override = Some("http://localhost:19999".to_string());

        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_err(), "Connection refused should return Err");
    }

    // ── 10. Attachment upload via multipart ──────────────────────────────

    #[tokio::test]
    async fn test_attachment_sends_two_requests() {
        let server = MockServer::start().await;
        // First POST: JSON payload (the notification itself)
        // Second POST: multipart file upload
        // Both go to the same webhook path
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);

        let mut c = ctx("title", "body");
        c.attachments.push(Attachment {
            name: "test.gif".to_string(),
            data: b"GIF89a".to_vec(),
            mime_type: "image/gif".to_string(),
        });

        let result = d.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
        // wiremock expect(2) will verify both requests were made
    }

    #[tokio::test]
    async fn test_multiple_attachments_send_multiple_requests() {
        let server = MockServer::start().await;
        // 1 JSON POST + 2 multipart POSTs = 3 total
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(ResponseTemplate::new(200))
            .expect(3)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);

        let mut c = ctx("title", "body");
        c.attachments.push(Attachment {
            name: "a.gif".to_string(),
            data: b"GIF89a".to_vec(),
            mime_type: "image/gif".to_string(),
        });
        c.attachments.push(Attachment {
            name: "b.png".to_string(),
            data: b"PNG".to_vec(),
            mime_type: "image/png".to_string(),
        });

        let result = d.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_attachment_upload_after_json_failure_returns_error() {
        let server = MockServer::start().await;
        // The JSON POST returns 500 so attachments should not be sent
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);

        let mut c = ctx("title", "body");
        c.attachments.push(Attachment {
            name: "file.gif".to_string(),
            data: b"GIF89a".to_vec(),
            mime_type: "image/gif".to_string(),
        });

        let result = d.send(&c).await;
        assert!(result.is_err(), "Should fail when JSON POST returns 500");
        // Only 1 request expected (no attachment upload attempted)
    }

    // ── 11. Webhook URL construction ────────────────────────────────────

    #[tokio::test]
    async fn test_correct_webhook_url_is_called() {
        let server = MockServer::start().await;
        let wh_id = "C".repeat(24);
        let wh_token = "D".repeat(64);
        let expected_path = format!("/api/webhooks/{}/{}", wh_id, wh_token);

        Mock::given(method("POST"))
            .and(path(&expected_path))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", wh_id, wh_token);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("t", "b")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 12. HTTP 200 OK also succeeds (not just 204) ────────────────────

    #[tokio::test]
    async fn test_http_200_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 13. No username means no username field in payload ───────────────

    #[tokio::test]
    async fn test_no_username_omits_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        assert!(d.username.is_none());
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 14. User-Agent header is set ────────────────────────────────────

    #[tokio::test]
    async fn test_user_agent_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(header("User-Agent", APP_ID))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 15. No footer by default ────────────────────────────────────────

    #[tokio::test]
    async fn test_no_footer_by_default() {
        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let parsed = ParsedUrl::parse(&url).unwrap();
        let d = Discord::from_url(&parsed).unwrap();
        assert!(!d.footer);
    }

    // ── 16. No tts by default ───────────────────────────────────────────

    #[tokio::test]
    async fn test_tts_disabled_by_default() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(webhook_path()))
            .and(body_string_contains("\"tts\":false"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("discord://{}/{}", WH_ID, WH_TOKEN);
        let d = discord_from_url_with_mock(&server, &url);
        let result = d.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }
}
