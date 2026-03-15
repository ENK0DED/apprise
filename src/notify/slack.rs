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
                let webhook_url = format!(
                    "https://hooks.slack.com/services/{}/{}/{}",
                    token_a, token_b, token_c
                );
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
                    // Upload attachments in bot mode
                    for att in &ctx.attachments {
                        let part = reqwest::multipart::Part::bytes(att.data.clone())
                            .file_name(att.name.clone())
                            .mime_str(&att.mime_type)
                            .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()));
                        let form = reqwest::multipart::Form::new()
                            .text("channels", channel.clone())
                            .part("file", part);
                        let _ = client.post("https://slack.com/api/files.upload")
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
            "slack://username@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#nuxref",
            "slack://username@xoxb-1234-1234-abc124/#nuxref?footer=no&timestamp=yes",
            "slack://username@xoxb-1234-1234-abc124/#nuxref?footer=yes&timestamp=yes",
            "slack://username@xoxb-1234-1234-abc124/#nuxref?footer=yes&timestamp=no",
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
            "slack://username@xoxb-1234-1234-abc124/#nuxref",
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
}
