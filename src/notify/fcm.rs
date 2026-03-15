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
        // apikey from host or ?apikey= param
        let api_key = url.host.clone()
            .map(|h| urlencoding::decode(&h).unwrap_or_default().into_owned())
            .filter(|h| !h.is_empty() && !h.trim().is_empty())
            .or_else(|| url.get("apikey").map(|s| s.to_string()))?;
        // Reject whitespace-only api keys
        if api_key.trim().is_empty() { return None; }

        let mut targets: Vec<String> = url.path_parts.iter()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .collect();
        // Support ?to= query param
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }

        // Validate mode if provided
        if let Some(mode) = url.get("mode") {
            match mode.to_lowercase().as_str() {
                "legacy" | "oauth2" => {}
                _ => return None,
            }
            // oauth2 mode requires project and keyfile
            if mode.to_lowercase() == "oauth2" {
                if url.get("keyfile").is_none() { return None; }
            }
        }

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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "fcm://apikey/",
            "fcm://apikey/device",
            "fcm://apikey/#topic",
            "fcm://apikey/#topic1/device/%20/",
            "fcm://apikey?to=#topic1,device",
            "fcm://?apikey=abc123&to=device",
            "fcm://?apikey=abc123&to=device&image=yes",
            "fcm://?apikey=abc123&to=device&color=no",
            "fcm://?apikey=abc123&to=device&color=aabbcc",
            "fcm://?apikey=abc123&to=device&image_url=http://example.com/interesting.jpg",
            "fcm://?apikey=abc123&to=device&image_url=http://example.com/interesting.jpg&image=no",
            "fcm://?apikey=abc123&to=device&+key=value&+key2=value2",
            "fcm://apikey/#topic1/device/?mode=legacy",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "fcm://",
            "fcm://:@/",
            "fcm://project@%20%20/",
            "fcm://apikey/device?mode=invalid",
            "fcm://%20?to=device&keyfile=/invalid/path",
            "fcm://project_id?to=device&mode=oauth2",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    fn parse_fcm(url: &str) -> Fcm {
        let parsed = crate::utils::parse::ParsedUrl::parse(url).unwrap();
        Fcm::from_url(&parsed).unwrap()
    }

    #[test]
    fn test_from_url_apikey_from_host() {
        let f = parse_fcm("fcm://apikey/device");
        assert_eq!(f.api_key, "apikey");
        assert_eq!(f.targets, vec!["device"]);
        assert_eq!(f.priority, "normal");
    }

    #[test]
    fn test_from_url_apikey_from_query() {
        let f = parse_fcm("fcm://?apikey=abc123&to=device");
        assert_eq!(f.api_key, "abc123");
        assert!(f.targets.contains(&"device".to_string()));
    }

    #[test]
    fn test_from_url_topic_target() {
        let f = parse_fcm("fcm://apikey/#topic");
        assert_eq!(f.targets, vec!["#topic"]);
    }

    #[test]
    fn test_from_url_multiple_targets() {
        let f = parse_fcm("fcm://apikey?to=#topic1,device");
        assert!(f.targets.contains(&"#topic1".to_string()));
        assert!(f.targets.contains(&"device".to_string()));
    }

    #[test]
    fn test_from_url_legacy_mode() {
        let f = parse_fcm("fcm://apikey/#topic1/device/?mode=legacy");
        assert_eq!(f.api_key, "apikey");
        assert!(f.targets.contains(&"#topic1".to_string()));
        assert!(f.targets.contains(&"device".to_string()));
    }

    #[test]
    fn test_from_url_invalid_mode_returns_none() {
        let parsed = crate::utils::parse::ParsedUrl::parse("fcm://apikey/device?mode=invalid").unwrap();
        assert!(Fcm::from_url(&parsed).is_none());
    }

    #[test]
    fn test_from_url_oauth2_without_keyfile_returns_none() {
        let parsed = crate::utils::parse::ParsedUrl::parse("fcm://project_id?to=device&mode=oauth2").unwrap();
        assert!(Fcm::from_url(&parsed).is_none());
    }

    #[test]
    fn test_from_url_project_param() {
        let f = parse_fcm("fcm://?apikey=abc123&to=device&project=myproject");
        assert_eq!(f.project, Some("myproject".to_string()));
    }

    #[test]
    fn test_from_url_no_project() {
        let f = parse_fcm("fcm://apikey/device");
        assert_eq!(f.project, None);
    }

    #[test]
    fn test_service_details() {
        let details = Fcm::static_details();
        assert_eq!(details.service_name, "Firebase Cloud Messaging");
        assert_eq!(details.protocols, vec!["fcm"]);
        assert!(!details.attachment_support);
    }
}
