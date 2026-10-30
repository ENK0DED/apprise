use async_trait::async_trait;
use serde_json::Value;

use super::*;
use crate::notify::{Attachment, ConfiguredService, ServiceDetails, ServiceOptions};

/// A plugin whose limits, formats and dialect come from a test case.
#[derive(Clone)]
struct Probe {
  body_maxlen: usize,
  title_maxlen: usize,
  line_count: usize,
  amalgamate: bool,
  formats: Vec<NotifyFormat>,
  overflow: OverflowMode,
  dialect: Option<String>,
  attachments: bool,
  pin_overflow: bool,
}

impl Default for Probe {
  fn default() -> Self {
    Probe {
      body_maxlen: 32768,
      title_maxlen: 250,
      line_count: 0,
      amalgamate: false,
      formats: vec![NotifyFormat::Text],
      overflow: OverflowMode::Upstream,
      dialect: None,
      attachments: false,
      pin_overflow: false,
    }
  }
}

#[async_trait]
impl Notify for Probe {
  fn schemas(&self) -> &[&str] {
    &["probe"]
  }
  fn service_name(&self) -> &str {
    "Probe"
  }
  fn details(&self) -> ServiceDetails {
    ServiceDetails { service_name: "Probe", service_url: None, setup_url: None, protocols: vec!["probe"], description: "", attachment_support: false }
  }
  async fn send(&self, _ctx: &NotifyContext) -> Result<bool, NotifyError> {
    Ok(true)
  }
  fn attachment_support(&self) -> bool {
    self.attachments
  }
  fn notify_format(&self) -> NotifyFormat {
    self.formats[0].clone()
  }
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    self.formats.clone()
  }
  fn body_maxlen(&self) -> usize {
    self.body_maxlen
  }
  fn title_maxlen(&self) -> usize {
    self.title_maxlen
  }
  fn overflow_mode(&self) -> OverflowMode {
    self.overflow
  }
  fn overflow_mode_overridable(&self) -> bool {
    !self.pin_overflow
  }
  fn body_max_line_count(&self) -> usize {
    self.line_count
  }
  fn overflow_amalgamate_title(&self) -> bool {
    self.amalgamate
  }
  fn has_dialect(&self) -> bool {
    self.dialect.is_some()
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    match self.dialect.as_deref() {
      Some("bold") if *body_format == NotifyFormat::Markdown => body.replace("**", "*"),
      Some("entities") => body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"),
      _ => body.to_string(),
    }
  }
}

fn wrap(probe: Probe, options: ServiceOptions) -> ConfiguredService {
  ConfiguredService::new(Box::new(probe), options)
}

fn ctx(title: &str, body: &str, format: NotifyFormat) -> NotifyContext {
  NotifyContext { title: title.into(), body: body.into(), body_format: format, ..Default::default() }
}

fn pieces(svc: &dyn Notify, c: &NotifyContext) -> Vec<(String, String)> {
  prepare_contexts(svc, c).unwrap().into_iter().map(|c| (c.title, c.body)).collect()
}

fn pair(t: &str, b: &str) -> (String, String) {
  (t.to_string(), b.to_string())
}

fn fmt(v: &Value) -> Option<NotifyFormat> {
  v.as_str().and_then(|s| s.parse().ok())
}

fn mode(v: &Value) -> Option<OverflowMode> {
  v.as_str().and_then(|s| s.parse().ok())
}

