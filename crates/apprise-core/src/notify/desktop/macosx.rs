//! macOS notifications (`macosx://`) through terminal-notifier.
//!
//! Install it with `brew install terminal-notifier`. Version 2.x or 3.x is
//! detected from `terminal-notifier -version`; `?version=` overrides that.
//! Version 3 can explain notifications that do not appear with
//! `terminal-notifier -diagnose`.
//! See https://github.com/julienXX/terminal-notifier

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

/// Where terminal-notifier may be installed.
const NOTIFY_PATHS: &[&str] = &[
  "/opt/homebrew/bin/terminal-notifier",
  "/usr/local/bin/terminal-notifier",
  "/usr/bin/terminal-notifier",
  "/bin/terminal-notifier",
  "/opt/local/bin/terminal-notifier",
];

/// Used when the installed version cannot be detected.
const DEFAULT_VERSION: u8 = 3;

/// How long terminal-notifier may take to report its version.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A backstop so a wedged terminal-notifier cannot hold up the caller.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

pub struct MacOSX {
  include_image: bool,
  sound: Option<String>,
  click: Option<String>,
  /// Version 2 only; our app id when not given.
  sender: Option<String>,
  /// The major version asked for in the URL.
  version: Option<u8>,
  notify_path: Option<PathBuf>,
  /// The detected version, probed once.
  detected: Mutex<Option<u8>>,
  probe_timeout: Duration,
  send_timeout: Duration,
  tags: Vec<String>,
}

impl MacOSX {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let given = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(String::from);
    let version = match given("version") {
      None => None,
      Some(v) => match v.trim().parse::<u8>() {
        Ok(n @ (2 | 3)) => Some(n),
        _ => {
          tracing::warn!("The MacOSX terminal-notifier version specified ({}) is invalid.", v);
          return None;
        }
      },
    };
    Some(Self {
      include_image: super::include_image(url),
      sound: given("sound"),
      click: given("click"),
      sender: given("sender"),
      version,
      notify_path: NOTIFY_PATHS.iter().map(PathBuf::from).find(|p| is_executable(p)),
      detected: Mutex::new(None),
      probe_timeout: PROBE_TIMEOUT,
      send_timeout: SEND_TIMEOUT,
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "MacOSX Notification",
      service_url: Some("https://github.com/julienXX/terminal-notifier"),
      setup_url: Some("https://appriseit.com/services/macosx/"),
      protocols: vec!["macosx"],
      description: "Show a macOS notification through terminal-notifier.",
      attachment_support: false,
    }
  }

  /// The selected or detected terminal-notifier major version.
  async fn tn_version(&self, path: &Path) -> u8 {
    if let Some(version) = self.version {
      return version;
    }
    if let Some(version) = *self.detected.lock().unwrap_or_else(|e| e.into_inner()) {
      return version;
    }
    let path = path.to_path_buf();
    let limit = self.probe_timeout;
    let version = tokio::task::spawn_blocking(move || probe_version(&path, limit)).await.unwrap_or(DEFAULT_VERSION);
    *self.detected.lock().unwrap_or_else(|e| e.into_inner()) = Some(version);
    version
  }

  /// terminal-notifier's arguments for this notification.
  fn arguments(&self, ctx: &NotifyContext, version: u8) -> Vec<String> {
    let mut args = vec!["-message".to_string(), escape(&ctx.body)];
    if !ctx.title.is_empty() {
      args.extend(["-title".into(), escape(&ctx.title)]);
    }
    if let Some(click) = &self.click {
      args.extend(["-open".into(), escape(click)]);
    }
    if let Some(sound) = &self.sound {
      args.extend(["-sound".into(), escape(sound)]);
    }
    if version == 2 {
      let sender = self.sender.as_deref().unwrap_or(&ctx.asset.app_id);
      if !sender.is_empty() {
        args.extend(["-sender".into(), escape(sender)]);
      }
    }
    // Version 2 uses the image as the app icon; version 3 attaches it
    if self.include_image {
      let image =
        if version == 2 { super::image_url(&ctx.asset, &ctx.notify_type) } else { super::image_path(&ctx.notify_type).map(|p| p.display().to_string()) };
      if let Some(image) = image {
        args.extend([if version == 2 { "-appIcon" } else { "-contentImage" }.into(), escape(&image)]);
      }
    }
    args
  }
}

