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
    verify_certificate: bool,
    tags: Vec<String>,
}

impl PagerDuty {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // pagerduty://apikey@integration_key
        let integration_key = url.host.clone()?;
        let apikey = url.user.clone().unwrap_or_default();
        let region = url.get("region").unwrap_or("us").to_string();
        Some(Self { apikey, integration_key, region, verify_certificate: url.verify_certificate(), tags: url.tags() })
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
        let payload = json!({
            "routing_key": self.integration_key,
            "event_action": "trigger",
            "payload": {
                "summary": if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) },
                "severity": severity,
                "source": "Apprise",
            }
        });
        let client = build_client(self.verify_certificate)?;
        let url = if self.region == "eu" { "https://events.eu.pagerduty.com/v2/enqueue" } else { "https://events.pagerduty.com/v2/enqueue" };
        let resp = client.post(url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
