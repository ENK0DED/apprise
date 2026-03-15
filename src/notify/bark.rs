use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Bark {
    host: String, port: Option<u16>, device_keys: Vec<String>, secure: bool,
    sound: Option<String>, level: Option<String>, group: Option<String>, icon: Option<String>,
    verify_certificate: bool, tags: Vec<String>,
}
impl Bark {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        if host.is_empty() { return None; }
        let mut device_keys: Vec<String> = url.path_parts.clone();
        // Support ?to= query param for device keys
        if let Some(to) = url.get("to") {
            device_keys.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        // Python allows empty device_keys (will just use the host as endpoint)
        let sound = url.get("sound").map(|s| s.to_string());
        let level = url.get("level").map(|s| s.to_string());
        let group = url.get("group").map(|s| s.to_string());
        let icon = url.get("icon").map(|s| s.to_string());
        Some(Self { host, port: url.port, device_keys, secure: url.schema == "barks", sound, level, group, icon, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Bark", service_url: Some("https://bark.day.app"), setup_url: None, protocols: vec!["bark", "barks"], description: "Send notifications to iOS devices via Bark.", attachment_support: false } }
}
#[async_trait]
impl Notify for Bark {
    fn schemas(&self) -> &[&str] { &["bark", "barks"] }
    fn service_name(&self) -> &str { "Bark" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for key in &self.device_keys {
            let url = format!("{}://{}{}/push", schema, self.host, port_str);
            let mut payload = json!({ "device_key": key, "title": ctx.title, "body": ctx.body });
            if let Some(ref s) = self.sound { payload["sound"] = json!(s); }
            if let Some(ref l) = self.level { payload["level"] = json!(l); }
            if let Some(ref g) = self.group { payload["group"] = json!(g); }
            if let Some(ref i) = self.icon { payload["icon"] = json!(i); }
            let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "bark://localhost",
            "bark://192.168.0.6:8081/device_key",
            "bark://user@192.168.0.6:8081/device_key",
            "bark://192.168.0.6:8081/device_key/?sound=invalid",
            "bark://192.168.0.6:8081/device_key/?sound=alarm",
            "bark://192.168.0.6:8081/device_key/?sound=NOiR.cAf",
            "bark://192.168.0.6:8081/device_key/?badge=100",
            "barks://192.168.0.6:8081/device_key/?badge=invalid",
            "barks://192.168.0.6:8081/device_key/?badge=-12",
            "bark://192.168.0.6:8081/device_key/?category=apprise",
            "bark://192.168.0.6:8081/device_key/?image=no",
            "bark://192.168.0.6:8081/device_key/?group=apprise",
            "bark://192.168.0.6:8081/device_key/?level=invalid",
            "bark://192.168.0.6:8081/?to=device_key",
            "bark://192.168.0.6:8081/device_key/?click=http://localhost",
            "bark://192.168.0.6:8081/device_key/?level=active",
            "bark://192.168.0.6:8081/device_key/?level=critical",
            "bark://192.168.0.6:8081/device_key/?level=critical&volume=10",
            "bark://192.168.0.6:8081/device_key/?level=critical&volume=invalid",
            "bark://192.168.0.6:8081/device_key/?level=critical&volume=11",
            "bark://192.168.0.6:8081/device_key/?level=critical&volume=-1",
            "bark://192.168.0.6:8081/device_key/?level=critical&volume=",
            "bark://user:pass@192.168.0.5:8086/device_key/device_key2/",
            "bark://192.168.0.7/device_key",
            "bark://192.168.0.6:8081/device_key/?icon=https://example.com/icon.png",
            "bark://192.168.0.6:8081/device_key/?icon=https://example.com/icon.png&image=no",
            "bark://192.168.0.6:8081/device_key/?call=1",
            "bark://192.168.0.6:8081/device_key/?call=1&sound=alarm&level=critical",
            "bark://192.168.0.6:8081/device_key/?format=markdown",
            "bark://192.168.0.6:8081/device_key/?format=text",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "bark://",
            "bark://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
