use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

pub struct Mqtt { host: String, port: u16, topic: String, user: Option<String>, password: Option<String>, tags: Vec<String> }
impl Mqtt {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let port = url.port.unwrap_or(1883);
        let topic = url.path_parts.first().cloned().unwrap_or_else(|| "apprise".to_string());
        Some(Self { host, port, topic, user: url.user.clone(), password: url.password.clone(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "MQTT", service_url: None, setup_url: None, protocols: vec!["mqtt", "mqtts"], description: "Publish messages via MQTT.", attachment_support: false } }
}
#[async_trait]
impl Notify for Mqtt {
    fn schemas(&self) -> &[&str] { &["mqtt", "mqtts"] }
    fn service_name(&self) -> &str { "MQTT" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        use rumqttc::{AsyncClient, MqttOptions, QoS};
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut opts = MqttOptions::new("apprise", &self.host, self.port);
        opts.set_keep_alive(std::time::Duration::from_secs(5));
        if let (Some(u), Some(p)) = (&self.user, &self.password) {
            opts.set_credentials(u, p);
        }
        let (client, mut eventloop) = AsyncClient::new(opts, 10);
        client.publish(&self.topic, QoS::AtLeastOnce, false, msg.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))?;
        // Process events briefly to allow the publish to complete
        for _ in 0..5 {
            match eventloop.poll().await {
                Ok(_) => {},
                Err(_) => break,
            }
        }
        client.disconnect().await.map_err(|e| NotifyError::Other(e.to_string()))?;
        Ok(true)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::parse::ParsedUrl;

    #[test]
    fn test_valid_urls() {
        let valid_urls = vec![
            "mqtt://localhost",
            "mqtt://localhost/topic",
            "mqtts://localhost/topic",
            "mqtt://user:pass@localhost/topic",
        ];
        for url in &valid_urls {
            let parsed = ParsedUrl::parse(url);
            assert!(parsed.is_some(), "ParsedUrl::parse failed for: {}", url);
            let parsed = parsed.unwrap();
            assert!(
                Mqtt::from_url(&parsed).is_some(),
                "Mqtt::from_url returned None for valid URL: {}",
                url,
            );
        }
    }

    #[test]
    fn test_invalid_urls() {
        let invalid_urls = vec![
            "mqtt://",
        ];
        for url in &invalid_urls {
            let result = ParsedUrl::parse(url)
                .and_then(|p| Mqtt::from_url(&p));
            assert!(
                result.is_none(),
                "Mqtt::from_url should return None for: {}",
                url,
            );
        }
    }

    #[test]
    fn test_mqtt_default_port() {
        let parsed = ParsedUrl::parse("mqtt://localhost").unwrap();
        let m = Mqtt::from_url(&parsed).unwrap();
        assert_eq!(m.host, "localhost");
        assert_eq!(m.port, 1883);
    }

    #[test]
    fn test_mqtt_custom_port() {
        let parsed = ParsedUrl::parse("mqtt://localhost:1234").unwrap();
        let m = Mqtt::from_url(&parsed).unwrap();
        assert_eq!(m.port, 1234);
    }

    #[test]
    fn test_mqtt_topic_from_path() {
        let parsed = ParsedUrl::parse("mqtt://localhost/my_topic").unwrap();
        let m = Mqtt::from_url(&parsed).unwrap();
        assert_eq!(m.topic, "my_topic");
    }

    #[test]
    fn test_mqtt_default_topic() {
        let parsed = ParsedUrl::parse("mqtt://localhost").unwrap();
        let m = Mqtt::from_url(&parsed).unwrap();
        assert_eq!(m.topic, "apprise");
    }

    #[test]
    fn test_mqtt_user_password() {
        let parsed = ParsedUrl::parse("mqtt://myuser:mypass@localhost/topic").unwrap();
        let m = Mqtt::from_url(&parsed).unwrap();
        assert_eq!(m.user.as_deref(), Some("myuser"));
        assert_eq!(m.password.as_deref(), Some("mypass"));
    }

    #[test]
    fn test_mqtt_no_auth() {
        let parsed = ParsedUrl::parse("mqtt://localhost/topic").unwrap();
        let m = Mqtt::from_url(&parsed).unwrap();
        assert!(m.user.is_none());
        assert!(m.password.is_none());
    }

    #[test]
    fn test_mqtt_static_details() {
        let details = Mqtt::static_details();
        assert_eq!(details.service_name, "MQTT");
        assert_eq!(details.protocols, vec!["mqtt", "mqtts"]);
        assert!(!details.attachment_support);
    }
}
