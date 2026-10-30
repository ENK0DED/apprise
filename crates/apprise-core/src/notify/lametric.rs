//! LaMetric Time notifications, either straight to a device on the local
//! network (`mode=device`) or through an indicator app on LaMetric's cloud
//! (`mode=cloud`).

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{split_path, unquote};
use crate::utils::validate::{is_hostname, is_ipaddr};
use async_trait::async_trait;
use serde_json::{Value, json};

const LAMETRIC_PRIORITIES: &[&str] = &["info", "warning", "critical"];
const LAMETRIC_ICON_TYPES: &[&str] = &["info", "alert", "none"];

/// Sounds as (category, aliases), searched in upstream's `LAMETRIC_SOUNDS`
/// order; the first alias is the sound's ID.
const LAMETRIC_SOUNDS: &[(&str, &[&str])] = &[
  ("alarms", &["alarm13", "a13"]),
  ("alarms", &["alarm12", "a12"]),
  ("alarms", &["alarm11", "a11"]),
  ("alarms", &["alarm10", "a10"]),
  ("alarms", &["alarm9", "a9", "a09"]),
  ("alarms", &["alarm8", "a8", "a08"]),
  ("alarms", &["alarm7", "a7", "a07"]),
  ("alarms", &["alarm6", "a6", "a06"]),
  ("alarms", &["alarm5", "a5", "a05"]),
  ("alarms", &["alarm4", "a4", "a04"]),
  ("alarms", &["alarm3", "a3", "a03"]),
  ("alarms", &["alarm2", "a2", "a02"]),
  ("alarms", &["alarm1", "a1", "a01"]),
  ("notifications", &["bicycle", "bike"]),
  ("notifications", &["car"]),
  ("notifications", &["cash"]),
  ("notifications", &["cat"]),
  ("notifications", &["dog2", "dog02"]),
  ("notifications", &["dog", "dog1", "dog01"]),
  ("notifications", &["energy"]),
  ("notifications", &["knock-knock", "knock"]),
  ("notifications", &["letter_email", "letter", "email"]),
  ("notifications", &["lose2", "lose02"]),
  ("notifications", &["lose1", "lose01", "lose"]),
  ("notifications", &["negative1", "negative01", "neg01", "neg1", "-"]),
  ("notifications", &["negative2", "negative02", "neg02", "neg2", "--"]),
  ("notifications", &["negative3", "negative03", "neg03", "neg3", "---"]),
  ("notifications", &["negative4", "negative04", "neg04", "neg4", "----"]),
  ("notifications", &["negative5", "negative05", "neg05", "neg5", "-----"]),
  ("notifications", &["notification4", "notification04", "not04", "not4"]),
  ("notifications", &["notification3", "notification03", "not03", "not3"]),
  ("notifications", &["notification2", "notification02", "not02", "not2"]),
  ("notifications", &["notification", "notification1", "notification01", "not01", "not1"]),
  ("notifications", &["open_door", "open", "door"]),
  ("notifications", &["positive1", "positive01", "pos01", "p1", "+"]),
  ("notifications", &["positive2", "positive02", "pos02", "p2", "++"]),
  ("notifications", &["positive3", "positive03", "pos03", "p3", "+++"]),
  ("notifications", &["positive4", "positive04", "pos04", "p4", "++++"]),
  ("notifications", &["positive5", "positive05", "pos05", "p5", "+++++"]),
  ("notifications", &["statistic", "stat"]),
  // Upstream lists "thunder" as a bare string, which makes it unusable
  ("notifications", &["thunder"]),
  ("notifications", &["water2", "water02"]),
  ("notifications", &["water1", "water01"]),
  ("notifications", &["wind"]),
  ("notifications", &["wind_short"]),
  ("notifications", &["win", "win01", "win1"]),
  ("notifications", &["win2", "win02"]),
];

/// The icon shown for each notification type when `?icon=` is not given.
fn default_icon(notify_type: &NotifyType) -> &'static str {
  match notify_type {
    NotifyType::Info => "i620",
    NotifyType::Success => "i9182",
    NotifyType::Warning => "i9183",
    NotifyType::Failure => "i9184",
  }
}

