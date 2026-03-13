use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Dapnet { user: String, password: String, targets: Vec<String>, txgroups: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Dapnet {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        let txgroups: Vec<String> = url.get("txgroups").map(|s| s.split(',').map(|g| g.trim().to_string()).collect()).unwrap_or_else(|| vec!["dl-all".to_string()]);
        Some(Self { user, password, targets, txgroups, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "DAPNET", service_url: Some("https://hampager.de"), setup_url: None, protocols: vec!["dapnet"], description: "Send pager messages via DAPNET.", attachment_support: false } }
}
#[async_trait]
impl Notify for Dapnet {
    fn schemas(&self) -> &[&str] { &["dapnet"] }
    fn service_name(&self) -> &str { "DAPNET" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let payload = json!({ "text": msg, "callSignNames": self.targets, "transmitterGroupNames": self.txgroups, "emergency": false });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("http://www.hampager.de:8080/calls").header("User-Agent", APP_ID).basic_auth(&self.user, Some(&self.password)).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 201 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
