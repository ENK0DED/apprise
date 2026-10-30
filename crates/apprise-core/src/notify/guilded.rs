//! Guilded webhooks (upstream `NotifyGuilded`, a subclass of Discord that
//! only changes the service name and webhook URL).
//!
//! A webhook URL `https://media.guilded.gg/webhooks/WEBHOOK_ID/WEBHOOK_TOKEN`
//! is written `guilded://WEBHOOK_ID/WEBHOOK_TOKEN`.

use crate::notify::ServiceDetails;
use crate::notify::discord::{Discord, Flavor};
use crate::utils::parse::ParsedUrl;

const GUILDED: Flavor =
  Flavor { schemas: &["guilded"], service_name: "Guilded", base: "https://media.guilded.gg", path: "/webhooks", details: Guilded::static_details };

pub struct Guilded;

impl Guilded {
  /// Every Discord option applies; messages go to Guilded's webhook URL.
  pub fn from_url(url: &ParsedUrl) -> Option<Discord> {
    Discord::from_url_with(url, &GUILDED)
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Guilded",
      service_url: Some("https://guilded.gg/"),
      setup_url: Some("https://appriseit.com/services/guilded/"),
      protocols: vec!["guilded"],
      description: "Send notifications via Guilded webhooks.",
      attachment_support: true,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::notify::{Notify, NotifyContext, with_test_base};
  use crate::types::NotifyFormat;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn guilded(url: &str) -> Discord {
    Guilded::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_invalid_urls() {
    let no_token = format!("guilded://{}", "i".repeat(24));
    for url in ["guilded://", "guilded://:@/", no_token.as_str(), "guilded://%20%20/token", "guilded://id/%20%20"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let wid = "i".repeat(24);
    let wtok = "t".repeat(64);
    let urls = [
      format!("guilded://{wid}/{wtok}"),
      format!("guilded://l2g@{wid}/{wtok}"),
      format!("guilded://{wid}/{wtok}?format=markdown&footer=Yes&image=Yes"),
      format!("guilded://{wid}/{wtok}?format=markdown&footer=Yes&image=No&fields=no"),
      format!("guilded://{wid}/{wtok}?format=markdown&avatar=No&footer=No"),
      format!("guilded://{wid}/{wtok}?format=markdown"),
      format!("guilded://{wid}/{wtok}?format=text"),
      format!("guilded://{wid}/{wtok}?avatar_url=http://localhost/test.jpg"),
      format!("https://media.guilded.gg/webhooks/{}/{}", "0".repeat(10), "B".repeat(40)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_service_details() {
    let g = guilded(&format!("guilded://{}/{}", "A".repeat(24), "B".repeat(64)));
    assert_eq!(g.service_name(), "Guilded");
    assert_eq!(g.schemas(), &["guilded"]);
    assert_eq!(g.details().protocols, vec!["guilded"]);
    assert!(g.details().attachment_support);
    assert_eq!(g.url_identifier(), Some(vec!["guilded".to_string(), "A".repeat(24), "B".repeat(64)]));
  }

  /// Markdown input only uses embeds when Markdown was asked for, as Discord.
  #[tokio::test]
  async fn test_markdown_content() {
    let server = MockServer::start().await;
    let hook = format!("/webhooks/{}/{}", "i".repeat(24), "t".repeat(64));
    Mock::given(method("POST")).and(path(hook.as_str())).respond_with(ResponseTemplate::new(204)).mount(&server).await;

    for (extra, embeds) in [("", false), ("?format=markdown", true)] {
      let g = guilded(&format!("guilded://{}/{}/{}", "i".repeat(24), "t".repeat(64), extra));
      let ctx = NotifyContext { body: "## Summary\nAll **good**".into(), body_format: NotifyFormat::Markdown, ..Default::default() };
      assert!(with_test_base(server.uri(), g.send(&ctx)).await.unwrap());
      let requests = server.received_requests().await.unwrap();
      let last = requests.iter().rev().find(|r| r.url.path() == hook).unwrap();
      let payload: serde_json::Value = serde_json::from_slice(&last.body).unwrap();
      assert_eq!(payload.get("embeds").is_some(), embeds);
      assert_eq!(payload.get("content").is_some(), !embeds);
    }
  }

  /// The request matches upstream's (pycapture): Discord's payload, sent
  /// to Guilded's webhook path.
  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/webhooks/1234567890/abcdef")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let g = guilded("guilded://mybot@1234567890/abcdef");
    let ctx = NotifyContext { title: "apprise parity".into(), body: "hello from python".into(), ..Default::default() };
    assert!(with_test_base(server.uri(), g.send(&ctx)).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    let request = requests.iter().find(|r| r.url.path() == "/webhooks/1234567890/abcdef").unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
      payload,
      serde_json::json!({
        "tts": false,
        "wait": true,
        "avatar_url": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-256x256.png",
        "username": "mybot",
        "content": "apprise parity\r\nhello from python",
      })
    );
  }

  #[tokio::test]
  async fn test_send_failure() {
    let server = MockServer::start().await;
    let hook = format!("/webhooks/{}/{}", "a".repeat(24), "b".repeat(64));
    Mock::given(method("POST")).and(path(hook.as_str())).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    let g = guilded(&format!("guilded://{}/{}/", "a".repeat(24), "b".repeat(64)));
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(!with_test_base(server.uri(), g.send(&ctx)).await.unwrap());
  }
}
