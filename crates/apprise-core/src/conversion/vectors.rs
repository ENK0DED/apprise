//! Differential tests against vectors computed by upstream Apprise.
//!
//! `testdata/vectors.json` holds inputs with the output upstream produced
//! for each (see `testdata/README.md` for how it is generated). Set
//! `APPRISE_CONVERSION_VECTORS` to a larger file built the same way to run
//! the full set with `cargo test -- --ignored conversion::vectors`.

use serde_json::Value;

use super::*;
use crate::types::NotifyFormat;
use crate::utils::format::{html_adjust, html_tag_adjust, html_tag_end, markdown_adjust, smart_split};

fn s(v: &Value) -> &str {
  v.as_str().unwrap_or_default()
}

fn opt_char(v: &Value) -> Option<char> {
  v.as_str().and_then(|s| s.chars().next())
}

fn chars(v: &Value) -> Vec<char> {
  s(v).chars().collect()
}

fn idx(v: &Value) -> usize {
  v.as_u64().unwrap_or(0) as usize
}

fn fmt(v: &Value) -> NotifyFormat {
  s(v).parse().unwrap_or(NotifyFormat::Text)
}

fn opt_usize(v: Option<usize>) -> Value {
  v.map(|x| Value::from(x as u64)).unwrap_or(Value::Null)
}

fn state_to_json(st: &RepairState) -> Value {
  let mut m = serde_json::Map::new();
  for (k, v) in [("*", st.star), ("**", st.star2), ("_", st.underscore), ("__", st.underscore2)] {
    if v > 0 {
      m.insert(k.into(), v.into());
    }
  }
  if let Some(w) = st.in_code {
    m.insert("in_code".into(), w.into());
  }
  for (k, v) in [("in_link_dest", st.in_link_dest), ("dest_gt", st.dest_gt), ("in_autolink", st.in_autolink), ("in_escape", st.in_escape)] {
    if v {
      m.insert(k.into(), true.into());
    }
  }
  Value::Object(m)
}

fn state_from_json(v: &Value) -> RepairState {
  let get = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
  let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
  RepairState {
    star: get("*"),
    star2: get("**"),
    underscore: get("_"),
    underscore2: get("__"),
    in_code: v.get("in_code").and_then(Value::as_u64).map(|w| w as usize).filter(|w| *w > 0),
    in_link_dest: flag("in_link_dest"),
    dest_gt: flag("dest_gt"),
    in_autolink: flag("in_autolink"),
    in_escape: flag("in_escape"),
  }
}

// Python dicts keep zero counts and false flags; compare what matters.
fn normalize_state(v: &Value) -> Value {
  state_to_json(&state_from_json(v))
}

fn converter(name: &str) -> fn(&str) -> String {
  match name {
    "entities" => |s| pystr::escape_html(s),
    "bold" => |s| s.replace("**", "*"),
    "upper_tags" => |s| s.replace("<b>", "<B>").replace("</b>", "</B>"),
    _ => |s| s.to_string(),
  }
}

fn render_emphasis(body: &str) -> String {
  let text: Vec<char> = body.chars().collect();
  let sentinel = commonmark_pick_emphasis_sentinel(body);
  let (mut out, mut delims) = (String::new(), Vec::new());
  let n = text.len();
  let mut i = 0;
  while i < n {
    if text[i] == '*' || text[i] == '_' {
      i = commonmark_emphasis_run(&text, i, n, &mut delims, &mut out, &sentinel);
    } else {
      out.push(text[i]);
      i += 1;
    }
  }
  commonmark_render_emphasis_markers(&out, &mut delims, ("<b>", "</b>"), ("<i>", "</i>"), &sentinel)
}