/// The first sound with an alias starting with `lookup`.
fn sound_lookup(lookup: &str) -> Option<(&'static str, &'static str)> {
  LAMETRIC_SOUNDS.iter().find(|(_, aliases)| aliases.iter().any(|a| a.starts_with(lookup))).map(|(category, aliases)| (*category, aliases[0]))
}

/// Cloud app access tokens are 80+ alphanumerics followed by `==`.
fn is_app_token(token: &str) -> bool {
  token.strip_suffix("==").is_some_and(|t| t.len() >= 80 && t.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Upstream `LAMETRIC_APP_ID_DETECTOR_RE`: `[com.lametric.]{app_id}[/{ver}]`.
fn detect_app_id(value: &str) -> Option<(String, Option<String>)> {
  let rest = value.get(..13).filter(|p| p.eq_ignore_ascii_case("com.lametric.")).map(|_| &value[13..]).filter(|r| !r.is_empty()).unwrap_or(value);
  let app_id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')).take(64).collect();
  if app_id.is_empty() {
    return None;
  }
  let ver = rest[app_id.len()..]
    .strip_prefix('/')
    .map(|v| v.chars().take_while(char::is_ascii_digit).collect::<String>())
    .filter(|v| !v.is_empty() && !v.starts_with('0'));
  Some((app_id, ver))
}

fn is_app_ver(value: &str) -> bool {
  !value.is_empty() && !value.starts_with('0') && value.chars().all(|c| c.is_ascii_digit())
}

enum Mode {
  Cloud { app_id: String, app_ver: String, app_token: String },
  Device { host: String, port: u16, secure: bool, user: String, apikey: String },
}

pub struct LaMetric {
  mode: Mode,
  priority: &'static str,
  icon: Option<String>,
  icon_type: &'static str,
  cycles: u64,
  sound: Option<(&'static str, &'static str)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl LaMetric {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let qsd = |k: &str| url.get(k).filter(|v| !v.is_empty()).map(str::to_string);

    // A native https://developer.lametric.com/api/v1/dev/widget/update/
    // com.lametric.{app_id}[/{ver}] URL is an indicator app in the cloud
    let native = matches!(url.schema.as_str(), "http" | "https");
    let (host, password, mut entries, native_ver) = if native {
      let path = url.path.trim_end_matches('/');
      let mut parts = path.splitn(5, '/');
      let (Some("api"), Some(v), Some("dev"), Some("widget"), Some(rest)) = (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) else {
        return None;
      };
      let rest = rest.strip_prefix("update/")?;
      if !v.starts_with('v') || !v[1..].chars().all(|c| c.is_ascii_digit()) || v.len() < 2 {
        return None;
      }
      let (app, ver) = rest.split_once('/').map(|(a, v)| (a, Some(v.to_string()))).unwrap_or((rest, None));
      let app = app.get(..13).filter(|p| p.eq_ignore_ascii_case("com.lametric.")).map(|_| &app[13..])?;
      (Some(app.to_string()), None, Vec::new(), ver)
    } else {
      // A lone user is the API key or token
      let password = url.password.clone().filter(|p| !p.is_empty()).or_else(|| url.user.clone().filter(|_| url.password.is_none()));
      (url.host.clone().filter(|h| !h.is_empty()), password, split_path(url), None)
    };
    let user = if native || url.password.is_none() { None } else { url.user.clone().filter(|u| !u.is_empty()) };

    let app_id_arg = qsd("app");
    let app_ver_arg = qsd("app_ver").or(native_ver).or_else(|| if entries.is_empty() { None } else { Some(entries.remove(0)) });
    let token_arg = qsd("token");

    let mode = match qsd("mode") {
      Some(mode) => mode.trim().to_lowercase(),
      None => {
        // Upstream's is_hostname() allows underscores
        let device = host.as_deref().is_some_and(|h| is_hostname(&h.replace('_', "a")) || is_ipaddr(h))
          && password.as_deref().is_some_and(|p| !is_app_token(p))
          && app_id_arg.is_none()
          && app_ver_arg.is_none()
          && token_arg.is_none()
          && !native;
        if device { "device".to_string() } else { "cloud".to_string() }
      }
    };

    let mode = match mode.as_str() {
      "cloud" => {
        let app_id = app_id_arg.or_else(|| host.as_deref().map(unquote));
        let Some((app_id, detected_ver)) = app_id.as_deref().and_then(detect_app_id) else {
          tracing::warn!("An invalid LaMetric Application ID was specified.");
          return None;
        };
        let app_token = token_arg.or_else(|| password.as_deref().map(unquote)).unwrap_or_default();
        if !is_app_token(&app_token) {
          tracing::warn!("An invalid LaMetric Application Access Token was specified.");
          return None;
        }
        let app_ver = match app_ver_arg {
          Some(ver) if is_app_ver(&ver) => ver,
          Some(ver) => {
            tracing::warn!("An invalid LaMetric Application Version ({}) was specified.", ver);
            return None;
          }
          None => detected_ver.unwrap_or_else(|| "1".to_string()),
        };
        Mode::Cloud { app_id, app_ver, app_token }
      }
      "device" => {
        let apikey = qsd("apikey").or_else(|| password.as_deref().map(unquote));
        let Some(apikey) = apikey.as_deref().and_then(|k| k.split_whitespace().next()).map(str::to_string) else {
          tracing::warn!("An invalid LaMetric Device API Key was specified.");
          return None;
        };
        let host = host?;
        Mode::Device { host, port: url.port.unwrap_or(8080), secure: url.schema == "lametrics", user: user.unwrap_or_else(|| "dev".to_string()), apikey }
      }
      other => {
        tracing::warn!("An invalid LaMetric Mode ({}) was specified.", other);
        return None;
      }
    };

    let priority = qsd("priority").map(|p| p.trim().to_lowercase()).and_then(|p| LAMETRIC_PRIORITIES.iter().copied().find(|v| *v == p)).unwrap_or("info");
    let icon_type = qsd("icon_type").map(|p| p.trim().to_lowercase()).and_then(|p| LAMETRIC_ICON_TYPES.iter().copied().find(|v| *v == p)).unwrap_or("none");

    // Leading '#' and spaces are dropped, unless nothing else is left
    let icon = qsd("icon").map(|i| i.trim().to_lowercase()).filter(|i| !i.is_empty()).map(|i| {
      let stripped = i.trim_start_matches(|c: char| c == '#' || c.is_whitespace());
      if stripped.is_empty() { i.chars().last().map(String::from).unwrap_or_default() } else { stripped.trim_end().to_string() }
    });

    let sound = qsd("sound").map(|s| s.trim().to_lowercase()).and_then(|s| {
      let found = sound_lookup(&s);
      if found.is_none() {
        tracing::warn!("An invalid LaMetric sound ({}) was specified.", s);
      }
      found
    });

    let cycles = qsd("cycles").and_then(|c| c.trim().parse::<i64>().ok()).map(|c| c.unsigned_abs()).filter(|c| *c > 0).unwrap_or(1);

    Some(Self { mode, priority, icon, icon_type, cycles, sound, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "LaMetric",
      service_url: Some("https://lametric.com"),
      setup_url: Some("https://appriseit.com/services/lametric/"),
      protocols: vec!["lametric", "lametrics"],
      description: "Send notifications to LaMetric Time devices.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for LaMetric {
  fn schemas(&self) -> &[&str] {
    &["lametric", "lametrics"]
  }
  fn service_name(&self) -> &str {
    "LaMetric"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // The title is merged into the body
  fn title_maxlen(&self) -> usize {
    0
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.2
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let icon = self.icon.clone().unwrap_or_else(|| default_icon(&ctx.notify_type).to_string());
    let client = build_client(self.verify_certificate)?;
    let request = |url: &str| {
      client
        .post(url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Cache-Control", "no-cache")
    };

    let (request, payload, target) = match &self.mode {
      Mode::Cloud { app_id, app_ver, app_token } => {
        for (unused, set) in
          [("sound", self.sound.is_some()), ("priority", self.priority != "info"), ("icon_type", self.icon_type != "none"), ("cycle", self.cycles != 1)]
        {
          if set {
            tracing::warn!("LaMetric {} setting is unavailable in Cloud mode", unused);
          }
        }
        let url = format!("{}/api/v1/dev/widget/update/com.lametric.{}/{}", service_base("https://developer.lametric.com"), app_id, app_ver);
        let payload = json!({ "frames": [{ "icon": icon, "text": ctx.body, "index": 0 }] });
        // Upstream means to send the app's access token here but sends none
        (request(&url).header("X-Access-Token", app_token), payload, "developer.lametric.com".to_string())
      }
      Mode::Device { host, port, secure, user, apikey } => {
        let url = format!("{}://{}:{}/api/v2/device/notifications", if *secure { "https" } else { "http" }, host, port);
        let mut payload: Value = json!({
          "priority": self.priority,
          "icon_type": self.icon_type,
          "lifetime": 120000,
          "model": { "cycles": self.cycles, "frames": [{ "icon": icon, "text": ctx.body }] },
        });
        if let Some((category, id)) = self.sound {
          payload["model"]["sound"] = json!({ "category": category, "id": id, "repeat": 1 });
        }
        (request(&url).basic_auth(user, Some(apikey)), payload, host.clone())
      }
    };

    let resp = match request.body(serde_json::to_vec(&payload)?).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending LaMetric notification to {}.", target);
        tracing::debug!("Socket Exception: {}", e);
        return Ok(false);
      }
    };
    let status = resp.status();
    if matches!(status.as_u16(), 200 | 201) {
      tracing::info!("Sent LaMetric notification.");
      return Ok(true);
    }
    let reason = status.canonical_reason().unwrap_or("");
    tracing::warn!("Failed to send LaMetric notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
    Ok(false)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const UUID4: &str = "8b799edf-6f98-4d3a-9be7-2862fb4e5752";
  const APP: &str = "com.lametric.941c51dff3135bd87aa72db9d855dd50";

  fn lametric(url: &str) -> LaMetric {
    LaMetric::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      format!("lametric://root:{}@192.168.0.5:8080/", UUID4),
      format!("lametric://{}@192.168.0.4:8000/", UUID4),
      format!("lametric://{}@192.168.0.5/", UUID4),
      format!("lametrics://{}@192.168.0.6/?mode=device", UUID4),
      format!("https://developer.lametric.com/api/v1/dev/widget/update/com.lametric.ABCD123/1?token={}==", "D".repeat(88)),
      "lametric://192.168.2.8/?mode=device&apikey=abc123".to_string(),
      format!("lametrics://{}==@{}/?mode=cloud&app_ver=2", "A".repeat(88), APP),
      format!("lametric://?app={}&token={}==&mode=cloud", APP, "B".repeat(88)),
      format!("lametrics://{}==@abcd/?mode=cloud&sound=knock&icon_type=info&priority=critical&cycles=10", "C".repeat(88)),
      format!("lametrics://{}@192.168.0.6/?sound=alarm1", UUID4),
      format!("lametrics://{}@192.168.0.8/?sound=invalid!", UUID4),
      format!("lametrics://{}@192.168.0.10/?icon_type=invalid", UUID4),
      format!("lametrics://{}@192.168.1.2/?priority=invalid", UUID4),
      format!("lametric://{}@192.168.1.2/?icon=#", UUID4),
      format!("lametrics://{}@192.168.1.5/?cycles=invalid", UUID4),
      format!("lametric://{}@example.com/", UUID4),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "lametric://".to_string(),
      "lametric://:@/".to_string(),
      format!("lametric://{}/", APP),
      format!("lametrics://{}==@{}/?app_ver=invalid", "A".repeat(88), APP),
      format!("lametrics://{}@192.168.0.7/?mode=invalid", UUID4),
      format!("lametric://short==@{}/?mode=cloud", APP),
      "https://developer.lametric.com/api/v1/other".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_options() {
    let obj = lametric(&format!("lametric://{}@192.168.1.2/?icon=%23Heart&sound=bike&cycles=-3&priority=Warning&icon_type=ALERT", UUID4));
    assert_eq!((obj.icon.as_deref(), obj.sound, obj.cycles), (Some("heart"), Some(("notifications", "bicycle")), 3));
    assert_eq!((obj.priority, obj.icon_type), ("warning", "alert"));
    assert_eq!(lametric(&format!("lametric://{}@h/?icon=%23%20%20%20", UUID4)).icon.as_deref(), Some("#"));
    assert_eq!(lametric(&format!("lametric://{}@h/?icon=%23230", UUID4)).icon.as_deref(), Some("230"));
    assert_eq!(lametric(&format!("lametric://{}@h/?cycles=0", UUID4)).cycles, 1);
    assert_eq!(sound_lookup("a1"), Some(("alarms", "alarm13")));
    assert_eq!(sound_lookup("alarm1"), Some(("alarms", "alarm13")));
    assert_eq!(sound_lookup("a01"), Some(("alarms", "alarm1")));
    assert_eq!(sound_lookup("thunder"), Some(("notifications", "thunder")));

    let Mode::Cloud { app_id, app_ver, .. } = lametric(&format!("lametrics://{}==@{}/?mode=cloud&app_ver=2", "A".repeat(88), APP)).mode else { panic!() };
    assert_eq!((app_id.as_str(), app_ver.as_str()), ("941c51dff3135bd87aa72db9d855dd50", "2"));
    let Mode::Cloud { app_id, app_ver, .. } =
      lametric(&format!("https://developer.lametric.com/api/v1/dev/widget/update/com.lametric.ABCD123/3?token={}==", "D".repeat(88))).mode
    else {
      panic!()
    };
    assert_eq!((app_id.as_str(), app_ver.as_str()), ("ABCD123", "3"));
    // A path entry is the app version, which selects cloud mode
    assert!(matches!(lametric(&format!("lametric://{}==@abcd/4", "A".repeat(88))).mode, Mode::Cloud { .. }));
    assert!(matches!(lametric(&format!("lametric://{}@h_x/", UUID4)).mode, Mode::Device { .. }));
    let Mode::Device { user, apikey, port, .. } = lametric("lametric://192.168.2.8/?mode=device&apikey=abc123").mode else { panic!() };
    assert_eq!((user.as_str(), apikey.as_str(), port), ("dev", "abc123", 8080));
  }

  #[tokio::test]
  async fn test_send_device() {
    // Matches pycapture.py for this URL
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v2/device/notifications"))
      .and(header("authorization", format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("root:{}", UUID4))).as_str()))
      .and(header("cache-control", "no-cache"))
      .and(body_json(json!({"priority": "critical", "icon_type": "alert", "lifetime": 120000, "model": {"cycles": 3, "frames": [{"icon": "heart", "text": "T\r\nB"}], "sound": {"category": "notifications", "id": "bicycle", "repeat": 1}}})))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let addr = server.address();
    let svc =
      from_url(&format!("lametric://root:{}@{}:{}/?sound=bike&icon=%23Heart&icon_type=alert&priority=critical&cycles=-3", UUID4, addr.ip(), addr.port()))
        .unwrap();
    assert_eq!(svc.title_maxlen(), 0);
    let ctx = NotifyContext { body: "T\r\nB".into(), notify_type: NotifyType::Failure, ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_device_default_user() {
    // apprise-go parity golden "lametric-default": dev:{apikey}
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(header("authorization", "Basic ZGV2OmFwaWtleQ=="))
      .and(body_json(json!({"priority": "info", "icon_type": "none", "lifetime": 120000, "model": {"cycles": 1, "frames": [{"icon": "i620", "text": "B"}]}})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let addr = server.address();
    let svc = from_url(&format!("lametric://apikey@{}:{}", addr.ip(), addr.port())).unwrap();
    assert!(svc.send(&NotifyContext { body: "B".into(), ..Default::default() }).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_cloud() {
    let token = format!("{}==", "D".repeat(88));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/dev/widget/update/com.lametric.ABCD123/2"))
      .and(header("x-access-token", token.as_str()))
      .and(body_json(json!({"frames": [{"icon": "i620", "text": "B", "index": 0}]})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("https://developer.lametric.com/api/v1/dev/widget/update/com.lametric.ABCD123/2?token={}", token)).unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999, 204] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let addr = server.address();
      let svc = from_url(&format!("lametric://{}@{}:{}/", UUID4, addr.ip(), addr.port())).unwrap();
      assert!(!svc.send(&NotifyContext { body: "B".into(), ..Default::default() }).await.unwrap());
    }
  }

  #[test]
  fn test_service_details() {
    let details = LaMetric::static_details();
    assert_eq!(details.service_name, "LaMetric");
    assert_eq!(details.protocols, vec!["lametric", "lametrics"]);
  }
}
