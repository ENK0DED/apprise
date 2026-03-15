use async_trait::async_trait;
use base64::Engine;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Pushsafer { privatekey: String, verify_certificate: bool, tags: Vec<String> }
impl Pushsafer {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let privatekey = url.host.clone()?;
        if privatekey.is_empty() { return None; }
        // Validate priority if provided
        if let Some(priority) = url.get("priority") {
            if !priority.is_empty() {
                match priority.to_lowercase().as_str() {
                    "-2" | "-1" | "0" | "1" | "2" | "3"
                    | "low" | "moderate" | "normal" | "high" | "emergency" | "confirmation" => {}
                    _ => return None,
                }
            }
        }
        // Validate sound if provided
        if let Some(sound) = url.get("sound") {
            if !sound.is_empty() {
                // Sound can be a name or a number 0-62
                if let Ok(num) = sound.parse::<i32>() {
                    if num < 0 || num > 62 { return None; }
                } else {
                    // Named sounds
                    let valid_sounds = [
                        "", "none", "default", "device_default",
                        "ok", "alarm", "alarm2", "alarm3",
                        "ring", "ring2", "ring3", "bell", "bell2",
                        "notification", "notification2",
                        "positive", "positive2", "positive3",
                        "positive4", "positive5", "positive6",
                        "negative", "negative2",
                        "failed", "failed2",
                        "incoming", "incoming2", "incoming3", "incoming4",
                        "incoming5", "incoming6", "incoming7", "incoming8",
                        "incoming9", "incoming10",
                        "doorbell", "doorbell2", "doorbell3",
                        "knock", "knock2", "knock3", "knock4",
                        "bike", "honk", "tada", "tada2",
                        "cash", "cash2",
                        "laser", "laser2", "laser3",
                        "beep", "beep2",
                        "magic", "magic2",
                        "fireworks", "fireworks2",
                        "whoops", "pirate",
                    ];
                    if !valid_sounds.contains(&sound.to_lowercase().as_str()) { return None; }
                }
            }
        }
        // Validate vibration if provided
        if let Some(vib) = url.get("vibration") {
            if !vib.is_empty() {
                let val: i32 = vib.parse().ok()?;
                if val < 0 || val > 3 { return None; }
            }
        }
        Some(Self { privatekey, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Pushsafer", service_url: Some("https://www.pushsafer.com"), setup_url: None, protocols: vec!["psafer", "psafers"], description: "Send push notifications via Pushsafer.", attachment_support: true } }
}
#[async_trait]
impl Notify for Pushsafer {
    fn schemas(&self) -> &[&str] { &["psafer", "psafers"] }
    fn service_name(&self) -> &str { "Pushsafer" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let mut params: Vec<(String, String)> = vec![
            ("k".into(), self.privatekey.clone()),
            ("t".into(), ctx.title.clone()),
            ("m".into(), ctx.body.clone()),
            ("d".into(), "a".into()),
            ("s".into(), "11".into()),
            ("v".into(), "1".into()),
        ];
        // Attach up to 3 image attachments as data URLs (p, p2, p3)
        let image_attachments: Vec<_> = ctx.attachments.iter()
            .filter(|att| att.mime_type.starts_with("image/"))
            .take(3)
            .collect();
        let pic_keys = ["p", "p2", "p3"];
        for (i, att) in image_attachments.iter().enumerate() {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&att.data);
            let data_url = format!("data:{};base64,{}", att.mime_type, b64);
            params.push((pic_keys[i].into(), data_url));
        }
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://www.pushsafer.com/api").header("User-Agent", APP_ID).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "psafer://aaaaaaaaaaaaaaaaaaaa",
            "psafer://bbbbbbbbbbbbbbbbbbbb",
            "psafer://cccccccccccccccccccc",
            "psafers://dddddddddddddddddddd",
            "psafer://eeeeeeeeeeeeeeeeeeee",
            "psafer://eeeeeeeeeeeeeeeeeeee/12/24/53",
            "psafer://eeeeeeeeeeeeeeeeeeee?to=12,24,53",
            "psafer://ffffffffffffffffffff?priority=emergency",
            "psafer://ffffffffffffffffffff?priority=-1",
            "psafer://gggggggggggggggggggg?sound=ok",
            "psafers://gggggggggggggggggggg?sound=14",
            "psafers://hhhhhhhhhhhhhhhhhhhh?vibration=1",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "psafer://:@/",
            "psafer://",
            "psafers://",
            "psafer://ffffffffffffffffffff?priority=invalid",
            "psafer://ffffffffffffffffffff?priority=25",
            "psafer://hhhhhhhhhhhhhhhhhhhh?sound=invalid",
            "psafer://hhhhhhhhhhhhhhhhhhhh?sound=94000",
            "psafer://hhhhhhhhhhhhhhhhhhhh?vibration=invalid",
            "psafer://hhhhhhhhhhhhhhhhhhhh?vibration=25000",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
