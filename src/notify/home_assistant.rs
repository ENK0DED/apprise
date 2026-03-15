use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct HomeAssistant {
    host: String,
    port: Option<u16>,
    access_token: String,
    secure: bool,
    notification_id: Option<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl HomeAssistant {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // hassio://access_token@host[:port]
        // hassio://host/access_token
        // hassio://host/path?accesstoken=token
        let host = url.host.clone()?;
        if host.is_empty() { return None; }
        let secure = url.schema == "hassios";

        // Try to get access token from:
        // 1. ?accesstoken= query param
        // 2. Last path part
        // 3. user field (when path_parts also present - user is just auth, token is in path)
        let access_token = url.get("accesstoken")
            .map(|s| s.to_string())
            .or_else(|| url.path_parts.last().cloned())
            .or_else(|| {
                // Only use user/password as token if path_parts has content
                if !url.path_parts.is_empty() {
                    url.user.clone().or_else(|| url.password.clone())
                } else {
                    None
                }
            })?;

        if access_token.trim().is_empty() { return None; }

        // Validate notification ID if provided
        let notification_id = match url.get("nid").or_else(|| url.get("id")) {
            Some(nid) => {
                let nid = nid.to_string();
                // Reject invalid chars in notification ID
                if nid.contains('!') || nid.contains('%') { return None; }
                Some(nid)
            }
            None => None,
        };

        // Default insecure port is 8123 (matching Python)
        let port = url.port.or(if secure { None } else { Some(8123) });
        Some(Self { host, port, access_token, secure, notification_id, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Home Assistant", service_url: Some("https://www.home-assistant.io"), setup_url: None, protocols: vec!["hassio", "hassios"], description: "Send via Home Assistant persistent notifications.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for HomeAssistant {
    fn schemas(&self) -> &[&str] { &["hassio", "hassios"] }
    fn service_name(&self) -> &str { "Home Assistant" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/api/services/persistent_notification/create", schema, self.host, port_str);
        let mut payload = json!({ "title": ctx.title, "message": ctx.body });
        if let Some(ref id) = self.notification_id { payload["notification_id"] = json!(id); }
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.access_token)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "hassio://localhost/long-lived-access-token",
            "hassio://user:pass@localhost/long-lived-access-token/",
            "hassio://localhost:80/long-lived-access-token",
            "hassio://user@localhost:8123/llat",
            "hassios://localhost/llat?nid=abcd",
            "hassios://user:pass@localhost/llat",
            "hassios://localhost:8443/path/llat/",
            "hassio://localhost:8123/a/path?accesstoken=llat",
            "hassios://user:password@localhost:80/llat/",
            "hassio://user:pass@localhost/llat",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "hassio://:@/",
            "hassio://",
            "hassios://",
            "hassio://user@localhost",
            "hassios://localhost/llat?nid=!%",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
