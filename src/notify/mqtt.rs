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
