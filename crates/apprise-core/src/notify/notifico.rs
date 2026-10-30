use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, parse_bool, unquote};
use async_trait::async_trait;

/// IRC formatting codes.
const COLOR_RESET: &str = "\x03";
const FORMAT_BOLD: &str = "\x02";
const FORMAT_RESET: &str = "\x0f";

/// Upstream `parse_bool(qsd.get(key, True))`: yes when absent, no when
/// given an unrecognised value.
fn flag(url: &ParsedUrl, key: &str) -> bool {
  url.get(key).is_none_or(parse_bool)
}

/// Remove literal `\x03NN[,NN]` colour escapes typed into the body.
fn strip_color_escapes(body: &str) -> String {
  let mut out = String::with_capacity(body.len());
  let mut rest = body;
  while let Some(i) = rest.find("\\x03") {
    out.push_str(&rest[..i]);
    let mut after = &rest[i + 4..];
    let digits = after.chars().take(2).take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
      after = &after[digits..];
      if let Some(bg) = after.strip_prefix(',') {
        let n = bg.chars().take(2).take_while(|c| c.is_ascii_digit()).count();
        if n > 0 {
          after = &bg[n..];
        }
      }
    }
    rest = after;
  }
  out.push_str(rest);
  out
}

pub struct Notifico {
  project_id: String,
  msghook: String,
  /// A self-hosted server; `None` for the official n.tkte.ch service.
  host: Option<String>,
  port: Option<u16>,
  user: Option<String>,
  password: Option<String>,
  secure: bool,
  color: bool,
  prefix: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Notifico {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().unwrap_or_default();
    let entries = &url.path_parts;

    // Native https://n.tkte.ch/h/{project}/{hook} URLs arrive here already
    // rewritten by the registry as notifico://{project}/{hook}
    let (mut project_id, mut msghook, selfhosted) = if !host.is_empty() && host.chars().all(|c| c.is_ascii_digit()) {
      // notifico://{project}/{hook}: the official service
      (Some(host.clone()), entries.first().cloned(), false)
    } else {
      (entries.first().cloned(), entries.get(1).cloned(), !host.is_empty())
    };
    // Upstream unquotes these a second time
    if let Some(t) = url.get("token").filter(|t| !t.is_empty()) {
      msghook = Some(unquote(t));
    }
    if let Some(p) = url.get("project").filter(|p| !p.is_empty()) {
      project_id = Some(unquote(p));
    }

    let Some(project_id) = project_id.map(|p| p.trim().to_string()).filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) else {
      tracing::warn!("An invalid Notifico Project ID was specified.");
      return None;
    };
    let Some(msghook) = msghook.map(|m| m.trim().to_string()).filter(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_alphanumeric())) else {
      tracing::warn!("An invalid Notifico Message Token was specified.");
      return None;
    };

    Some(Self {
      project_id,
      msghook,
      host: selfhosted.then_some(host),
      port: if selfhosted { url.port } else { None },
      user: if selfhosted { url.user.clone() } else { None },
      password: if selfhosted { url.password.clone() } else { None },
      secure: url.schema == "notificos",
      color: flag(url, "color"),
      prefix: flag(url, "prefix"),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Notifico",
      service_url: Some("https://notifico.tech/"),
      setup_url: Some("https://appriseit.com/services/notifico/"),
      protocols: vec!["notifico", "notificos"],
      description: "Send IRC notifications via Notifico.",
      attachment_support: false,
    }
  }

  fn notify_url(&self) -> String {
    let base = match &self.host {
      None => service_base("https://n.tkte.ch"),
      Some(host) => {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        format!("{}://{}{}", schema, host, port_str)
      }
    };
    format!("{}/h/{}/{}", base, self.project_id, self.msghook)
  }

  /// The IRC line: `[i] Apprise: body`, coloured unless `?color=no`.
  fn message(&self, ctx: &NotifyContext) -> String {
    let body = if self.color { ctx.body.clone() } else { strip_color_escapes(&ctx.body) };
    if !self.prefix {
      return body;
    }
    let (color, token) = match ctx.notify_type {
      NotifyType::Info => ("\x0310", "i"),
      NotifyType::Success => ("\x0309", "\u{2714}"),
      NotifyType::Warning => ("\x0307", "!"),
      NotifyType::Failure => ("\x0304", "\u{2717}"),
    };
    let c = |code: &'static str| if self.color { code } else { "" };
    format!("{}[{}]{} {}{}{}: {}{}", c(color), token, c(COLOR_RESET), c(FORMAT_BOLD), ctx.asset.app_id, c(FORMAT_RESET), body, c(FORMAT_RESET))
  }
}

