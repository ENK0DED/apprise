use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Fluxer { webhook_id: String, token: String, verify_certificate: bool, tags: Vec<String> }
impl Fluxer {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let webhook_id = url.host.clone()?;
        let token = url.path_parts.first()?.clone();
        if token.is_empty() { return None; }
        // Validate mode if provided
        if let Some(mode) = url.get("mode") {
            match mode.to_lowercase().as_str() {
                "private" | "public" | "" => {}
                _ => return None,
            }
        }
        // Validate flags if provided
        if let Some(flags) = url.get("flags") {
            if !flags.is_empty() {
                let val: i64 = flags.parse().ok()?;
                if val < 0 { return None; }
            }
        }
        Some(Self { webhook_id, token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Fluxer", service_url: None, setup_url: None, protocols: vec!["fluxer", "fluxers"], description: "Send via Fluxer webhooks.", attachment_support: true } }
}
#[async_trait]
impl Notify for Fluxer {
    fn schemas(&self) -> &[&str] { &["fluxer", "fluxers"] }
    fn service_name(&self) -> &str { "Fluxer" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://api.fluxer.io/webhooks/{}/{}", self.webhook_id, self.token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let payload = json!({ "content": text });
        let client = build_client(self.verify_certificate)?;
        let resp = if !ctx.attachments.is_empty() {
            let payload_str = serde_json::to_string(&payload).unwrap_or_default();
            let mut form = reqwest::multipart::Form::new()
                .text("payload_json", payload_str);
            for (i, att) in ctx.attachments.iter().enumerate() {
                let part = reqwest::multipart::Part::bytes(att.data.clone())
                    .file_name(att.name.clone())
                    .mime_str(&att.mime_type)
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()));
                form = form.part(format!("files[{}]", i), part);
            }
            client.post(&url).header("User-Agent", APP_ID).multipart(form).send().await?
        } else {
            client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?
        };
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "fluxer://l2g@0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "fluxer://api.fluxer.app/0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?mode=private",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?format=markdown&footer=Yes&image=Yes&ping=Joe",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?format=markdown&footer=Yes&image=No&fields=no",
            "fluxer://jack@0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?format=markdown&footer=Yes&image=Yes",
            "fluxer://jack@0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?mode=private&host=example.ca",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?mode=private&host=example.ca&name=jack",
            "fluxer://example.ca:123/0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "https://api.fluxer.app/webhooks/0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "https://api.fluxer.app/v1/webhooks/0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?footer=yes",
            "https://api.fluxer.app/v1/webhooks/0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?footer=yes&botname=joe",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?format=markdown&avatar=No&footer=No",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?flags=1",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?format=markdown",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?format=markdown&thread=abc123",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?format=text",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?hmarkdown=true&ref=http://localhost",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?markdown=true&url=http://localhost",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?avatar_url=http://localhost/test.jpg",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "fluxer://",
            "fluxer://:@/",
            "fluxer://0000000000",
            "fluxer://jack@0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?mode=invalid",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?flags=-1",
            "fluxer://0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB?flags=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
