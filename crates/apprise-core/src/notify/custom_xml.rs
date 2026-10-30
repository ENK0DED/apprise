use crate::error::NotifyError;
use crate::notify::custom_json::{attachment_filename, basic_auth, custom_args, fullpath, header_map, parse_method};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use base64::Engine;

/// Schema version placed in every payload.
const XSD_VER: &str = "1.1";

/// Published schema, referenced only while the payload keeps its default shape.
const XSD_DEFAULT_URL: &str = "https://raw.githubusercontent.com/caronc/apprise/master/apprise/assets/NotifyXML-1.1.xsd";

/// The core elements, in payload order. `:Subject=Title` renames one and
/// `:Subject=` drops it.
const CORE_FIELDS: [&str; 4] = ["Version", "Subject", "Message", "MessageType"];

pub struct Xml {
  host: String,
  port: Option<u16>,
  fullpath: String,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  method: String,
  headers: Vec<(String, String)>,
  /// Element name for each of [`CORE_FIELDS`]; empty drops the element.
  payload_map: [String; 4],
  payload_extras: Vec<(String, String)>,
  xsd_url: Option<&'static str>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Xml {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let method = parse_method(url)?;

    let mut payload_map = CORE_FIELDS.map(String::from);
    let mut overridden = false;
    let mut payload_extras: Vec<(String, String)> = Vec::new();
    for (k, v) in custom_args(url, ':') {
      let key: String = k.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
      if key.is_empty() {
        tracing::warn!("Ignoring invalid XML Stanza element name({})", k);
        continue;
      }
      if let Some(i) = CORE_FIELDS.iter().position(|f| *f == key) {
        payload_map[i] = v;
        overridden = true;
      } else {
        upsert(&mut payload_extras, key, v);
      }
    }
    let xsd_url = if overridden || !payload_extras.is_empty() { None } else { Some(XSD_DEFAULT_URL) };

