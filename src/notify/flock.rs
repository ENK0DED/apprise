use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Flock { token: String, verify_certificate: bool, tags: Vec<String> }
impl Flock {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        if token.is_empty() { return None; }
        // Validate path targets: g: and u: prefixed targets must have content after the prefix
        for p in &url.path_parts {
            if (p == "g:" || p == "u:") { return None; }
        }
        // Also check ?to= targets
        if let Some(to) = url.get("to") {
            for t in to.split(',') {
                let t = t.trim();
                if t == "g:" || t == "u:" { return None; }
            }
        }
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Flock", service_url: Some("https://flock.com"), setup_url: None, protocols: vec!["flock"], description: "Send via Flock webhooks.", attachment_support: false } }
}
#[async_trait]
impl Notify for Flock {
    fn schemas(&self) -> &[&str] { &["flock"] }
    fn service_name(&self) -> &str { "Flock" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://api.flock.com/hooks/sendMessage/{}", self.token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("<b>{}</b>\n{}", ctx.title, ctx.body) };
        let payload = json!({ "text": text });
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
            "flock://tttttttttttttttttttttttt",
            "flock://tttttttttttttttttttttttt?image=True",
            "flock://tttttttttttttttttttttttt?image=False",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii?to=u:uuuuuuuuuuuu&format=markdown",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii?format=markdown",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii?format=text",
            "https://api.flock.com/hooks/sendMessage/iiiiiiiiiiiiiiiiiiiiiiii/",
            "https://api.flock.com/hooks/sendMessage/iiiiiiiiiiiiiiiiiiiiiiii/?format=markdown",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii/u:uuuuuuuuuuuu?format=markdown",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii/u:uuuuuuuuuuuu?format=html",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii/uuuuuuuuuuuu?format=text",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii/g:gggggggggggg/u:uuuuuuuuuuuu?format=text",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii/#gggggggggggg/@uuuuuuuuuuuu?format=text",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii/g:gggggggggggg/u:uuuuuuuuuu?format=text",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii/g:gggggggggggggg/u:uuuuuuuuuu?format=text",
            "flock://tttttttttttttttttttttttt/",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "flock://",
            "flock://:@/",
            "flock://iiiiiiiiiiiiiiiiiiiiiiii/g:/u:?format=text",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
