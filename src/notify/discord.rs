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
    footer_logo: Option<String>,
    include_image: bool,
    thread_id: Option<String>,
    href: Option<String>,
    verify_certificate: bool,
    tags: Vec<String>,
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
    fn schemas(&self) -> &[&str] { &["discord"] }
    fn service_name(&self) -> &str { "Discord" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    fn attachment_support(&self) -> bool { true }
    fn body_maxlen(&self) -> usize { 2000 }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let mut url = format!(
            "https://discord.com/api/webhooks/{}/{}",
            self.webhook_id, self.webhook_token
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
                        .post(format!("https://discord.com/api/webhooks/{}/{}", self.webhook_id, self.webhook_token))
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
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "discord://",
            "discord://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