#[async_trait]
impl Notify for MacOSX {
  fn schemas(&self) -> &[&str] {
    &["macosx"]
  }
  fn service_name(&self) -> &str {
    "MacOSX Notification"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_max_line_count(&self) -> usize {
    10
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let Some(path) = self.notify_path.as_deref().filter(|p| is_executable(p)) else {
      tracing::warn!("MacOSX Notifications requires one of the following to be in place: '{}'.", NOTIFY_PATHS.join("', '"));
      return Ok(false);
    };

    let version = self.tn_version(path).await;
    let args = self.arguments(ctx, version);
    tracing::debug!("MacOSX CMD: {} {}", path.display(), args.join(" "));

    let program = path.to_path_buf();
    let limit = self.send_timeout;
    let output = tokio::task::spawn_blocking(move || run(&program, &args, limit)).await.map_err(|e| NotifyError::Other(e.to_string()))?;
    let output = match output {
      Ok(output) => output,
      Err(e) => {
        // The program can still vanish or wedge after our check
        tracing::warn!("Failed to run the MacOSX terminal-notifier.");
        tracing::debug!("MacOSX Exception: {}", e);
        return Err(NotifyError::Io(e));
      }
    };

    if !output.status.success() {
      let reason = exit_reason(output.status.code());
      tracing::warn!("Failed to send MacOSX notification: {}", reason);
      tracing::debug!("MacOSX Response: {}", String::from_utf8_lossy(&output.stderr).trim());
      return Err(NotifyError::Other(format!("MacOSX: {}", reason)));
    }
    tracing::info!("Sent MacOSX notification.");
    Ok(true)
  }
}

/// A leading backslash stops terminal-notifier from reading a value as
/// structured data; it removes the backslash again.
fn escape(value: &str) -> String {
  format!("\\{}", value)
}

/// What terminal-notifier's exit code means (version 3 added 2 through 6).
fn exit_reason(code: Option<i32>) -> String {
  match code {
    Some(1) => "No message was provided.".into(),
    Some(2) => "An argument could not be read.".into(),
    Some(3) => "Notifications are not authorized for terminal-notifier. Run 'terminal-notifier -diagnose' to find out why.".into(),
    Some(4) => "Could not reach the notification service; is a user logged in?".into(),
    Some(5) => "The notification service refused the request.".into(),
    Some(6) => "Timed out waiting for a response.".into(),
    Some(code) => format!("terminal-notifier returned {}.", code),
    None => "terminal-notifier was stopped by a signal.".into(),
  }
}

/// Ask terminal-notifier for its version; 2 for anything older than 3.
fn probe_version(path: &Path, limit: Duration) -> u8 {
  match run(path, &["-version".to_string()], limit) {
    // Only a clean exit tells us anything reliable
    Ok(output) if output.status.success() => parse_version(&String::from_utf8_lossy(&output.stdout)).unwrap_or(DEFAULT_VERSION),
    Ok(_) => DEFAULT_VERSION,
    Err(e) => {
      tracing::warn!("Could not determine the MacOSX terminal-notifier version.");
      tracing::debug!("MacOSX Exception: {}", e);
      DEFAULT_VERSION
    }
  }
}

/// The major version from output such as `terminal-notifier 3.1.0.`.
fn parse_version(output: &str) -> Option<u8> {
  let bytes = output.as_bytes();
  // The first run of digits followed by '.' and another digit
  let mut start = 0;
  while start < bytes.len() {
    if !bytes[start].is_ascii_digit() {
      start += 1;
      continue;
    }
    let end = start + bytes[start..].iter().take_while(|b| b.is_ascii_digit()).count();
    if bytes.get(end) == Some(&b'.') && bytes.get(end + 1).is_some_and(u8::is_ascii_digit) {
      // A number too large to read falls back to the default
      let major: u32 = output[start..end].parse().ok()?;
      return Some(if major < 3 { 2 } else { 3 });
    }
    start = end;
  }
  None
}

/// Run `program`, giving up (and killing it) after `limit`.
fn run(program: &Path, args: &[String], limit: Duration) -> std::io::Result<Output> {
  let spawn = || Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
  // A program that was just installed can briefly still be open for writing
  let mut child = spawn();
  for _ in 0..5 {
    match &child {
      Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => std::thread::sleep(Duration::from_millis(20)),
      _ => break,
    }
    child = spawn();
  }
  let mut child = child?;
  let collect = |pipe: Option<Box<dyn Read + Send>>| {
    std::thread::spawn(move || {
      let mut buf = Vec::new();
      if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf);
      }
      buf
    })
  };
  let stdout = collect(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
  let stderr = collect(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));

  let deadline = Instant::now() + limit;
  let status = loop {
    if let Some(status) = child.try_wait()? {
      break status;
    }
    if Instant::now() >= deadline {
      let _ = child.kill();
      let _ = child.wait();
      return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("{} did not finish within {:?}", program.display(), limit)));
    }
    std::thread::sleep(Duration::from_millis(10));
  };
  Ok(Output { status, stdout: stdout.join().unwrap_or_default(), stderr: stderr.join().unwrap_or_default() })
}

