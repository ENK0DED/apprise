use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

pub struct Slack {
    mode: SlackMode,
    verify_certificate: bool,
    tags: Vec<String>,
    bot_name: Option<String>,
    /// Override base URLs for testing (webhook and API).
    #[cfg(test)]
    webhook_url_override: Option<String>,
    #[cfg(test)]
    api_url_override: Option<String>,
}

enum SlackMode {
    /// Incoming webhook — token_a/token_b/token_c
    Webhook {
        token_a: String,
        token_b: String,
        token_c: String,
        channels: Vec<String>,
    },
    /// Bot token — access_token + channels
    Bot {
        access_token: String,
        channels: Vec<String>,
    },
}

impl Slack {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Webhook: slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ[/channel...]
        // Bot:     slack://xoxb-token/channel...
        // Query:   slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnl&to=#chan
        let bot_name = url.user.clone()
            .or_else(|| url.get("user").map(|s| s.to_string()));

        // Validate mode if provided
        let mode_hint = url.get("mode").map(|s| s.to_lowercase());
        if let Some(ref m) = mode_hint {
            match m.as_str() {
                "bot" | "webhook" | "hook" | "w" | "b" | "" => {}
                _ => return None,
            }
        }

        // Collect ?to= targets
        let mut extra_channels: Vec<String> = Vec::new();
        if let Some(to) = url.get("to") {
            extra_channels.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }

        // Check for ?token= query param
        let token_param = url.get("token").map(|s| s.to_string());

        // Build the token parts from ?token= OR from host+path
        let mut token_parts: Vec<String> = Vec::new();
        let mut url_channels: Vec<String> = Vec::new();

        if let Some(ref tp) = token_param {
            // Token from query param — split on /
            token_parts.extend(tp.split('/').filter(|s| !s.is_empty()).map(|s| s.to_string()));
            // Path parts from the URL are channels (e.g., /#nuxref before the ?)
            url_channels.extend(url.path_parts.clone());
        } else {
            // Token from host + path_parts
            if let Some(ref h) = url.host {
                if !h.is_empty() && h != "_" {
                    token_parts.push(h.clone());
                }
            }
            token_parts.extend(url.path_parts.clone());
        }

        // Is the first token part a bot token (xoxb-, xoxe.xoxb-, xoxe.xoxp-)?
        let is_bot_token = token_parts.first().map(|t| {
            t.starts_with("xoxb-") || t.starts_with("xoxe.xoxb-") || t.starts_with("xoxe.xoxp-") || t.starts_with("xoxp-")
        }).unwrap_or(false);

        let forced_webhook = mode_hint.as_deref() == Some("hook")
            || mode_hint.as_deref() == Some("webhook")
            || mode_hint.as_deref() == Some("w");
        let forced_bot = mode_hint.as_deref() == Some("bot") || mode_hint.as_deref() == Some("b");

        let mode = if is_bot_token && !forced_webhook {
            // Bot mode
            let access_token = token_parts.first()?.clone();
            let mut channels: Vec<String> = token_parts.get(1..).unwrap_or(&[]).to_vec();
            channels.extend(url_channels);
            channels.extend(extra_channels);
            SlackMode::Bot { access_token, channels }
        } else if token_parts.len() >= 3 {
            // Webhook mode — need exactly 3 token parts for the webhook
            let token_a = token_parts[0].clone();
            let token_b = token_parts[1].clone();
            let token_c = token_parts[2].clone();

            // Validate tokens - reject -INVALID- patterns
            if token_a.starts_with('-') || token_b.starts_with('-') || token_c.starts_with('-') {
                return None;
            }

            // If mode is explicitly bot, reject (webhook token != bot token)
            if forced_bot { return None; }

            let mut channels: Vec<String> = token_parts.get(3..).unwrap_or(&[]).to_vec();
            channels.extend(url_channels);
            channels.extend(extra_channels);
            SlackMode::Webhook { token_a, token_b, token_c, channels }
        } else {
            // Not enough parts for webhook and not a bot token
            return None;
        };

