use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Zulip { user: String, token: String, org_url: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Zulip {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone()?;
        let token = url.password.clone()?;
        let host = url.host.clone()?;
        let org_url = format!("https://{}", host);
        let mut targets = url.path_parts.clone();
        if targets.is_empty() { targets.push("general".to_string()); }
        Some(Self { user, token, org_url, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Zulip", service_url: Some("https://zulip.com"), setup_url: None, protocols: vec!["zulip"], description: "Send messages via Zulip.", attachment_support: false } }
}
#[async_trait]
impl Notify for Zulip {
    fn schemas(&self) -> &[&str] { &["zulip"] }
    fn service_name(&self) -> &str { "Zulip" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let url = format!("{}/api/v1/messages", self.org_url);
        let mut all_ok = true;

        for target in &self.targets {
            // Detect target type: emails → private, else → stream
            let (msg_type, to_field) = if target.contains('@') {
                ("private", target.as_str())
            } else {
                ("stream", target.as_str())
            };
            let params = [
                ("type", msg_type),
                ("to", to_field),
                ("topic", if ctx.title.is_empty() { "Notification" } else { ctx.title.as_str() }),
                ("content", ctx.body.as_str()),
            ];
            let resp = client.post(&url)
                .header("User-Agent", APP_ID)
                .basic_auth(&self.user, Some(&self.token))
                .form(&params)
                .send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "zulip://",
            "zulip://:@/",
            "zulip://apprise",
            "zulip://botname@apprise",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
