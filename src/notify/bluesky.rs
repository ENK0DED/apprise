use async_trait::async_trait;
use serde_json::{json, Value};
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct BlueSky { user: String, password: String, verify_certificate: bool, tags: Vec<String> }
impl BlueSky {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // bluesky://user:password or bluesky://user@password
        let user = url.user.clone()?;
        let password = url.password.clone()
            .or_else(|| url.host.clone().filter(|h| !h.is_empty()))?;
        Some(Self { user, password, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "BlueSky", service_url: Some("https://bsky.app"), setup_url: None, protocols: vec!["bsky", "bluesky"], description: "Post to BlueSky.", attachment_support: true } }
}
#[async_trait]
impl Notify for BlueSky {
    fn schemas(&self) -> &[&str] { &["bsky", "bluesky"] }
    fn service_name(&self) -> &str { "BlueSky" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        // Login
        let login_payload = json!({ "identifier": self.user, "password": self.password });
        let session: Value = client.post("https://bsky.social/xrpc/com.atproto.server.createSession").header("User-Agent", APP_ID).json(&login_payload).send().await?.json().await.map_err(|e| NotifyError::Auth(e.to_string()))?;
        let access_jwt = session["accessJwt"].as_str().ok_or_else(|| NotifyError::Auth("No access JWT".into()))?;
        let did = session["did"].as_str().ok_or_else(|| NotifyError::Auth("No DID".into()))?;

        // Upload image attachments as blobs
        let image_attachments: Vec<_> = ctx.attachments.iter()
            .filter(|att| att.mime_type.starts_with("image/"))
            .collect();
        let mut blob_refs: Vec<Value> = Vec::new();
        for att in &image_attachments {
            let upload_resp: Value = client
                .post("https://bsky.social/xrpc/com.atproto.repo.uploadBlob")
                .header("User-Agent", APP_ID)
                .header("Authorization", format!("Bearer {}", access_jwt))
                .header("Content-Type", &att.mime_type)
                .body(att.data.clone())
                .send().await?
                .json().await
                .map_err(|e| NotifyError::Other(format!("Failed to upload blob: {}", e)))?;
            if let Some(blob) = upload_resp.get("blob") {
                blob_refs.push(json!({
                    "image": blob,
                    "alt": att.name.clone(),
                }));
            }
        }

        // Post
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n\n{}", ctx.title, ctx.body) };
        let mut record_inner = json!({
            "$type": "app.bsky.feed.post",
            "text": text,
            "createdAt": chrono::Utc::now().to_rfc3339(),
        });
        if !blob_refs.is_empty() {
            record_inner["embed"] = json!({
                "$type": "app.bsky.embed.images",
                "images": blob_refs,
            });
        }
        let record = json!({ "repo": did, "collection": "app.bsky.feed.post", "record": record_inner });
        let resp = client.post("https://bsky.social/xrpc/com.atproto.repo.createRecord").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", access_jwt)).json(&record).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "bluesky://user@app-pw",
            "bluesky://user@app-pw1?cache=no",
            "bluesky://user@app-pw2?cache=no",
            "bluesky://user@app-pw3",
            "bluesky://user.example.ca@app-pw3",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "bluesky://",
            "bluesky://:@/",
            "bluesky://app-pw",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