/// Run upstream-recorded cases; returns `(passed, failures)`.
fn run_cases(data: &str) -> (usize, Vec<String>) {
  let cases: Vec<Value> = serde_json::from_str(data).expect("valid JSON");
  let mut passed = 0;
  let mut failures = Vec::new();
  for case in &cases {
    let calls = case["calls"].as_array().unwrap();
    let input = fmt(&case["input_format"]).unwrap();
    // Markdown to HTML is checked by the conversion vectors.
    if input == NotifyFormat::Markdown && calls.first().map(|c| c["body_format"] == "html").unwrap_or(true) {
      continue;
    }
    let probe = Probe {
      body_maxlen: case["body_maxlen"].as_u64().unwrap() as usize,
      title_maxlen: case["title_maxlen"].as_u64().unwrap() as usize,
      line_count: case["body_max_line_count"].as_u64().unwrap() as usize,
      amalgamate: case["amalgamate"].as_bool().unwrap(),
      formats: case["formats"].as_array().unwrap().iter().filter_map(fmt).collect(),
      overflow: mode(&case["overflow_mode"]).unwrap(),
      dialect: case["dialect"].as_str().map(String::from),
      ..Default::default()
    };
    let options = ServiceOptions { format: fmt(&case["format"]), overflow: mode(&case["overflow"]), emojis: case["emojis"].as_bool(), ..Default::default() };
    let svc = wrap(probe, options);
    let mut c = ctx(case["title"].as_str().unwrap(), case["body"].as_str().unwrap(), input);
    c.interpret_escapes = case["interpret_escapes"].as_bool().unwrap();
    c.interpret_emojis = case["asset_emojis"].as_bool().unwrap_or(false);
    let got: Vec<Value> = match prepare_contexts(&svc, &c) {
      Ok(list) => list.into_iter().map(|p| serde_json::json!({"title": p.title, "body": p.body, "body_format": p.body_format.as_str()})).collect(),
      Err(_) => Vec::new(),
    };
    if got == *calls {
      passed += 1;
    } else {
      let mut shown = case.clone();
      shown.as_object_mut().unwrap().remove("calls");
      failures.push(format!("case={shown}\n  want={}\n  got ={}", Value::Array(calls.clone()), Value::Array(got)));
    }
  }
  (passed, failures)
}

fn report(passed: usize, failures: &[String]) {
  if !failures.is_empty() {
    panic!(
      "{} of {} cases differ from upstream:\n{}",
      failures.len(),
      passed + failures.len(),
      failures.iter().take(10).cloned().collect::<Vec<_>>().join("\n")
    );
  }
}

#[test]
fn test_upstream_prepare_vectors() {
  let (passed, failures) = run_cases(include_str!("testdata/prepare.json"));
  assert!(passed > 100, "too few cases ran: {passed}");
  report(passed, &failures);
}

#[test]
#[ignore]
fn test_upstream_prepare_vectors_full() {
  let Ok(path) = std::env::var("APPRISE_PREPARE_VECTORS") else {
    return;
  };
  let (passed, failures) = run_cases(&std::fs::read_to_string(path).unwrap());
  eprintln!("{passed} passed, {} failed", failures.len());
  report(passed, &failures);
}

// ── Title blending for services without a title field ─────────────────

#[test]
fn test_title_blended_into_text_body() {
  let svc = wrap(Probe { title_maxlen: 0, ..Default::default() }, ServiceOptions::default());
  assert_eq!(pieces(&svc, &ctx("Alert", "disk full", NotifyFormat::Text)), vec![pair("", "Alert\r\ndisk full")]);
}

#[test]
fn test_title_blended_into_html_body() {
  let probe = Probe { title_maxlen: 0, formats: vec![NotifyFormat::Html], ..Default::default() };
  let svc = wrap(probe, ServiceOptions::default());
  assert_eq!(pieces(&svc, &ctx("Alert", "<p>disk full</p>", NotifyFormat::Html)), vec![pair("", "<b>Alert</b><br />\r\n<p>disk full</p>")]);
  // A text title is escaped when it is converted with the body.
  assert_eq!(pieces(&svc, &ctx("A & B", "x", NotifyFormat::Text)), vec![pair("", "<b>A&nbsp;&amp;&nbsp;B</b><br />\r\nx")]);
}

#[test]
fn test_title_blended_into_markdown_body() {
  let probe = Probe { title_maxlen: 0, formats: vec![NotifyFormat::Markdown], ..Default::default() };
  let svc = wrap(probe, ServiceOptions::default());
  assert_eq!(pieces(&svc, &ctx("## Alert", "**disk** full", NotifyFormat::Markdown)), vec![pair("", "# Alert\n**disk** full")]);
  // A title of only heading markers adds nothing.
  assert_eq!(pieces(&svc, &ctx("#-", "body", NotifyFormat::Markdown)), vec![pair("", "body")]);
}

#[test]
fn test_blended_title_counts_against_split_and_truncate() {
  // Expected pieces come from upstream with the same settings.
  let probe = Probe { title_maxlen: 0, body_maxlen: 12, overflow: OverflowMode::Split, ..Default::default() };
  let svc = wrap(probe.clone(), ServiceOptions::default());
  assert_eq!(pieces(&svc, &ctx("Title", "one two three four", NotifyFormat::Text)), vec![pair("", "Title"), pair("", "one two"), pair("", "three four")]);
  let svc = wrap(Probe { overflow: OverflowMode::Truncate, ..probe }, ServiceOptions::default());
  assert_eq!(pieces(&svc, &ctx("Title", "one two three four", NotifyFormat::Text)), vec![pair("", "Title\r\none t")]);
}

