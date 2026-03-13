use async_trait::async_trait;
use serde_json::{json, Value};
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Json {
    host: String,
    port: Option<u16>,
    path: String,
    secure: bool,
    user: Option<String>,
    password: Option<String>,
    method: String,
    headers: Vec<(String, String)>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Json {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema == "jsons";
        let path = if url.path.is_empty() { "/".to_string() } else { format!("/{}", url.path) };
        let method = url.get("method").unwrap_or("POST").to_uppercase();
        // Collect +header=value pairs
        let headers: Vec<(String, String)> = url.qsd.iter()
            .filter(|(k, _)| k.starts_with('+'))
            .map(|(k, v)| (k[1..].to_string(), v.clone()))
            .collect();
        Some(Self { host, port: url.port, path, secure, user: url.user.clone(), password: url.password.clone(), method, headers, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "JSON", service_url: None, setup_url: None, protocols: vec!["json", "jsons"], description: "Send a JSON notification to any HTTP endpoint.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Json {
    fn schemas(&self) -> &[&str] { &["json", "jsons"] }
    fn service_name(&self) -> &str { "JSON" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}{}", schema, self.host, port_str, self.path);
        let payload = json!({
            "version": "1.0",
            "title": ctx.title,
            "message": ctx.body,
            "type": ctx.notify_type.as_str(),
        });
        let client = build_client(self.verify_certificate)?;
        let mut req = match self.method.as_str() {
            "GET" => client.get(&url),
            "PUT" => client.put(&url),
            "PATCH" => client.patch(&url),
            "DELETE" => client.delete(&url),
            _ => client.post(&url),
        };
        req = req.header("User-Agent", APP_ID).json(&payload);
        if let (Some(u), Some(p)) = (&self.user, &self.password) { req = req.basic_auth(u, Some(p)); }
        for (k, v) in &self.headers { req = req.header(k.as_str(), v.as_str()); }
        let resp = req.send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
