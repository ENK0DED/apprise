use async_trait::async_trait;
use base64::Engine;
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
    payload_extras: Vec<(String, String)>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Json {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema == "jsons";
        let path = if url.path.is_empty() { "/".to_string() } else { format!("/{}", url.path) };
        let method = url.get("method").unwrap_or("POST").to_uppercase();
        // Validate HTTP method
        match method.as_str() {
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" => {}
            _ => return None,
        }
        // Collect +header=value pairs
        let headers: Vec<(String, String)> = url.qsd.iter()
            .filter(|(k, _)| k.starts_with('+'))
            .map(|(k, v)| (k[1..].to_string(), v.clone()))
            .collect();
        // Collect :field=value payload extras
        let payload_extras: Vec<(String, String)> = url.qsd.iter()
            .filter(|(k, _)| k.starts_with(':'))
            .map(|(k, v)| (k[1..].to_string(), v.clone()))
            .collect();
        Some(Self { host, port: url.port, path, secure, user: url.user.clone(), password: url.password.clone(), method, headers, payload_extras, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "JSON", service_url: None, setup_url: None, protocols: vec!["json", "jsons"], description: "Send a JSON notification to any HTTP endpoint.", attachment_support: true }
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
        let mut payload = json!({
            "version": "1.0",
            "title": ctx.title,
            "message": ctx.body,
            "type": ctx.notify_type.as_str(),
        });
        // Apply payload extras (:field=value from URL)
        for (k, v) in &self.payload_extras {
            if v.is_empty() {
                // Empty value removes the field
                if let Some(obj) = payload.as_object_mut() { obj.remove(k); }
            } else if payload.get(k).is_some() {
                // Existing field: remap payload[k] to payload[v]
                if let Some(obj) = payload.as_object_mut() {
                    if let Some(val) = obj.remove(k) { obj.insert(v.clone(), val); }
                }
            } else {
                // New field: add it
                payload[k] = json!(v);
            }
        }
        if !ctx.attachments.is_empty() {
            payload["attachments"] = json!(ctx.attachments.iter().map(|att| json!({
                "filename": att.name,
                "base64": base64::engine::general_purpose::STANDARD.encode(&att.data),
                "mimetype": att.mime_type,
            })).collect::<Vec<_>>());
        }
        let client = build_client(self.verify_certificate)?;
        let mut req = match self.method.as_str() {
            "GET" => client.get(&url),
            "PUT" => client.put(&url),
            "PATCH" => client.patch(&url),
            "DELETE" => client.delete(&url),
            "HEAD" => client.head(&url),
            _ => client.post(&url),
        };
        req = req.header("User-Agent", APP_ID).json(&payload);
        if let (Some(u), Some(p)) = (&self.user, &self.password) { req = req.basic_auth(u, Some(p)); }
        for (k, v) in &self.headers { req = req.header(k.as_str(), v.as_str()); }
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
            "json://localhost",
            "json://user:pass@localhost",
            "json://user@localhost",
            "json://user@localhost?method=put",
            "json://user@localhost?method=get",
            "json://user@localhost?method=post",
            "json://user@localhost?method=head",
            "json://user@localhost?method=delete",
            "json://user@localhost?method=patch",
            "json://localhost:8080",
            "json://user:pass@localhost:8080",
            "jsons://localhost",
            "jsons://user:pass@localhost",
            "jsons://localhost:8080/path/",
            "json://localhost:8080/path?-ParamA=Value",
            "jsons://user:password@localhost:8080",
            "json://localhost:8080/path?+HeaderKey=HeaderValue",
            "json://user:pass@localhost:8083",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "json://:@/",
            "json://",
            "jsons://",
            "json://user@localhost?method=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    // ── Behavioral tests using wiremock ──────────────────────────────────

    use super::*;
    use base64::Engine;
    use crate::asset::AppriseAsset;
    use crate::notify::{Notify, NotifyContext};
    use crate::types::{NotifyFormat, NotifyType};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Helper: build a NotifyContext with sensible defaults.
    fn ctx(title: &str, body: &str) -> NotifyContext {
        NotifyContext {
            body: body.to_string(),
            title: title.to_string(),
            notify_type: NotifyType::Info,
            body_format: NotifyFormat::Text,
            attachments: vec![],
            interpret_escapes: false,
            interpret_emojis: false,
            tags: vec![],
            asset: AppriseAsset::default(),
        }
    }

    /// Helper: create a Json instance pointing at the mock server.
    fn json_for_mock(server: &MockServer, url_str: &str) -> Json {
        let parsed = ParsedUrl::parse(url_str).expect("parse test URL");
        Json::from_url(&parsed).expect("create Json from test URL")
    }

    // ── 1. Basic POST with JSON payload ─────────────────────────────────

    #[tokio::test]
    async fn test_basic_post_json_payload() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("json://localhost:{}", port);
        let j = json_for_mock(&server, &url);
        let result = j.send(&ctx("hello", "world")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["title"], "hello");
        assert_eq!(body["message"], "world");
        assert_eq!(body["type"], "info");
        assert_eq!(body["version"], "1.0");
    }

    // ── 2. GET method via ?method=GET ───────────────────────────────────

    #[tokio::test]
    async fn test_get_method() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("json://localhost:{}/?method=GET", port);
        let j = json_for_mock(&server, &url);
        let result = j.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_put_method() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("PUT"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("json://localhost:{}/?method=put", port);
        let j = json_for_mock(&server, &url);
        let result = j.send(&ctx("t", "b")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 3. Custom key mappings via :field=value ─────────────────────────

    #[tokio::test]
    async fn test_custom_key_mappings() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("GET"))
            .and(path("/command"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        // :message=msg remaps "message" to "msg"
        // :type= removes the "type" field
        // :test=value adds a new field "test"="value"
        let url = format!(
            "json://localhost:{}/command?:message=msg&:test=value&method=GET&:type=",
            port
        );
        let j = json_for_mock(&server, &url);
        let result = j.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();

        // title remains
        assert_eq!(body["title"], "title");
        // "message" was remapped to "msg"
        assert!(body.get("message").is_none());
        assert_eq!(body["msg"], "body");
        // "type" was removed (empty value)
        assert!(body.get("type").is_none());
        // new field "test" was added
        assert_eq!(body["test"], "value");
        // version remains
        assert_eq!(body["version"], "1.0");
        // Ensure NotifyType enum string does not leak
        let raw = String::from_utf8_lossy(&requests[0].body);
        assert!(!raw.contains("NotifyType."));
    }

    #[tokio::test]
    async fn test_type_field_present_when_not_removed() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("GET"))
            .and(path("/command"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!(
            "json://localhost:{}/command?:message=msg&method=GET",
            port
        );
        let j = json_for_mock(&server, &url);
        let result = j.send(&ctx("title", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        // type should still be present with value "info"
        assert_eq!(body["type"], "info");
        // NotifyType enum string must not leak
        let raw = String::from_utf8_lossy(&requests[0].body);
        assert!(!raw.contains("NotifyType."));
    }

    // ── 4. Correct endpoint URL construction ────────────────────────────

    #[tokio::test]
    async fn test_custom_port_and_path() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("POST"))
            .and(path("/some/path"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("json://localhost:{}/some/path", port);
        let j = json_for_mock(&server, &url);
        let result = j.send(&ctx("t", "b")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_https_url_construction() {
        // jsons:// should produce https:// in the constructed URL
        let parsed = ParsedUrl::parse("jsons://myhost:8443/api/notify").unwrap();
        let j = Json::from_url(&parsed).unwrap();
        assert!(j.secure);
        assert_eq!(j.host, "myhost");
        assert_eq!(j.port, Some(8443));
    }

    #[test]
    fn test_http_url_construction() {
        let parsed = ParsedUrl::parse("json://myhost/path").unwrap();
        let j = Json::from_url(&parsed).unwrap();
        assert!(!j.secure);
    }

    // ── 5. Attachment handling (base64 in JSON) ─────────────────────────

    #[tokio::test]
    async fn test_single_attachment_base64() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("json://localhost:{}", port);
        let j = json_for_mock(&server, &url);

        let file_data = b"GIF89a fake gif data";
        let mut c = ctx("title", "body");
        c.attachments.push(crate::notify::Attachment {
            name: "apprise-test.gif".to_string(),
            data: file_data.to_vec(),
            mime_type: "image/gif".to_string(),
        });

        let result = j.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let attachments = body["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["filename"], "apprise-test.gif");
        assert_eq!(attachments[0]["mimetype"], "image/gif");
        // Verify base64 encoding
        let expected_b64 = base64::engine::general_purpose::STANDARD.encode(file_data);
        assert_eq!(attachments[0]["base64"], expected_b64);
    }

    #[tokio::test]
    async fn test_multiple_attachments() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("json://localhost:{}", port);
        let j = json_for_mock(&server, &url);

        let mut c = ctx("title", "body");
        for i in 0..3 {
            c.attachments.push(crate::notify::Attachment {
                name: format!("file{}.gif", i),
                data: format!("data{}", i).into_bytes(),
                mime_type: "image/gif".to_string(),
            });
        }

        let result = j.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let attachments = body["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 3);
        for i in 0..3 {
            assert_eq!(attachments[i]["filename"], format!("file{}.gif", i));
        }
    }

    #[tokio::test]
    async fn test_no_attachments_omits_field() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("json://localhost:{}", port);
        let j = json_for_mock(&server, &url);
        let result = j.send(&ctx("t", "b")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(body.get("attachments").is_none());
    }

    // ── 6. Error handling ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_http_500_returns_error() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("json://localhost:{}", port);
        let j = json_for_mock(&server, &url);
        let result = j.send(&ctx("title", "body")).await;
        assert!(result.is_err(), "HTTP 500 should return Err");
    }

    #[tokio::test]
    async fn test_connection_refused_returns_error() {
        let parsed = ParsedUrl::parse("json://localhost:19999").unwrap();
        let j = Json::from_url(&parsed).unwrap();
        let result = j.send(&ctx("title", "body")).await;
        assert!(result.is_err(), "Connection refused should return Err");
    }

    // ── 7. User:password basic auth ─────────────────────────────────────

    #[tokio::test]
    async fn test_basic_auth_header() {
        let server = MockServer::start().await;
        let port = server.address().port();

        let expected_auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("user:pass")
        );
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("Authorization", expected_auth.as_str()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("json://user:pass@localhost:{}", port);
        let j = json_for_mock(&server, &url);
        let result = j.send(&ctx("t", "b")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_no_auth_when_no_credentials() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("json://localhost:{}", port);
        let j = json_for_mock(&server, &url);
        let result = j.send(&ctx("t", "b")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());

        // Verify no Authorization header was sent
        let requests = server.received_requests().await.unwrap();
        let has_auth = requests[0].headers.get("Authorization").is_some();
        assert!(!has_auth, "Should not send Authorization header without credentials");
    }

    // ── 8. Custom headers via +key=value ────────────────────────────────

    #[tokio::test]
    async fn test_custom_headers() {
        let server = MockServer::start().await;
        let port = server.address().port();

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // Custom headers via +key=value query params
        let url = format!("json://localhost:{}/?+HeaderKey=HeaderValue", port);
        let parsed = ParsedUrl::parse(&url).expect("parse URL with header");
        let j = Json::from_url(&parsed).expect("create Json with header");
        let result = j.send(&ctx("t", "b")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 9. Notification types ───────────────────────────────────────────

    #[tokio::test]
    async fn test_notify_type_values() {
        for (ntype, expected) in &[
            (NotifyType::Info, "info"),
            (NotifyType::Success, "success"),
            (NotifyType::Warning, "warning"),
            (NotifyType::Failure, "failure"),
        ] {
            let server = MockServer::start().await;
            let port = server.address().port();

            Mock::given(method("POST"))
                .and(path("/"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;

            let url = format!("json://localhost:{}", port);
            let j = json_for_mock(&server, &url);
            let mut c = ctx("t", "b");
            c.notify_type = ntype.clone();
            let result = j.send(&c).await;
            assert!(result.is_ok());
            assert!(result.unwrap());

            let requests = server.received_requests().await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(body["type"], *expected);
        }
    }
}
