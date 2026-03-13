use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Xml {
    host: String,
    port: Option<u16>,
    path: String,
    secure: bool,
    user: Option<String>,
    password: Option<String>,
    headers: Vec<(String, String)>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Xml {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema == "xmls";
        let path = if url.path.is_empty() { "/".to_string() } else { format!("/{}", url.path) };
        let headers: Vec<(String, String)> = url.qsd.iter()
            .filter(|(k, _)| k.starts_with('+'))
            .map(|(k, v)| (k[1..].to_string(), v.clone()))
            .collect();
        Some(Self { host, port: url.port, path, secure, user: url.user.clone(), password: url.password.clone(), headers, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "XML", service_url: None, setup_url: None, protocols: vec!["xml", "xmls"], description: "Send an XML notification to any HTTP endpoint.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Xml {
    fn schemas(&self) -> &[&str] { &["xml", "xmls"] }
    fn service_name(&self) -> &str { "XML" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}{}", schema, self.host, port_str, self.path);
        let body = format!(
            "<?xml version='1.0' encoding='UTF-8'?><notification><version>1.0</version><title>{}</title><message>{}</message><type>{}</type></notification>",
            xml_escape(&ctx.title), xml_escape(&ctx.body), ctx.notify_type.as_str()
        );
        let client = build_client(self.verify_certificate)?;
        let mut req = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/xml").body(body);
        if let (Some(u), Some(p)) = (&self.user, &self.password) { req = req.basic_auth(u, Some(p)); }
        for (k, v) in &self.headers { req = req.header(k.as_str(), v.as_str()); }
        let resp = req.send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
