use async_trait::async_trait;
use serde_json::{json, Value};
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Mastodon {
    host: String,
    port: Option<u16>,
    token: String,
    secure: bool,
    visibility: String,
    spoiler_text: Option<String>,
    language: Option<String>,
    sensitive: bool,
    targets: Vec<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Mastodon {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let token = url.user.clone()?;
        let visibility = url.get("visibility").unwrap_or("public").to_string();
        let spoiler_text = url.get("spoiler").map(|s| s.to_string());
        let language = url.get("language").or_else(|| url.get("lang")).map(|s| s.to_string());
        let sensitive = url.get("sensitive").map(crate::utils::parse::parse_bool).unwrap_or(false);
        let targets = url.path_parts.clone();
        Some(Self {
            host, port: url.port, token,
            secure: url.schema.ends_with('s'),
            visibility, spoiler_text, language, sensitive, targets,
            verify_certificate: url.verify_certificate(), tags: url.tags(),
        })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Mastodon", service_url: Some("https://mastodon.social"), setup_url: None, protocols: vec!["mastodon", "toot", "mastodons", "toots"], description: "Post a toot on Mastodon.", attachment_support: true }
    }
}

#[async_trait]
impl Notify for Mastodon {
    fn schemas(&self) -> &[&str] { &["mastodon", "toot", "mastodons", "toots"] }
    fn service_name(&self) -> &str { "Mastodon" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let base_url = format!("{}://{}{}", schema, self.host, port_str);
        let client = build_client(self.verify_certificate)?;

        // Upload attachments and collect media IDs
        let mut media_ids: Vec<String> = Vec::new();
        for att in &ctx.attachments {
            let upload_url = format!("{}/api/v1/media", base_url);
            let part = reqwest::multipart::Part::bytes(att.data.clone())
                .file_name(att.name.clone())
                .mime_str(&att.mime_type)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()));
            let form = reqwest::multipart::Form::new().part("file", part);
            let upload_resp = client.post(&upload_url)
                .header("User-Agent", APP_ID)
                .header("Authorization", format!("Bearer {}", self.token))
                .multipart(form)
                .send().await?;
            if upload_resp.status().is_success() {
                let media: Value = upload_resp.json().await.unwrap_or_default();
                if let Some(id) = media["id"].as_str() {
                    media_ids.push(id.to_string());
                }
            }
        }

        let status_url = format!("{}/api/v1/statuses", base_url);
        let status = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n\n{}", ctx.title, ctx.body) };

        let mut payload = json!({
            "status": status,
            "visibility": self.visibility,
            "sensitive": self.sensitive,
        });
        if let Some(ref spoiler) = self.spoiler_text { payload["spoiler_text"] = json!(spoiler); }
        if let Some(ref lang) = self.language { payload["language"] = json!(lang); }
        if !media_ids.is_empty() { payload["media_ids"] = json!(media_ids); }

        let resp = client.post(&status_url)
            .header("User-Agent", APP_ID)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&payload)
            .send().await?;

        if resp.status().is_success() { Ok(true) }
        else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "mastodon://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
