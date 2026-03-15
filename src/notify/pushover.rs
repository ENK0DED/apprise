use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

pub struct Pushover {
    user_key: String,
    token: String,
    targets: Vec<String>,
    priority: i32,
    sound: Option<String>,
    retry: Option<i32>,
    expire: Option<i32>,
    supplemental_url: Option<String>,
    supplemental_url_title: Option<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Pushover {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // pover://userkey@token/device1/device2
        let token = url.host.clone()?;
        let user_key = url.user.clone()?;
        let priority = url.get("priority").and_then(|p| p.parse().ok()).unwrap_or(0);
        let sound = url.get("sound").map(|s| s.to_string());
        let retry = url.get("retry").and_then(|p| p.parse().ok());
        let expire = url.get("expire").and_then(|p| p.parse().ok());
        let supplemental_url = url.get("url").map(|s| s.to_string());
        let supplemental_url_title = url.get("url_title").map(|s| s.to_string());
        let targets = url.path_parts.clone();
        Some(Self { user_key, token, targets, priority, sound, retry, expire, supplemental_url, supplemental_url_title, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Pushover", service_url: Some("https://pushover.net"), setup_url: None, protocols: vec!["pover"], description: "Send notifications via Pushover.", attachment_support: true }
    }
}

#[async_trait]
impl Notify for Pushover {
    fn schemas(&self) -> &[&str] { &["pover"] }
    fn service_name(&self) -> &str { "Pushover" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    fn body_maxlen(&self) -> usize { 1024 }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut payload = json!({
            "token": self.token,
            "user": self.user_key,
            "message": ctx.body,
            "title": ctx.title,
            "priority": self.priority,
        });
        // Devices are comma-joined in a single request (matching Python)
        if !self.targets.is_empty() {
            payload["device"] = json!(self.targets.join(","));
        }
        if let Some(ref sound) = self.sound { payload["sound"] = json!(sound); }
        if self.priority == 2 {
            // Emergency priority requires retry and expire
            payload["retry"] = json!(self.retry.unwrap_or(30));
            payload["expire"] = json!(self.expire.unwrap_or(3600));
        }
        if let Some(ref url) = self.supplemental_url { payload["url"] = json!(url); }
        if let Some(ref title) = self.supplemental_url_title { payload["url_title"] = json!(title); }

        // Pushover supports one attachment per message via multipart
        let resp = if let Some(att) = ctx.attachments.first() {
            let mut form = reqwest::multipart::Form::new();
            // Add all JSON payload fields as text parts
            if let Some(obj) = payload.as_object() {
                for (k, v) in obj {
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        _ => v.to_string(),
                    };
                    form = form.text(k.clone(), val);
                }
            }
            let part = reqwest::multipart::Part::bytes(att.data.clone())
                .file_name(att.name.clone())
                .mime_str(&att.mime_type)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()));
            form = form.part("attachment", part);
            client.post("https://api.pushover.net/1/messages.json").header("User-Agent", APP_ID).multipart(form).send().await?
        } else {
            client.post("https://api.pushover.net/1/messages.json").header("User-Agent", APP_ID).json(&payload).send().await?
        };
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "pover://",
            "pover://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
