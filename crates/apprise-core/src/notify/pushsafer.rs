use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_list, split_path, unquote};
use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;

/// Named sounds (`PUSHSAFER_SOUND_MAP`), matched in this order by prefix.
const PUSHSAFER_SOUND_MAP: &[(&str, i64)] = &[
  ("silent", 0),
  ("ahem", 1),
  ("applause", 2),
  ("arrow", 3),
  ("baby", 4),
  ("bell", 5),
  ("bicycle", 6),
  ("bike", 6),
  ("boing", 7),
  ("buzzer", 8),
  ("camera", 9),
  ("carhorn", 10),
  ("horn", 10),
  ("cashregister", 11),
  ("chime", 12),
  ("creakydoor", 13),
  ("cuckooclock", 14),
  ("cuckoo", 14),
  ("disconnect", 15),
  ("dog", 16),
  ("doorbell", 17),
  ("fanfare", 18),
  ("gunshot", 19),
  ("honk", 20),
  ("jawharp", 21),
  ("morse", 22),
  ("electric", 23),
  ("radiotuner", 24),
  ("sirens", 25),
  ("militarytrumpets", 26),
  ("military", 26),
  ("trumpets", 26),
  ("ufo", 27),
  ("whahwhah", 28),
  ("whah", 42),
  ("goodye", 29),
  ("hello", 30),
  ("no", 31),
  ("okay", 32),
  ("ok", 32),
  ("ooohhhweee", 33),
  ("warn", 34),
  ("warning", 34),
  ("welcome", 35),
  ("yeah", 36),
  ("yes", 37),
  ("beep", 38),
  ("beep1", 38),
  ("weee", 39),
  ("wee", 39),
  ("cutinout", 40),
  ("flickglass", 41),
  ("laser", 43),
  ("windchime", 44),
  ("echo", 45),
  ("zipper", 46),
  ("hihat", 47),
  ("beep2", 48),
  ("beep3", 49),
  ("beep4", 50),
  ("alarmarmed", 51),
  ("armed", 51),
  ("alarmdisarmed", 52),
  ("disarmed", 52),
  ("backupready", 53),
  ("dooropen", 55),
  ("dopen", 55),
  ("doorclosed", 54),
  ("dclosed", 54),
  ("windowopen", 57),
  ("wopen", 57),
  ("windowclosed", 56),
  ("wclosed", 56),
  ("lighton", 58),
  ("lon", 58),
  ("lightoff", 59),
  ("loff", 59),
  ("doorbellrang", 60),
];

/// Named priorities (`PUSHSAFER_PRIORITY_MAP`).
const PUSHSAFER_PRIORITY_MAP: &[(&str, i64)] = &[("low", -2), ("medium", -1), ("normal", 0), ("high", 1), ("emergency", 2)];

/// Accepted vibrations (`PUSHSAFER_VIBRATIONS`).
const PUSHSAFER_VIBRATIONS: &[i64] = &[1, 2, 3];

/// Form fields carrying up to three pictures per request.
const PICTURE_PARAMETER: &[&str] = &["p", "p2", "p3"];

/// The recipient used when no target is given: every device.
const PUSHSAFER_SEND_TO_ALL: &str = "a";

/// The icon every message uses.
const DEFAULT_PUSHSAFER_ICON: &str = "25";

pub struct Pushsafer {
  privatekey: String,
  targets: Vec<String>,
  /// Parsed and validated as upstream does; Pushsafer receives no priority
  #[allow(dead_code)]
  priority: Option<i64>,
  sound: Option<i64>,
  vibration: Option<i64>,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// An integer, or a name matched by prefix in `map`; `None` when the
/// value is absent, `Err` when it is invalid.
fn int_or_name(value: Option<&str>, map: &[(&str, i64)]) -> Result<Option<i64>, ()> {
  let Some(value) = value else { return Ok(None) };
  let parsed = match value.trim().parse::<i64>() {
    Ok(n) => n,
    Err(_) => {
      let name = value.trim().to_lowercase();
      if name.is_empty() {
        return Err(());
      }
      map.iter().find(|(k, _)| k.starts_with(&name)).map(|(_, v)| *v).ok_or(())?
    }
  };
  if map.iter().any(|(_, v)| *v == parsed) { Ok(Some(parsed)) } else { Err(()) }
}

impl Pushsafer {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let qsd = |k: &str| url.get(k).filter(|v| !v.is_empty());