// ── Limits are counted in characters ──────────────────────────────────

#[test]
fn test_multibyte_title_and_body_are_cut_by_characters() {
  let probe = Probe { title_maxlen: 5, body_maxlen: 4, overflow: OverflowMode::Truncate, ..Default::default() };
  let svc = wrap(probe.clone(), ServiceOptions::default());
  assert_eq!(pieces(&svc, &ctx("ÜñïçødéÜñï", "🐧🐧🐧🐧🐧🐧", NotifyFormat::Text)), vec![pair("Üñïçø", "🐧🐧🐧🐧")]);
  let svc = wrap(Probe { overflow: OverflowMode::Split, ..probe }, ServiceOptions::default());
  assert_eq!(pieces(&svc, &ctx("Ünï", "éééééé ééé", NotifyFormat::Text)), vec![pair("Ünï", "éééé"), pair("Ünï", "éé"), pair("Ünï", "ééé")]);
}

#[test]
fn test_multibyte_text_fitting_in_characters_is_untouched() {
  // 4 characters but 16 bytes: within a 4-character limit.
  let probe = Probe { body_maxlen: 4, overflow: OverflowMode::Truncate, ..Default::default() };
  let svc = wrap(probe, ServiceOptions::default());
  assert_eq!(pieces(&svc, &ctx("", "🐧🐧🐧🐧", NotifyFormat::Text)), vec![pair("", "🐧🐧🐧🐧")]);
}

#[test]
fn test_upstream_mode_keeps_long_title_and_body() {
  let probe = Probe { title_maxlen: 3, body_maxlen: 3, ..Default::default() };
  let svc = wrap(probe, ServiceOptions::default());
  assert_eq!(pieces(&svc, &ctx(" Long title ", "long body \n", NotifyFormat::Text)), vec![pair("Long title", "long body")]);
}

#[test]
fn test_split_counter_on_long_titles() {
  let probe = Probe { body_maxlen: 10, overflow: OverflowMode::Split, ..Default::default() };
  let svc = wrap(probe, ServiceOptions::default());
  let got = pieces(&svc, &ctx("Report", "aaaa bbbb cccc", NotifyFormat::Text));
  assert_eq!(got, vec![pair("Report [1/2]", "aaaa bbbb"), pair("Report [2/2]", "cccc")]);
}

#[test]
fn test_body_max_line_count() {
  let probe = Probe { line_count: 2, ..Default::default() };
  let svc = wrap(probe, ServiceOptions::default());
  assert_eq!(pieces(&svc, &ctx("", "one\r\ntwo\nthree", NotifyFormat::Text)), vec![pair("", "one\r\ntwo")]);
}

// ── Format negotiation and URL options ────────────────────────────────

fn resolved(svc: &dyn Notify, input: NotifyFormat) -> NotifyFormat {
  prepare_contexts(svc, &ctx("", "x", input)).unwrap()[0].body_format.clone()
}

#[test]
fn test_single_format_plugin_uses_url_format() {
  let probe = Probe { formats: vec![NotifyFormat::Text], ..Default::default() };
  assert_eq!(resolved(&wrap(probe.clone(), ServiceOptions::default()), NotifyFormat::Html), NotifyFormat::Text);
  let opts = ServiceOptions { format: Some(NotifyFormat::Markdown), ..Default::default() };
  assert_eq!(resolved(&wrap(probe, opts), NotifyFormat::Html), NotifyFormat::Markdown);
}

#[test]
fn test_multi_format_plugin_negotiates() {
  let probe = Probe { formats: vec![NotifyFormat::Text, NotifyFormat::Markdown], ..Default::default() };
  // The input format wins when the plugin lists it.
  assert_eq!(resolved(&wrap(probe.clone(), ServiceOptions::default()), NotifyFormat::Markdown), NotifyFormat::Markdown);
  // Otherwise the first listed format is the default.
  assert_eq!(resolved(&wrap(probe.clone(), ServiceOptions::default()), NotifyFormat::Html), NotifyFormat::Text);
  // A listed ?format= wins; an unlisted one is ignored.
  let opts = ServiceOptions { format: Some(NotifyFormat::Markdown), ..Default::default() };
  assert_eq!(resolved(&wrap(probe.clone(), opts), NotifyFormat::Text), NotifyFormat::Markdown);
  let opts = ServiceOptions { format: Some(NotifyFormat::Html), ..Default::default() };
  assert_eq!(resolved(&wrap(probe, opts), NotifyFormat::Markdown), NotifyFormat::Markdown);
}

