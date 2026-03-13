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
        // Webhook: slack://webhook_id/token_a/token_b/token_c[/channel...]
        // Bot:     slack://access_token/channel...
        let bot_name = url.user.clone();

        // The host is either a webhook_id (for webhook) or token (for bot)
        let first = url.host.clone()?;
        let parts = &url.path_parts;

        // Heuristic: if we have at least 3 path parts, it's webhook mode
        let mode = if parts.len() >= 3 {
            let token_a = parts.get(0)?.clone();
            let token_b = parts.get(1)?.clone();
            let token_c = parts.get(2)?.clone();
            let channels = parts.get(3..).unwrap_or(&[]).to_vec();
            SlackMode::Webhook { token_a, token_b, token_c, channels }
        } else {
            // Bot mode — host is the token
            let channels = parts.to_vec();
            SlackMode::Bot { access_token: first, channels }
        };

        Some(Self {
            mode,
            verify_certificate: url.verify_certificate(),
            tags: url.tags(),
            bot_name,
        })
    }

    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "Slack",
            service_url: Some("https://slack.com"),
            setup_url: Some("https://api.slack.com/incoming-webhooks"),
            protocols: vec!["slack"],
            description: "Send Slack messages via webhooks or bot tokens.",
            attachment_support: false,
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
                let webhook_url = format!(
                    "https://hooks.slack.com/services/{}/{}/{}",
                    token_a, token_b, token_c
                );
                let mut payload = json!({
                    "username": bot_name,
                    "attachments": [{
                        "fallback": ctx.body,
                        "title": ctx.title,
                        "text": ctx.body,
                        "color": color,
                    }]
                });
                if !channels.is_empty() {
                    payload["channel"] = json!(format!("#{}", channels[0]));
                }
                let resp = client
                    .post(&webhook_url)
                    .header("User-Agent", APP_ID)
                    .json(&payload)
                    .send()
                    .await?;
                if resp.status().is_success() {
                    Ok(true)
                } else {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    Err(NotifyError::ServiceError { status, body })
                }
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
                    let resp = client
                        .post("https://slack.com/api/chat.postMessage")
                        .header("User-Agent", APP_ID)
                        .header("Authorization", format!("Bearer {}", access_token))
                        .json(&payload)
                        .send()
                        .await?;
                    if !resp.status().is_success() {
                        all_ok = false;
                    }
                }
                Ok(all_ok)
            }
        }
    }
}