    let Ok(priority) = int_or_name(qsd("priority"), PUSHSAFER_PRIORITY_MAP) else {
      tracing::warn!("An invalid PushSafer priority was specified.");
      return None;
    };
    let Ok(sound) = int_or_name(qsd("sound"), PUSHSAFER_SOUND_MAP) else {
      tracing::warn!("An invalid PushSafer sound was specified.");
      return None;
    };
    // 0 passes, as upstream only checks a non-zero vibration
    let vibration = match qsd("vibration").map(|v| v.trim().parse::<i64>()) {
      None => None,
      Some(Ok(v)) if v == 0 || PUSHSAFER_VIBRATIONS.contains(&v) => Some(v),
      Some(_) => {
        tracing::warn!("An invalid PushSafer vibration was specified.");
        return None;
      }
    };

    let Some(privatekey) = url.host.as_deref().map(unquote).and_then(|k| k.split_whitespace().next().map(str::to_string)) else {
      tracing::warn!("An invalid PushSafer Private Key was specified.");
      return None;
    };

    let mut targets = split_path(url);
    if let Some(to) = qsd("to") {
      targets.push(to.to_string());
    }
    let mut targets = parse_list(targets, true, true);
    if targets.is_empty() {
      targets.push(PUSHSAFER_SEND_TO_ALL.to_string());
    }

    Some(Self {
      privatekey,
      targets,
      priority,
      sound,
      vibration,
      secure: url.schema == "psafers",
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pushsafer",
      service_url: Some("https://www.pushsafer.com/"),
      setup_url: Some("https://appriseit.com/services/pushsafer/"),
      protocols: vec!["psafer", "psafers"],
      description: "Send push notifications via Pushsafer.",
      attachment_support: true,
    }
  }

