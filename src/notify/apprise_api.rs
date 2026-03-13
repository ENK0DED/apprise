use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct AppriseApi {
    host: String,
    port: Option<u16>,
    token: String,
    secure: bool,
    user: Option<String>,
    password: Option<String>,
    tags: Vec<String>,
    verify_certificate: bool,
}

impl AppriseApi {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let token = url.path_parts.first()?.clone();
        Some(Self {
            host, port: url.port, token,
            secure: url.schema == "apprises",
            user: url.user.clone(), password: url.password.clone(),
            tags: url.tags(), verify_certificate: url.verify_certificate(),
        })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Apprise API", service_url: Some("https://github.com/caronc/apprise-api"), setup_url: None, protocols: vec!["apprise", "apprises"], description: "Send via the Apprise REST API.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for AppriseApi {
    fn schemas(&self) -> &[&str] { &["apprise", "apprises"] }
    fn service_name(&self) -> &str { "Apprise API" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/notify/{}", schema, self.host, port_str, self.token);
        let payload = json!({ "title": ctx.title, "body": ctx.body, "type": ctx.notify_type.as_str() });
        let client = build_client(self.verify_certificate)?;
        let mut req = client.post(&url).header("User-Agent", APP_ID).json(&payload);
        if let (Some(u), Some(p)) = (&self.user, &self.password) { req = req.basic_auth(u, Some(p)); }
        let resp = req.send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