#[test]
fn test_url_overflow_and_pinned_overflow() {
  let probe = Probe { body_maxlen: 5, ..Default::default() };
  let opts = ServiceOptions { overflow: Some(OverflowMode::Split), ..Default::default() };
  assert_eq!(pieces(&wrap(probe.clone(), opts.clone()), &ctx("", "aaaa bbbb", NotifyFormat::Text)).len(), 2);
  let pinned = Probe { pin_overflow: true, ..probe };
  assert_eq!(pieces(&wrap(pinned, opts), &ctx("", "aaaa bbbb", NotifyFormat::Text)), vec![pair("", "aaaa bbbb")]);
}

#[test]
fn test_emojis_follow_url_then_context() {
  let c = NotifyContext { body: ":smile:".into(), interpret_emojis: true, ..Default::default() };
  assert_eq!(pieces(&wrap(Probe::default(), ServiceOptions::default()), &c), vec![pair("", "😄")]);
  let off = ServiceOptions { emojis: Some(false), ..Default::default() };
  assert_eq!(pieces(&wrap(Probe::default(), off), &c), vec![pair("", ":smile:")]);
  let on = ServiceOptions { emojis: Some(true), ..Default::default() };
  let c = NotifyContext { interpret_emojis: false, ..c };
  assert_eq!(pieces(&wrap(Probe::default(), on), &c), vec![pair("", "😄")]);
}

#[test]
fn test_escapes_apply_to_title_and_body() {
  let c = NotifyContext { title: r"a\tb".into(), body: r"line\nnext C:\Users".into(), interpret_escapes: true, ..Default::default() };
  assert_eq!(pieces(&wrap(Probe::default(), ServiceOptions::default()), &c), vec![pair("a\tb", "line\nnext C:\\Users")]);
}

// ── Dialects ──────────────────────────────────────────────────────────

#[test]
fn test_dialect_output_is_resplit_to_fit() {
  let probe = Probe { body_maxlen: 10, overflow: OverflowMode::Split, dialect: Some("entities".into()), ..Default::default() };
  let svc = wrap(probe, ServiceOptions::default());
  // "a&b&c&d" fits before conversion (7 chars) but not after (19).
  let got = pieces(&svc, &ctx("T", "a&b&c&d", NotifyFormat::Text));
  assert!(got.len() > 1, "{got:?}");
  assert!(got.iter().all(|(_, b)| b.chars().count() <= 10), "{got:?}");
  assert_eq!(got.iter().map(|(_, b)| b.as_str()).collect::<String>(), "a&amp;b&amp;c&amp;d");
  assert_eq!(got[0].0, "T");
}

// ── Failures and attachments ──────────────────────────────────────────

#[test]
fn test_empty_body_without_attachment_fails() {
  let svc = wrap(Probe::default(), ServiceOptions::default());
  assert!(prepare_contexts(&svc, &ctx("t", "", NotifyFormat::Text)).is_err());
  // HTML that converts to nothing counts as empty too.
  assert!(prepare_contexts(&svc, &ctx("t", "<p> </p>", NotifyFormat::Html)).is_err());
}

#[test]
fn test_attachments_go_on_the_first_piece_only() {
  let attachment = Attachment { name: "a.txt".into(), data: b"x".to_vec(), mime_type: "text/plain".into() };
  let probe = Probe { body_maxlen: 5, overflow: OverflowMode::Split, attachments: true, ..Default::default() };
  let mut c = ctx("", "aaaa bbbb", NotifyFormat::Text);
  c.attachments = vec![attachment.clone(), attachment];
  let out = prepare_contexts(&wrap(probe.clone(), ServiceOptions::default()), &c).unwrap();
  assert_eq!(out.iter().map(|c| c.attachments.len()).collect::<Vec<_>>(), vec![2, 0]);
  // Truncate mode keeps only the first attachment.
  let out = prepare_contexts(&wrap(Probe { overflow: OverflowMode::Truncate, ..probe }, ServiceOptions::default()), &c).unwrap();
  assert_eq!(out.iter().map(|c| c.attachments.len()).collect::<Vec<_>>(), vec![1]);
}
