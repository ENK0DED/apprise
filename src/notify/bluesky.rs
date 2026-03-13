use async_trait::async_trait;
use serde_json::{json, Value};
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct BlueSky { user: String, password: String, verify_certificate: bool, tags: Vec<String> }
impl BlueSky {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        Some(Self { user, password, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "BlueSky", service_url: Some("https://bsky.app"), setup_url: None, protocols: vec!["bsky", "bluesky"], description: "Post to BlueSky.", attachment_support: false } }
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
        // Post
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n\n{}", ctx.title, ctx.body) };
        let record = json!({ "repo": did, "collection": "app.bsky.feed.post", "record": { "$type": "app.bsky.feed.post", "text": text, "createdAt": chrono::Utc::now().to_rfc3339() } });
        let resp = client.post("https://bsky.social/xrpc/com.atproto.repo.createRecord").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", access_jwt)).json(&record).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
