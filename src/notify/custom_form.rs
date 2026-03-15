use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Form {
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

impl Form {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema == "forms";
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
        ServiceDetails { service_name: "Form", service_url: None, setup_url: None, protocols: vec!["form", "forms"], description: "Send a form-encoded notification to any HTTP endpoint.", attachment_support: true }
    }
}

#[async_trait]
impl Notify for Form {
    fn schemas(&self) -> &[&str] { &["form", "forms"] }
    fn service_name(&self) -> &str { "Form" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}{}", schema, self.host, port_str, self.path);
        let params = [("title", ctx.title.as_str()), ("message", ctx.body.as_str()), ("type", ctx.notify_type.as_str())];
        let client = build_client(self.verify_certificate)?;
        let mut req = match self.method.as_str() {
            "GET" => client.get(&url),
            "PUT" => client.put(&url),
            "PATCH" => client.patch(&url),
            "DELETE" => client.delete(&url),
            "HEAD" => client.head(&url),
            _ => client.post(&url),
        };
        if !ctx.attachments.is_empty() {
            // Use multipart form when attachments are present
            let mut form = reqwest::multipart::Form::new()
                .text("title", ctx.title.clone())
                .text("message", ctx.body.clone())
                .text("type", ctx.notify_type.as_str().to_string());
            for (i, att) in ctx.attachments.iter().enumerate() {
                let field_name = format!("file{:02}", i + 1);
                let part = reqwest::multipart::Part::bytes(att.data.clone())
                    .file_name(att.name.clone())
                    .mime_str(&att.mime_type)
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()).file_name(att.name.clone()));
                form = form.part(field_name, part);
            }
            req = req.header("User-Agent", APP_ID).multipart(form);
        } else {
            req = req.header("User-Agent", APP_ID).form(&params);
        }
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
            "form://localhost",
            "form://user:pass@localhost",
            "form://user@localhost",
            "form://user@localhost?method=put",
            "form://user@localhost?method=get",
            "form://user@localhost?method=post",
            "form://user@localhost?method=head",
            "form://user@localhost?method=delete",
            "form://user@localhost?method=patch",
            "form://localhost:8080?:key=value&:key2=value2",
            "form://localhost:8080",
            "form://user:pass@localhost:8080",
            "forms://localhost",
            "forms://user:pass@localhost",
            "forms://localhost:8080/path/",
            "forms://user:password@localhost:8080",
            "form://localhost:8080/path?-ParamA=Value",
            "form://localhost:8080/path?+HeaderKey=HeaderValue",
            "form://user:pass@localhost:8083",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "form://:@/",
            "form://",
            "forms://",
            "form://user@localhost?method=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
