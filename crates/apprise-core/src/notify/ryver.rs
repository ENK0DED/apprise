use async_trait::async_trait;
use serde_json::{Value, json};

use crate::asset::AppriseAsset;
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, parse_bool};

/// Upstream `AppriseAsset.image_url_mask` default.
const DEFAULT_IMAGE_URL_MASK: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/{THEME}/apprise-{TYPE}-{XY}{EXTENSION}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  Slack,
  Ryver,
}

pub struct Ryver {
  organization: String,
  token: String,
  botname: Option<String>,
  mode: Mode,
  include_image: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Ryver {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (organization, token) = if url.schema == "https" || url.schema == "http" {
      // Native URL: https://{org}.ryver.com/application/webhook/{token}
      let host = url.host.as_deref()?.to_lowercase();
      let org = host.strip_suffix(".ryver.com")?;
      if org.is_empty() || !org.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
      }
      match url.path_parts.as_slice() {
        [app, hook, token]
          if app.eq_ignore_ascii_case("application") && hook.eq_ignore_ascii_case("webhook") && token.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
          (org.to_string(), token.clone())
        }
        _ => return None,
      }
    } else {
      // ryver://{botname}@{organization}/{token}
      (url.host.as_deref().map(unquote).unwrap_or_default(), url.path_parts.first().cloned().unwrap_or_default())
    };

    let token = token.trim().to_string();
    if token.len() != 15 || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("An invalid Ryver API Token was specified.");
      return None;
    }
    let organization = organization.trim().to_string();
    if !(3..=32).contains(&organization.len()) || !organization.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
      tracing::warn!("An invalid Ryver Organization ({}) was specified.", organization);
      return None;
    }

    let mode = match url.get("mode").map(|m| m.to_lowercase()) {
      None => Mode::Ryver,
      Some(m) if m == "ryver" => Mode::Ryver,
      Some(m) if m == "slack" => Mode::Slack,
      Some(m) => {
        tracing::warn!("The Ryver webhook mode specified ({}) is invalid.", m);
        return None;
      }
    };

    Some(Self {
      organization,
      token,
      botname: url.user.clone().filter(|u| !u.is_empty()),
      mode,
      include_image: url.get("image").map(parse_bool).unwrap_or(true),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Ryver",
      service_url: Some("https://ryver.com/"),
      setup_url: Some("https://appriseit.com/services/ryver/"),
      protocols: vec!["ryver"],
      description: "Send via Ryver webhooks.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    // Slack-mode webhooks need the markup characters escaped
    let (title, body) = match self.mode {
      Mode::Slack => (slack_escape(&ctx.title), slack_escape(&ctx.body)),
      Mode::Ryver => (ctx.title.clone(), ctx.body.clone()),
    };
    let avatar = if self.include_image { image_url(&ctx.asset, &ctx.notify_type) } else { None };
    json!({
      "body": if title.is_empty() { body } else { format!("**{}**\r\n{}", title, body) },
      "createSource": {
        "displayName": self.botname,
        "avatar": avatar,
      },
    })
  }
}

