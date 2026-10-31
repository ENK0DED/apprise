use super::*;
use wiremock::matchers::{basic_auth, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn write_tmp(dir: &tempfile::TempDir, name: &str, data: &[u8]) -> String {
  let p = dir.path().join(name);
  std::fs::write(&p, data).unwrap();
  p.to_string_lossy().into_owned()
}

#[tokio::test]
async fn test_plain_path() {
  let dir = tempfile::tempdir().unwrap();
  let p = write_tmp(&dir, "report.json", b"{}");
  let att = load_attachment(&p).await.unwrap();
  assert_eq!((att.name.as_str(), att.mime_type.as_str(), att.data.as_slice()), ("report.json", "application/json", b"{}".as_slice()));
  // Unknown extensions fall back to octet-stream.
  let p = write_tmp(&dir, "hostname", b"x");
  assert_eq!(load_attachment(&p).await.unwrap().mime_type, "application/octet-stream");
  // A query string on a plain path is part of the file name.
  assert!(matches!(load_attachment(&format!("{p}?mime=bogus/type")).await, Err(AttachError::NotFound(_))));
}

#[tokio::test]
async fn test_file_url_options() {
  let dir = tempfile::tempdir().unwrap();
  let p = write_tmp(&dir, "hostname", b"data");
  let att = load_attachment(&format!("file://{p}?name=other.png")).await.unwrap();
  assert_eq!((att.name.as_str(), att.mime_type.as_str()), ("other.png", "image/png"));
  // Directories are stripped from name=; an empty name is ignored.
  let att = load_attachment(&format!("file://{p}?name=../../etc/my.txt&mime=application/pdf")).await.unwrap();
  assert_eq!((att.name.as_str(), att.mime_type.as_str()), ("my.txt", "application/pdf"));
  let att = load_attachment(&format!("file://{p}?name=/")).await.unwrap();
  assert_eq!(att.name, "hostname");
  assert!(matches!(parse_attachment(&format!("file://{p}?mime=bogus/x")), Err(AttachError::InvalidMime(m)) if m == "bogus/x"));
}

#[tokio::test]
async fn test_errors_and_locations() {
  assert!(matches!(parse_attachment("foo://x"), Err(AttachError::UnsupportedSchema(s)) if s == "foo"));
  assert!(matches!(load_attachment("/nonexistent/file").await, Err(AttachError::NotFound(_))));
  let dir = tempfile::tempdir().unwrap();
  assert!(matches!(load_attachment(&dir.path().to_string_lossy()).await, Err(AttachError::NotFound(_))));
  assert!(matches!(parse_attachment_from("/etc/hostname", ContentLocation::Hosted), Err(AttachError::Disallowed(_))));
  assert!(parse_attachment_from("https://example.com/a.png", ContentLocation::Hosted).is_ok());
  assert!(matches!(parse_attachment_from("https://example.com/a.png", ContentLocation::Inaccessible), Err(AttachError::Disallowed(_))));
}

#[tokio::test]
async fn test_http_attachment_request() {
  crate::ensure_crypto_provider();
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/file/x.bin"))
    .and(header("X-Token", "abc"))
    .and(header("Drop", "1"))
    .and(header("User-Agent", crate::notify::APP_ID))
    .and(basic_auth("user", "pw"))
    .and(query_param("foo", "bar"))
    .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello attachment".to_vec()).insert_header("Content-Type", "text/plain; charset=utf-8"))
    .expect(1)
    .mount(&server)
    .await;
  let base = server.uri().replace("http://", "http://user:pw@");
  let source = parse_attachment(&format!("{base}/file/x.bin?+X-Token=abc&-Drop=1&foo=bar&name=../../etc/my.txt&verify=no&cache=no")).unwrap();
  let AttachSource::Http(h) = &source else { panic!() };
  // Headers, credentials and reserved arguments are not sent as query args.
  assert_eq!(h.url.query(), Some("foo=bar"));
  assert!(!h.display.contains(":pw@"), "{}", h.display);
  let att = source.load().await.unwrap();
  assert_eq!((att.name.as_str(), att.mime_type.as_str(), att.data.as_slice()), ("my.txt", "text/plain; charset=utf-8", b"hello attachment".as_slice()));
}

#[tokio::test]
async fn test_http_attachment_names() {
  crate::ensure_crypto_provider();
  let server = MockServer::start().await;
  Mock::given(path("/dl"))
    .respond_with(ResponseTemplate::new(200).set_body_bytes(b"x".to_vec()).insert_header("Content-Disposition", "attachment; filename=\"report.txt\""))
    .mount(&server)
    .await;
  Mock::given(path("/ok/thing.json")).respond_with(ResponseTemplate::new(200).set_body_bytes(b"{}".to_vec())).mount(&server).await;
  Mock::given(path("/ok/")).respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok".to_vec())).mount(&server).await;
  Mock::given(path("/missing")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
  Mock::given(path("/moved")).respond_with(ResponseTemplate::new(302).insert_header("Location", "/ok/")).mount(&server).await;
  let att = load_attachment(&format!("{}/dl", server.uri())).await.unwrap();
  assert_eq!((att.name.as_str(), att.mime_type.as_str()), ("report.txt", "text/plain"));
  let att = load_attachment(&format!("{}/ok/thing.json", server.uri())).await.unwrap();
  assert_eq!((att.name.as_str(), att.mime_type.as_str()), ("thing.json", "application/json"));
  let att = load_attachment(&format!("{}/ok/", server.uri())).await.unwrap();
  assert_eq!((att.name.as_str(), att.mime_type.as_str()), ("apprise-attachment.bin", "application/octet-stream"));
  assert!(matches!(load_attachment(&format!("{}/missing", server.uri())).await, Err(AttachError::NotFound(_))));
  // Redirects are followed unless redirect=no.
  assert_eq!(load_attachment(&format!("{}/moved", server.uri())).await.unwrap().data, b"ok");
  assert!(load_attachment(&format!("{}/moved?redirect=no", server.uri())).await.is_err());
}

#[test]
fn test_mime_helpers() {
  assert!(is_known_mime("text/plain"));
  assert!(!is_known_mime("bogus/type"));
  assert_eq!(guess_extension("image/jpeg").as_deref(), Some(".jpg"));
  assert_eq!(guess_extension("application/json").as_deref(), Some(".json"));
  assert_eq!(guess_extension("text/plain; charset=utf-8").as_deref(), Some(".txt"));
  assert_eq!(guess_mime("a.webp").as_deref(), Some("image/webp"));
  assert_eq!(guess_mime("a.csv").as_deref(), Some("text/csv"));
  assert_eq!(final_name(None, None, "bogus/none"), "apprise-attachment.obj");
}

#[test]
fn test_names_lose_control_characters() {
  // %0d%0a in an attachment URL decodes to CR/LF, which would inject headers
  assert_eq!(safe_name("a.txt\"\r\nContent-Type: text/html\r\n\r\nX"), "a.txt\"Content-Type: text/htmlX");
  assert_eq!(final_name(Some("bad\r\n\u{0}name.txt"), None, "text/plain"), "badname.txt");
  // A name of nothing but control characters falls back to the default
  assert_eq!(final_name(Some("\r\n"), None, "text/plain"), "apprise-attachment.txt");
}

#[tokio::test]
async fn test_source_limit_refuses_more_than_asked() {
  let server = MockServer::start().await;
  Mock::given(method("GET")).and(path("/key.pem")).respond_with(ResponseTemplate::new(200).set_body_string("k".repeat(9000))).mount(&server).await;
  let source = parse_attachment(&format!("{}/key.pem", server.uri())).unwrap();
  assert!(matches!(source.clone().with_max_file_size(8000).load().await, Err(AttachError::TooLarge { limit: 8000, .. })));
  assert_eq!(source.with_max_file_size(9000).load().await.unwrap().data.len(), 9000);
  // Key files go through the same limit
  assert!(crate::utils::keyfile::read_source(&format!("{}/key.pem", server.uri()), 8000).await.is_none());
}

#[tokio::test]
async fn test_http_attachment_redirects_follow_the_asset_default() {
  use crate::notify::{HttpOptions, NotifyContext, with_http_options};
  use crate::{Apprise, AppriseAsset};
  let server = MockServer::start().await;
  Mock::given(method("GET")).and(path("/start")).respond_with(ResponseTemplate::new(302).insert_header("Location", "/target")).mount(&server).await;
  Mock::given(method("GET")).and(path("/target")).respond_with(ResponseTemplate::new(200).set_body_string("attached")).mount(&server).await;
  Mock::given(method("POST")).and(path("/notify")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
  let target = format!("json://{}/notify", server.address());
  let ctx = NotifyContext { body: "body".into(), ..Default::default() };

  // The same cases as upstream: the asset decides unless the URL says ?redirect=
  for (http_redirects, suffix, sent) in [(true, "", true), (false, "", false), (false, "?redirect=yes", true)] {
    let mut a = Apprise::with_asset(AppriseAsset { http_redirects, ..Default::default() });
    a.add(&target);
    let r = a.notify_with_attachments(&ctx, &[format!("{}/start{suffix}", server.uri())]).await;
    assert_eq!(r.is_success(), sent, "http_redirects={http_redirects} suffix={suffix:?}");
  }
  let requests = server.received_requests().await.unwrap();
  let gets = |p: &str| requests.iter().filter(|r| r.method.as_str() == "GET" && r.url.path() == p).count();
  assert_eq!((gets("/start"), gets("/target")), (3, 2));

  // A load inside a send (templates, key files) takes the default from the
  // service's settings the same way
  let source = parse_attachment(&format!("{}/start", server.uri())).unwrap();
  let refused = HttpOptions { redirect: Some(false), ..Default::default() };
  assert!(with_http_options(refused.clone(), source.load()).await.is_err());
  assert_eq!(source.load().await.unwrap().data, b"attached");
  let source = parse_attachment(&format!("{}/start?redirect=yes", server.uri())).unwrap();
  assert_eq!(with_http_options(refused, source.load()).await.unwrap().data, b"attached");
}

/// Serves `size` bytes at `/big` without a Content-Length (the body ends
/// when the connection closes), so the limit must apply while reading.
async fn unsized_server(size: usize) -> String {
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  tokio::spawn(async move {
    while let Ok((mut stream, _)) = listener.accept().await {
      tokio::spawn(async move {
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
          match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
          }
        }
        // Anything else (stray requests from other local processes) is a 404
        let response = if request.starts_with(b"GET /big ") {
          [b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(), &vec![b'x'; size]].concat()
        } else {
          b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        };
        let _ = stream.write_all(&response).await;
      });
    }
  });
  format!("http://{addr}/big")
}

/// Each source keeps its own limit (upstream `attach[0].max_file_size`),
/// `0` meaning none, for a file, a download with and without a
/// Content-Length, and bytes in memory (`u3_size.py` against upstream).
#[tokio::test]
async fn test_each_source_has_its_own_limit() {
  let dir = tempfile::tempdir().unwrap();
  let server = MockServer::start().await;
  Mock::given(method("GET")).and(path("/big2048")).respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 2048])).mount(&server).await;
  let sources = [
    parse_attachment(&write_tmp(&dir, "big.bin", &[b'x'; 2048])).unwrap(),
    parse_attachment(&format!("{}/big2048", server.uri())).unwrap(),
    parse_attachment(&unsized_server(2048).await).unwrap(),
    memory::MemorySource::new("big.bin", vec![b'x'; 2048], "").into(),
  ];
  for source in sources {
    assert_eq!(source.max_file_size(), MAX_FILE_SIZE);
    for (limit, fits) in [(1024, false), (2047, false), (2048, true), (0, true), (4096, true)] {
      let loaded = source.clone().with_max_file_size(limit).load().await;
      match loaded {
        Ok(att) => assert!(fits && att.data.len() == 2048, "{} limit={limit}", source.loggable()),
        Err(AttachError::TooLarge { limit: l, .. }) => assert!(!fits && l == limit, "{} limit={limit}", source.loggable()),
        Err(e) => panic!("{} limit={limit}: {e}", source.loggable()),
      }
    }
  }
}

