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
}
