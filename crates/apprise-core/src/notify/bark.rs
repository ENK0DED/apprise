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
    use super::*;
    use crate::notify::registry::from_url;
    use crate::notify::NotifyContext;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    /// Helper: create a Bark instance pointing at the given mock server.
    fn bark_for_mock(server: &MockServer, device_key: &str, extra_params: &str) -> Bark {
        let addr = server.address();
        let sep = if extra_params.is_empty() { "" } else { "?" };
        let url_str = format!(
            "bark://{}:{}/{}{}{}",
            addr.ip(), addr.port(), device_key, sep, extra_params
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        Bark::from_url(&parsed).unwrap()
    }

    fn default_ctx() -> NotifyContext {
        NotifyContext {
            title: "Test Title".into(),
            body: "Test Body".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_send_basic_success() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/push"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "device_key": "mykey",
                "title": "Test Title",
                "body": "Test Body",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let bark = bark_for_mock(&server, "mykey", "");
        let result = bark.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_with_sound_group_level() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/push"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "device_key": "mykey",
                "title": "Test Title",
                "body": "Test Body",
                "sound": "alarm",
                "group": "mygroup",
                "level": "active",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let bark = bark_for_mock(&server, "mykey", "sound=alarm&group=mygroup&level=active");
        let result = bark.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_with_icon() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/push"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "device_key": "mykey",
                "title": "Test Title",
                "body": "Test Body",
                "icon": "https://example.com/icon.png",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let bark = bark_for_mock(&server, "mykey", "icon=https://example.com/icon.png");
        let result = bark.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_multiple_device_keys() {
        let server = MockServer::start().await;

        // Expect two POST requests, one per device key
        Mock::given(method("POST"))
            .and(path("/push"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;

        let addr = server.address();
        let url_str = format!(
            "bark://{}:{}/key1/key2/",
            addr.ip(), addr.port()
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let bark = Bark::from_url(&parsed).unwrap();

        let result = bark.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_no_device_keys_returns_true() {
        // When no device keys are specified, the loop body never executes
        // and all_ok remains true
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let addr = server.address();
        let url_str = format!("bark://{}:{}", addr.ip(), addr.port());
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let bark = Bark::from_url(&parsed).unwrap();

        let result = bark.send(&default_ctx()).await;
        assert!(result.is_ok());
        // No keys means no requests, all_ok stays true
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_error_500() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/push"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Server Error"))
            .expect(1)
            .mount(&server)
            .await;

        let bark = bark_for_mock(&server, "mykey", "");
        let result = bark.send(&default_ctx()).await;
        // Bark returns Ok(false) on non-success status, not Err
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_send_multiple_keys_partial_failure() {
        let server = MockServer::start().await;

        // First request succeeds, second fails
        // wiremock serves all matching requests with the same response,
        // so we use separate mounts with body matchers
        Mock::given(method("POST"))
            .and(path("/push"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "device_key": "goodkey",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/push"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "device_key": "badkey",
            })))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let addr = server.address();
        let url_str = format!(
            "bark://{}:{}/goodkey/badkey/",
            addr.ip(), addr.port()
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let bark = Bark::from_url(&parsed).unwrap();

        let result = bark.send(&default_ctx()).await;
        assert!(result.is_ok());
        // One key failed, so all_ok should be false
        assert_eq!(result.unwrap(), false);
    }

    #[test]
    fn test_secure_flag() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "bark://host/key",
        ).unwrap();
        let bark = Bark::from_url(&parsed).unwrap();
        assert!(!bark.secure);

        let parsed = crate::utils::parse::ParsedUrl::parse(
            "barks://host/key",
        ).unwrap();
        let bark = Bark::from_url(&parsed).unwrap();
        assert!(bark.secure);
    }

    #[test]
    fn test_to_query_param_adds_device_key() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "bark://host/?to=device_key",
        ).unwrap();
        let bark = Bark::from_url(&parsed).unwrap();
        assert!(bark.device_keys.contains(&"device_key".to_string()));
    }
}
