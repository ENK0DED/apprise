use async_trait::async_trait;
use serde_json::{json, Value};

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
    include_image: bool,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Discord {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // discord://webhook_id/webhook_token
        // discord://botname@webhook_id/webhook_token
        let webhook_id = url.host.clone()?;
        let webhook_token = url.path_parts.first()?.clone();
        if webhook_token.is_empty() {
            return None;
        }

        let username = url.user.clone();
        let tts = url.get("tts").map(crate::utils::parse::parse_bool).unwrap_or(false);
        let avatar_url = url.get("avatar_url").or_else(|| url.get("avatar")).map(|s| s.to_string());
        let footer = url.get("footer").map(crate::utils::parse::parse_bool).unwrap_or(false);
        let include_image = url.get("image").map(crate::utils::parse::parse_bool).unwrap_or(false);

        Some(Self {
            webhook_id,
            webhook_token,
            tts,
            avatar_url,
            username,
            footer,
            include_image,
            verify_certificate: url.verify_certificate(),
            tags: url.tags(),
        })
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
    fn schemas(&self) -> &[&str] {
        &["discord"]
    }

    fn service_name(&self) -> &str {
        "Discord"
    }

    fn details(&self) -> ServiceDetails {
        Self::static_details()
    }

    fn tags(&self) -> Vec<String> {
        self.tags.clone()
    }

    fn attachment_support(&self) -> bool {
        true
    }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!(
            "https://discord.com/api/webhooks/{}/{}",
            self.webhook_id, self.webhook_token
        );

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
        if self.footer {
            embed["footer"] = json!({ "text": APP_ID });
        }

        payload["embeds"] = json!([embed]);

        let client = build_client(self.verify_certificate)?;
        let resp = client
            .post(&url)
            .header("User-Agent", APP_ID)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() || status.as_u16() == 204 {
            tracing::info!("Discord notification sent successfully");
            Ok(true)
        } else {
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!("Discord notification failed: {} - {}", status, body);
            Err(NotifyError::ServiceError {
                status: status.as_u16(),
                body,
            })
        }
    }
}
