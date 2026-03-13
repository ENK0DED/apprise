use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct SignalApi { host: String, port: Option<u16>, source: String, targets: Vec<String>, secure: bool, user: Option<String>, password: Option<String>, verify_certificate: bool, tags: Vec<String> }
impl SignalApi {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let source = url.path_parts.first()?.clone();
        let targets = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
        if targets.is_empty() { return None; }
        Some(Self { host, port: url.port, source, targets, secure: url.schema == "signals", user: url.user.clone(), password: url.password.clone(), verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Signal API", service_url: Some("https://signal.org"), setup_url: None, protocols: vec!["signal", "signals"], description: "Send Signal messages via signal-cli REST API.", attachment_support: false } }
}
#[async_trait]
impl Notify for SignalApi {
    fn schemas(&self) -> &[&str] { &["signal", "signals"] }
    fn service_name(&self) -> &str { "Signal API" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/v2/send", schema, self.host, port_str);
        let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
        let payload = json!({ "message": msg, "number": self.source, "recipients": self.targets });
        let client = build_client(self.verify_certificate)?;
        let mut req = client.post(&url).header("User-Agent", APP_ID).json(&payload);
        if let (Some(u), Some(p)) = (&self.user, &self.password) { req = req.basic_auth(u, Some(p)); }
        let resp = req.send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