/// Compute the port's answer for one vector; `None` for unknown groups.
fn evaluate(group: &str, a: &Value) -> Option<Value> {
  let name = group.strip_prefix("rec_").unwrap_or(group);
  Some(match name {
    "html_to_text" => html_to_text(s(&a["content"])).into(),
    "html_to_markdown" => html_to_markdown(s(&a["content"])).into(),
    "html_tag_reducer" => HtmlTagReducer::default().reduce(s(&a["content"])).into(),
    "markdown_to_html" | "markdown_to_html_plain" => {
      let exts: Vec<MarkdownExtension> = match a.get("extensions").and_then(Value::as_array) {
        None => markdown::DEFAULT_EXTENSIONS.to_vec(),
        Some(list) => list
          .iter()
          .filter_map(|e| match s(e) {
            x if x.ends_with("nl2br") => Some(MarkdownExtension::Nl2br),
            x if x.ends_with("tables") => Some(MarkdownExtension::Tables),
            _ => None,
          })
          .collect(),
      };
      markdown_to_html_with(s(&a["content"]), &exts).into()
    }
    "text_to_html" => text_to_html(s(&a["content"])).into(),
    "text_to_markdown" => text_to_markdown(s(&a["content"])).into(),
    "convert_between" => {
      let (Some(from), Some(to)) = (a["from_format"].as_str(), a["to_format"].as_str()) else {
        return Some(a["content"].clone());
      };
      convert_between(&from.parse().ok()?, &to.parse().ok()?, s(&a["content"])).into()
    }
    "commonmark_headings_to_bold" => commonmark_headings_to_bold(s(&a["body"])).into(),
    "commonmark_decode_backslash_escapes" => commonmark_decode_backslash_escapes(s(&a["text"])).into(),
    "commonmark_escape_link_url" => commonmark_escape_link_url(s(&a["url"])).into(),
    "commonmark_pick_emphasis_sentinel" => commonmark_pick_emphasis_sentinel(s(&a["body"])).into(),
    "commonmark_sections" => {
      let (d, secs) = commonmark_sections(s(&a["markdown"]), idx(&a["name_maxlen"]), idx(&a["value_maxlen"]));
      serde_json::json!([d, secs.into_iter().map(|(n, t)| serde_json::json!([n, t])).collect::<Vec<_>>()])
    }
    "commonmark_lookahead_closer_widths" => {
      let w = commonmark_lookahead_closer_widths(&chars(&a["next_chunk"]), opt_char(&a["boundary_prev_ch"]), opt_char(&a["boundary_next_ch"]));
      let mut m = serde_json::Map::new();
      if w.star > 0 {
        m.insert("*".into(), w.star.into());
      }
      if w.underscore > 0 {
        m.insert("_".into(), w.underscore.into());
      }
      Value::Object(m)
    }
    "render_emphasis" => render_emphasis(s(&a["body"])).into(),
    "repair_chain" => {
      let input = chars(&a["input"]);
      let width = idx(&a["width"]);
      let pieces: Vec<String> = if input.is_empty() { vec![String::new()] } else { input.chunks(width).map(|c| c.iter().collect()).collect() };
      let mut state = RepairState::default();
      let mut trace = Vec::new();
      for (i, piece) in pieces.iter().enumerate() {
        let next = pieces.get(i + 1);
        let (fixed, st) = commonmark_repair_chunk(piece, &state, next.map(String::as_str), None);
        state = st;
        trace.push(serde_json::json!([piece, next, fixed, state_to_json(&state)]));
      }
      Value::Array(trace)
    }
    "commonmark_repair_chunk" => {
      let (fixed, st) =
        commonmark_repair_chunk(s(&a["text"]), &state_from_json(&a["pending"]), a["next_chunk"].as_str(), opt_char(&a["next_chunk_boundary_ch"]));
      serde_json::json!([fixed, state_to_json(&st)])
    }
    "smart_split" => {
      let limit = a["limit"].as_i64().unwrap_or(0).max(0) as usize;
      let f = a["body_format"].as_str().map(|f| f.parse().unwrap_or(NotifyFormat::Text)).unwrap_or(NotifyFormat::Text);
      serde_json::to_value(smart_split(s(&a["text"]), limit, &f)).ok()?
    }
    "split_dialect" => {
      let f = fmt(&a["body_format"]);
      serde_json::to_value(split_dialect_chunk(s(&a["body"]), idx(&a["limit"]), &converter(s(&a["converter"])), Some(&f))).ok()?
    }
    "truncate_dialect" => {
      let f = fmt(&a["body_format"]);
      truncate_dialect_chunk(s(&a["body"]), idx(&a["limit"]), &converter(s(&a["converter"])), Some(&f)).into()
    }
    "can_emphasis" => {
      let d = opt_char(&a["delim"]).unwrap_or('*');
      let (p, q) = (opt_char(&a["prev"]), opt_char(&a["next"]));
      serde_json::json!([commonmark_can_open_emphasis(d, p, q), commonmark_can_close_emphasis(d, p, q)])
    }
    "commonmark_scan_angle_dest" | "commonmark_scan_paren_dest" => {
      let body = chars(a.get("body").unwrap_or(&a["text"]));
      let mut budget = a["budget"].as_array().and_then(|b| b.first()).and_then(Value::as_i64).map(ScanBudget);
      let (i, n) = (idx(&a["i"]), idx(&a["n"]));
      let r = if name == "commonmark_scan_angle_dest" {
        commonmark_scan_angle_dest(&body, i, n, budget.as_mut())
      } else {
        commonmark_scan_paren_dest(&body, i, n, budget.as_mut())
      };
      opt_usize(r)
    }
    "commonmark_scan_autolink_dest" => {
      let (end, ok) = commonmark_scan_autolink_dest(&chars(&a["text"]), idx(&a["i"]), idx(&a["n"]));
      serde_json::json!([opt_usize(end), ok])
    }
    "commonmark_scan_delimiter_run" => {
      let (j, p, q) = commonmark_scan_delimiter_run(&chars(&a["text"]), idx(&a["i"]), opt_char(&a["boundary_prev_ch"]), opt_char(&a["boundary_next_ch"]));
      serde_json::json!([j, p.map(String::from), q.map(String::from)])
    }
    "html_adjust" => html_adjust(&chars(&a["text"]), idx(&a["window_start"]), idx(&a["split_at"])).into(),
    "html_tag_adjust" => {
      let we = a["window_end"].as_u64().map(|v| v as usize);
      html_tag_adjust(&chars(&a["text"]), idx(&a["window_start"]), idx(&a["split_at"]), we).into()
    }
    "markdown_adjust" => markdown_adjust(&chars(&a["text"]), idx(&a["window_start"]), idx(&a["split_at"])).into(),
    "html_tag_end" => {
      let end = a["end"].as_u64().map(|v| v as usize);
      html_tag_end(&chars(&a["text"]), idx(&a["start"]), end).map(|v| Value::from(v as i64)).unwrap_or(Value::from(-1))
    }
    _ => return None,
  })
}