#[async_trait]
impl Notify for Ryver {
  fn schemas(&self) -> &[&str] {
    &["ryver"]
  }
  fn service_name(&self) -> &str {
    "Ryver"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    1000
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let base = service_base(&format!("https://{}.ryver.com", self.organization));
    let url = format!("{}/application/webhook/{}", base, self.token);
    tracing::debug!("Ryver POST URL for organization {} (cert_verify={})", self.organization, self.verify_certificate);

    let client = build_client(self.verify_certificate)?;
    let resp = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(self.payload(ctx).to_string()).send().await?;
    let status = resp.status();
    if status.as_u16() == 200 {
      tracing::info!("Sent Ryver notification.");
      Ok(true)
    } else {
      let reason = status.canonical_reason().unwrap_or("");
      tracing::warn!("Failed to send Ryver notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

fn slack_escape(s: &str) -> String {
  s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Upstream `AppriseAsset.image_url()` at the 72x72 size Ryver uses.
fn image_url(asset: &AppriseAsset, notify_type: &NotifyType) -> Option<String> {
  let mask = asset.image_url_mask.as_deref().unwrap_or(DEFAULT_IMAGE_URL_MASK);
  if mask.is_empty() {
    return None;
  }
  Some(mask.replace("{THEME}", &asset.theme).replace("{TYPE}", notify_type.as_str()).replace("{XY}", "72x72").replace("{EXTENSION}", ".png"))
}

fn unquote(s: &str) -> String {
  urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "ryver://apprise/ckhrjW8w672m6HG?mode=slack",
      "ryver://apprise/ckhrjW8w672m6HG?mode=ryver",
      "ryver://apprise/ckhrjW8w672m6HG?webhook=slack",
      "ryver://apprise/ckhrjW8w672m6HG?webhook=ryver",
      "https://apprise.ryver.com/application/webhook/ckhrjW8w672m6HG",
      "https://apprise.ryver.com/application/webhook/ckhrjW8w672m6HG?webhook=ryver",
      "ryver://caronc@apprise/ckhrjW8w672m6HG",
      "ryver://apprise/ckhrjW8w672m6HG",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["ryver://", "ryver://:@/", "ryver://apprise", "ryver://apprise/ckhrjW8w672m6HG?mode=invalid", "ryver://x/ckhrjW8w672m6HG?mode=slack"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let parsed = crate::utils::parse::ParsedUrl::parse("ryver://apprise/ckhrjW8w672m6HG?mode=slack").unwrap();
    let r = Ryver::from_url(&parsed).unwrap();
    assert_eq!(r.organization, "apprise");
    assert_eq!(r.token, "ckhrjW8w672m6HG");
  }

  #[test]
  fn test_native_url_fields() {
    let parsed = crate::utils::parse::ParsedUrl::parse("https://apprise.ryver.com/application/webhook/ckhrjW8w672m6HG").unwrap();
    let r = Ryver::from_url(&parsed).unwrap();
    assert_eq!(r.organization, "apprise");
    assert_eq!(r.token, "ckhrjW8w672m6HG");
  }

  #[test]
  fn test_from_url_options() {
    let parse = |u: &str| Ryver::from_url(&crate::utils::parse::ParsedUrl::parse(u).unwrap());
    let r = parse("ryver://caronc@apprise/ckhrjW8w672m6HG?image=no&mode=SLACK").unwrap();
    assert_eq!(r.botname.as_deref(), Some("caronc"));
    assert!(!r.include_image);
    assert_eq!(r.mode, Mode::Slack);
    assert_eq!(parse("ryver://apprise/ckhrjW8w672m6HG").unwrap().mode, Mode::Ryver);
    // Tokens must be 15 alphanumeric characters; an empty mode is invalid
    assert!(parse("ryver://apprise/short").is_none());
    assert!(parse("ryver://apprise/ckhrjW8w672m6HG?mode=").is_none());
    assert!(parse("https://apprise.ryver.com/application/webhook/bad-token-12345").is_none());
  }

  #[test]
  fn test_payloads() {
    let parse = |u: &str| Ryver::from_url(&crate::utils::parse::ParsedUrl::parse(u).unwrap()).unwrap();
    let ctx = NotifyContext { title: "T<1>".into(), body: "a & b".into(), ..Default::default() };
    assert_eq!(
      parse("ryver://caronc@apprise/ckhrjW8w672m6HG").payload(&ctx),
      json!({
        "body": "**T<1>**\r\na & b",
        "createSource": {
          "displayName": "caronc",
          "avatar": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-72x72.png",
        },
      })
    );
    let ctx = NotifyContext { body: "a & <b>".into(), ..Default::default() };
    assert_eq!(
      parse("ryver://apprise/ckhrjW8w672m6HG?mode=slack&image=no").payload(&ctx),
      json!({"body": "a &amp; &lt;b&gt;", "createSource": {"displayName": null, "avatar": null}})
    );
  }

  #[tokio::test]
  async fn test_send() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/application/webhook/ckhrjW8w672m6HG"))
      .and(header("Content-Type", "application/json"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("ryver://apprise/ckhrjW8w672m6HG").unwrap();
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.is_err());
  }

  #[test]
  fn test_service_details() {
    let d = Ryver::static_details();
    assert_eq!(d.service_name, "Ryver");
    assert!(d.protocols.contains(&"ryver"));
    assert!(!d.attachment_support);
  }

  #[test]
  fn test_edge_case_no_token() {
    // Organization only, no token
    let parsed = crate::utils::parse::ParsedUrl::parse("ryver://apprise").unwrap();
    assert!(Ryver::from_url(&parsed).is_none());
  }

  #[test]
  fn test_edge_case_short_org() {
    // org "x" is only 1 char, must be >= 3
    let parsed = crate::utils::parse::ParsedUrl::parse("ryver://x/ckhrjW8w672m6HG").unwrap();
    assert!(Ryver::from_url(&parsed).is_none());
  }
}
