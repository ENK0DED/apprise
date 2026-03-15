use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Zulip { user: String, token: String, org_url: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Zulip {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone()?;
        if user.is_empty() { return None; }
        // Validate user (bot name) - must contain at least one alphanumeric
        if !user.chars().any(|c| c.is_ascii_alphanumeric()) { return None; }
        let host = url.host.clone()?;
        let org_url = format!("https://{}", host);
        // Token from password, first path part, or ?token= query
        let token = url.password.clone()
            .or_else(|| url.get("token").map(|s| s.to_string()))
            .or_else(|| url.path_parts.first().cloned())?;
        if token.is_empty() { return None; }
        // Token must be at least 32 chars
        if token.len() < 32 { return None; }
        // Targets are remaining path parts (after token) + ?to=
        let path_targets: Vec<String> = if url.password.is_some() || url.get("token").is_some() {
            url.path_parts.clone()
        } else {
            url.path_parts.get(1..).unwrap_or(&[]).to_vec()
        };
        let mut targets = path_targets;
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
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
