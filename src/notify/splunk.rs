use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

/// Splunk On-Call (formerly VictorOps) plugin.
/// Sends alerts via the VictorOps REST integration endpoint.
pub struct Splunk {
    apikey: String,
    routing_key: String,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Splunk {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // splunk://route@apikey  or  splunk://?apikey=abc&routing_key=db
        // or victorops://apikey/routing_key
        let apikey = url.host.clone()
            .filter(|h| !h.is_empty())
            .or_else(|| url.get("apikey").map(|s| s.to_string()))?;

        // Reject invalid tokens (percent-encoded bad values)
        let decoded_api = urlencoding::decode(&apikey).unwrap_or_default().into_owned();
        if decoded_api.trim().is_empty() || decoded_api.contains('%') { return None; }

        // Routing key from user field, ?routing_key=, ?route=, or path
        // If a user@ was present in the URL but decoded to empty, reject the URL
        let has_userinfo = url.raw.contains('@') && !url.raw.starts_with("https://");
        if has_userinfo && url.user.as_ref().map_or(true, |u| u.trim().is_empty()) {
            // Had @ in URL but user decoded to nothing — invalid
            if url.get("routing_key").is_none() && url.get("route").is_none() {
                return None;
            }
        }

        let routing_key = url.user.clone().filter(|u| !u.trim().is_empty())
            .or_else(|| url.get("routing_key").or_else(|| url.get("route")).map(|s| s.to_string()))
            .or_else(|| url.path_parts.first().cloned())
            .unwrap_or_else(|| "everyone".to_string());

        // Validate routing key — reject if empty
        if routing_key.trim().is_empty() { return None; }
        let decoded_route = routing_key.clone();

        // If no user and no query params, require both host and routing key from path
        if url.user.is_none() && url.get("apikey").is_none() && url.path_parts.is_empty() {
            return None;
        }

        // Validate action if provided
        if let Some(action) = url.get("action") {
            match action.to_lowercase().as_str() {
                "recovery" | "resolve" | "r" | "acknowledgement" | "ack" | "a"
                | "critical" | "crit" | "c" | "warning" | "warn" | "w"
                | "info" | "i" | "" => {}
                _ => return None,
            }
        }

        // Validate message type remapping (:field=value)
        let valid_types = ["recovery", "resolve", "acknowledgement", "ack",
            "critical", "crit", "warning", "warn", "info"];
        for (k, v) in &url.qsd {
            if k.starts_with(':') {
                let key = &k[1..];
                // Key must be a valid notification type
                if !matches!(key.to_lowercase().as_str(), "info" | "success" | "warning" | "failure") {
                    return None;
                }
                // Value must be a valid VictorOps message type
                if !valid_types.iter().any(|t| v.to_lowercase() == *t) {
                    return None;
                }
            }
        }

        Some(Self { apikey: decoded_api, routing_key: decoded_route, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "Splunk On-Call",
            service_url: Some("https://www.splunk.com/en_us/products/on-call.html"),
            setup_url: None,
            protocols: vec!["splunk", "victorops"],
            description: "Send alerts via Splunk On-Call (VictorOps).",
            attachment_support: false,
        }
    }
}

#[async_trait]
impl Notify for Splunk {
    fn schemas(&self) -> &[&str] { &["splunk", "victorops"] }
    fn service_name(&self) -> &str { "Splunk On-Call" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let message_type = match ctx.notify_type {
            NotifyType::Info | NotifyType::Success => "INFO",
            NotifyType::Warning => "WARNING",
            NotifyType::Failure => "CRITICAL",
        };

        let payload = json!({
            "message_type": message_type,
            "entity_id": ctx.title,
            "entity_display_name": if ctx.title.is_empty() { "Apprise Notification" } else { ctx.title.as_str() },
            "state_message": ctx.body,
            "monitoring_tool": "Apprise",
        });

        let url = format!(
            "https://alert.victorops.com/integrations/generic/20131114/alert/{}/{}",
            self.apikey, self.routing_key
        );
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "splunk://?apikey=abc123&routing_key=db",
            "splunk://route@abc123/entity_id",
            "splunk://route@abc123/?entity_id=my_entity",
            "https://alert.victorops.com/integrations/generic/20131114/alert/apikey/routing_key",
            "https://alert.victorops.com/integrations/generic/20131114/alert/apikey/routing_key/entity_id",
            "victorops://?apikey=abc123&route=db",
            "splunk://?apikey=abc123&route=db",
            "splunk://db@apikey?action=recovery",
            "splunk://db@apikey?action=resolve",
            "splunk://db@apikey?action=r",
            "splunk://db@apikey?action=acknowledgement",
            "splunk://db@apikey?action=ack",
            "splunk://db@apikey?action=critical",
            "splunk://db@apikey?action=crit",
            "splunk://db@apikey?action=warning",
            "splunk://db@apikey?action=warn",
            "splunk://db@apikey?action=info",
            "splunk://db@apikey?action=i",
            "splunk://db@apikey?:warning=critical",
            "splunk://db@token",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "splunk://",
            "splunk://:@/",
            "splunk://routekey@%badapi%",
            "splunk://abc123",
            "splunk://%badroute%@apikey",
            "splunk://db@apikey?action=invalid",
            "splunk://db@apikey?:invalid=critical",
            "splunk://db@apikey?:warning=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
