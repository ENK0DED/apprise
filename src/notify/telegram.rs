use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;

pub struct Telegram {
    bot_token: String,
    targets: Vec<String>,
    verify_certificate: bool,
    tags: Vec<String>,
    parse_mode: String,
    silent: bool,
}

impl Telegram {
    const API_BASE: &'static str = "https://api.telegram.org/bot";

    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Bot token can be in the host (123456789:abcdefg_hijklmnop) when
        // the fallback parser handles the colon, OR split as user:password
        // by the url crate when there's an @ sign (e.g., botname@token).
        let bot_token = if let Some(ref h) = url.host {
            if h.contains(':') {
                // Fallback parser kept the full "id:token" as host
                h.clone()
            } else if let Some(ref user) = url.user {
                if user.chars().all(|c| c.is_ascii_digit()) {
                    // user = bot_id (numeric), password = bot_token part
                    // This shouldn't normally happen, but handle it
                    if let Some(ref pass) = url.password {
                        format!("{}:{}", user, pass)
                    } else {
                        h.clone()
                    }
                } else {
                    // user is a bot name (like "bottest"), host is the token
                    h.clone()
                }
            } else {
                h.clone()
            }
        } else {
            return None;
        };

        if bot_token.is_empty() { return None; }

        // Validate bot token format: should be digits:alphanumeric
        if bot_token.contains(':') {
            let parts: Vec<&str> = bot_token.splitn(2, ':').collect();
            if parts.len() != 2 { return None; }
            // First part should be numeric (bot ID)
            if !parts[0].chars().all(|c| c.is_ascii_digit()) { return None; }
            // Second part should be non-empty
            if parts[1].is_empty() { return None; }
        }

        let mut targets: Vec<String> = url.path_parts.clone();
        // Support ?to= query param
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }

        // Validate topic/thread if provided
        if let Some(topic) = url.get("topic").or_else(|| url.get("thread")) {
            if topic.parse::<i64>().is_err() { return None; }
        }

        // Validate content param if provided
        if let Some(content) = url.get("content") {
            match content.to_lowercase().as_str() {
                "before" | "after" | "" => {}
                _ => return None,
            }
        }

        let parse_mode = url.get("format").unwrap_or("html").to_string();
        let silent = url.get("silent").map(crate::utils::parse::parse_bool).unwrap_or(false);
        Some(Self { bot_token, targets, verify_certificate: url.verify_certificate(), tags: url.tags(), parse_mode, silent })
    }

    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "Telegram",
            service_url: Some("https://telegram.org"),
            setup_url: Some("https://core.telegram.org/bots"),
            protocols: vec!["tgram"],
            description: "Send Telegram messages via bot API.",
            attachment_support: true,
        }
    }

    /// Determine the Telegram API method and form field name for a given MIME type
    fn endpoint_for_mime(mime: &str) -> (&'static str, &'static str) {
        let m = mime.to_lowercase();
        if m == "image/gif" || m.starts_with("video/h264") {
            ("sendAnimation", "animation")
        } else if m.starts_with("image/") {
            ("sendPhoto", "photo")
        } else if m == "video/mp4" {
            ("sendVideo", "video")
        } else if m == "audio/ogg" || m == "application/ogg" {
            ("sendVoice", "voice")
        } else if m.starts_with("audio/") {
            ("sendAudio", "audio")
        } else {
            ("sendDocument", "document")
        }
    }
}

#[async_trait]
impl Notify for Telegram {
    fn schemas(&self) -> &[&str] { &["tgram"] }
    fn service_name(&self) -> &str { "Telegram" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    fn attachment_support(&self) -> bool { true }
    fn notify_format(&self) -> crate::types::NotifyFormat { crate::types::NotifyFormat::Html }
    fn body_maxlen(&self) -> usize { 4096 }
    fn title_maxlen(&self) -> usize { 0 }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        if self.targets.is_empty() {
            return Err(NotifyError::MissingParam("chat_id".into()));
        }
        let client = build_client(self.verify_certificate)?;
        let text = if ctx.title.is_empty() {
            ctx.body.clone()
        } else {
            format!("<b>{}</b>\n{}", ctx.title, ctx.body)
        };

        let mut all_ok = true;
        for target in &self.targets {
            // Always send text message first
            let url = format!("{}{}/sendMessage", Self::API_BASE, self.bot_token);
            let payload = json!({
                "chat_id": target,
                "text": text,
                "parse_mode": "HTML",
                "disable_notification": self.silent,
            });
            let resp = client
                .post(&url)
                .header("User-Agent", APP_ID)
                .json(&payload)
                .send()
                .await?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!("Telegram send to {} failed: {}", target, body);
                all_ok = false;
            }

            // Upload attachments via sendDocument
            for attach in &ctx.attachments {
                let part = reqwest::multipart::Part::bytes(attach.data.clone())
                    .file_name(attach.name.clone())
                    .mime_str(&attach.mime_type).unwrap_or_else(|_| reqwest::multipart::Part::bytes(attach.data.clone()).file_name(attach.name.clone()));
                let form = reqwest::multipart::Form::new()
                    .text("chat_id", target.clone())
                    .part("document", part);
                let _ = client.post(format!("https://api.telegram.org/bot{}/sendDocument", self.bot_token))
                    .multipart(form)
                    .send().await;
            }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/",
            "tgram://123456789:abcdefg_hijklmnop/id1/id2/",
            "tgram://123456789:abcdefg_hijklmnop/?to=id1,id2",
            "tgram://123456789:abcdefg_hijklmnop/id1/id2/23423/-30/",
            "tgram://bottest@123456789:abcdefg_hijklmnop/lead2gold/",
            "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?topic=12345",
            "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?thread=12345",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?image=Yes",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=invalid",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=markdown",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=markdown&mdv=v1",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=markdown&mdv=v2",
            "tgram://123456789:abcdefg_hijklmnop/l2g/?format=markdown&mdv=bad",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=html",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=text",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?silent=yes",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?silent=no",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?preview=yes",
            "tgram://123456789:abcdefg_hijklmnop/lead2gold/?preview=no",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "tgram://",
            "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?topic=invalid",
            "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?content=invalid",
            "tgram://alpha:abcdefg_hijklmnop/lead2gold/",
            "tgram://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
