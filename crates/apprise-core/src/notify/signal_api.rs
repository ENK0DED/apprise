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
    use super::*;
    use crate::notify::registry::from_url;
    use crate::notify::{Attachment, NotifyContext};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    #[test]
    fn test_valid_urls() {
        let source = "1".repeat(11);
        let target = "3".repeat(11);
        let urls = vec![
            format!("signal://localhost:8080/{}/", source),
            format!("signal://localhost/{}/{}", source, target),
            format!("signals://localhost/{}/{}", source, target),
            format!("signal://localhost:8080/?from={}&to={},{}", source, "2".repeat(11), target),
            format!("signals://user:password@localhost/{}/{}", source, target),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    fn signal_for_mock(server: &MockServer, source: &str, targets: &[&str]) -> SignalApi {
        let addr = server.address();
        let targets_path = targets.iter().map(|t| format!("/{}", t)).collect::<String>();
        let url_str = format!("signal://{}:{}/{}{}", addr.ip(), addr.port(), source, targets_path);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        SignalApi::from_url(&parsed).unwrap()
    }

    #[tokio::test]
    async fn test_send_basic_success() {
        let server = MockServer::start().await;
        let source = format!("+{}", "1".repeat(11));
        let target = format!("+{}", "3".repeat(11));

        Mock::given(method("POST"))
            .and(path("/v2/send"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sig = signal_for_mock(&server, &source, &[&target]);
        let ctx = NotifyContext {
            title: "Title".into(),
            body: "Body".into(),
            ..Default::default()
        };
        let result = sig.send(&ctx).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_send_json_payload_structure() {
        let server = MockServer::start().await;
        let source = format!("+{}", "1".repeat(11));
        let target = format!("+{}", "3".repeat(11));

        Mock::given(method("POST"))
            .and(path("/v2/send"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "message": "My Title\nMy Body",
                "number": source,
                "recipients": [&target],
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sig = signal_for_mock(&server, &source, &[&target]);
        let ctx = NotifyContext {
            title: "My Title".into(),
            body: "My Body".into(),
            ..Default::default()
        };
        let result = sig.send(&ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_body_only_no_title() {
        let server = MockServer::start().await;
        let source = format!("+{}", "2".repeat(11));
        let target = format!("+{}", "4".repeat(11));

        Mock::given(method("POST"))
            .and(path("/v2/send"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "message": "Just body",
                "number": source,
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sig = signal_for_mock(&server, &source, &[&target]);
        let ctx = NotifyContext {
            body: "Just body".into(),
            ..Default::default()
        };
        let result = sig.send(&ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_multiple_recipients() {
        let server = MockServer::start().await;
        let source = format!("+{}", "1".repeat(11));
        let t1 = format!("+{}", "2".repeat(11));
        let t2 = format!("+{}", "3".repeat(11));

        Mock::given(method("POST"))
            .and(path("/v2/send"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "recipients": [&t1, &t2],
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sig = signal_for_mock(&server, &source, &[&t1, &t2]);
        let ctx = NotifyContext {
            body: "test".into(),
            ..Default::default()
        };
        let result = sig.send(&ctx).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_send_with_attachments() {
        let server = MockServer::start().await;
        let source = format!("+{}", "1".repeat(11));
        let target = format!("+{}", "3".repeat(11));

        Mock::given(method("POST"))
            .and(path("/v2/send"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "base64_attachments": [
                    base64::engine::general_purpose::STANDARD.encode(b"fake image data"),
                ],
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sig = signal_for_mock(&server, &source, &[&target]);
        let ctx = NotifyContext {
            body: "with attachment".into(),
            attachments: vec![Attachment {
                name: "test.gif".into(),
                data: b"fake image data".to_vec(),
                mime_type: "image/gif".into(),
            }],
            ..Default::default()
        };
        let result = sig.send(&ctx).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_send_no_attachments_no_base64_field() {
        let server = MockServer::start().await;
        let source = format!("+{}", "1".repeat(11));
        let target = format!("+{}", "3".repeat(11));

        // Capture the request to verify no base64_attachments field
        Mock::given(method("POST"))
            .and(path("/v2/send"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sig = signal_for_mock(&server, &source, &[&target]);
        let ctx = NotifyContext {
            body: "no attachments".into(),
            ..Default::default()
        };
        let result = sig.send(&ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_server_error() {
        let server = MockServer::start().await;
        let source = format!("+{}", "1".repeat(11));
        let target = format!("+{}", "4".repeat(11));

        Mock::given(method("POST"))
            .and(path("/v2/send"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let sig = signal_for_mock(&server, &source, &[&target]);
        let ctx = NotifyContext {
            body: "test".into(),
            ..Default::default()
        };
        let result = sig.send(&ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_non_standard_error() {
        let server = MockServer::start().await;
        let source = format!("+{}", "1".repeat(11));
        let target = format!("+{}", "4".repeat(11));

        Mock::given(method("POST"))
            .and(path("/v2/send"))
            .respond_with(ResponseTemplate::new(418))
            .expect(1)
            .mount(&server)
            .await;

        let sig = signal_for_mock(&server, &source, &[&target]);
        let ctx = NotifyContext {
            body: "test".into(),
            ..Default::default()
        };
        let result = sig.send(&ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_with_basic_auth() {
        let server = MockServer::start().await;
        let source = format!("+{}", "1".repeat(11));
        let target = format!("+{}", "3".repeat(11));
        let addr = server.address();

        let url_str = format!(
            "signal://myuser:mypass@{}:{}/{}/{}",
            addr.ip(), addr.port(), source, target
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let sig = SignalApi::from_url(&parsed).unwrap();

        Mock::given(method("POST"))
            .and(path("/v2/send"))
            .and(wiremock::matchers::header("Authorization", "Basic bXl1c2VyOm15cGFzcw=="))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let ctx = NotifyContext {
            body: "auth test".into(),
            ..Default::default()
        };
        let result = sig.send(&ctx).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_secure_vs_insecure() {
        let source = "1".repeat(11);
        let target = "3".repeat(11);

        let parsed = crate::utils::parse::ParsedUrl::parse(
            &format!("signal://localhost/{}/{}", source, target),
        ).unwrap();
        let sig = SignalApi::from_url(&parsed).unwrap();
        assert!(!sig.secure);

        let parsed = crate::utils::parse::ParsedUrl::parse(
            &format!("signals://localhost/{}/{}", source, target),
        ).unwrap();
        let sig = SignalApi::from_url(&parsed).unwrap();
        assert!(sig.secure);
    }

    #[test]
    fn test_from_query_params() {
        let source = "1".repeat(11);
        let t1 = "2".repeat(11);
        let t2 = "3".repeat(11);

        let url = format!("signal://localhost:8080/?from={}&to={},{}", source, t1, t2);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
        let sig = SignalApi::from_url(&parsed).unwrap();
        assert_eq!(sig.source, source);
        assert!(sig.targets.contains(&t1));
        assert!(sig.targets.contains(&t2));
    }
}