fn expected(group: &str, row: &Value) -> Value {
  let name = group.strip_prefix("rec_").unwrap_or(group);
  match name {
    "repair_chain" => {
      Value::Array(row["result"].as_array().unwrap().iter().map(|step| serde_json::json!([step[0], step[1], step[2], normalize_state(&step[3])])).collect())
    }
    "commonmark_repair_chunk" => serde_json::json!([row["result"][0], normalize_state(&row["result"][1])]),
    _ => row["result"].clone(),
  }
}

/// Run every vector in `data`; returns `(passed, failures)`.
fn run(data: &str, skip: &[&str]) -> (usize, Vec<String>) {
  let v: Value = serde_json::from_str(data).expect("valid vector JSON");
  let mut passed = 0;
  let mut failures = Vec::new();
  for (group, rows) in v.as_object().unwrap() {
    if skip.iter().any(|g| group.ends_with(g)) {
      continue;
    }
    for row in rows.as_array().unwrap() {
      let args = &row["args"];
      // Large index-based scans recorded with odd arguments are skipped when
      // they reference positions past the text.
      let Some(got) = std::panic::catch_unwind(|| evaluate(group, args)).unwrap_or_else(|_| Some(Value::String("<panic>".into()))) else {
        continue;
      };
      let want = expected(group, row);
      if got == want {
        passed += 1;
      } else {
        failures.push(format!("{group}: args={args}\n  want={want}\n  got ={got}"));
      }
    }
  }
  (passed, failures)
}

fn report(passed: usize, failures: &[String]) {
  if !failures.is_empty() {
    let shown: Vec<&String> = failures.iter().take(15).collect();
    panic!(
      "{} of {} vectors differ from upstream:\n{}",
      failures.len(),
      passed + failures.len(),
      shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
    );
  }
}

// Groups whose port is still in progress.
const PENDING: &[&str] = &[];

#[test]
fn test_upstream_vectors() {
  let (passed, failures) = run(include_str!("testdata/vectors.json"), PENDING);
  assert!(passed > 1000, "too few vectors ran: {passed}");
  report(passed, &failures);
}

#[test]
#[ignore]
fn test_upstream_vectors_full() {
  let Ok(path) = std::env::var("APPRISE_CONVERSION_VECTORS") else {
    return;
  };
  let data = std::fs::read_to_string(path).expect("readable vector file");
  let (passed, failures) = run(&data, PENDING);
  eprintln!("{passed} passed, {} failed", failures.len());
  report(passed, &failures);
}

/// Convert every input in `APPRISE_MD_IN` (a JSON list of strings) and write
/// the results to `APPRISE_MD_OUT`; a helper for shrinking failures.
#[test]
#[ignore]
fn dump_markdown_conversions() {
  let (Ok(input), Ok(output)) = (std::env::var("APPRISE_MD_IN"), std::env::var("APPRISE_MD_OUT")) else {
    return;
  };
  let inputs: Vec<String> = serde_json::from_str(&std::fs::read_to_string(input).unwrap()).unwrap();
  let outputs: Vec<String> = inputs.iter().map(|s| std::panic::catch_unwind(|| markdown_to_html(s)).unwrap_or_else(|_| "<panic>".to_string())).collect();
  std::fs::write(output, serde_json::to_string(&outputs).unwrap()).unwrap();
}
