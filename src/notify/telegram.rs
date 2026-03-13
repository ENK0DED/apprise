use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
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
        // tgram://bottoken/chat_id1/chat_id2
        let bot_token = url.host.clone()?;
        if bot_token.is_empty() {
            return None;
        }
        let targets: Vec<String> = url.path_parts.clone();

        let parse_mode = url
            .get("format")
            .unwrap_or("html")
            .to_string();
        let silent = url.get("silent").map(crate::utils::parse::parse_bool).unwrap_or(false);

        Some(Self {
            bot_token,
            targets,
            verify_certificate: url.verify_certificate(),
            tags: url.tags(),
            parse_mode,
            silent,
        })
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
}

#[async_trait]
impl Notify for Telegram {
    fn schemas(&self) -> &[&str] { &["tgram"] }
    fn service_name(&self) -> &str { "Telegram" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    fn attachment_support(&self) -> bool { true }

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
        }
        Ok(all_ok)
    }
}