    // Upstream stores `-` GET parameters for url() but never sends them
    Some(Self {
      host,
      port: url.port,
      fullpath: fullpath(url),
      secure: url.schema == "xmls",
      user: url.user.clone(),
      password: url.password.clone(),
      method,
      headers: custom_args(url, '+'),
      payload_map,
      payload_extras,
      xsd_url,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "XML",
      service_url: None,
      setup_url: Some("https://appriseit.com/services/xml/"),
      protocols: vec!["xml", "xmls"],
      description: "Send an XML notification to any HTTP endpoint.",
      attachment_support: true,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> String {
    let values = [XSD_VER.to_string(), escape(&ctx.title), escape(&ctx.body), escape(ctx.notify_type.as_str())];
    let mut elements: Vec<(String, String)> = Vec::new();
    for (name, value) in self.payload_map.iter().zip(values) {
      if !name.is_empty() {
        upsert(&mut elements, name.clone(), value);
      }
    }
    for (k, v) in &self.payload_extras {
      upsert(&mut elements, k.clone(), escape(v));
    }
    let core: String = elements.iter().map(|(k, v)| format!("<{k}>{v}</{k}>")).collect();

    let attachments = if ctx.attachments.is_empty() {
      String::new()
    } else {
      let entries: String = ctx
        .attachments
        .iter()
        .enumerate()
        .map(|(i, att)| {
          format!(
            "<Attachment filename=\"{}\" mimetype=\"{}\">{}</Attachment>",
            escape(&attachment_filename(att, i + 1)),
            escape(&att.mime_type),
            base64::engine::general_purpose::STANDARD.encode(&att.data)
          )
        })
        .collect();
      format!("<Attachments format=\"base64\">{}</Attachments>", entries)
    };

    let xsd = self.xsd_url.map(|u| format!(" xmlns:xsi=\"{}\"", u)).unwrap_or_default();
    format!(
      "<?xml version='1.0' encoding='utf-8'?>
<soapenv:Envelope
    xmlns:soapenv=\"http://schemas.xmlsoap.org/soap/envelope/\"
    xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\"
    xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">
    <soapenv:Body>
        <Notification{xsd}>
            {core}
            {attachments}
       </Notification>
    </soapenv:Body>
</soapenv:Envelope>"
    )
  }
}

/// Insert or replace `key`, keeping its first position (Python dict order).
fn upsert(list: &mut Vec<(String, String)>, key: String, value: String) {
  match list.iter_mut().find(|(k, _)| *k == key) {
    Some(entry) => entry.1 = value,
    None => list.push((key, value)),
  }
}

/// Upstream `escape_html(..., whitespace=False)`: XML-escape `&`, `<`, `>`,
/// `"` and `'`.
fn escape(s: &str) -> String {
  s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[async_trait]
impl Notify for Xml {
  fn schemas(&self) -> &[&str] {
    &["xml", "xmls"]
  }
  fn service_name(&self) -> &str {
    "XML"
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

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}{}", schema, self.host, port_str, self.fullpath);

    let headers = header_map(&[("User-Agent", APP_ID), ("Content-Type", "application/xml")], &self.headers)?;
    let method = reqwest::Method::from_bytes(self.method.as_bytes()).map_err(|_| NotifyError::InvalidParam(self.method.clone()))?;

    tracing::debug!("XML {} URL: {} (cert_verify={})", self.method, url, self.verify_certificate);

    let client = build_client(self.verify_certificate)?;
    let mut req = client.request(method, &url).headers(headers).body(self.payload(ctx));
    if let Some((user, password)) = basic_auth(&self.user, &self.password) {
      req = req.basic_auth(user, Some(password));
    }

    let resp = req.send().await?;
    let status = resp.status();
    if status.is_success() {
      tracing::info!("Sent XML {} notification.", self.method);
      Ok(true)
    } else {
      tracing::warn!("Failed to send XML {} notification: error={}.", self.method, status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "xml://localhost",
      "xml://user@localhost",
      "xml://user:pass@localhost",
      "xml://user@localhost?method=put",
      "xml://user@localhost?method=get",
      "xml://user@localhost?method=post",
      "xml://user@localhost?method=head",
      "xml://user@localhost?method=delete",
      "xml://user@localhost?method=patch",
      "xml://user@localhost?method=update",
      "xml://user@localhost?method=options",
      "xml://localhost:8080",
      "xml://user:pass@localhost:8080",
      "xmls://localhost",
      "xmls://user:pass@localhost",
      "xml://user@localhost:8080/path/",
      "xmls://localhost:8080/path/",
      "xmls://user:pass@localhost:8080",
      "xml://localhost:8080/path?-ParamA=Value",
      "xml://localhost:8080/path?+HeaderKey=HeaderValue",
      "xml://user:pass@localhost:8081",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["xml://:@/", "xml://", "xmls://", "xml://user@localhost?method=invalid"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  fn xml_for(url: &str) -> Xml {
    Xml::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_default_payload_matches_upstream() {
    let x = xml_for("xml://localhost");
    assert_eq!(x.xsd_url, Some(XSD_DEFAULT_URL));
    let mut c = ctx("T<&>\"'", "B\tc d");
    c.attachments.push(Attachment { name: "a.txt".into(), data: b"hello".to_vec(), mime_type: "text/plain".into() });
    // Captured from upstream with pycapture.py
    let expected = "<?xml version='1.0' encoding='utf-8'?>
<soapenv:Envelope
    xmlns:soapenv=\"http://schemas.xmlsoap.org/soap/envelope/\"
    xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\"
    xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">
    <soapenv:Body>
        <Notification xmlns:xsi=\"https://raw.githubusercontent.com/caronc/apprise/master/apprise/assets/NotifyXML-1.1.xsd\">
            <Version>1.1</Version><Subject>T&lt;&amp;&gt;&quot;&apos;</Subject><Message>B\tc d</Message><MessageType>info</MessageType>
            <Attachments format=\"base64\"><Attachment filename=\"a.txt\" mimetype=\"text/plain\">aGVsbG8=</Attachment></Attachments>
       </Notification>
    </soapenv:Body>
</soapenv:Envelope>";
    assert_eq!(x.payload(&c), expected);
  }

  #[test]
  fn test_payload_overrides_and_extras() {
    let x = xml_for("xml://localhost?:Message=Body&:Subject=&:Version=&:My$Key=v<1&:extra=2&:,=invalid");
    assert_eq!(x.xsd_url, None);
    let expected = "<?xml version='1.0' encoding='utf-8'?>
<soapenv:Envelope
    xmlns:soapenv=\"http://schemas.xmlsoap.org/soap/envelope/\"
    xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\"
    xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">
    <soapenv:Body>
        <Notification>
            <Body>B</Body><MessageType>info</MessageType><MyKey>v&lt;1</MyKey><extra>2</extra>\n            \n       </Notification>
    </soapenv:Body>
</soapenv:Envelope>";
    assert_eq!(x.payload(&ctx("T", "B")), expected);

    // Upstream edge case: overrides are case-sensitive and placeholders in
    // the content are not substituted again
    let x = xml_for("xml://localhost:8080/command?:Message=Body&method=GET&:Key=value&:,=invalid&:MessageType=");
    let p = x.payload(&ctx("title", "{{ATTACHMENTS}}"));
    assert!(p.contains("<Version>1.1</Version><Subject>title</Subject><Body>{{ATTACHMENTS}}</Body><Key>value</Key>"));
    assert!(!p.contains("<Message>") && !p.contains("<MessageType>"));

    // A new element alone also drops the schema reference
    assert_eq!(xml_for("xml://localhost:8081/command?method=POST&:New=Value").xsd_url, None);
  }

  #[tokio::test]
  async fn test_send_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/command"))
      .and(header("Content-Type", "application/xml"))
      .and(header("User-Agent", APP_ID))
      .and(header("X-H", "v+1"))
      .and(header("Authorization", "Basic dXNlcjpOb25l"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let x = xml_for(&format!("xml://user@localhost:{}/command?method=GET&+X-H=v+1&-Param=1", server.address().port()));
    assert!(x.send(&ctx("t", "b")).await.unwrap());

    // GET parameters are not sent, as upstream
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].url.query(), None);
    assert_eq!(String::from_utf8_lossy(&requests[0].body), x.payload(&ctx("t", "b")));
  }

  #[tokio::test]
  async fn test_send_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(404)).expect(1).mount(&server).await;
    let x = xml_for(&format!("xml://localhost:{}/", server.address().port()));
    assert!(matches!(x.send(&ctx("t", "b")).await, Err(NotifyError::ServiceError { status: 404, .. })));
    assert!(matches!(xml_for("xml://127.0.0.1:1/").send(&ctx("t", "b")).await, Err(NotifyError::Http(_))));
  }

  #[test]
  fn test_attachment_filename_fallback() {
    let x = xml_for("xml://localhost");
    let mut c = ctx("t", "b");
    c.attachments.push(Attachment { name: String::new(), data: b"x".to_vec(), mime_type: "a\"b".into() });
    assert!(x.payload(&c).contains("<Attachment filename=\"file001.dat\" mimetype=\"a&quot;b\">eA==</Attachment>"));
  }
}
