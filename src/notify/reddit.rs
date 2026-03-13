use async_trait::async_trait;
use serde_json::Value;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Reddit {
    app_id: String,
    app_secret: String,
    user: String,
    password: String,
    subreddits: Vec<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Reddit {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // reddit://app_id:app_secret@user:password/subreddit1/subreddit2
        let app_id = url.user.clone()?;
        let app_secret = url.password.clone()?;
        let host_parts: Vec<&str> = url.host.as_deref()?.splitn(2, ':').collect();
        let user = host_parts.get(0)?.to_string();
        let password = host_parts.get(1).unwrap_or(&"").to_string();
        let subreddits = url.path_parts.clone();
        if subreddits.is_empty() { return None; }
        Some(Self { app_id, app_secret, user, password, subreddits, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Reddit", service_url: Some("https://reddit.com"), setup_url: None, protocols: vec!["reddit"], description: "Post to Reddit subreddits.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Reddit {
    fn schemas(&self) -> &[&str] { &["reddit"] }
    fn service_name(&self) -> &str { "Reddit" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        // Get OAuth token
        let token_params = [("grant_type", "password"), ("username", self.user.as_str()), ("password", self.password.as_str())];
        let token_resp = client.post("https://www.reddit.com/api/v1/access_token")
            .header("User-Agent", APP_ID).basic_auth(&self.app_id, Some(&self.app_secret)).form(&token_params).send().await?;
        let token_json: Value = token_resp.json().await.map_err(|e| NotifyError::Other(e.to_string()))?;
        let access_token = token_json["access_token"].as_str().ok_or_else(|| NotifyError::Auth("No access token".into()))?;

        let mut all_ok = true;
        for sub in &self.subreddits {
            let params = [("sr", sub.as_str()), ("kind", "self"), ("title", ctx.title.as_str()), ("text", ctx.body.as_str()), ("resubmit", "true")];
            let resp = client.post("https://oauth.reddit.com/api/submit").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", access_token)).form(&params).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