  /// POST one form; Pushsafer reports success with `"status": 1`.
  async fn post(&self, client: &reqwest::Client, mut form: Vec<(&str, String)>) -> bool {
    let url = format!("{}/api", service_base(if self.secure { "https://www.pushsafer.com" } else { "http://www.pushsafer.com" }));
    form.push(("k", self.privatekey.clone()));
    let resp = match client.post(&url).header("User-Agent", APP_ID).form(&form).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred communicating with PushSafer.");
        tracing::debug!("Socket Exception: {}", e);
        return false;
      }
    };
    let status = resp.status();
    let response: Option<Value> = resp.json().await.ok();
    if !matches!(status.as_u16(), 200 | 204) {
      let reason = status.canonical_reason().unwrap_or("");
      tracing::warn!("Failed to deliver payload to PushSafer: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      return false;
    }
    let code = response.as_ref().and_then(|r| r.get("status")).and_then(Value::as_i64);
    if code != Some(1) {
      let error = response.as_ref().and_then(|r| r.get("error")).and_then(Value::as_str).unwrap_or("Unknown");
      tracing::warn!("Failed to deliver payload to PushSafer; error={}.", error);
      return false;
    }
    true
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
  fn attachment_support(&self) -> bool {
    true
  }
  fn body_maxlen(&self) -> usize {
    5000
  }
  fn request_rate_per_sec(&self) -> f64 {
    1.2
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // Only images can be attached, as data URLs
    let pictures: Vec<String> = ctx
      .attachments
      .iter()
      .filter(|a| {
        let image = a.mime_type.starts_with("image/");
        if !image {
          tracing::debug!("Ignoring unsupported PushSafer attachment {}.", a.name);
        }
        image
      })
      .map(|a| format!("data:{};base64,{}", a.mime_type, base64::engine::general_purpose::STANDARD.encode(&a.data)))
      .collect();

    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for recipient in &self.targets {
      let base = |title: &str, body: &str| {
        let mut form: Vec<(&str, String)> = vec![
          ("t", title.to_string()),
          ("m", body.to_string()),
          ("i", DEFAULT_PUSHSAFER_ICON.to_string()),
          ("c", ctx.notify_type.color_hex()),
          ("d", recipient.clone()),
        ];
        if let Some(sound) = self.sound {
          form.push(("s", sound.to_string()));
        }
        if let Some(vibration) = self.vibration {
          form.push(("v", vibration.to_string()));
        }
        form
      };

      if pictures.is_empty() {
        if self.post(&client, base(&ctx.title, &ctx.body)).await {
          tracing::info!("Sent PushSafer notification to \"{}\".", recipient);
        } else {
          has_error = true;
        }
        continue;
      }

      // Three pictures per request; requests after the first carry no text
      for (index, chunk) in pictures.chunks(PICTURE_PARAMETER.len()).enumerate() {
        let mut form = if index == 0 { base(&ctx.title, &ctx.body) } else { base("", "...") };
        form.extend(PICTURE_PARAMETER.iter().zip(chunk).map(|(field, picture)| (*field, picture.clone())));
        if self.post(&client, form).await {
          tracing::info!("Sent PushSafer attachment to \"{}\".", recipient);
        } else {
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use wiremock::matchers::{body_string, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn psafer(url: &str) -> Pushsafer {
    Pushsafer::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn good() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": 1, "success": "message transmitted"}))
  }

  #[test]
  fn test_urls() {
    let valid = [
      format!("psafer://{}", "a".repeat(20)),
      format!("psafers://{}", "d".repeat(20)),
      format!("psafer://{}/12/24/53", "e".repeat(20)),
      format!("psafer://{}?to=12,24,53", "e".repeat(20)),
      format!("psafer://{}?priority=emergency", "f".repeat(20)),
      format!("psafer://{}?priority=-1", "f".repeat(20)),
      format!("psafer://{}?sound=ok", "g".repeat(20)),
      format!("psafers://{}?sound=14", "g".repeat(20)),
      format!("psafers://{}?vibration=1", "h".repeat(20)),
      format!("psafers://{}?vibration=0", "h".repeat(20)),
    ];
    for url in &valid {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
    let invalid = [
      "psafer://:@/".to_string(),
      "psafer://".to_string(),
      "psafers://".to_string(),
      format!("psafer://{}?priority=invalid", "f".repeat(20)),
      format!("psafer://{}?priority=25", "f".repeat(20)),
      format!("psafer://{}?sound=invalid", "h".repeat(20)),
      format!("psafer://{}?sound=94000", "h".repeat(20)),
      format!("psafer://{}?vibration=invalid", "h".repeat(20)),
      format!("psafer://{}?vibration=4", "h".repeat(20)),
    ];
    for url in &invalid {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_options() {
    let obj = psafer("psafer://key?sound=ok&priority=h&vibration=2");
    assert_eq!((obj.sound, obj.priority, obj.vibration), (Some(32), Some(1), Some(2)));
    assert_eq!(psafer("psafer://key?sound=b").sound, Some(4));
    assert_eq!(psafer("psafer://key?sound=whah").sound, Some(28));
    assert_eq!(psafer("psafer://key").targets, vec![PUSHSAFER_SEND_TO_ALL]);
    assert_eq!(psafer("psafer://key/b/a?to=b").targets, vec!["a", "b"]);
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api"))
      .and(body_string("t=T&m=B&i=25&c=%23CACF29&d=a&s=32&v=2&k=pkey"))
      .respond_with(good())
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("psafers://pkey?sound=okay&vibration=2&priority=high").unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), notify_type: NotifyType::Warning, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_pictures() {
    // Matches pycapture.py for one image; four images take two requests
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(good()).expect(2).mount(&server).await;
    let svc = from_url("psafer://pkey/dev1").unwrap();
    let image = |n: &str| Attachment { name: n.into(), data: b"jpeg-bytes".to_vec(), mime_type: "image/jpeg".into() };
    let text = Attachment { name: "a.txt".into(), data: b"x".to_vec(), mime_type: "text/plain".into() };
    let ctx =
      NotifyContext { title: "T".into(), body: "B".into(), attachments: vec![image("1"), text, image("2"), image("3"), image("4")], ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let picture = "data%3Aimage%2Fjpeg%3Bbase64%2CanBlZy1ieXRlcw%3D%3D";
    let requests = server.received_requests().await.unwrap();
    assert_eq!(String::from_utf8_lossy(&requests[0].body), format!("t=T&m=B&i=25&c=%233AA3E3&d=dev1&p={0}&p2={0}&p3={0}&k=pkey", picture));
    assert_eq!(String::from_utf8_lossy(&requests[1].body), format!("t=&m=...&i=25&c=%233AA3E3&d=dev1&p={0}&k=pkey", picture));
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    for response in [
      ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": 0, "error": "invalid key"})),
      ResponseTemplate::new(200).set_body_string("garbage"),
      ResponseTemplate::new(500),
      ResponseTemplate::new(999),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(response).expect(2).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), from_url("psafer://pkey/d1/d2").unwrap().send(&ctx)).await.unwrap());
    }
  }

  #[test]
  fn test_service_details() {
    let details = Pushsafer::static_details();
    assert_eq!(details.service_name, "Pushsafer");
    assert!(details.attachment_support);
    assert_eq!(from_url("psafer://pkey").unwrap().body_maxlen(), 5000);
  }
}