/// Through Apprise, an attachment over its limit fails the services that
/// take attachments and the others still send the message, as upstream;
/// `memory://` names an empty memory attachment, which never loads.
#[tokio::test]
async fn test_attachment_over_its_limit_fails_only_services_with_attachments() {
  use crate::notify::NotifyContext;
  let dir = tempfile::tempdir().unwrap();
  let server = MockServer::start().await;
  Mock::given(method("GET")).and(path("/big2048")).respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 2048])).mount(&server).await;
  Mock::given(method("POST")).and(path("/")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
  Mock::given(method("POST")).and(path("/message")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
  let mut a = crate::Apprise::new();
  a.add(&format!("json://{}/", server.address()));
  a.add(&format!("gotify://{}/abcdefghijklmno", server.address()));
  let ctx = NotifyContext { body: "body".into(), ..Default::default() };
  let posts = || async {
    let requests = server.received_requests().await.unwrap();
    let count = |p: &str| requests.iter().filter(|r| r.method.as_str() == "POST" && r.url.path() == p).count();
    (count("/"), count("/message"))
  };

  let sources = [
    parse_attachment(&write_tmp(&dir, "big.bin", &[b'x'; 2048])).unwrap(),
    parse_attachment(&format!("{}/big2048", server.uri())).unwrap(),
    parse_attachment(&unsized_server(2048).await).unwrap(),
    memory::MemorySource::new("big.bin", vec![b'x'; 2048], "").into(),
  ];
  let mut sent = (0, 0);
  for source in &sources {
    let r = a.notify_sources(&ctx, &crate::TagFilter::All, std::slice::from_ref(&source.clone().with_max_file_size(1024))).await;
    assert_eq!(r.status, crate::ResultStatus::Partial, "{}", source.loggable());
    sent.1 += 1;
    assert_eq!(posts().await, sent, "{}", source.loggable());
    let r = a.notify_sources(&ctx, &crate::TagFilter::All, std::slice::from_ref(&source.clone().with_max_file_size(0))).await;
    assert!(r.is_success(), "{}", source.loggable());
    sent = (sent.0 + 1, sent.1 + 1);
    assert_eq!(posts().await, sent, "{}", source.loggable());
  }

  let r = a.notify_with_attachments(&ctx, &["memory://x.txt".into()]).await;
  assert_eq!(r.status, crate::ResultStatus::Partial);
  assert_eq!(posts().await, (sent.0, sent.1 + 1));

  // A hosted instance accepts web content only
  a.set_location(ContentLocation::Hosted);
  let memory: AttachSource = memory::MemorySource::text("x").into();
  assert_eq!(a.notify_sources(&ctx, &crate::TagFilter::All, &[memory]).await.status, crate::ResultStatus::Failure);
  assert_eq!(posts().await, (sent.0, sent.1 + 1));
}