#[async_trait]
impl Notify for Notifico {
  fn schemas(&self) -> &[&str] {
    &["notifico", "notificos"]
  }
  fn service_name(&self) -> &str {
    "Notifico"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    512
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = self.notify_url();
    tracing::debug!("Notifico GET URL: {} (cert_verify={})", url, self.verify_certificate);
    let client = build_client(self.verify_certificate)?;
    let mut req = client
      .get(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8")
      .query(&[("payload", self.message(ctx))]);
    if let Some(user) = &self.user {
      req = req.basic_auth(user, Some(self.password.clone().unwrap_or_default()));
    }

    crate::notify::throttle().await;
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    if status == 200 {
      tracing::info!("Sent Notifico notification.");
      Ok(true)
    } else {
      tracing::warn!("Failed to send Notifico notification: error={}.", status);
      Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_urls() {
    for url in [
      "notifico://",
      "notifico://:@/",
      "notifico://1234",
      "notifico://abcd/ckhrjW8w672m6HG",
      "notifico://example.com/abcd/ckhrjW8w672m6HG",
      "notifico://1234/bad!",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    for url in [
      "notifico://1234/ckhrjW8w672m6HG",
      "notifico://1234/ckhrjW8w672m6HG?prefix=no",
      "notifico://1234/ckhrjW8w672m6HG?color=no",
      "https://n.tkte.ch/h/2144/uJmKaBW9WFk42miB146ci3Kj",
      "notifico://example.com/1234/ckhrjW8w672m6HG",
      "notificos://example.com/1234/ckhrjW8w672m6HG",
      "notifico://user@example.com:20/1234/ckhrjW8w672m6HG?color=yes",
      "notificos://user:pass@example.com/1234/ckhrjW8w672m6HG?color=yes",
      "notificos://user:pass@example.com/?project=1234&token=ckhrjW8w672m6HG&color=yes",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  fn nf(url: &str) -> Notifico {
    Notifico::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_parse() {
    // What the registry makes of https://n.tkte.ch/h/2144/uJmKaBW9WFk42miB146ci3Kj?prefix=no
    let n = nf("notifico://2144/uJmKaBW9WFk42miB146ci3Kj/?prefix=no");
    assert_eq!((n.project_id.as_str(), n.msghook.as_str(), n.host.is_none(), n.prefix), ("2144", "uJmKaBW9WFk42miB146ci3Kj", true, false));
    let n = nf("notificos://user:pass@example.com:8443/?project=1234&token=hook");
    assert_eq!(n.notify_url(), "https://example.com:8443/h/1234/hook");
    // The official service ignores credentials and ports
    let n = nf("notifico://user:pass@1234:80/hook");
    assert_eq!((n.host.is_none(), n.user.is_none(), n.port), (true, true, None));
    assert!(!nf("notifico://1234/hook?color=bogus").color);
    assert!(nf("notifico://1234/hook").color);
  }

  #[test]
  fn test_message() {
    let ctx = |nt: NotifyType| NotifyContext { body: "body \\x0304,02red\\x03 x".into(), notify_type: nt, ..Default::default() };
    assert_eq!(nf("notifico://1234/hook").message(&ctx(NotifyType::Info)), "\x0310[i]\x03 \x02Apprise\x0f: body \\x0304,02red\\x03 x\x0f");
    assert_eq!(nf("notifico://1234/hook?color=no").message(&ctx(NotifyType::Success)), "[\u{2714}] Apprise: body red x");
    assert_eq!(nf("notifico://1234/hook?prefix=no&color=no").message(&ctx(NotifyType::Failure)), "body red x");
    assert_eq!(strip_color_escapes("a\\x03123b\\x031,2c"), "a3bc");
  }

  #[tokio::test]
  async fn test_official_and_selfhosted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/h/1234/ckhrjW8w672m6HG"))
      .and(query_param("payload", "[!] Apprise: hello"))
      .and(header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8"))
      .and(header("User-Agent", APP_ID))
      .respond_with(ResponseTemplate::new(200))
      .expect(2)
      .mount(&server)
      .await;
    let ctx = NotifyContext { title: "t".into(), body: "hello".into(), notify_type: NotifyType::Warning, ..Default::default() };

    let n = nf("notifico://1234/ckhrjW8w672m6HG?color=no");
    assert!(crate::notify::with_test_base(server.uri(), n.send(&ctx)).await.unwrap());

    let n = nf(&format!("notifico://user@localhost:{}/1234/ckhrjW8w672m6HG?color=no", server.address().port()));
    assert!(n.send(&ctx).await.unwrap());
    let requests = requests_to(&server, "GET", "/h/1234/ckhrjW8w672m6HG").await;
    assert!(requests[0].headers.get("Authorization").is_none());
    // A user without a password authenticates with an empty one
    assert_eq!(requests[1].headers.get("Authorization").unwrap(), "Basic dXNlcjo=");
  }

  #[tokio::test]
  async fn test_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/h/1/hook")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    let n = nf(&format!("notifico://localhost:{}/1/hook", server.address().port()));
    assert!(matches!(n.send(&NotifyContext::default()).await, Err(NotifyError::ServiceError { status: 500, .. })));
  }
}
