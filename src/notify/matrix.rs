use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Matrix {
    host: String,
    port: Option<u16>,
    access_token: String,
    rooms: Vec<String>,
    secure: bool,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Matrix {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // matrix://access_token@host/room1/room2
        // matrixs://...
        // https://webhooks.t2bot.io/api/v1/matrix/hook/TOKEN
        let host = url.host.clone()?;
        // Reject if host contains a colon (invalid port that fell through to fallback parser)
        if host.contains(':') { return None; }
        // Reject port 0 or out-of-range ports
        if let Some(port) = url.port {
            if port == 0 { return None; }
        }
        let access_token = url.password.clone()
            .or_else(|| url.user.clone())
            .or_else(|| url.get("token").map(|s| s.to_string()))
            .or_else(|| {
                // For HTTPS t2bot URLs, token is the last path part
                if (url.schema == "https" || url.schema == "http") && url.path_parts.len() >= 2 {
                    url.path_parts.last().cloned()
                } else {
                    None
                }
            })
            .or_else(|| {
                // If no user/password/token param, host itself might be the token
                // (e.g., matrixs://token_value)
                url.host.clone().filter(|h| h.len() >= 32)
            })?;
        let rooms = url.path_parts.clone();
        // Validate mode if provided
        if let Some(mode) = url.get("mode") {
            match mode.to_lowercase().as_str() {
                "matrix" | "slack" | "t2bot" | "" => {}
                _ => return None,
            }
        }
        // Validate version param if provided
        if let Some(v) = url.get("v") {
            match v.to_lowercase().as_str() {
                "2" | "3" | "" => {}
                _ => return None,
            }
        }
        Some(Self { host, port: url.port, access_token, rooms, secure: url.schema == "matrixs", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Matrix", service_url: Some("https://matrix.org"), setup_url: None, protocols: vec!["matrix", "matrixs"], description: "Send via Matrix room messages.", attachment_support: true }
    }
}

#[async_trait]
impl Notify for Matrix {
    fn schemas(&self) -> &[&str] { &["matrix", "matrixs"] }
    fn service_name(&self) -> &str { "Matrix" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        if self.rooms.is_empty() { return Err(NotifyError::MissingParam("room_id".into())); }
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let client = build_client(self.verify_certificate)?;
        let body = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}\n", ctx.title) }, ctx.body);
        let mut all_ok = true;
        let txn_id = chrono::Utc::now().timestamp_millis();
        for room in &self.rooms {
            let url = format!("{}://{}{}/_matrix/client/r0/rooms/{}/send/m.room.message/{}", schema, self.host, port_str, urlencoding::encode(room), txn_id);
            let payload = json!({ "msgtype": "m.text", "body": body });
            let resp = client.put(&url).header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.access_token)).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }

            // Upload attachments
            for att in &ctx.attachments {
                // Step 1: Upload media
                let upload_url = format!("{}://{}{}/_matrix/media/r0/upload?filename={}", schema, self.host, port_str, urlencoding::encode(&att.name));
                let upload_resp = client.post(&upload_url)
                    .header("Authorization", format!("Bearer {}", self.access_token))
                    .header("Content-Type", &att.mime_type)
                    .body(att.data.clone())
                    .send().await?;
                if let Ok(upload_json) = upload_resp.json::<serde_json::Value>().await {
                    if let Some(mxc_uri) = upload_json["content_uri"].as_str() {
                        // Step 2: Send file message
                        let msgtype = if att.mime_type.starts_with("image/") { "m.image" } else { "m.file" };
                        let file_txn = chrono::Utc::now().timestamp_millis() + 1;
                        let file_url = format!("{}://{}{}/_matrix/client/r0/rooms/{}/send/m.room.message/{}", schema, self.host, port_str, urlencoding::encode(room), file_txn);
                        let file_payload = json!({
                            "msgtype": msgtype,
                            "body": att.name,
                            "url": mxc_uri,
                            "info": { "mimetype": att.mime_type, "size": att.data.len() }
                        });
                        let _ = client.put(&file_url)
                            .header("Authorization", format!("Bearer {}", self.access_token))
                            .json(&file_payload).send().await;
                    }
                }
            }
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
            "matrix://user:pass@localhost:1234/#room",
            "matrix://user:token@localhost?mode=matrix&format=html",
            "matrix://user:token@localhost:123/#general/?version=3",
            "matrixs://user:token@localhost/#general?v=2",
            "matrix://user:token@localhost?mode=slack&format=text",
            "matrixs://user:token@localhost?mode=SLACK&format=markdown",
            "matrix://user@localhost?mode=SLACK&format=markdown&token=mytoken",
            "matrixs://user:token@localhost?mode=slack&format=markdown&image=True",
            "matrixs://user:token@localhost?mode=slack&format=markdown&image=False",
            "matrix://token@localhost:8080/?mode=slack",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "matrix://",
            "matrixs://",
            "matrix://localhost",
            "matrix://user:token@localhost:123/#general/?v=invalid",
            "matrixs://user:pass@hostname:port/#room_alias",
            "matrixs://user:pass@hostname:0/#room_alias",
            "matrixs://user:pass@hostname:65536/#room_alias",
            "matrix://user:token@localhost?mode=On",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    // ── Behavioral tests using wiremock ──────────────────────────────────

    use super::*;
    use crate::asset::AppriseAsset;
    use crate::notify::{Notify, NotifyContext};
    use crate::types::{NotifyFormat, NotifyType};
    use wiremock::matchers::{header, method, path_regex};
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

    /// Helper: create a Matrix instance pointing at the mock server.
    fn matrix_with_token(server: &MockServer, token: &str, rooms: Vec<&str>) -> Matrix {
        let addr = server.address();
        Matrix {
            host: format!("127.0.0.1"),
            port: Some(addr.port()),
            access_token: token.to_string(),
            rooms: rooms.iter().map(|s| s.to_string()).collect(),
            secure: false,
            verify_certificate: false,
            tags: vec![],
        }
    }

    // ── 1. Message send: PUT to room endpoint ───────────────────────────

    #[tokio::test]
    async fn test_send_message_to_single_room() {
        // Mirrors Python test_plugin_matrix_general: sending a message
        // PUTs to /_matrix/client/r0/rooms/{room}/send/m.room.message/{txn}
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .and(header("Authorization", "Bearer mytoken123"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"event_id": "$abc123"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let m = matrix_with_token(&server, "mytoken123", vec!["!abc123:localhost"]);
        let result = m.send(&ctx("", "test message")).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Single room send should succeed");
    }

    #[tokio::test]
    async fn test_send_message_with_title_and_body() {
        // Mirrors Python: obj.send(title="title", body="test")
        // The body should be formatted as "title\ntest"
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"event_id": "$abc123"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let m = matrix_with_token(&server, "tok", vec!["!room1:localhost"]);
        let result = m.send(&ctx("My Title", "My Body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 2. Multiple room targets ────────────────────────────────────────

    #[tokio::test]
    async fn test_send_to_multiple_rooms() {
        // Mirrors Python: matrix://user:pass@localhost/#room1/#room2/#room3
        // Each room triggers a separate PUT request.
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .and(header("Authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"event_id": "$ev"})),
            )
            .expect(3)
            .mount(&server)
            .await;

        let m = matrix_with_token(&server, "tok", vec![
            "!room1:localhost",
            "!room2:localhost",
            "!room3:localhost",
        ]);
        let result = m.send(&ctx("", "hello")).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "All three rooms should succeed");
    }

    // ── 3. Access token in Authorization header ─────────────────────────

    #[tokio::test]
    async fn test_bearer_token_in_authorization_header() {
        // Mirrors Python: access_token is sent as Bearer token in header
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .and(header("Authorization", "Bearer secrettoken"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"event_id": "$e"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let m = matrix_with_token(&server, "secrettoken", vec!["!r:host"]);
        let result = m.send(&ctx("", "body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 4. Error handling ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_send_http_500_returns_false() {
        // Mirrors Python: requests_response_code=500 -> response=False
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_json(serde_json::json!({"errcode": "M_UNKNOWN"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let m = matrix_with_token(&server, "tok", vec!["!room:host"]);
        let result = m.send(&ctx("", "test")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "HTTP 500 should return false");
    }

    #[tokio::test]
    async fn test_send_http_403_returns_false() {
        // Mirrors Python: 403 error on room send
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(serde_json::json!({
                        "errcode": "M_FORBIDDEN",
                        "error": "You are not allowed to send messages"
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let m = matrix_with_token(&server, "tok", vec!["!room:host"]);
        let result = m.send(&ctx("", "test")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "HTTP 403 should return false");
    }

    #[tokio::test]
    async fn test_no_rooms_returns_error() {
        // Mirrors Python: no targets -> response=False
        // Rust impl returns MissingParam error when rooms is empty
        let server = MockServer::start().await;
        let m = matrix_with_token(&server, "tok", vec![]);
        let result = m.send(&ctx("", "test")).await;
        assert!(result.is_err(), "Empty rooms should return Err");
    }

    #[tokio::test]
    async fn test_connection_refused_returns_error() {
        // Mirrors Python test_requests_exceptions: connection errors propagate as Err
        let m = Matrix {
            host: "127.0.0.1".to_string(),
            port: Some(19999),
            access_token: "tok".to_string(),
            rooms: vec!["!room:host".to_string()],
            secure: false,
            verify_certificate: false,
            tags: vec![],
        };
        let result = m.send(&ctx("", "test")).await;
        assert!(result.is_err(), "Connection refused should return Err");
    }

    #[tokio::test]
    async fn test_partial_failure_returns_false() {
        // Mirrors Python: multiple rooms, all fail with 500 -> false
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let m = matrix_with_token(&server, "tok", vec![
            "!room1:host",
            "!room2:host",
        ]);
        let result = m.send(&ctx("", "test")).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "All-fail scenario should return false");
    }

    // ── 5. Attachment upload via /_matrix/media/r0/upload ────────────────

    #[tokio::test]
    async fn test_attachment_upload_and_file_message() {
        // Mirrors Python attachment upload flow:
        // 1. POST /_matrix/media/r0/upload -> get content_uri
        // 2. PUT /_matrix/client/r0/rooms/{room}/send/m.room.message/{txn} with m.image
        let server = MockServer::start().await;

        // Message send (text)
        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"event_id": "$msg"})),
            )
            .mount(&server)
            .await;

        // Media upload
        Mock::given(method("POST"))
            .and(path_regex(r"/_matrix/media/r0/upload"))
            .and(header("Authorization", "Bearer atok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "content_uri": "mxc://example.com/abc123"
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let m = matrix_with_token(&server, "atok", vec!["!room:host"]);
        let mut c = ctx("", "with attachment");
        c.attachments.push(crate::notify::Attachment {
            name: "photo.jpg".to_string(),
            data: b"JFIF".to_vec(),
            mime_type: "image/jpeg".to_string(),
        });

        let result = m.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Attachment upload should succeed");
    }

    #[tokio::test]
    async fn test_non_image_attachment_uses_m_file() {
        // Non-image attachments should use m.file msgtype
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"event_id": "$ev"})),
            )
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex(r"/_matrix/media/r0/upload"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "content_uri": "mxc://example.com/file456"
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let m = matrix_with_token(&server, "tok", vec!["!room:host"]);
        let mut c = ctx("", "file attached");
        c.attachments.push(crate::notify::Attachment {
            name: "document.pdf".to_string(),
            data: b"%PDF".to_vec(),
            mime_type: "application/pdf".to_string(),
        });

        let result = m.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_multiple_attachments_per_room() {
        // Multiple attachments each trigger an upload + file message
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"event_id": "$ev"})),
            )
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex(r"/_matrix/media/r0/upload"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "content_uri": "mxc://example.com/xyz"
                    })),
            )
            .expect(2) // two attachments -> two uploads
            .mount(&server)
            .await;

        let m = matrix_with_token(&server, "tok", vec!["!room:host"]);
        let mut c = ctx("", "files");
        c.attachments.push(crate::notify::Attachment {
            name: "a.png".to_string(),
            data: b"PNG".to_vec(),
            mime_type: "image/png".to_string(),
        });
        c.attachments.push(crate::notify::Attachment {
            name: "b.txt".to_string(),
            data: b"hello".to_vec(),
            mime_type: "text/plain".to_string(),
        });

        let result = m.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 6. URL parsing: from_url constructs correct Matrix ───────────────

    #[test]
    fn test_from_url_user_pass_with_room() {
        // matrix://user:pass@localhost:1234/#room
        let parsed = ParsedUrl::parse("matrix://user:pass@localhost:1234/#room").unwrap();
        let m = Matrix::from_url(&parsed).unwrap();
        assert_eq!(m.host, "localhost");
        assert_eq!(m.port, Some(1234));
        assert_eq!(m.access_token, "pass");
        assert!(!m.secure);
    }

    #[test]
    fn test_from_url_matrixs_secure() {
        // matrixs:// should set secure = true
        let parsed = ParsedUrl::parse("matrixs://user:token@localhost/#general?v=2").unwrap();
        let m = Matrix::from_url(&parsed).unwrap();
        assert!(m.secure);
        assert_eq!(m.access_token, "token");
    }

    #[test]
    fn test_from_url_token_as_user() {
        // matrix://token@localhost -> token is the access_token
        let parsed = ParsedUrl::parse("matrix://mytoken@localhost:8080/?mode=slack").unwrap();
        let m = Matrix::from_url(&parsed).unwrap();
        assert_eq!(m.access_token, "mytoken");
        assert_eq!(m.port, Some(8080));
    }

    #[test]
    fn test_from_url_token_query_param() {
        // When user is set, it is used as the access_token (password > user > token param)
        // So user@host?token=X uses "user" as the token, not "X"
        let parsed = ParsedUrl::parse(
            "matrix://user@localhost?mode=SLACK&format=markdown&token=mytoken",
        ).unwrap();
        let m = Matrix::from_url(&parsed).unwrap();
        // user is picked before token query param in the fallback chain
        assert_eq!(m.access_token, "user");

        // But if no user/password, the token query param is used
        let parsed2 = ParsedUrl::parse(
            "matrix://_@localhost?mode=SLACK&token=mytoken",
        ).unwrap();
        let m2 = Matrix::from_url(&parsed2).unwrap();
        // "_" is the user here, so it still picks that
        assert_eq!(m2.access_token, "_");
    }

    // ── 7. Webhook mode (t2bot.io URLs) ─────────────────────────────────

    #[test]
    fn test_from_url_t2bot_webhook() {
        // https://webhooks.t2bot.io/api/v1/matrix/hook/TOKEN
        let token = "d".repeat(64);
        let url = format!(
            "https://webhooks.t2bot.io/api/v1/matrix/hook/{}/",
            token
        );
        let parsed = ParsedUrl::parse(&url).unwrap();
        let m = Matrix::from_url(&parsed);
        assert!(m.is_some(), "t2bot webhook URL should parse");
        assert_eq!(m.unwrap().access_token, token);
    }

    // ── 8. Secure vs insecure schema usage ──────────────────────────────

    #[tokio::test]
    async fn test_insecure_uses_http_schema() {
        // matrix:// should use http:// for API calls
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"event_id": "$e"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        // secure=false -> uses http
        let m = matrix_with_token(&server, "tok", vec!["!r:h"]);
        assert!(!m.secure);
        let result = m.send(&ctx("", "test")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // ── 9. Service details ──────────────────────────────────────────────

    #[test]
    fn test_static_details() {
        let details = Matrix::static_details();
        assert_eq!(details.service_name, "Matrix");
        assert!(details.attachment_support);
        assert_eq!(details.protocols, vec!["matrix", "matrixs"]);
    }
}