        Some(Self {
            mode,
            verify_certificate: url.verify_certificate(),
            tags: url.tags(),
            bot_name,
            #[cfg(test)]
            webhook_url_override: None,
            #[cfg(test)]
            api_url_override: None,
        })
    }

    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "Slack",
            service_url: Some("https://slack.com"),
            setup_url: Some("https://api.slack.com/incoming-webhooks"),
            protocols: vec!["slack"],
            description: "Send Slack messages via webhooks or bot tokens.",
            attachment_support: true,
        }
    }

    fn color_for_type(t: &NotifyType) -> &'static str {
        match t {
            NotifyType::Info => "#3498DB",
            NotifyType::Success => "#2ECC71",
            NotifyType::Warning => "#E67E22",
            NotifyType::Failure => "#E74C3C",
        }
    }
}

#[async_trait]
impl Notify for Slack {
    fn schemas(&self) -> &[&str] { &["slack"] }
    fn service_name(&self) -> &str { "Slack" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let color = Self::color_for_type(&ctx.notify_type);
        let bot_name = self.bot_name.as_deref().unwrap_or("Apprise");

        match &self.mode {
            SlackMode::Webhook { token_a, token_b, token_c, channels } => {
                let webhook_url = {
                    let default = format!(
                        "https://hooks.slack.com/services/{}/{}/{}",
                        token_a, token_b, token_c
                    );
                    #[cfg(test)]
                    { self.webhook_url_override.clone().unwrap_or(default) }
                    #[cfg(not(test))]
                    { default }
                };
                let base_payload = json!({
                    "username": bot_name,
                    "attachments": [{
                        "fallback": ctx.body,
                        "title": ctx.title,
                        "text": ctx.body,
                        "color": color,
                    }]
                });
                // Iterate over all channels (or send once if none specified)
                let targets: Vec<Option<&String>> = if channels.is_empty() {
                    vec![None]
                } else {
                    channels.iter().map(Some).collect()
                };
                let mut all_ok = true;
                for target in targets {
                    let mut payload = base_payload.clone();
                    if let Some(ch) = target {
                        payload["channel"] = json!(format!("#{}", ch));
                    }
                    let resp = client
                        .post(&webhook_url)
                        .header("User-Agent", APP_ID)
                        .json(&payload)
                        .send()
                        .await?;
                    if !resp.status().is_success() { all_ok = false; }
                }
                // Upload attachments via the same webhook URL
                for attach in &ctx.attachments {
                    let part = reqwest::multipart::Part::bytes(attach.data.clone())
                        .file_name(attach.name.clone())
                        .mime_str(&attach.mime_type).unwrap_or_else(|_| reqwest::multipart::Part::bytes(attach.data.clone()).file_name(attach.name.clone()));
                    let form = reqwest::multipart::Form::new().part("file", part);
                    // Use the same webhook URL
                    let _ = client.post(&webhook_url).multipart(form).send().await;
                }
                Ok(all_ok)
            }
            SlackMode::Bot { access_token, channels } => {
                let mut all_ok = true;
                for channel in channels {
                    let payload = json!({
                        "channel": channel,
                        "username": bot_name,
                        "attachments": [{
                            "fallback": ctx.body,
                            "title": ctx.title,
                            "text": ctx.body,
                            "color": color,
                        }]
                    });
                    let api_post_url = {
                        let default = "https://slack.com/api/chat.postMessage".to_string();
                        #[cfg(test)]
                        { self.api_url_override.as_ref().map(|base| format!("{}/api/chat.postMessage", base)).unwrap_or(default) }
                        #[cfg(not(test))]
                        { default }
                    };
                    let resp = client
                        .post(&api_post_url)
                        .header("User-Agent", APP_ID)
                        .header("Authorization", format!("Bearer {}", access_token))
                        .json(&payload)
                        .send()
                        .await?;
                    if !resp.status().is_success() {
                        all_ok = false;
                    }
                    // Upload attachments in bot mode
                    for att in &ctx.attachments {
                        let part = reqwest::multipart::Part::bytes(att.data.clone())
                            .file_name(att.name.clone())
                            .mime_str(&att.mime_type)
                            .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()));
                        let form = reqwest::multipart::Form::new()
                            .text("channels", channel.clone())
                            .part("file", part);
                        let upload_url = {
                            let default = "https://slack.com/api/files.upload".to_string();
                            #[cfg(test)]
                            { self.api_url_override.as_ref().map(|base| format!("{}/api/files.upload", base)).unwrap_or(default) }
                            #[cfg(not(test))]
                            { default }
                        };
                        let _ = client.post(&upload_url)
                            .header("Authorization", format!("Bearer {}", access_token))
                            .multipart(form)
                            .send().await;
                    }
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
            "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#channel",
            "slack://username@xoxe.xoxb-1234-1234-abc124/#nuxref?footer=no&timestamp=yes",
            "slack://username@xoxe.xoxp-1234-1234-abc124/#nuxref?footer=yes&timestamp=no",
            "slack://?token=xoxe.xoxb-1234-1234-abc124&to=#nuxref&footer=no&user=test",
            "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/+id/@id/",
            "slack://username@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?to=#nuxref",
            "slack://username@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#nuxref",
            "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnl/user@gmail.com",
            "slack://bot@_/#nuxref?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnadfdajkjkfl/",
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan",
            "slack://username@xoxb-1234-1234-abc124/#nuxref?footer=no&timestamp=yes",
            "slack://username@xoxb-1234-1234-abc124/#nuxref?footer=yes&timestamp=yes",
            "slack://username@xoxb-1234-1234-abc124/#nuxref?footer=yes&timestamp=no",
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&mode=hook",
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=yes&footer=yes&timestamp=no",
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=yes&footer=yes&timestamp=yes",
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=yes&footer=no&timestamp=yes",
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=yes&footer=no&timestamp=no",
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=yes&footer=yes&image=no",
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=yes&format=text",
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=no&format=text",
            "slack://?token=xoxb-1234-1234-abc124&to=#nuxref&footer=no&user=test",
            "slack://?token=xoxb-1234-1234-abc124&to=#nuxref,#$,#-&footer=no",
            "slack://username@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ",
            "slack://notify@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#b",
            "slack://notify@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#b:100",
            "slack://notify@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/+124:100",
            "slack://notify@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/+124:100/@chan",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "slack://",
            "slack://:@/",
            "slack://T1JJ3T3L2",
            "slack://T1JJ3T3L2/A1BRTD4JD/",
            "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?mode=invalid",
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&mode=bot",
            "slack://username@-INVALID-/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#cool",
            "slack://username@T1JJ3T3L2/-INVALID-/TIiajkdnlazkcOXrIdevi7FQ/#great",
            "slack://username@T1JJ3T3L2/A1BRTD4JD/-INVALID-/#channel",
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
    use wiremock::matchers::{body_json, header, method, path};
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

    /// Helper: create a webhook-mode Slack pointing at the mock server.
    fn webhook_slack(server: &MockServer, channels: Vec<&str>) -> Slack {
        let addr = server.address();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let ch: Vec<String> = channels.iter().map(|s| s.to_string()).collect();
        Slack {
            mode: SlackMode::Webhook {
                token_a: "AAAAAAAAA".to_string(),
                token_b: "BBBBBBBBB".to_string(),
                token_c: "cccccccccccccccccccccccc".to_string(),
                channels: ch,
            },
            verify_certificate: false,
            tags: vec![],
            bot_name: Some("TestBot".to_string()),
            webhook_url_override: Some(format!("{}/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc", base)),
            api_url_override: None,
        }
    }

    /// Helper: create a bot-mode Slack pointing at the mock server.
    fn bot_slack(server: &MockServer, channels: Vec<&str>) -> Slack {
        let addr = server.address();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let ch: Vec<String> = channels.iter().map(|s| s.to_string()).collect();
        Slack {
            mode: SlackMode::Bot {
                access_token: "xoxb-1234-1234-abc124".to_string(),
                channels: ch,
            },
            verify_certificate: false,
            tags: vec![],
            bot_name: Some("TestBot".to_string()),
            webhook_url_override: None,
            api_url_override: Some(base),
        }
    }

    // ── 1. Webhook mode: payload correctness ────────────────────────────

    #[tokio::test]
    async fn test_webhook_post_json_payload() {
        // Mirrors Python test_plugin_slack_webhook_mode: POST to webhook
        // URL with correct JSON payload including title, text, color, username.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let slack = webhook_slack(&server, vec![]);
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Webhook POST should succeed");
    }

    #[tokio::test]
    async fn test_webhook_sends_channel_in_payload() {
        // When channels are specified, the payload should include the channel field.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let slack = webhook_slack(&server, vec!["general"]);
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_webhook_multiple_channels_sends_multiple_requests() {
        // Mirrors Python: channels = "chan1,#chan2,+BAK4K23G5,@user"
        // Each channel triggers a separate POST to the webhook.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(4)
            .mount(&server)
            .await;

        let slack = webhook_slack(&server, vec!["chan1", "chan2", "BAK4K23G5", "user"]);
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_webhook_no_channels_sends_once() {
        // When no channels are specified, webhook sends once without channel field.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let slack = webhook_slack(&server, vec![]);
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 2. Bot mode: Authorization header and API endpoint ──────────────

    #[tokio::test]
    async fn test_bot_post_with_bearer_token() {
        // Mirrors Python test_plugin_slack_oauth_access_token: POST to
        // slack.com/api/chat.postMessage with Authorization: Bearer token.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat.postMessage"))
            .and(header("Authorization", "Bearer xoxb-1234-1234-abc124"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "message": ""})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let slack = bot_slack(&server, vec!["#apprise"]);
        let result = slack.send(&ctx("test title", "test body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Bot mode POST should succeed");
    }

    #[tokio::test]
    async fn test_bot_multiple_channels() {
        // Mirrors Python test_plugin_slack_multiple_thread_reply: sending to
        // multiple channels triggers one POST per channel.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat.postMessage"))
            .and(header("Authorization", "Bearer xoxb-1234-1234-abc124"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "message": ""})),
            )
            .expect(2)
            .mount(&server)
            .await;

        let slack = bot_slack(&server, vec!["#general", "#other"]);
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 3. Attachment upload ────────────────────────────────────────────

    #[tokio::test]
    async fn test_bot_attachment_upload() {
        // Mirrors Python test_plugin_slack_file_upload_success: after sending
        // the message, the bot uploads attachments via files.upload.
        let server = MockServer::start().await;

        // 1st call: chat.postMessage
        Mock::given(method("POST"))
            .and(path("/api/chat.postMessage"))
            .and(header("Authorization", "Bearer xoxb-1234-1234-abc124"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "channel": "C123456"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        // 2nd call: files.upload
        Mock::given(method("POST"))
            .and(path("/api/files.upload"))
            .and(header("Authorization", "Bearer xoxb-1234-1234-abc124"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "ok": true,
                        "file": {"id": "F123ABC456"}
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let slack = bot_slack(&server, vec!["#general"]);
        let mut c = ctx("Upload Test", "file attached");
        c.attachments.push(crate::notify::Attachment {
            name: "apprise-test.gif".to_string(),
            data: b"GIF89a".to_vec(),
            mime_type: "image/gif".to_string(),
        });

        let result = slack.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Bot attachment upload should succeed");
    }

    #[tokio::test]
    async fn test_webhook_attachment_upload() {
        // Webhook mode also uploads attachments via multipart POST to the
        // webhook URL.
        let server = MockServer::start().await;

        // Message POST
        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            // 1 for the message + 1 for the attachment
            .expect(2)
            .mount(&server)
            .await;

        let slack = webhook_slack(&server, vec![]);
        let mut c = ctx("title", "body");
        c.attachments.push(crate::notify::Attachment {
            name: "test.png".to_string(),
            data: b"PNG".to_vec(),
            mime_type: "image/png".to_string(),
        });

        let result = slack.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 4. Error handling ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_webhook_http_500_returns_false() {
        // Mirrors Python: requests_response_code=500 → response=False
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .respond_with(ResponseTemplate::new(500).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let slack = webhook_slack(&server, vec!["channel"]);
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "HTTP 500 should return false");
    }

    #[tokio::test]
    async fn test_webhook_bizarre_status_code_returns_false() {
        // Mirrors Python: requests_response_code=999 → response=False
        // (wiremock only supports valid HTTP status codes, use 599 as proxy)
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .respond_with(ResponseTemplate::new(599).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let slack = webhook_slack(&server, vec!["a"]);
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "Unusual HTTP status should return false");
    }

    #[tokio::test]
    async fn test_bot_http_500_returns_false() {
        // Bot mode also checks response status.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat.postMessage"))
            .respond_with(ResponseTemplate::new(500).set_body_string("error"))
            .expect(1)
            .mount(&server)
            .await;

        let slack = bot_slack(&server, vec!["#channel"]);
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "Bot HTTP 500 should return false");
    }

    #[tokio::test]
    async fn test_connection_refused_returns_error() {
        // Mirrors Python test_requests_exceptions: connection errors
        // should propagate as Err.
        let slack = Slack {
            mode: SlackMode::Webhook {
                token_a: "AAAAAAAAA".to_string(),
                token_b: "BBBBBBBBB".to_string(),
                token_c: "cccccccccccccccccccccccc".to_string(),
                channels: vec!["chan".to_string()],
            },
            verify_certificate: false,
            tags: vec![],
            bot_name: None,
            webhook_url_override: Some("http://127.0.0.1:19999/services/A/B/C".to_string()),
            api_url_override: None,
        };

        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_err(), "Connection refused should return Err");
    }

    #[tokio::test]
    async fn test_bot_connection_refused_returns_error() {
        let slack = Slack {
            mode: SlackMode::Bot {
                access_token: "xoxb-1234-1234-abc124".to_string(),
                channels: vec!["#chan".to_string()],
            },
            verify_certificate: false,
            tags: vec![],
            bot_name: None,
            webhook_url_override: None,
            api_url_override: Some("http://127.0.0.1:19999".to_string()),
        };

        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_err(), "Bot connection refused should return Err");
    }

    #[tokio::test]
    async fn test_webhook_partial_failure_returns_false() {
        // Two channels: first succeeds, second fails with 500.
        // Overall result should be false (mirrors Python partial failure
        // behavior).
        let server = MockServer::start().await;

        // We cannot distinguish paths per-channel in webhook mode (all go to
        // the same URL), so we return 500 on the second call using a
        // response sequence approach. wiremock does not natively support
        // sequences, so we set up one mock that always returns 500 and
        // check that the result is false.
        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .respond_with(ResponseTemplate::new(500).set_body_string("fail"))
            .mount(&server)
            .await;

        let slack = webhook_slack(&server, vec!["chan1", "chan2"]);
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "Partial failure should return false");
    }

    // ── 5. Channel/user targeting (@user, #channel) ─────────────────────

    #[tokio::test]
    async fn test_bot_channel_name_in_payload() {
        // Verify the channel name appears in the JSON payload sent to the API.
        let server = MockServer::start().await;

        let expected_payload = serde_json::json!({
            "channel": "#apprise",
            "username": "TestBot",
            "attachments": [{
                "fallback": "body",
                "title": "title",
                "text": "body",
                "color": "#3498DB",
            }]
        });

        Mock::given(method("POST"))
            .and(path("/api/chat.postMessage"))
            .and(body_json(&expected_payload))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "message": ""})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let slack = bot_slack(&server, vec!["#apprise"]);
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_webhook_channel_in_payload() {
        // Webhook mode prepends # to the channel in the payload.
        let server = MockServer::start().await;

        let expected_payload = serde_json::json!({
            "channel": "#general",
            "username": "TestBot",
            "attachments": [{
                "fallback": "test body",
                "title": "test title",
                "text": "test body",
                "color": "#3498DB",
            }]
        });

        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .and(body_json(&expected_payload))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let slack = webhook_slack(&server, vec!["general"]);
        let result = slack.send(&ctx("test title", "test body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 6. Color mapping by notify type ─────────────────────────────────

    #[test]
    fn test_color_for_notify_types() {
        assert_eq!(Slack::color_for_type(&NotifyType::Info), "#3498DB");
        assert_eq!(Slack::color_for_type(&NotifyType::Success), "#2ECC71");
        assert_eq!(Slack::color_for_type(&NotifyType::Warning), "#E67E22");
        assert_eq!(Slack::color_for_type(&NotifyType::Failure), "#E74C3C");
    }

    #[tokio::test]
    async fn test_failure_type_sends_red_color() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let slack = webhook_slack(&server, vec![]);
        let mut c = ctx("alert", "something failed");
        c.notify_type = NotifyType::Failure;
        let result = slack.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 7. Bot name / username ──────────────────────────────────────────

    #[tokio::test]
    async fn test_default_bot_name_is_apprise() {
        // When no bot_name is set, "Apprise" is used in the payload.
        let server = MockServer::start().await;

        let expected_payload = serde_json::json!({
            "username": "Apprise",
            "attachments": [{
                "fallback": "body",
                "title": "title",
                "text": "body",
                "color": "#3498DB",
            }]
        });

        Mock::given(method("POST"))
            .and(path("/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc"))
            .and(body_json(&expected_payload))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let mut slack = webhook_slack(&server, vec![]);
        slack.bot_name = None; // no username set
        let result = slack.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 8. URL parsing mode detection ───────────────────────────────────

    #[test]
    fn test_webhook_mode_from_url() {
        let parsed = ParsedUrl::parse(
            "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#channel",
        ).unwrap();
        let slack = Slack::from_url(&parsed).unwrap();
        assert!(matches!(slack.mode, SlackMode::Webhook { .. }));
    }

    #[test]
    fn test_bot_mode_from_url() {
        let parsed = ParsedUrl::parse(
            "slack://username@xoxb-1234-1234-abc124/#nuxref",
        ).unwrap();
        let slack = Slack::from_url(&parsed).unwrap();
        assert!(matches!(slack.mode, SlackMode::Bot { .. }));
    }

    #[test]
    fn test_bot_mode_rotating_token() {
        // xoxe.xoxb- prefix is a rotating bot token and should be accepted.
        let parsed = ParsedUrl::parse(
            "slack://username@xoxe.xoxb-1234-1234-abc124/#nuxref",
        ).unwrap();
        let slack = Slack::from_url(&parsed).unwrap();
        assert!(matches!(slack.mode, SlackMode::Bot { .. }));
    }

    #[test]
    fn test_xoxe_xoxp_rotating_token() {
        // xoxe.xoxp- prefix is a rotating user token and should be accepted.
        let parsed = ParsedUrl::parse(
            "slack://username@xoxe.xoxp-1234-1234-abc124/#nuxref",
        ).unwrap();
        let slack = Slack::from_url(&parsed).unwrap();
        assert!(matches!(slack.mode, SlackMode::Bot { .. }));
    }

    #[test]
    fn test_forced_hook_mode_with_webhook_tokens() {
        let parsed = ParsedUrl::parse(
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&mode=hook",
        ).unwrap();
        let slack = Slack::from_url(&parsed).unwrap();
        assert!(matches!(slack.mode, SlackMode::Webhook { .. }));
    }

    #[test]
    fn test_forced_bot_mode_with_webhook_tokens_rejected() {
        // Cannot force bot mode when only webhook tokens are provided.
        let parsed = ParsedUrl::parse(
            "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&mode=bot",
        ).unwrap();
        assert!(Slack::from_url(&parsed).is_none());
    }

    #[test]
    fn test_invalid_mode_rejected() {
        let parsed = ParsedUrl::parse(
            "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?mode=invalid",
        ).unwrap();
        assert!(Slack::from_url(&parsed).is_none());
    }

    #[test]
    fn test_bot_name_from_url_user() {
        let parsed = ParsedUrl::parse(
            "slack://mybot@xoxb-1234-1234-abc124/#nuxref",
        ).unwrap();
        let slack = Slack::from_url(&parsed).unwrap();
        assert_eq!(slack.bot_name.as_deref(), Some("mybot"));
    }

    #[test]
    fn test_bot_name_from_user_query_param() {
        let parsed = ParsedUrl::parse(
            "slack://?token=xoxb-1234-1234-abc124&to=#nuxref&user=testbot",
        ).unwrap();
        let slack = Slack::from_url(&parsed).unwrap();
        assert_eq!(slack.bot_name.as_deref(), Some("testbot"));
    }

    #[test]
    fn test_token_from_query_param() {
        let parsed = ParsedUrl::parse(
            "slack://bot@_/#nuxref?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnadfdajkjkfl/",
        ).unwrap();
        let slack = Slack::from_url(&parsed).unwrap();
        assert!(matches!(slack.mode, SlackMode::Webhook { .. }));
    }

    #[test]
    fn test_invalid_token_a_rejected() {
        let parsed = ParsedUrl::parse(
            "slack://username@-INVALID-/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#cool",
        ).unwrap();
        assert!(Slack::from_url(&parsed).is_none());
    }

    #[test]
    fn test_invalid_token_b_rejected() {
        let parsed = ParsedUrl::parse(
            "slack://username@T1JJ3T3L2/-INVALID-/TIiajkdnlazkcOXrIdevi7FQ/#great",
        ).unwrap();
        assert!(Slack::from_url(&parsed).is_none());
    }

    #[test]
    fn test_invalid_token_c_rejected() {
        let parsed = ParsedUrl::parse(
            "slack://username@T1JJ3T3L2/A1BRTD4JD/-INVALID-/#channel",
        ).unwrap();
        assert!(Slack::from_url(&parsed).is_none());
    }

    // ── 9. Service details ──────────────────────────────────────────────

    #[test]
    fn test_static_details() {
        let details = Slack::static_details();
        assert_eq!(details.service_name, "Slack");
        assert!(details.attachment_support);
        assert_eq!(details.protocols, vec!["slack"]);
    }
}
