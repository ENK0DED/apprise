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
        let host = url.host.clone()?;
        let access_token = url.password.clone().or_else(|| url.user.clone())?;
        let rooms = url.path_parts.clone();
        Some(Self { host, port: url.port, access_token, rooms, secure: url.schema == "matrixs", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Matrix", service_url: Some("https://matrix.org"), setup_url: None, protocols: vec!["matrix", "matrixs"], description: "Send via Matrix room messages.", attachment_support: false }
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
        }
        Ok(all_ok)
    }
}
