use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Jellyfin { host: String, port: Option<u16>, apikey: String, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Jellyfin {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let apikey = url.user.clone()
            .or_else(|| url.password.clone())
            .or_else(|| url.get("apikey").map(|s| s.to_string()))
            .filter(|s| !s.is_empty())?;
        Some(Self { host, port: url.port, apikey, secure: url.schema == "jellyfins", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Jellyfin", service_url: Some("https://jellyfin.org"), setup_url: None, protocols: vec!["jellyfin", "jellyfins"], description: "Send notifications via Jellyfin.", attachment_support: false } }
}
#[async_trait]
impl Notify for Jellyfin {
    fn schemas(&self) -> &[&str] { &["jellyfin", "jellyfins"] }
    fn service_name(&self) -> &str { "Jellyfin" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/Notifications/Admin", schema, self.host, port_str);
        let payload = json!({ "Name": ctx.title, "Description": ctx.body, "ImageUrl": serde_json::Value::Null, "Url": serde_json::Value::Null });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).header("X-Emby-Authorization", format!("MediaBrowser Token=\"{}\"", self.apikey)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;
    use crate::notify::{Notify, NotifyContext};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "jellyfin://",
            "jellyfins://",
            "jellyfin://localhost",
            "jellyfin://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    fn jellyfin_for_mock(server: &MockServer, apikey: &str) -> super::Jellyfin {
        let addr = server.address();
        let url_str = format!("jellyfin://{}@{}:{}", apikey, addr.ip(), addr.port());
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        super::Jellyfin::from_url(&parsed).unwrap()
    }

    fn default_ctx() -> NotifyContext {
        NotifyContext {
            title: "Test Title".into(),
            body: "Test Body".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_send_success() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/Notifications/Admin"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let jf = jellyfin_for_mock(&server, "my-api-key");
        let ctx = default_ctx();
        let result = jf.send(&ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_verifies_json_payload() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/Notifications/Admin"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "Name": "My Title",
                "Description": "My Body",
                "ImageUrl": null,
                "Url": null
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let jf = jellyfin_for_mock(&server, "my-api-key");
        let ctx = NotifyContext {
            title: "My Title".into(),
            body: "My Body".into(),
            ..Default::default()
        };
        let result = jf.send(&ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_server_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/Notifications/Admin"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let jf = jellyfin_for_mock(&server, "my-api-key");
        let ctx = default_ctx();
        let result = jf.send(&ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_bizarre_status_code() {
        let server = MockServer::start().await;

        // wiremock doesn't support 999, use 418 as a non-standard error
        Mock::given(method("POST"))
            .and(path("/Notifications/Admin"))
            .respond_with(ResponseTemplate::new(418))
            .expect(1)
            .mount(&server)
            .await;

        let jf = jellyfin_for_mock(&server, "my-api-key");
        let ctx = default_ctx();
        let result = jf.send(&ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_auth_header() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/Notifications/Admin"))
            .and(wiremock::matchers::header(
                "X-Emby-Authorization",
                "MediaBrowser Token=\"my-api-key\"",
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let jf = jellyfin_for_mock(&server, "my-api-key");
        let ctx = default_ctx();
        let result = jf.send(&ctx).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "jellyfin://l2g@localhost",
            "jellyfins://l2g:password@localhost",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }
}
