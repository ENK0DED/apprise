use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Revolt { bot_token: String, channel_id: String, verify_certificate: bool, tags: Vec<String> }
impl Revolt {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let bot_token = url.get("bot_token").map(|s| s.to_string())
            .or_else(|| url.host.clone().filter(|h| !h.is_empty() && h != "_"))?;
        if bot_token.is_empty() { return None; }
        // Channel from path, ?channel=, or ?to=
        let channel_id = url.path_parts.first().cloned()
            .or_else(|| url.get("channel").map(|s| s.split(',').next().unwrap_or("").trim().to_string()))
            .or_else(|| url.get("to").map(|s| s.split(',').next().unwrap_or("").trim().to_string()))?;
        if channel_id.is_empty() { return None; }
        Some(Self { bot_token, channel_id, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Revolt", service_url: Some("https://revolt.chat"), setup_url: None, protocols: vec!["revolt"], description: "Send messages via Revolt.", attachment_support: false } }
}
#[async_trait]
impl Notify for Revolt {
    fn schemas(&self) -> &[&str] { &["revolt"] }
    fn service_name(&self) -> &str { "Revolt" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://api.revolt.chat/channels/{}/messages", self.channel_id);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let payload = json!({ "content": text });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).header("X-Bot-Token", self.bot_token.as_str()).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;
    use crate::utils::parse::ParsedUrl;

    #[test]
    fn test_invalid_urls() {
        let no_channel = format!("revolt://{}", "i".repeat(24));
        let no_token = format!("revolt://?channel={}", "i".repeat(24));
        let urls: Vec<&str> = vec![
            "revolt://",
            "revolt://:@/",
            &no_channel,
            &no_token,
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_valid_urls() {
        let bot = "i".repeat(24);
        let chan = "t".repeat(64);
        let urls = vec![
            // channel via path
            format!("revolt://{}/{}", bot, chan),
            // channel via ?channel=
            format!("revolt://{}/?channel={}", bot, chan),
            // channel via ?to=
            format!("revolt://{}/?to={}", bot, chan),
            // bot_token via ?bot_token=
            format!("revolt://_?bot_token={}&channel={}", bot, chan),
            // format=markdown
            format!("revolt://{}/{}?format=markdown", bot, chan),
            // format=text
            format!("revolt://{}/{}?format=text", bot, chan),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_fields_path() {
        let bot = "A".repeat(24);
        let chan = "B".repeat(64);
        let url_str = format!("revolt://{}/{}", bot, chan);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let r = Revolt::from_url(&parsed).unwrap();
        assert_eq!(r.bot_token, bot);
        assert_eq!(r.channel_id, chan);
    }

    #[test]
    fn test_from_url_fields_channel_param() {
        let bot = "i".repeat(24);
        let chan = "i".repeat(24);
        let url_str = format!("revolt://{}/?channel={}", bot, chan);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let r = Revolt::from_url(&parsed).unwrap();
        assert_eq!(r.bot_token, bot);
        assert_eq!(r.channel_id, chan);
    }

    #[test]
    fn test_from_url_fields_to_param() {
        let bot = "i".repeat(24);
        let chan = "i".repeat(24);
        let url_str = format!("revolt://{}/?to={}", bot, chan);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let r = Revolt::from_url(&parsed).unwrap();
        assert_eq!(r.bot_token, bot);
        assert_eq!(r.channel_id, chan);
    }

    #[test]
    fn test_from_url_bot_token_param() {
        let bot = "i".repeat(24);
        let chan = "t".repeat(64);
        let url_str = format!("revolt://_?bot_token={}&channel={}", bot, chan);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let r = Revolt::from_url(&parsed).unwrap();
        assert_eq!(r.bot_token, bot);
        assert_eq!(r.channel_id, chan);
    }

    #[test]
    fn test_no_channel_returns_none() {
        let bot = "i".repeat(24);
        // Bot token only, no channel
        let parsed = ParsedUrl::parse(&format!("revolt://{}", bot)).unwrap();
        assert!(Revolt::from_url(&parsed).is_none());
    }

    #[test]
    fn test_static_details() {
        let details = Revolt::static_details();
        assert_eq!(details.service_name, "Revolt");
        assert_eq!(details.service_url, Some("https://revolt.chat"));
        assert!(details.protocols.contains(&"revolt"));
        assert!(!details.attachment_support);
    }

    #[test]
    fn test_channel_comma_separated_takes_first() {
        let bot = "i".repeat(24);
        let chan1 = "a".repeat(24);
        let chan2 = "b".repeat(24);
        let url_str = format!("revolt://{}/?channel={},{}", bot, chan1, chan2);
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let r = Revolt::from_url(&parsed).unwrap();
        // Takes the first channel from comma-separated list
        assert_eq!(r.channel_id, chan1);
    }
}
