use crate::error::NotifyError;
use crate::notify::custom_json::{attachment_filename, basic_auth, custom_args, fullpath, header_map, parse_method};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use crate::utils::qsd;
use async_trait::async_trait;

/// The core fields, in payload order. `:message=text` renames one and
/// `:message=` drops it.
const CORE_FIELDS: [&str; 4] = ["version", "title", "message", "type"];

/// Field name used for uploaded files (`?attach-as=`).
#[derive(Debug, Clone, PartialEq)]
struct AttachAs {
  prefix: String,
  suffix: String,
  /// Number each file (`file01`, `file02`, ...) between prefix and suffix.
  numbered: bool,
}

impl AttachAs {
  /// Upstream default `file*`: `file01`, `file02`, ...
  fn default_name() -> Self {
    Self { prefix: "file".into(), suffix: String::new(), numbered: true }
  }

  /// Parse `?attach-as=` like upstream's `__attach_as_re`: one run of
  /// wildcards (`*?+$:.%`) between two names becomes the file number, a
  /// single trailing wildcard appends it, and a bare name is used as-is.
  /// Anything after the matched part is ignored, as `re.match` does.
  fn parse(value: &str) -> Option<Self> {
    let is_id = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let is_wc = |c: char| "*?+$:.%".contains(c);
    let s = value.trim();

    let head: String = s.chars().take_while(|c| is_id(*c)).collect();
    let rest = &s[head.len()..];
    let wildcards = rest.chars().take_while(|c| is_wc(*c)).count();
    let tail: String = rest[wildcards..].chars().take_while(|c| is_id(*c)).collect();

    if wildcards > 0 && !tail.is_empty() {
      Some(Self { prefix: head, suffix: tail, numbered: true })
    } else if !head.is_empty() {
      Some(Self { prefix: head, suffix: String::new(), numbered: wildcards > 0 })
    } else {
      None
    }
  }

  /// The field name for attachment `no` (1-based).
  fn field(&self, no: usize) -> String {
    if self.numbered { format!("{}{:02}{}", self.prefix, no, self.suffix) } else { self.prefix.clone() }
  }
}

