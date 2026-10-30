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
