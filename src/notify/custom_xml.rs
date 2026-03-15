use async_trait::async_trait;
use base64::Engine;
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
    method: String,
    headers: Vec<(String, String)>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Xml {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema == "xmls";
        let path = if url.path.is_empty() { "/".to_string() } else { format!("/{}", url.path) };
        let method = url.get("method").unwrap_or("POST").to_uppercase();
        // Validate HTTP method
        match method.as_str() {
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" => {}
            _ => return None,
        }
        let headers: Vec<(String, String)> = url.qsd.iter()
            .filter(|(k, _)| k.starts_with('+'))
            .map(|(k, v)| (k[1..].to_string(), v.clone()))
            .collect();
        Some(Self { host, port: url.port, path, secure, user: url.user.clone(), password: url.password.clone(), method, headers, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "XML", service_url: None, setup_url: None, protocols: vec!["xml", "xmls"], description: "Send an XML notification to any HTTP endpoint.", attachment_support: true }
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
        let mut body = format!(
            "<?xml version='1.0' encoding='UTF-8'?><notification><version>1.0</version><title>{}</title><message>{}</message><type>{}</type>",
            xml_escape(&ctx.title), xml_escape(&ctx.body), ctx.notify_type.as_str()
        );
        if !ctx.attachments.is_empty() {
            body.push_str("<attachments>");
            for att in &ctx.attachments {
                body.push_str(&format!(
                    "<attachment><filename>{}</filename><mimetype>{}</mimetype><base64>{}</base64></attachment>",
                    xml_escape(&att.name),
                    xml_escape(&att.mime_type),
                    base64::engine::general_purpose::STANDARD.encode(&att.data),
                ));
            }
            body.push_str("</attachments>");
        }
        body.push_str("</notification>");
        let client = build_client(self.verify_certificate)?;
        let mut req = match self.method.as_str() {
            "GET" => client.get(&url),
            "PUT" => client.put(&url),
            "PATCH" => client.patch(&url),
            "DELETE" => client.delete(&url),
            "HEAD" => client.head(&url),
            _ => client.post(&url),
        };
        req = req.header("User-Agent", APP_ID).header("Content-Type", "application/xml").body(body);
        if let (Some(u), Some(p)) = (&self.user, &self.password) { req = req.basic_auth(u, Some(p)); }
        for (k, v) in &self.headers { req = req.header(k.as_str(), v.as_str()); }
        let resp = req.send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "xml://localhost",
            "xml://user@localhost",
            "xml://user:pass@localhost",
            "xml://user@localhost?method=put",
            "xml://user@localhost?method=get",
            "xml://user@localhost?method=post",
            "xml://user@localhost?method=head",
            "xml://user@localhost?method=delete",
            "xml://user@localhost?method=patch",
            "xml://localhost:8080",
            "xml://user:pass@localhost:8080",
            "xmls://localhost",
            "xmls://user:pass@localhost",
            "xml://user@localhost:8080/path/",
            "xmls://localhost:8080/path/",
            "xmls://user:pass@localhost:8080",
            "xml://localhost:8080/path?-ParamA=Value",
            "xml://localhost:8080/path?+HeaderKey=HeaderValue",
            "xml://user:pass@localhost:8083",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "xml://:@/",
            "xml://",
            "xmls://",
            "xml://user@localhost?method=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    use crate::notify::{Notify, NotifyContext, Attachment};
    use crate::types::{NotifyType, NotifyFormat};
    use crate::asset::AppriseAsset;
    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{method, path, header};

    fn make_ctx(body: &str, title: &str) -> NotifyContext {
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

    #[tokio::test]
    async fn test_xml_basic_post() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("Content-Type", "application/xml"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("xml://localhost:{}", server.address().port());
        let svc = from_url(&url).unwrap();
        let ctx = make_ctx("test body", "test title");
        let result = svc.send(&ctx).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_xml_get_method() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("xml://localhost:{}?method=GET", server.address().port());
        let svc = from_url(&url).unwrap();
        let ctx = make_ctx("body", "title");
        let result = svc.send(&ctx).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_xml_custom_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/notify"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("xml://localhost:{}/api/notify", server.address().port());
        let svc = from_url(&url).unwrap();
        let ctx = make_ctx("body", "title");
        let result = svc.send(&ctx).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_xml_http_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("xml://localhost:{}", server.address().port());
        let svc = from_url(&url).unwrap();
        let ctx = make_ctx("body", "title");
        let result = svc.send(&ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_xml_with_attachments_base64() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("xml://localhost:{}", server.address().port());
        let svc = from_url(&url).unwrap();
        let mut ctx = make_ctx("body", "title");
        ctx.attachments.push(Attachment {
            name: "test.txt".to_string(),
            data: b"hello world".to_vec(),
            mime_type: "text/plain".to_string(),
        });
        let result = svc.send(&ctx).await.unwrap();
        assert!(result);
    }
}
