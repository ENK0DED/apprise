use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Sfr { service_id: String, service_password: String, space_id: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Sfr {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let service_id = url.user.clone()?;
        let service_password = url.password.clone()?;
        let space_id = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { service_id, service_password, space_id, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SFR", service_url: Some("https://www.sfr.fr"), setup_url: None, protocols: vec!["sfr"], description: "Send SMS via SFR (France).", attachment_support: false } }
}
#[async_trait]
impl Notify for Sfr {
    fn schemas(&self) -> &[&str] { &["sfr"] }
    fn service_name(&self) -> &str { "SFR" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!({ "login": { "serviceId": self.service_id, "servicePassword": self.service_password, "spaceId": self.space_id, "lang": "fr" }, "to": target, "textMsg": msg });
            let resp = client.post("https://www.dmc.sfr-sh.fr/DmcWS/1.5.8/JsonService/MessagesUnitairesWS/addSingleCall").header("User-Agent", APP_ID).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            format!("sfr://service_id:service_password@{}/{}?from=MyApp&timeout=30", "0".repeat(3), "0".repeat(10)),
            format!("sfr://service_id:service_password@{}/{}?voice=laura8k&lang=en_US", "0".repeat(3), "0".repeat(10)),
            format!("sfr://service_id:service_password@{}/{}?media=SMS", "0".repeat(3), "0".repeat(10)),
            format!("sfr://service_id:service_password@{}/{}", "0".repeat(3), "0".repeat(10)),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "sfr://",
            "sfr://:@/",
            "sfr://:service_password",
            "sfr://testing:serv@ice_password",
            "sfr://testing:service_password@/5555555555",
            "sfr://testing:service_password@12345/",
            "sfr://:service_password@space_id/targets?media=TEST",
            "sfr://service_id:",
            "sfr://service_id:@",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_fields() {
        let url_str = format!(
            "sfr://srv:pwd@{}/{}",
            "1".repeat(8), "0".repeat(10),
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let sfr = Sfr::from_url(&parsed).unwrap();
        assert_eq!(sfr.service_id, "srv");
        assert_eq!(sfr.service_password, "pwd");
        assert_eq!(sfr.space_id, "1".repeat(8));
        assert_eq!(sfr.targets, vec!["0".repeat(10)]);
    }

    #[test]
    fn test_from_url_multiple_targets() {
        let url_str = format!(
            "sfr://444444:other_password@{}/{}/{}",
            "1".repeat(8), "6".repeat(10), "8".repeat(10),
        );
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        let sfr = Sfr::from_url(&parsed).unwrap();
        assert_eq!(sfr.service_id, "444444");
        assert_eq!(sfr.service_password, "other_password");
        assert_eq!(sfr.space_id, "1".repeat(8));
        assert_eq!(sfr.targets.len(), 2);
        assert!(sfr.targets.contains(&"6".repeat(10)));
        assert!(sfr.targets.contains(&"8".repeat(10)));
    }

    #[test]
    fn test_no_targets_fails() {
        let url_str = format!("sfr://service_id:service_password@{}/", "0".repeat(3));
        let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
        assert!(Sfr::from_url(&parsed).is_none());
    }

    #[test]
    fn test_no_service_id_fails() {
        let parsed = crate::utils::parse::ParsedUrl::parse("sfr://:service_password@12345/0959290404").unwrap();
        assert!(Sfr::from_url(&parsed).is_none());
    }

    #[test]
    fn test_no_password_fails() {
        let parsed = crate::utils::parse::ParsedUrl::parse("sfr://service_id@12345/0959290404").unwrap();
        assert!(Sfr::from_url(&parsed).is_none());
    }

    #[test]
    fn test_service_details() {
        let d = Sfr::static_details();
        assert_eq!(d.service_name, "SFR");
        assert!(d.protocols.contains(&"sfr"));
        assert!(!d.attachment_support);
    }
}