pub struct Form {
  host: String,
  port: Option<u16>,
  fullpath: String,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  method: String,
  headers: Vec<(String, String)>,
  params: Vec<(String, String)>,
  /// Field name for each of [`CORE_FIELDS`]; empty drops the field.
  payload_map: [String; 4],
  payload_extras: Vec<(String, String)>,
  attach_as: AttachAs,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Form {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let method = parse_method(url)?;

    let attach_as = match qsd::get(url, "attach-as").filter(|v| !v.is_empty()) {
      None => AttachAs::default_name(),
      Some(v) => match AttachAs::parse(&v) {
        Some(a) => a,
        None => {
          tracing::warn!("The attach-as specified ({}) is invalid.", v);
          return None;
        }
      },
    };

    let mut payload_map = CORE_FIELDS.map(String::from);
    let mut payload_extras = Vec::new();
    for (k, v) in custom_args(url, ':') {
      match CORE_FIELDS.iter().position(|f| *f == k) {
        Some(i) => payload_map[i] = v,
        None => payload_extras.push((k, v)),
      }
    }

    Some(Self {
      host,
      port: url.port,
      fullpath: fullpath(url),
      secure: url.schema == "forms",
      user: url.user.clone(),
      password: url.password.clone(),
      method,
      headers: custom_args(url, '+'),
      params: custom_args(url, '-'),
      payload_map,
      payload_extras,
      attach_as,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Form",
      service_url: None,
      setup_url: Some("https://appriseit.com/services/form/"),
      protocols: vec!["form", "forms"],
      description: "Send a form-encoded notification to any HTTP endpoint.",
      attachment_support: true,
    }
  }

  /// The form fields, in upstream's order.
  fn payload(&self, ctx: &NotifyContext) -> Vec<(String, String)> {
    let values = ["1.0", ctx.title.as_str(), ctx.body.as_str(), ctx.notify_type.as_str()];
    let mut payload: Vec<(String, String)> = Vec::new();
    let core = self.payload_map.iter().zip(values).filter(|(name, _)| !name.is_empty()).map(|(name, v)| (name.clone(), v.to_string()));
    for (k, v) in core.chain(self.payload_extras.iter().cloned()) {
      upsert(&mut payload, k, v);
    }
    payload
  }
}

/// Insert or replace `key`, keeping its first position (Python dict order).
fn upsert(list: &mut Vec<(String, String)>, key: String, value: String) {
  match list.iter_mut().find(|(k, _)| *k == key) {
    Some(entry) => entry.1 = value,
    None => list.push((key, value)),
  }
}

#[async_trait]
impl Notify for Form {
  fn schemas(&self) -> &[&str] {
    &["form", "forms"]
  }
  fn service_name(&self) -> &str {
    "Form"
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

    if !self.attach_as.numbered && ctx.attachments.len() > 1 {
      tracing::warn!("Multiple attachments provided while form:// Multi-Attachment Support not enabled");
    }

    let mut payload = self.payload(ctx);
    let is_get = self.method == "GET";
    let query = if is_get {
      // GET sends the payload, with the `-` parameters on top, as the query
      for (k, v) in &self.params {
        upsert(&mut payload, k.clone(), v.clone());
      }
      std::mem::take(&mut payload)
    } else {
      self.params.clone()
    };

    let headers = header_map(&[("User-Agent", APP_ID)], &self.headers)?;
    let method = reqwest::Method::from_bytes(self.method.as_bytes()).map_err(|_| NotifyError::InvalidParam(self.method.clone()))?;

    tracing::debug!("Form {} URL: {} (cert_verify={})", self.method, url, self.verify_certificate);

    let client = build_client(self.verify_certificate)?;
    let mut req = client.request(method, &url).query(&query);
    if !ctx.attachments.is_empty() {
      // Fields first, then the files, as `requests` lays out the multipart body
      let mut form = reqwest::multipart::Form::new();
      for (k, v) in payload {
        form = form.text(k, v);
      }
      for (i, att) in ctx.attachments.iter().enumerate() {
        let part = reqwest::multipart::Part::bytes(att.data.clone())
          .file_name(attachment_filename(att, i + 1))
          .mime_str(&att.mime_type)
          .map_err(|_| NotifyError::InvalidParam(format!("invalid attachment mime type: {}", att.mime_type)))?;
        form = form.part(self.attach_as.field(i + 1), part);
      }
      req = req.multipart(form);
    } else if !payload.is_empty() {
      req = req.form(&payload);
    }
    // Applied last so a `+Content-Type` override wins, as with `requests`
    req = req.headers(headers);
    if let Some((user, password)) = basic_auth(&self.user, &self.password) {
      req = req.basic_auth(user, Some(password));
    }

    let resp = req.send().await?;
    let status = resp.status();
    if status.is_success() {
      tracing::info!("Sent Form {} notification.", self.method);
      Ok(true)
    } else {
      tracing::warn!("Failed to send Form {} notification: error={}.", self.method, status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "form://localhost",
      "form://user:pass@localhost",
      "form://user@localhost",
      "form://user@localhost?method=put",
      "form://user@localhost?method=get",
      "form://user@localhost?method=post",
      "form://user@localhost?method=head",
      "form://user@localhost?method=delete",
      "form://user@localhost?method=patch",
      "form://user@localhost?method=update",
      "form://user@localhost?method=options",
      "form://localhost:8080?:key=value&:key2=value2",
      "form://localhost:8080",
      "form://user:pass@localhost:8080",
      "forms://localhost",
      "forms://user:pass@localhost",
      "forms://localhost:8080/path/",
      "forms://user:password@localhost:8080",
      "form://localhost:8080/path?-ParamA=Value",
      "form://localhost:8080/path?+HeaderKey=HeaderValue",
      "form://user:pass@localhost:8081",
      "form://localhost/?attach-as=",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["form://:@/", "form://", "forms://", "form://user@localhost?method=invalid", "form://localhost/?attach-as={", "form://localhost/?attach-as=*"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_attach_as() {
    let numbered = |prefix: &str, suffix: &str| Some(AttachAs { prefix: prefix.into(), suffix: suffix.into(), numbered: true });
    assert_eq!(AttachAs::parse("file"), Some(AttachAs { prefix: "file".into(), suffix: String::new(), numbered: false }));
    for wc in ["*", ":", "?", ".", "+", "$", "%"] {
      assert_eq!(AttachAs::parse(&format!("file{wc}")), numbered("file", ""), "{wc}");
      assert_eq!(AttachAs::parse(&format!("{wc}file")), numbered("", "file"), "{wc}");
      assert_eq!(AttachAs::parse(&format!("file{wc}file")), numbered("file", "file"), "{wc}");
    }
    // Wildcard runs collapse and trailing text is ignored, as `re.match` does
    assert_eq!(AttachAs::parse("meta*.data*x"), numbered("meta", "data"));
    assert_eq!(AttachAs::parse(" Upload_File-1 "), Some(AttachAs { prefix: "Upload_File-1".into(), suffix: String::new(), numbered: false }));
    for bad in ["", "  ", "*", "{", "@file"] {
      assert_eq!(AttachAs::parse(bad), None, "{bad}");
    }
    assert_eq!(numbered("img", "x").unwrap().field(3), "img03x");
    assert_eq!(AttachAs::default_name().field(12), "file12");

    // `+` must survive URL parsing
    let f = Form::from_url(&ParsedUrl::parse("form://localhost/?attach-as=file+").unwrap()).unwrap();
    assert_eq!(f.attach_as, AttachAs::default_name());
  }

  fn form_for(url: &str) -> Form {
    Form::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  fn fields(c: &NotifyContext, f: &Form) -> Vec<(String, String)> {
    f.payload(c)
  }

  fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
  }

  #[test]
  fn test_payload_mapping() {
    let f = form_for("form://localhost:8080/command?:message=msg&:abcd=test&method=POST");
    assert_eq!(fields(&ctx("title", "body"), &f), pairs(&[("version", "1.0"), ("title", "title"), ("msg", "body"), ("type", "info"), ("abcd", "test")]));

    let f = form_for("form://localhost:8080/command?:type=&:message=msg&method=POST");
    assert_eq!(fields(&ctx("title", "body"), &f), pairs(&[("version", "1.0"), ("title", "title"), ("msg", "body")]));

    // An extra named like a mapped field replaces its value in place
    let f = form_for("form://localhost/?:message=title&:title=x");
    assert_eq!(fields(&ctx("T", "B"), &f), pairs(&[("version", "1.0"), ("x", "T"), ("title", "B"), ("type", "info")]));
  }

  #[tokio::test]
  async fn test_post_urlencoded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/command"))
      .and(query_param("ParamA", "a+b"))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(header("User-Agent", APP_ID))
      .and(header("X-Key", "v"))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let f = form_for(&format!("form://user:pass@localhost:{}/command?:message=msg&:abcd=test&-ParamA=a+b&+X-Key=v", server.address().port()));
    assert!(f.send(&ctx("title", "body & more")).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    let got: Vec<(String, String)> = url::form_urlencoded::parse(&requests[0].body).into_owned().collect();
    assert_eq!(got, pairs(&[("version", "1.0"), ("title", "title"), ("msg", "body & more"), ("type", "info"), ("abcd", "test")]));
  }

  #[tokio::test]
  async fn test_get_sends_payload_as_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/command"))
      .and(query_param("version", "1.0"))
      .and(query_param("title", "title"))
      .and(query_param("message", "test-override"))
      .and(query_param("type", "info"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let f = form_for(&format!("form://localhost:{}/command?method=GET&-message=test-override", server.address().port()));
    assert!(f.send(&ctx("title", "body")).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].body.is_empty());
    assert!(requests[0].headers.get("Content-Type").is_none());
  }

  #[tokio::test]
  async fn test_multipart_attachments() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let f = form_for(&format!("form://localhost:{}/?attach-as=meta*data", server.address().port()));
    let mut c = ctx("title", "body");
    c.attachments.push(Attachment { name: "a.txt".into(), data: b"hello".to_vec(), mime_type: "text/plain".into() });
    c.attachments.push(Attachment { name: String::new(), data: b"GIF89a".to_vec(), mime_type: "image/gif".into() });
    assert!(f.send(&c).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    let ct = requests[0].headers.get("Content-Type").unwrap().to_str().unwrap().to_string();
    assert!(ct.starts_with("multipart/form-data; boundary="));
    let body = String::from_utf8_lossy(&requests[0].body);
    for expected in [
      "Content-Disposition: form-data; name=\"version\"\r\n\r\n1.0\r\n",
      "Content-Disposition: form-data; name=\"message\"\r\n\r\nbody\r\n",
      "Content-Disposition: form-data; name=\"meta01data\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n",
      "Content-Disposition: form-data; name=\"meta02data\"; filename=\"file002.dat\"\r\nContent-Type: image/gif\r\n\r\nGIF89a\r\n",
    ] {
      assert!(body.contains(expected), "missing {expected:?} in {body}");
    }
    assert!(body.find("name=\"type\"").unwrap() < body.find("meta01data").unwrap());
  }

  #[tokio::test]
  async fn test_single_name_for_every_file() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let f = form_for(&format!("form://localhost:{}/?attach-as=upload", server.address().port()));
    let mut c = ctx("t", "b");
    for _ in 0..2 {
      c.attachments.push(Attachment { name: "x.gif".into(), data: b"GIF89a".to_vec(), mime_type: "image/gif".into() });
    }
    assert!(f.send(&c).await.unwrap());
    let body = String::from_utf8_lossy(&server.received_requests().await.unwrap()[0].body).to_string();
    assert_eq!(body.matches("name=\"upload\"; filename=\"x.gif\"").count(), 2);
  }

  #[tokio::test]
  async fn test_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    let f = form_for(&format!("form://localhost:{}/", server.address().port()));
    assert!(matches!(f.send(&ctx("t", "b")).await, Err(NotifyError::ServiceError { status: 500, .. })));
    assert!(matches!(form_for("form://127.0.0.1:1/").send(&ctx("t", "b")).await, Err(NotifyError::Http(_))));
  }
}
