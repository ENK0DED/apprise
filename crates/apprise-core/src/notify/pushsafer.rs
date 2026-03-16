use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use base64::Engine;
pub struct Pushsafer {
  privatekey: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Pushsafer {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let privatekey = url.host.clone()?;
    if privatekey.is_empty() {
      return None;
    }
    // Validate priority if provided
    if let Some(priority) = url.get("priority") {
      if !priority.is_empty() {
        match priority.to_lowercase().as_str() {
          "-2" | "-1" | "0" | "1" | "2" | "3" | "low" | "moderate" | "normal" | "high" | "emergency" | "confirmation" => {}
          _ => return None,
        }
      }
    }
    // Validate sound if provided
    if let Some(sound) = url.get("sound") {
      if !sound.is_empty() {
        // Sound can be a name or a number 0-62
        if let Ok(num) = sound.parse::<i32>() {
          if !(0..=62).contains(&num) {
            return None;
          }
        } else {
          // Named sounds
          let valid_sounds = [
            "",
            "none",
            "default",
            "device_default",
            "ok",
            "alarm",
            "alarm2",
            "alarm3",
            "ring",
            "ring2",
            "ring3",
            "bell",
            "bell2",
            "notification",
            "notification2",
            "positive",
            "positive2",
            "positive3",
            "positive4",
            "positive5",
            "positive6",
            "negative",
            "negative2",
            "failed",
            "failed2",
            "incoming",
            "incoming2",
            "incoming3",
            "incoming4",
            "incoming5",
            "incoming6",
            "incoming7",
            "incoming8",
            "incoming9",
            "incoming10",
            "doorbell",
            "doorbell2",
            "doorbell3",
            "knock",
            "knock2",
            "knock3",
            "knock4",
            "bike",
            "honk",
            "tada",
            "tada2",
            "cash",
            "cash2",
            "laser",
            "laser2",
            "laser3",
            "beep",
            "beep2",
            "magic",
            "magic2",
            "fireworks",
            "fireworks2",
            "whoops",
            "pirate",
          ];
          if !valid_sounds.contains(&sound.to_lowercase().as_str()) {
            return None;
          }
        }
      }
    }
    // Validate vibration if provided
    if let Some(vib) = url.get("vibration") {
      if !vib.is_empty() {
        let val: i32 = vib.parse().ok()?;
        if !(0..=3).contains(&val) {
          return None;
        }
      }
    }
    Some(Self { privatekey, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pushsafer",
      service_url: Some("https://www.pushsafer.com"),
      setup_url: None,
      protocols: vec!["psafer", "psafers"],
      description: "Send push notifications via Pushsafer.",
      attachment_support: true,
    }
  }
}
#[async_trait]
impl Notify for Pushsafer {
  fn schemas(&self) -> &[&str] {
    &["psafer", "psafers"]
  }
  fn service_name(&self) -> &str {
    "Pushsafer"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
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
    let image_attachments: Vec<_> = ctx.attachments.iter().filter(|att| att.mime_type.starts_with("image/")).take(3).collect();
    let pic_keys = ["p", "p2", "p3"];
    for (i, att) in image_attachments.iter().enumerate() {
      let b64 = base64::engine::general_purpose::STANDARD.encode(&att.data);
      let data_url = format!("data:{};base64,{}", att.mime_type, b64);
      params.push((pic_keys[i].into(), data_url));
    }
    let client = build_client(self.verify_certificate)?;
    let resp = client.post("https://www.pushsafer.com/api").header("User-Agent", APP_ID).form(&params).send().await?;
    if resp.status().is_success() {
      Ok(true)
    } else {
      Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::utils::parse::ParsedUrl;

  #[test]
  fn test_invalid_urls() {
    let urls: Vec<String> = vec![
      "psafer://:@/".into(),
      "psafer://".into(),
      "psafers://".into(),
      // Invalid priority
      format!("psafer://{}?priority=invalid", "f".repeat(20)),
      format!("psafer://{}?priority=25", "f".repeat(20)),
      // Invalid sound
      format!("psafer://{}?sound=invalid", "h".repeat(20)),
      format!("psafer://{}?sound=94000", "h".repeat(20)),
      // Invalid vibration
      format!("psafer://{}?vibration=invalid", "h".repeat(20)),
      format!("psafer://{}?vibration=25000", "h".repeat(20)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      format!("psafer://{}", "a".repeat(20)),
      format!("psafer://{}/12/24/53", "e".repeat(20)),
      format!("psafer://{}?to=12,24,53", "e".repeat(20)),
      format!("psafer://{}?priority=emergency", "f".repeat(20)),
      format!("psafer://{}?priority=-1", "f".repeat(20)),
      format!("psafer://{}?sound=ok", "g".repeat(20)),
      format!("psafers://{}?sound=14", "g".repeat(20)),
      format!("psafers://{}?vibration=1", "h".repeat(20)),
      format!("psafers://{}", "d".repeat(20)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let parsed = ParsedUrl::parse(&format!("psafer://{}", "a".repeat(20))).unwrap();
    let p = Pushsafer::from_url(&parsed).unwrap();
    assert_eq!(p.privatekey, "a".repeat(20));
  }

  #[test]
  fn test_static_details() {
    let details = Pushsafer::static_details();
    assert_eq!(details.service_name, "Pushsafer");
    assert_eq!(details.service_url, Some("https://www.pushsafer.com"));
    assert!(details.protocols.contains(&"psafer"));
    assert!(details.protocols.contains(&"psafers"));
    assert!(details.attachment_support);
  }

  #[test]
  fn test_priority_validation() {
    // Valid priorities
    for p in &["-2", "-1", "0", "1", "2", "3", "low", "moderate", "normal", "high", "emergency", "confirmation"] {
      let url = format!("psafer://{}?priority={}", "a".repeat(20), p);
      let parsed = ParsedUrl::parse(&url).unwrap();
      assert!(Pushsafer::from_url(&parsed).is_some(), "Priority {} should be valid", p);
    }
  }

  #[test]
  fn test_sound_validation() {
    // Valid numeric sounds
    for s in &["0", "14", "62"] {
      let url = format!("psafer://{}?sound={}", "a".repeat(20), s);
      let parsed = ParsedUrl::parse(&url).unwrap();
      assert!(Pushsafer::from_url(&parsed).is_some(), "Sound {} should be valid", s);
    }
    // Valid named sounds
    for s in &["ok", "alarm", "ring", "bell"] {
      let url = format!("psafer://{}?sound={}", "a".repeat(20), s);
      let parsed = ParsedUrl::parse(&url).unwrap();
      assert!(Pushsafer::from_url(&parsed).is_some(), "Sound {} should be valid", s);
    }
  }

  #[test]
  fn test_vibration_validation() {
    // Valid vibrations 0..3
    for v in 0..=3 {
      let url = format!("psafer://{}?vibration={}", "a".repeat(20), v);
      let parsed = ParsedUrl::parse(&url).unwrap();
      assert!(Pushsafer::from_url(&parsed).is_some(), "Vibration {} should be valid", v);
    }
    // Invalid vibration 4
    let url = format!("psafer://{}?vibration=4", "a".repeat(20));
    let parsed = ParsedUrl::parse(&url).unwrap();
    assert!(Pushsafer::from_url(&parsed).is_none());
  }
}
