use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Nextcloud { host: String, port: Option<u16>, targets: Vec<String>, secure: bool, user: Option<String>, password: Option<String>, verify_certificate: bool, tags: Vec<String> }
impl Nextcloud {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { host, port: url.port, targets, secure: url.schema == "nclouds", user: url.user.clone(), password: url.password.clone(), verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Nextcloud", service_url: Some("https://nextcloud.com"), setup_url: None, protocols: vec!["ncloud", "nclouds"], description: "Send Nextcloud notifications.", attachment_support: false } }
}
#[async_trait]
impl Notify for Nextcloud {
    fn schemas(&self) -> &[&str] { &["ncloud", "nclouds"] }
    fn service_name(&self) -> &str { "Nextcloud" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let url = format!("{}://{}{}/ocs/v2.php/apps/notifications/api/v2/admin_notifications/{}", schema, self.host, port_str, target);
            let params = [("shortMessage", ctx.title.as_str()), ("longMessage", ctx.body.as_str())];
            let mut req = client.post(&url).header("User-Agent", APP_ID).header("OCS-APIREQUEST", "true");
            if let (Some(u), Some(p)) = (&self.user, &self.password) { req = req.basic_auth(u, Some(p)); }
            let resp = req.form(&params).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
