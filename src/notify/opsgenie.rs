use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

pub struct OpsGenie {
    apikey: String,
    targets: Vec<String>,
    region: String,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl OpsGenie {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // opsgenie://apikey/target1/target2 or opsgenie://?apikey=abc&to=user
        let apikey = url.host.clone()
            .filter(|h| !h.is_empty())
            .or_else(|| url.get("apikey").map(|s| s.to_string()))?;
        let decoded = urlencoding::decode(&apikey).unwrap_or_default().into_owned();
        if decoded.trim().is_empty() { return None; }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        let region = url.get("region").unwrap_or("us").to_string();
        // Validate region
        match region.to_lowercase().as_str() {
            "us" | "eu" | "" => {}
            _ => return None,
        }
        // Validate action if provided
        if let Some(action) = url.get("action") {
            match action.to_lowercase().as_str() {
                "new" | "close" | "delete" | "acknowledge" | "ack" | "note" | "" => {}
                _ => return None,
            }
        }
        // Validate notification type-to-action mappings (:type=action params)
        let valid_types = ["info", "success", "warning", "failure"];
        let valid_map_actions = ["new", "close", "delete", "acknowledge", "ack", "note"];
        for (key, val) in &url.qsd {
            if key.starts_with(':') {
                let ntype = &key[1..];
                if !valid_types.contains(&ntype) { return None; }
                if !val.is_empty() && !valid_map_actions.contains(&val.to_lowercase().as_str()) {
                    return None;
                }
            }
        }
        Some(Self { apikey: decoded, targets, region, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "OpsGenie", service_url: Some("https://www.opsgenie.com"), setup_url: None, protocols: vec!["opsgenie"], description: "Send alerts via OpsGenie.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for OpsGenie {
    fn schemas(&self) -> &[&str] { &["opsgenie"] }
    fn service_name(&self) -> &str { "OpsGenie" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let priority = match ctx.notify_type {
            NotifyType::Info => "P3",
            NotifyType::Success => "P3",
            NotifyType::Warning => "P2",
            NotifyType::Failure => "P1",
        };
        let mut payload = json!({
            "message": if ctx.title.is_empty() { ctx.body.clone() } else { ctx.title.clone() },
            "description": ctx.body,
            "priority": priority,
        });
        if !self.targets.is_empty() {
            payload["responders"] = json!(self.targets.iter().map(|t| {
                // Detect type from prefix: @=user, #=team, *=schedule, ^=escalation
                let (name, rtype) = if let Some(n) = t.strip_prefix('@') {
                    (n, "user")
                } else if let Some(n) = t.strip_prefix('#') {
                    (n, "team")
                } else if let Some(n) = t.strip_prefix('*') {
                    (n, "schedule")
                } else if let Some(n) = t.strip_prefix('^') {
                    (n, "escalation")
                } else {
                    (t.as_str(), "team")
                };
                json!({ "name": name, "type": rtype })
            }).collect::<Vec<_>>());
        }
        let url = if self.region == "eu" { "https://api.eu.opsgenie.com/v2/alerts" } else { "https://api.opsgenie.com/v2/alerts" };
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(url).header("User-Agent", APP_ID).header("Authorization", format!("GenieKey {}", self.apikey)).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 202 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "opsgenie://user@apikey/",
            "opsgenie://apikey/",
            "opsgenie://apikey/user",
            "opsgenie://apikey/@user?region=eu",
            "opsgenie://apikey/@user?entity=A%20Entity",
            "opsgenie://apikey/@user?alias=An%20Alias",
            "opsgenie://apikey/@user?entity=index&action=new",
            "opsgenie://apikey/@user?entity=index&action=acknowledge",
            "opsgenie://from@apikey/@user?entity=index&action=note",
            "opsgenie://apikey/@user?entity=index&action=close",
            "opsgenie://apikey/@user?entity=index&action=delete",
            "opsgenie://apikey/@user?entity=index2&:info=new",
            "opsgenie://joe@apikey/@user?priority=p3",
            "opsgenie://apikey/?tags=comma,separated",
            "opsgenie://apikey/@user?priority=invalid",
            "opsgenie://apikey/user@email.com/#team/*sche/^esc/%20/a",
            "opsgenie://apikey?to=#team,user&+key=value&+type=override",
            "opsgenie://apikey/#team/@user/?batch=yes",
            "opsgenie://apikey/#team/@user/?batch=no",
            "opsgenie://?apikey=abc&to=user",
            "opsgenie://apikey/#topic1/device/",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "opsgenie://",
            "opsgenie://:@/",
            "opsgenie://%20%20/",
            "opsgenie://apikey/user/?region=xx",
            "opsgenie://apikey/@user?action=invalid",
            "opsgenie://from@apikey/@user?:invalid=note",
            "opsgenie://apikey/@user?:warning=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
