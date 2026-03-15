use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Nextcloud { host: String, port: Option<u16>, targets: Vec<String>, secure: bool, user: Option<String>, password: Option<String>, verify_certificate: bool, tags: Vec<String> }
impl Nextcloud {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        // Validate version if provided
        if let Some(ver) = url.get("version") {
            match ver.parse::<u32>() {
                Ok(v) if v >= 1 => {}
                _ => return None,
            }
        }
        Some(Self { host, port: url.port, targets, secure: url.schema == "nclouds", user: url.user.clone(), password: url.password.clone(), verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Nextcloud", service_url: Some("https://nextcloud.com"), setup_url: None, protocols: vec!["ncloud", "nclouds"], description: "Send Nextcloud notifications.", attachment_support: false } }
}
#[async_trait]
impl Notify for Nextcloud {
    fn schemas(&self) -> &[&str] { &["ncloud", "nclouds"] }
    fn service_name(&self) -> &str { "Nextcloud" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let url = format!("{}://{}{}/ocs/v2.php/apps/notifications/api/v2/admin_notifications/{}", schema, self.host, port_str, target);
            let params = [("shortMessage", ctx.title.as_str()), ("longMessage", ctx.body.as_str())];
            let mut req = client.post(&url).header("User-Agent", APP_ID).header("OCS-APIREQUEST", "true");
            if let (Some(u), Some(p)) = (&self.user, &self.password) { req = req.basic_auth(u, Some(p)); }
            let resp = req.form(&params).send().await?;
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
    use wiremock::matchers::{method, path, header};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "ncloud://localhost",
            "ncloud://localhost/admin",
            "ncloud://user@localhost/admin",
            "ncloud://user@localhost?to=user1,user2",
            "ncloud://user@localhost?to=user1,user2&version=20",
            "ncloud://user@localhost?to=user1,user2&version=21",
            "ncloud://user@localhost?to=user1&version=20&url_prefix=/abcd",
            "ncloud://user@localhost?to=user1&version=21&url_prefix=/abcd",
            "ncloud://user:pass@localhost/user1/user2",
            "ncloud://user:pass@localhost/#group1/#group2/#group1",
            "ncloud://user:pass@localhost:8080/admin",
            "nclouds://user:pass@localhost/admin",
            "nclouds://user:pass@localhost:8080/admin/",
            "nclouds://user:pass@localhost:8080/#group/",
            "ncloud://localhost:8080/admin?+HeaderKey=HeaderValue",
            "ncloud://user:pass@localhost:8083/user1/user2/user3",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "ncloud://:@/",
            "ncloud://",
            "nclouds://",
            "ncloud://user@localhost?to=user1,user2&version=invalid",
            "ncloud://user@localhost?to=user1,user2&version=0",
            "ncloud://user@localhost?to=user1,user2&version=-23",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    /// Build a Nextcloud instance pointing at the mock server.
    fn nextcloud_for_mock(server: &MockServer, user: &str, pass: &str, targets: &[&str]) -> Nextcloud {
        let addr = server.address();
        let target_path = targets.iter().map(|t| format!("/{}", t)).collect::<String>();
        let url_str = format!(
            "ncloud://{}:{}@{}:{}{}",
            user, pass, addr.ip(), addr.port(), target_path
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        Nextcloud::from_url(&parsed).unwrap()
    }

    fn default_ctx() -> NotifyContext {
        NotifyContext {
            title: "Test Title".into(),
            body: "Test Body".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_send_single_target_success() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/ocs/v2.php/apps/notifications/api/v2/admin_notifications/admin"))
            .and(header("User-Agent", APP_ID))
            .and(header("OCS-APIREQUEST", "true"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let nc = nextcloud_for_mock(&server, "user", "pass", &["admin"]);
        let result = nc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_with_basic_auth() {
        let server = MockServer::start().await;

        // base64("myuser:mypass") = "bXl1c2VyOm15cGFzcw=="
        Mock::given(method("POST"))
            .and(path("/ocs/v2.php/apps/notifications/api/v2/admin_notifications/target1"))
            .and(header("Authorization", "Basic bXl1c2VyOm15cGFzcw=="))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let nc = nextcloud_for_mock(&server, "myuser", "mypass", &["target1"]);
        let result = nc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_multiple_targets() {
        let server = MockServer::start().await;

        // Each target should get its own POST request
        for target in &["user1", "user2", "user3"] {
            let p = format!("/ocs/v2.php/apps/notifications/api/v2/admin_notifications/{}", target);
            Mock::given(method("POST"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .named(&format!("target {}", target))
                .mount(&server)
                .await;
        }

        let nc = nextcloud_for_mock(&server, "user", "pass", &["user1", "user2", "user3"]);
        let result = nc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_error_500_returns_false() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/ocs/v2.php/apps/notifications/api/v2/admin_notifications/admin"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let nc = nextcloud_for_mock(&server, "user", "pass", &["admin"]);
        let result = nc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_send_multiple_targets_one_fails() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/ocs/v2.php/apps/notifications/api/v2/admin_notifications/good"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .named("good target")
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/ocs/v2.php/apps/notifications/api/v2/admin_notifications/bad"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .named("bad target")
            .mount(&server)
            .await;

        let nc = nextcloud_for_mock(&server, "user", "pass", &["good", "bad"]);
        let result = nc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_send_no_targets_returns_true() {
        // With no targets, the loop body never executes, so all_ok stays true
        let server = MockServer::start().await;
        let addr = server.address();
        let url_str = format!("ncloud://{}:{}", addr.ip(), addr.port());
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let nc = Nextcloud::from_url(&parsed).unwrap();

        let result = nc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_connection_failure() {
        // Point at a port nothing listens on
        let url_str = "ncloud://user:pass@127.0.0.1:1/admin";
        let parsed = crate::utils::parse::ParsedUrl::parse(url_str).unwrap();
        let nc = Nextcloud::from_url(&parsed).unwrap();

        let result = nc.send(&default_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_form_params() {
        // Verify the form body contains shortMessage and longMessage
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/ocs/v2.php/apps/notifications/api/v2/admin_notifications/user1"))
            .and(wiremock::matchers::body_string_contains("shortMessage=Test+Title"))
            .and(wiremock::matchers::body_string_contains("longMessage=Test+Body"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let nc = nextcloud_for_mock(&server, "user", "pass", &["user1"]);
        let result = nc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_send_bizarre_status_code() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/ocs/v2.php/apps/notifications/api/v2/admin_notifications/admin"))
            .respond_with(ResponseTemplate::new(418))
            .expect(1)
            .mount(&server)
            .await;

        let nc = nextcloud_for_mock(&server, "user", "pass", &["admin"]);
        let result = nc.send(&default_ctx()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }
}
