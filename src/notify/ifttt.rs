use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Ifttt { webhook_id: String, events: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Ifttt {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // ifttt://webhook_id/event1/event2
        // or ifttt://webhook_id@event/?+Key=Value
        // or https://maker.ifttt.com/use/WebHookID/EventID/
        let (webhook_id, mut events) = if let Some(ref host) = url.host {
            if host == "maker.ifttt.com" {
                // https://maker.ifttt.com/use/WebHookID/EventID/
                let parts = &url.path_parts;
                if parts.len() >= 2 && parts[0] == "use" {
                    let wid = parts[1].clone();
                    let evts = parts[2..].to_vec();
                    (wid, evts)
                } else {
                    return None;
                }
            } else if let Some(ref user) = url.user {
                // user is webhook_id, host is the first event
                let mut events = vec![host.clone()];
                events.extend(url.path_parts.clone());
                (user.clone(), events)
            } else {
                let webhook_id = host.clone();
                let events = url.path_parts.clone();
                (webhook_id, events)
            }
        } else {
            return None;
        };
        // Support ?to= param for events
        if let Some(to) = url.get("to") {
            events.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if events.is_empty() { return None; }
        Some(Self { webhook_id, events, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "IFTTT", service_url: Some("https://ifttt.com"), setup_url: None, protocols: vec!["ifttt"], description: "Trigger IFTTT webhooks.", attachment_support: false } }
}
#[async_trait]
impl Notify for Ifttt {
    fn schemas(&self) -> &[&str] { &["ifttt"] }
    fn service_name(&self) -> &str { "IFTTT" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for event in &self.events {
            let url = format!("https://maker.ifttt.com/trigger/{}/with/key/{}", event, self.webhook_id);
            let payload = json!({ "value1": ctx.title, "value2": ctx.body, "value3": ctx.notify_type.as_str() });
            let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "ifttt://WebHookID@EventID/?+TemplateKey=TemplateVal",
            "ifttt://WebHookID?to=EventID,EventID2",
            "ifttt://WebHookID@EventID/?-Value1=&-Value2",
            "ifttt://WebHookID@EventID/EventID2/",
            "https://maker.ifttt.com/use/WebHookID/EventID/",
            "https://maker.ifttt.com/use/WebHookID/EventID/?-Value1=",
            "ifttt://WebHookID@EventID",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "ifttt://",
            "ifttt://:@/",
            "ifttt://EventID/",
            "https://maker.ifttt.com/use/WebHookID/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
