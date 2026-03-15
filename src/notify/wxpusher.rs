use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct WxPusher { token: String, uids: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl WxPusher {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Token can be in host (if starts with AT_) or ?token= param
        let token = if let Some(ref h) = url.host {
            if h.starts_with("AT_") {
                h.clone()
            } else {
                // Host is not a token; try ?token= param
                url.get("token").map(|s| s.to_string())?
            }
        } else {
            url.get("token").map(|s| s.to_string())?
        };
        if !token.starts_with("AT_") { return None; }

        // UIDs from host (if not token), path_parts, and ?to= param
        let mut uids: Vec<String> = Vec::new();
        if let Some(ref h) = url.host {
            if !h.starts_with("AT_") && !h.is_empty() {
                uids.push(h.clone());
            }
        }
        uids.extend(url.path_parts.clone());
        if let Some(to) = url.get("to") {
            uids.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if uids.is_empty() { return None; }
        Some(Self { token, uids, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "WxPusher", service_url: Some("https://wxpusher.zjiecode.com"), setup_url: None, protocols: vec!["wxpusher"], description: "Send messages via WxPusher WeChat service.", attachment_support: false } }
}
#[async_trait]
impl Notify for WxPusher {
    fn schemas(&self) -> &[&str] { &["wxpusher"] }
    fn service_name(&self) -> &str { "WxPusher" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({ "appToken": self.token, "content": format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}\n", ctx.title) }, ctx.body), "contentType": 1, "uids": self.uids });
        let resp = client.post("https://wxpusher.zjiecode.com/api/send/message").header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "wxpusher://AT_appid/123/",
            "wxpusher://123?token=AT_abc1234",
            "wxpusher://?token=AT_abc1234&to=UID_abc",
            "wxpusher://AT_appid/UID_abcd/",
            "wxpusher://AT_appid/?to=22222222222,33333333333",
            "wxpusher://AT_appid/?to=22222222222,33333333333,555",
            "wxpusher://AT_appid/22222222222/33333333333/",
            "wxpusher://AT_appid/33333333333",
            "wxpusher://AT_appid/44444444444",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "wxpusher://",
            "wxpusher://:@/",
            "wxpusher://invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
