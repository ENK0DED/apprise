use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

pub struct PagerDuty {
    apikey: String,
    integration_key: String,
    region: String,
    source: String,
    component: Option<String>,
    group: Option<String>,
    class: Option<String>,
    click: Option<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl PagerDuty {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let integration_key = url.host.clone()
            .filter(|h| !h.is_empty())
            .or_else(|| url.get("integrationkey").map(|s| s.to_string()))?;
        let decoded_ik = urlencoding::decode(&integration_key).unwrap_or_default().into_owned();
        if decoded_ik.trim().is_empty() { return None; }
        // Also validate user (apikey) if present
        if let Some(ref u) = url.user {
            let decoded_u = urlencoding::decode(u).unwrap_or_default().into_owned();
            if decoded_u.trim().is_empty() { return None; }
        }
        let apikey = url.user.clone()
            .or_else(|| url.get("apikey").map(|s| s.to_string()))
            .unwrap_or_default();
        let region = url.get("region").unwrap_or("us").to_string();
        // Validate region
        match region.to_lowercase().as_str() {
            "us" | "eu" | "" => {}
            _ => return None,
        }
        let source = url.get("source").unwrap_or("Apprise").to_string();
        let component = url.get("component").map(|s| s.to_string());
        let group = url.get("group").map(|s| s.to_string());
        let class = url.get("class").map(|s| s.to_string());
        let click = url.get("click").map(|s| s.to_string());
        // Reject whitespace-only path parts
        for pp in &url.path_parts {
            if pp.trim().is_empty() { return None; }
        }
        // Validate severity if provided
        if let Some(severity) = url.get("severity") {
            match severity.to_lowercase().as_str() {
                "info" | "warning" | "error" | "err" | "critical" | "crit" | "" => {}
                _ => return None,
            }
        }
        Some(Self { apikey, integration_key: decoded_ik, region, source, component, group, class, click, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "PagerDuty", service_url: Some("https://pagerduty.com"), setup_url: None, protocols: vec!["pagerduty"], description: "Send alerts via PagerDuty Events v2 API.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for PagerDuty {
    fn schemas(&self) -> &[&str] { &["pagerduty"] }
    fn service_name(&self) -> &str { "PagerDuty" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let severity = match ctx.notify_type {
            NotifyType::Info => "info",
            NotifyType::Success => "info",
            NotifyType::Warning => "warning",
            NotifyType::Failure => "critical",
        };
        let mut pd_payload = json!({
            "summary": if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) },
            "severity": severity,
            "source": self.source,
        });
        if let Some(ref c) = self.component { pd_payload["component"] = json!(c); }
        if let Some(ref g) = self.group { pd_payload["group"] = json!(g); }
        if let Some(ref c) = self.class { pd_payload["class"] = json!(c); }
        let mut payload = json!({
            "routing_key": self.integration_key,
            "event_action": "trigger",
            "payload": pd_payload,
        });
        if let Some(ref click_url) = self.click {
            payload["links"] = json!([{ "href": click_url, "text": "View" }]);
        }
        let client = build_client(self.verify_certificate)?;
        let url = if self.region == "eu" { "https://events.eu.pagerduty.com/v2/enqueue" } else { "https://events.pagerduty.com/v2/enqueue" };
        let resp = client.post(url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "pagerduty://myroutekey@myapikey",
            "pagerduty://myroutekey@myapikey?image=no",
            "pagerduty://myroutekey@myapikey?region=eu",
            "pagerduty://myroutekey@myapikey?severity=critical",
            "pagerduty://myroutekey@myapikey?severity=err",
            "pagerduty://myroutekey@myapikey?+key=value&+key2=value2",
            "pagerduty://myroutekey@myapikey/mysource/mycomponent",
            "pagerduty://routekey@apikey/ms/mc?group=mygroup&class=myclass",
            "pagerduty://?integrationkey=r&apikey=a&source=s&component=c&group=g&class=c&image=no&click=http://localhost",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "pagerduty://",
            "pagerduty://%20@%20/",
            "pagerduty://%20/",
            "pagerduty://%20@abcd/",
            "pagerduty://myroutekey@myapikey/%20",
            "pagerduty://myroutekey@myapikey/mysource/%20",
            "pagerduty://myroutekey@myapikey?region=invalid",
            "pagerduty://myroutekey@myapikey?severity=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
