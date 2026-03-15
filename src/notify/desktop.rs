use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

pub struct Desktop { app_name: String, tags: Vec<String> }
impl Desktop {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let app_name = url.host.clone().unwrap_or_else(|| "Apprise".to_string());
        Some(Self { app_name, tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Desktop Notification", service_url: None, setup_url: None, protocols: vec!["dbus", "kde", "qt", "glib", "gnome"], description: "Send desktop notifications.", attachment_support: false } }
}
#[async_trait]
impl Notify for Desktop {
    fn schemas(&self) -> &[&str] { &["dbus", "kde", "qt", "glib", "gnome"] }
    fn service_name(&self) -> &str { "Desktop Notification" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        #[cfg(feature = "desktop")]
        {
            use notify_rust::Notification;
            Notification::new()
                .appname(&self.app_name)
                .summary(&ctx.title)
                .body(&ctx.body)
                .show()
                .map_err(|e| NotifyError::Other(e.to_string()))?;
            return Ok(true);
        }
        #[cfg(not(feature = "desktop"))]
        {
            Err(NotifyError::Other("Desktop notifications not compiled in. Build with --features desktop".into()))
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::parse::ParsedUrl;

    #[test]
    fn test_valid_urls() {
        let valid_urls = vec![
            "dbus://MyApp",
            "kde://MyApp",
            "gnome://MyApp",
        ];
        for url in &valid_urls {
            let parsed = ParsedUrl::parse(url);
            assert!(parsed.is_some(), "ParsedUrl::parse failed for: {}", url);
            let parsed = parsed.unwrap();
            assert!(
                Desktop::from_url(&parsed).is_some(),
                "Desktop::from_url returned None for valid URL: {}",
                url,
            );
        }
    }
}
