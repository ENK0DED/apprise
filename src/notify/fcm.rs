use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Fcm {
    api_key: String,
    project: Option<String>,
    targets: Vec<String>,
    priority: String,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Fcm {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let api_key = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        let project = url.get("project").map(|s| s.to_string());
        let priority = url.get("priority").unwrap_or("normal").to_string();
        Some(Self { api_key, project, targets, priority, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }

    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "Firebase Cloud Messaging",
            service_url: Some("https://firebase.google.com/docs/cloud-messaging"),
            setup_url: None,
            protocols: vec!["fcm"],
            description: "Send push notifications via Google FCM.",
            attachment_support: false,
        }
    }
}

#[async_trait]
impl Notify for Fcm {
    fn schemas(&self) -> &[&str] { &["fcm"] }
    fn service_name(&self) -> &str { "Firebase Cloud Messaging" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;

        for target in &self.targets {
            // Determine if target is a topic (prefixed with #) or device token
            let (to_field, to_value) = if target.starts_with('#') {
                ("to", format!("/topics/{}", &target[1..]))
            } else {
                ("to", target.clone())
            };

            let payload = json!({
                to_field: to_value,
                "priority": self.priority,
                "notification": {
                    "title": ctx.title,
                    "body": ctx.body,
                }
            });

            let resp = client.post("https://fcm.googleapis.com/fcm/send")
                .header("User-Agent", APP_ID)
                .header("Authorization", format!("key={}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send().await?;

            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