fn is_executable(path: &Path) -> bool {
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
  }
  #[cfg(not(unix))]
  {
    path.is_file()
  }
}

#[cfg(all(test, unix))]
mod tests {
  use super::*;
  use crate::asset::AppriseAsset;
  use crate::types::NotifyType;

  fn parse(url: &str) -> Option<MacOSX> {
    ParsedUrl::parse(url).and_then(|p| MacOSX::from_url(&p))
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  /// A surrogate terminal-notifier: reports `version`, records its
  /// arguments, then runs `then` (e.g. `exit 3`).
  struct Surrogate {
    dir: tempfile::TempDir,
  }

  impl Surrogate {
    fn new(version_output: &str, then: &str) -> Self {
      let dir = tempfile::tempdir().unwrap();
      let surrogate = Self { dir };
      surrogate.write(version_output, then);
      surrogate
    }

    fn write(&self, version_output: &str, then: &str) {
      self.write_raw(&format!(
        "#!/bin/sh\nif [ \"$1\" = \"-version\" ]; then\n  printf '%s' '{}'\n  exit 0\nfi\nprintf '%s\\0' \"$@\" > \"$0.args\"\n{}\n",
        version_output, then
      ));
    }

    fn write_raw(&self, script: &str) {
      use std::os::unix::fs::PermissionsExt;
      let path = self.path();
      // Replace rather than rewrite in place, which a running copy could be reading
      let tmp = self.dir.path().join("tn.tmp");
      std::fs::write(&tmp, script).unwrap();
      std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
      std::fs::rename(&tmp, &path).unwrap();
    }

    fn path(&self) -> PathBuf {
      self.dir.path().join("terminal-notifier")
    }

    fn args(&self) -> Vec<String> {
      let raw = std::fs::read(self.dir.path().join("terminal-notifier.args")).unwrap();
      raw.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
    }

    fn notifier(&self, url: &str) -> MacOSX {
      let mut m = parse(url).unwrap();
      m.notify_path = Some(self.path());
      m
    }
  }

  fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str())
  }

  #[test]
  fn test_urls() {
    for url in [
      "macosx://",
      "macosx://_/?image=True",
      "macosx://_/?image=False",
      "macosx://_/?sound=default",
      "macosx://_/?click=http://google.com",
      "macosx://_/?version=2",
      "macosx://_/?version=3&sender=me",
    ] {
      assert!(parse(url).is_some(), "should load: {}", url);
    }
    for url in ["macosx://_/?version=99", "macosx://_/?version=abc", "macosx://_/?version=3.0"] {
      assert!(parse(url).is_none(), "should not load: {}", url);
    }
    let m = parse("macosx://_/?click=http://google.com&sound=default&sender=me&image=no").unwrap();
    assert_eq!(m.click.as_deref(), Some("http://google.com"));
    assert_eq!(m.sound.as_deref(), Some("default"));
    assert_eq!(m.sender.as_deref(), Some("me"));
    assert!(!m.include_image);
    assert!(m.version.is_none());
  }

  #[test]
  fn test_parse_version() {
    assert_eq!(parse_version("terminal-notifier 2.0.0."), Some(2));
    assert_eq!(parse_version("terminal-notifier 3.1.0."), Some(3));
    assert_eq!(parse_version("terminal-notifier 4.0.0."), Some(3));
    assert_eq!(parse_version("terminal-notifier 1.7"), Some(2));
    assert_eq!(parse_version(""), None);
    assert_eq!(parse_version("terminal-notifier 3"), None);
    assert_eq!(parse_version(&format!("terminal-notifier {}.0", "9".repeat(5000))), None);
  }

  #[tokio::test]
  async fn test_escaping() {
    let tn = Surrogate::new("terminal-notifier 3.1.0.", "exit 0");
    let m = tn.notifier("macosx://_/?sound=default&click=http://google.com&image=no");
    assert!(m.send(&ctx("[Alert]", "12345")).await.unwrap());
    assert_eq!(tn.args(), vec!["-message", "\\12345", "-title", "\\[Alert]", "-open", "\\http://google.com", "-sound", "\\default"]);
  }

  #[tokio::test]
  async fn test_version_detection() {
    let tn = Surrogate::new("terminal-notifier 3.1.0.", "exit 0");
    let m = tn.notifier("macosx://");
    assert_eq!(m.tn_version(&tn.path()).await, 3);
    // The answer is remembered rather than probed again
    tn.write("terminal-notifier 2.0.0.", "exit 0");
    assert_eq!(m.tn_version(&tn.path()).await, 3);
    // ...and a fresh instance probes again
    assert_eq!(tn.notifier("macosx://").tn_version(&tn.path()).await, 2);

    for (output, expected) in [("terminal-notifier 4.0.0.", 3), ("", 3)] {
      tn.write(output, "exit 0");
      assert_eq!(tn.notifier("macosx://").tn_version(&tn.path()).await, expected, "{}", output);
    }

    // A version reported alongside a non-zero exit is not trusted
    let unclean = Surrogate::new("x", "exit 0");
    unclean.write_raw("#!/bin/sh\necho 'terminal-notifier 2.0.0.'\nexit 1\n");
    assert_eq!(unclean.notifier("macosx://").tn_version(&unclean.path()).await, 3);

    // A probe that hangs falls back to the current release
    unclean.write_raw("#!/bin/sh\nsleep 10\n");
    let mut m = unclean.notifier("macosx://");
    m.probe_timeout = Duration::from_millis(200);
    let started = Instant::now();
    assert_eq!(m.tn_version(&unclean.path()).await, 3);
    assert!(started.elapsed() < Duration::from_secs(5));
  }

  #[tokio::test]
  async fn test_sender() {
    let tn = Surrogate::new("terminal-notifier 2.0.0.", "exit 0");

    // Version 2 passes -sender, defaulting to our app id
    assert!(tn.notifier("macosx://_/?sender=me&image=no").send(&ctx("t", "b")).await.unwrap());
    assert_eq!(value_of(&tn.args(), "-sender"), Some("\\me"));
    assert!(tn.notifier("macosx://_/?image=no").send(&ctx("t", "b")).await.unwrap());
    assert_eq!(value_of(&tn.args(), "-sender"), Some("\\Apprise"));

    // ...unless there is neither a sender nor an app id
    let empty = NotifyContext { asset: AppriseAsset { app_id: String::new(), ..Default::default() }, ..ctx("t", "b") };
    assert!(tn.notifier("macosx://_/?image=no").send(&empty).await.unwrap());
    assert!(value_of(&tn.args(), "-sender").is_none());

    // Version 3 has no -sender
    assert!(tn.notifier("macosx://_/?version=3&sender=me&image=no").send(&ctx("t", "b")).await.unwrap());
    assert!(value_of(&tn.args(), "-sender").is_none());
  }

  #[tokio::test]
  async fn test_images() {
    let tn = Surrogate::new("terminal-notifier 3.1.0.", "exit 0");

    // Version 2 sets the app icon from a URL
    let failure = NotifyContext { notify_type: NotifyType::Failure, ..ctx("title", "body") };
    assert!(tn.notifier("macosx://_/?version=2&image=yes").send(&failure).await.unwrap());
    let args = tn.args();
    assert_eq!(value_of(&args, "-appIcon"), Some("\\https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-failure-128x128.png"));
    assert!(value_of(&args, "-contentImage").is_none());

    // Version 3 attaches a local file terminal-notifier can read
    assert!(tn.notifier("macosx://_/?version=3&image=yes").send(&ctx("title", "body")).await.unwrap());
    let args = tn.args();
    assert!(value_of(&args, "-appIcon").is_none());
    let image = value_of(&args, "-contentImage").unwrap();
    assert!(Path::new(&image[1..]).is_file(), "{}", image);

    // No title, no image
    assert!(tn.notifier("macosx://_/?image=no").send(&ctx("", "body")).await.unwrap());
    assert_eq!(tn.args(), vec!["-message", "\\body"]);
  }

  #[tokio::test]
  async fn test_failures() {
    for code in [1, 2, 3, 4, 5, 6, 99] {
      let tn = Surrogate::new("terminal-notifier 3.1.0.", &format!("echo 'Notifications are not allowed' >&2\nexit {}", code));
      assert!(tn.notifier("macosx://").send(&ctx("title", "body")).await.is_err(), "exit {}", code);
    }
    assert!(exit_reason(Some(3)).contains("-diagnose"));

    // A program that hangs is stopped
    let tn = Surrogate::new("terminal-notifier 3.1.0.", "sleep 10");
    let mut m = tn.notifier("macosx://_/?version=3");
    m.send_timeout = Duration::from_millis(200);
    assert!(m.send(&ctx("title", "body")).await.is_err());

    // No terminal-notifier at all
    let mut m = parse("macosx://").unwrap();
    m.notify_path = None;
    assert!(!m.send(&ctx("title", "body")).await.unwrap());
    m.notify_path = Some(tn.dir.path().join("missing"));
    assert!(!m.send(&ctx("title", "body")).await.unwrap());
  }

  #[test]
  fn test_static_details() {
    assert_eq!(MacOSX::static_details().protocols, vec!["macosx"]);
  }
}
