use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct SignalApi { host: String, port: Option<u16>, source: String, targets: Vec<String>, secure: bool, user: Option<String>, password: Option<String>, verify_certificate: bool, tags: Vec<String> }
impl SignalApi {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        // Source can come from path or ?from= query param
        let source = url.get("from").or_else(|| url.get("source")).map(|s| s.to_string())
            .or_else(|| url.path_parts.first().cloned())?;
        if source.is_empty() { return None; }
        // Validate source looks like a phone number (at least 10 digits)
        let source_digits: String = source.chars().filter(|c| c.is_ascii_digit()).collect();
        if source_digits.len() < 10 { return None; }
        let mut targets: Vec<String> = if url.get("from").is_some() || url.get("source").is_some() {
            // If source from query, all path parts are targets
            url.path_parts.clone()
        } else {
            url.path_parts.get(1..).unwrap_or(&[]).to_vec()
        };
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        Some(Self { host, port: url.port, source, targets, secure: url.schema == "signals", user: url.user.clone(), password: url.password.clone(), verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Signal API", service_url: Some("https://signal.org"), setup_url: None, protocols: vec!["signal", "signals"], description: "Send Signal messages via signal-cli REST API.", attachment_support: true } }
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
        let mut payload = json!({ "message": msg, "number": self.source, "recipients": self.targets });
        if !ctx.attachments.is_empty() {
            payload["base64_attachments"] = json!(ctx.attachments.iter().map(|att| {
                base64::engine::general_purpose::STANDARD.encode(&att.data)
            }).collect::<Vec<_>>());
        }
        let client = build_client(self.verify_certificate)?;
        let mut req = client.post(&url).header("User-Agent", APP_ID).json(&payload);
        if let (Some(u), Some(p)) = (&self.user, &self.password) { req = req.basic_auth(u, Some(p)); }
        let resp = req.send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "signal://localhost:8080/11111111111/",
            "signal://localhost:8082/+22222222222/@group.abcd/",
            "signals://localhost/11111111111/33333333333?format=markdown",
            "signal://localhost:8080/+11111111111/group.abcd/",
            "signal://localhost:8080/?from=11111111111&to=22222222222,33333333333",
            "signal://localhost:8080/?from=11111111111&to=22222222222,33333333333,555",
            "signal://localhost:8080/11111111111/22222222222/?from=33333333333",
            "signals://user@localhost/11111111111/33333333333",
            "signals://user:password@localhost/11111111111/33333333333",
            "signals://localhost/11111111111/33333333333/44444444444?batch=True",
            "signals://localhost/11111111111/33333333333/44444444444?status=True",
            "signal://localhost/11111111111/44444444444",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "signal://",
            "signal://:@/",
            "signal://localhost",
            "signal://localhost/123",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
