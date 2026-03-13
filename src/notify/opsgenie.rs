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
        // opsgenie://apikey/target1/target2
        let apikey = url.host.clone()?;
        let targets = url.path_parts.clone();
        let region = url.get("region").unwrap_or("us").to_string();
        Some(Self { apikey, targets, region, verify_certificate: url.verify_certificate(), tags: url.tags() })
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
            payload["responders"] = json!(self.targets.iter().map(|t| json!({ "name": t, "type": "team" })).collect::<Vec<_>>());
        }
        let url = if self.region == "eu" { "https://api.eu.opsgenie.com/v2/alerts" } else { "https://api.opsgenie.com/v2/alerts" };
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(url).header("User-Agent", APP_ID).header("Authorization", format!("GenieKey {}", self.apikey)).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 202 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
