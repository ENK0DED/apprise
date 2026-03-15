use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Ryver { organization: String, token: String, verify_certificate: bool, tags: Vec<String> }
impl Ryver {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Handle https://ORG.ryver.com/application/webhook/TOKEN
        let (organization, token) = if let Some(ref host) = url.host {
            if host.ends_with(".ryver.com") {
                let org = host.trim_end_matches(".ryver.com").to_string();
                // Find the token — it's the last path part after "application/webhook"
                let token = url.path_parts.last()?.clone();
                (org, token)
            } else {
                let org = host.clone();
                let token = url.path_parts.first()?.clone();
                (org, token)
            }
        } else {
            return None;
        };
        // Organization must be 3-32 alphanumeric/dash/underscore chars
        if organization.len() < 3 || organization.len() > 32
            || !organization.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return None;
        }
        // Validate mode if provided
        if let Some(mode) = url.get("mode") {
            match mode.to_lowercase().as_str() {
                "slack" | "ryver" | "" => {}
                _ => return None,
            }
        }
        Some(Self { organization, token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Ryver", service_url: Some("https://ryver.com"), setup_url: None, protocols: vec!["ryver"], description: "Send via Ryver webhooks.", attachment_support: false } }
}
#[async_trait]
impl Notify for Ryver {
    fn schemas(&self) -> &[&str] { &["ryver"] }
    fn service_name(&self) -> &str { "Ryver" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://{}.ryver.com/application/webhook/{}", self.organization, self.token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let payload = json!({ "body": text });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "ryver://apprise/ckhrjW8w672m6HG?mode=slack",
            "ryver://apprise/ckhrjW8w672m6HG?mode=ryver",
            "ryver://apprise/ckhrjW8w672m6HG?webhook=slack",
            "ryver://apprise/ckhrjW8w672m6HG?webhook=ryver",
            "https://apprise.ryver.com/application/webhook/ckhrjW8w672m6HG",
            "https://apprise.ryver.com/application/webhook/ckhrjW8w672m6HG?webhook=ryver",
            "ryver://caronc@apprise/ckhrjW8w672m6HG",
            "ryver://apprise/ckhrjW8w672m6HG",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "ryver://",
            "ryver://:@/",
            "ryver://apprise",
            "ryver://apprise/ckhrjW8w672m6HG?mode=invalid",
            "ryver://x/ckhrjW8w672m6HG?mode=slack",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
