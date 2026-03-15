use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct NextcloudTalk { host: String, port: Option<u16>, user: String, password: String, rooms: Vec<String>, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl NextcloudTalk {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        let mut rooms = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            rooms.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        Some(Self { host, port: url.port, user, password, rooms, secure: url.schema == "nctalks", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Nextcloud Talk", service_url: Some("https://nextcloud.com"), setup_url: None, protocols: vec!["nctalk", "nctalks"], description: "Send Nextcloud Talk messages.", attachment_support: false } }
}
#[async_trait]
impl Notify for NextcloudTalk {
    fn schemas(&self) -> &[&str] { &["nctalk", "nctalks"] }
    fn service_name(&self) -> &str { "Nextcloud Talk" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for room in &self.rooms {
            let url = format!("{}://{}{}/ocs/v2.php/apps/spreed/api/v1/chat/{}", schema, self.host, port_str, room);
            let params = [("message", msg.as_str())];
            let resp = client.post(&url).header("User-Agent", APP_ID).header("OCS-APIREQUEST", "true").basic_auth(&self.user, Some(&self.password)).form(&params).send().await?;
            if !resp.status().is_success() { all_ok = false; }
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
            "nctalk://user:pass@localhost",
            "nctalk://user:pass@localhost/roomid1/roomid2",
            "nctalk://user:pass@localhost:8080/roomid",
            "nctalk://user:pass@localhost:8080/roomid?url_prefix=/prefix",
            "nctalks://user:pass@localhost/roomid",
            "nctalks://user:pass@localhost:8080/roomid/",
            "nctalk://user:pass@localhost:8080/roomid?+HeaderKey=HeaderValue",
            "nctalk://user:pass@localhost:8083/roomid1/roomid2/roomid3",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "nctalk://:@/",
            "nctalk://",
            "nctalks://",
            "nctalk://localhost",
            "nctalk://localhost/roomid",
            "nctalk://user@localhost/roomid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
