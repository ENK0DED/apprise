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

        Some(Self { host, port: url.port, mode, channels, secure, verify_certificate: url.verify_certificate(), tags: url.tags() })
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
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let client = build_client(self.verify_certificate)?;

        match &self.mode {
            MattermostMode::Webhook { webhook_path } => {
                let url = format!("{}://{}{}/hooks/{}", schema, self.host, port_str, webhook_path);
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
                let url = format!("{}://{}{}/api/v4/posts", schema, self.host, port_str);
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
            "mmost://localhost/3ccdd113474722377935511fc85d3dd4?to=test&image=True",
            "mmost://localhost:8080/3ccdd113474722377935511fc85d3dd4",
            "mmost://localhost:8080/3ccdd113474722377935511fc85d3dd4",
            "mmosts://localhost/3ccdd113474722377935511fc85d3dd4",
            "https://mattermost.example.com/hooks/3ccdd113474722377935511fc85d3dd4",
            "mmosts://localhost/a/path/3ccdd113474722377935511fc85d3dd4",
            "mmosts://localhost/////3ccdd113474722377935511fc85d3dd4///",
            "mmost://localhost/token?mode=w",
            "mmost://localhost/token?mode=b&to=channel-id-1",
            "mmosts://localhost/a/path/3ccdd113474722377935511fc85d3dd4",
            "mmosts://localhost/////3ccdd113474722377935511fc85d3dd4///",
            "mmost://localhost/3ccdd113474722377935511fc85d3dd4",
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
}
