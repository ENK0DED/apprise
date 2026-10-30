//! HTML parsers for plain text, CommonMark, and reduced tag sets
//! (upstream `apprise/conversion/html.py`).

use std::collections::HashMap;

use super::commonmark::{commonmark_find_backtick_run, commonmark_index_backtick_runs};
use super::pyhtml::{self, Attrs, HtmlHandler};
use super::pystr;

/// Cap list indentation so deeply nested input stays linear.
pub const LIST_DEPTH_MAX: usize = 4;

/// Apply the same depth cap to blockquote prefixes.
pub const BLOCKQUOTE_DEPTH_MAX: usize = 4;

/// Bound the context-stack depth so adversarially nested HTML cannot
/// exhaust memory. Frames beyond this limit are silently dropped.
pub const MAX_FRAME_DEPTH: usize = 200;

// The following tags must start on a new line.
const TEXT_BLOCK_TAGS: &[&str] = &["p", "h1", "h2", "h3", "h4", "h5", "h6", "div", "td", "th", "code", "pre", "label", "li"];

// The following tags ignore any internal text.
const IGNORE_TAGS: &[&str] = &["form", "input", "textarea", "select", "ul", "ol", "style", "link", "meta", "title", "html", "head", "script"];

// ─── HTML to plain text ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum TextItem {
  BlockEnd,
  Text(String),
}

/// Upstream `HTMLConverter`: HTML to plain text, tuned for email messages.
struct TextConverter {
  do_store: bool,
  pre_depth: usize,
  result: Vec<TextItem>,
}

impl HtmlHandler for TextConverter {
  fn handle_data(&mut self, data: &str) {
    if self.do_store {
      // Text inside <pre> keeps its line breaks and indentation; only its
      // line break style is normalized.
      let content = if self.pre_depth > 0 { data.replace("\r\n", "\n").replace('\r', "\n") } else { pystr::collapse_whitespace(data) };
      self.result.push(TextItem::Text(content));
    }
  }

  fn handle_starttag(&mut self, tag: &str, _attrs: &Attrs) {
    self.do_store = !IGNORE_TAGS.contains(&tag);
    if tag == "pre" {
      self.pre_depth += 1;
    }
    if TEXT_BLOCK_TAGS.contains(&tag) {
      self.result.push(TextItem::BlockEnd);
    }
    match tag {
      "li" => self.result.push(TextItem::Text("- ".into())),
      "br" => self.result.push(TextItem::Text("\n".into())),
      "hr" => {
        if let Some(TextItem::Text(last)) = self.result.last_mut() {
          let trimmed = last.trim_end_matches(' ').len();
          last.truncate(trimmed);
        }
        self.result.push(TextItem::Text("\n---\n".into()));
      }
      "blockquote" => self.result.push(TextItem::Text(" >".into())),
      _ => {}
    }
  }

  fn handle_endtag(&mut self, tag: &str) {
    self.do_store = true;
    if tag == "pre" && self.pre_depth > 0 {
      self.pre_depth -= 1;
    }
    if TEXT_BLOCK_TAGS.contains(&tag) {
      self.result.push(TextItem::BlockEnd);
    }
  }
}

/// Convert HTML to plain text (upstream `html_to_text`).
pub fn html_to_text(content: &str) -> String {
  let mut parser = TextConverter { do_store: true, pre_depth: 0, result: Vec::new() };
  pyhtml::parse(content, &mut parser);

  // Join text fragments and collapse repeated block endings.
  let mut out = String::new();
  let mut accum: Option<String> = None;
  for item in parser.result {
    match item {
      TextItem::BlockEnd => {
        if let Some(a) = accum.take() {
          out.push_str(pystr::strip(&a));
          out.push('\n');
        }
      }
      TextItem::Text(t) => match accum.as_mut() {
        Some(a) => a.push_str(&t),
        None => accum = Some(t),
      },
    }
  }
  if let Some(a) = accum {
    out.push_str(pystr::strip(&a));
  }
  pystr::strip(&out).to_string()
}

// ─── HTML to CommonMark ──────────────────────────────────────────────────────

// Define tags that create Markdown block boundaries.
const MD_BLOCK_TAGS: &[&str] = &["p", "h1", "h2", "h3", "h4", "h5", "h6", "div", "td", "th", "pre", "samp", "label", "li"];

// Paragraph-like blocks need blank-line separation in CommonMark.
const PARA_LIKE_TAGS: &[&str] = &["p", "div", "td", "th", "label"];

// Block executable and local URI schemes while allowing app links.
const UNSAFE_HREF_SCHEMES: &[&str] = &["javascript", "vbscript", "data", "file", "blob"];

// Trim ASCII whitespace without removing deliberate non-breaking spaces.
const ASCII_WHITESPACE: &str = " \t\n\r\x0b\x0c";

#[derive(Debug, Clone, Copy, PartialEq)]
enum MarkerKind {
  // A generated list marker.
  List,
  // A generated quote prefix.
  Quote,
  // List continuation indentation.
  Indent,
}

#[derive(Debug, Clone, PartialEq)]
enum MdItem {
  BlockEnd,
  ParaBreak { prefix: String, in_quote: bool },
  Text(String),
  Marker(MarkerKind, String),
}

impl MdItem {
  fn is_marker(&self) -> bool {
    matches!(self, MdItem::Marker(..))
  }
  fn is_boundary(&self) -> bool {
    matches!(self, MdItem::BlockEnd | MdItem::ParaBreak { .. })
  }
  fn text(&self) -> &str {
    match self {
      MdItem::Text(s) | MdItem::Marker(_, s) => s,
      _ => "",
    }
  }
}

#[derive(Debug, Clone, Default)]
struct Frame {
  tag: Option<String>,
  do_store: bool,
  preserve_cr: bool,
  list_type: Option<&'static str>,
  depth: usize,
  counter: Option<u128>,
  list_do_store: bool,
  quote_depth: usize,
  list_indent: String,
  list_indent_base: String,
  // Keys a frame does not inherit from its parent.
  href: Option<String>,
  buffer: Option<Vec<String>>,
  content_seq_at_open: Option<u64>,
  emphasis_delim: Option<&'static str>,
  emphasis_started: bool,
  emphasis_result_start: usize,
  cells: Option<Vec<String>>,
  table_frame: Option<usize>,
  table_do_store: bool,
  table_rows: usize,
  cell_result_start: usize,
}

impl Frame {
  fn root() -> Self {
    Frame { do_store: true, list_do_store: true, ..Default::default() }
  }

  fn tag_is(&self, tag: &str) -> bool {
    self.tag.as_deref() == Some(tag)
  }
}

/// Upstream `HTMLMarkdownConverter`: HTML to notification-friendly
/// CommonMark. Plugins may adapt its output to their own dialects.
struct MarkdownConverter {
  indent_cache: Vec<String>,
  quote_prefix_cache: Vec<String>,
  tag_open_counts: HashMap<Option<String>, usize>,
  content_seq: u64,
  stack: Vec<Frame>,
  result: Vec<MdItem>,
}

fn py_int(value: &str) -> Option<i128> {
  // Python int(): surrounding whitespace, a sign, and "_" between digits.
  let s = pystr::strip(value);
  let (neg, digits) = match s.as_bytes().first() {
    Some(b'-') => (true, &s[1..]),
    Some(b'+') => (false, &s[1..]),
    _ => (false, s),
  };
  if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
    return None;
  }
  let mut n: i128 = 0;
  for c in digits.chars() {
    if c == '_' {
      continue;
    }
    let d = c.to_digit(10)? as i128;
    n = n.saturating_mul(10).saturating_add(d);
  }
  Some(if neg { -n } else { n })
}

fn attr<'a>(attrs: &'a Attrs, name: &str) -> Option<Option<&'a str>> {
  attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_deref())
}

impl MarkdownConverter {
  fn new() -> Self {
    MarkdownConverter {
      indent_cache: vec![String::new()],
      quote_prefix_cache: vec![String::new()],
      tag_open_counts: HashMap::new(),
      content_seq: 0,
      stack: vec![Frame::root()],
      result: Vec::new(),
    }
  }

  fn top(&self) -> &Frame {
    self.stack.last().expect("root frame")
  }

  fn top_mut(&mut self) -> &mut Frame {
    self.stack.last_mut().expect("root frame")
  }

  fn open_count(&self, tag: &str) -> usize {
    self.tag_open_counts.get(&Some(tag.to_string())).copied().unwrap_or(0)
  }

  fn make_frame(&self, tag: &str) -> Frame {
    let parent = self.stack.last().cloned().unwrap_or_else(Frame::root);
    Frame {
      tag: Some(tag.to_string()),
      do_store: parent.do_store,
      preserve_cr: parent.preserve_cr,
      list_type: parent.list_type,
      depth: parent.depth,
      counter: parent.counter,
      list_do_store: parent.list_do_store,
      quote_depth: parent.quote_depth,
      list_indent: parent.list_indent,
      list_indent_base: parent.list_indent_base,
      ..Default::default()
    }
  }

  fn indent(&mut self, depth: usize) -> String {
    let idx = depth.saturating_sub(1);
    while self.indent_cache.len() <= idx {
      let next = format!("{}  ", self.indent_cache.last().unwrap());
      self.indent_cache.push(next);
    }
    self.indent_cache[idx].clone()
  }

  fn quote_prefix(&mut self, depth: usize) -> String {
    while self.quote_prefix_cache.len() <= depth {
      let next = format!("{}> ", self.quote_prefix_cache.last().unwrap());
      self.quote_prefix_cache.push(next);
    }
    self.quote_prefix_cache[depth].clone()
  }

  fn continuation_prefix(&mut self, list_indent: &str, quote_depth: usize) -> String {
    let mut prefix = list_indent.to_string();
    if quote_depth > 0 {
      prefix.push_str(&self.quote_prefix(quote_depth));
    }
    prefix
  }

  fn ctx_prefix(&mut self, ctx: &Frame) -> String {
    self.continuation_prefix(&ctx.list_indent, ctx.quote_depth)
  }

  fn write_boundary(&mut self, ctx: &Frame, strong: bool) {
    let prefix = self.ctx_prefix(ctx);
    if !strong {
      self.result.push(MdItem::BlockEnd);
      if !prefix.is_empty() {
        let kind = if ctx.quote_depth > 0 { MarkerKind::Quote } else { MarkerKind::Indent };
        self.result.push(MdItem::Marker(kind, prefix));
      }
      return;
    }
    if !prefix.is_empty() {
      self.result.push(MdItem::ParaBreak { prefix, in_quote: ctx.quote_depth > 0 });
    } else {
      self.result.push(MdItem::ParaBreak { prefix: String::new(), in_quote: false });
    }
  }

  fn open_line_boundary(&mut self, ctx: &Frame, strong: bool) {
    if self.result.last().is_some_and(MdItem::is_marker) {
      return;
    }
    self.write_boundary(ctx, strong);
  }

  fn emit(&mut self, text: String) {
    self.result.push(MdItem::Text(text));
    self.content_seq += 1;
  }

  fn push_frame(&mut self, frame: Frame) -> bool {
    if self.stack.len() >= MAX_FRAME_DEPTH {
      return false;
    }
    *self.tag_open_counts.entry(frame.tag.clone()).or_insert(0) += 1;
    self.stack.push(frame);
    true
  }

  fn pop_to(&mut self, tag: &str) -> bool {
    if self.open_count(tag) == 0 {
      return false;
    }
    for i in (1..self.stack.len()).rev() {
      if self.stack[i].tag_is(tag) {
        for frame in &self.stack[i..] {
          if let Some(c) = self.tag_open_counts.get_mut(&frame.tag) {
            *c = c.saturating_sub(1);
          }
        }
        self.stack.truncate(i);
        return true;
      }
    }
    false
  }

  fn close_emphasis_frame(&mut self, frame: &Frame) {
    let has_content = self.content_seq != frame.content_seq_at_open.unwrap_or(self.content_seq);
    if !has_content {
      self.result.truncate(frame.emphasis_result_start);
      return;
    }
    // The closing delimiter must not follow whitespace; move it after.
    let mut trailing = String::new();
    while let Some(MdItem::Text(last)) = self.result.last() {
      let rstripped = pystr::rstrip_chars(last, ASCII_WHITESPACE);
      if rstripped.len() == last.len() {
        break;
      }
      trailing = format!("{}{}", &last[rstripped.len()..], trailing);
      if !rstripped.is_empty() {
        let keep = rstripped.to_string();
        *self.result.last_mut().unwrap() = MdItem::Text(keep);
        break;
      }
      self.result.pop();
    }
    self.emit(frame.emphasis_delim.unwrap_or("").to_string());
    if !trailing.is_empty() {
      self.result.push(MdItem::Text(trailing));
    }
  }

  fn escape_cell_pipes(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let runs = commonmark_index_backtick_runs(&chars);
    let n = chars.len();
    let mut out = String::with_capacity(text.len() + 4);
    let mut i = 0;
    while i < n {
      let ch = chars[i];
      if ch == '\\' && i + 1 < n {
        out.push(chars[i]);
        out.push(chars[i + 1]);
        i += 2;
        continue;
      }
      if ch == '`' {
        let mut j = i;
        while j < n && chars[j] == '`' {
          j += 1;
        }
        let run = j - i;
        if let Some(close) = commonmark_find_backtick_run(&runs, j, run) {
          out.extend(&chars[i..close + run]);
          i = close + run;
          continue;
        }
        out.extend(&chars[i..j]);
        i = j;
        continue;
      }
      if ch == '|' {
        out.push_str("\\|");
        i += 1;
        continue;
      }
      out.push(ch);
      i += 1;
    }
    out
  }

  fn close_cell_frame(&mut self, frame: &Frame) {
    let start = frame.cell_result_start.min(self.result.len());
    let chunk: Vec<MdItem> = self.result.split_off(start);
    if !frame.do_store {
      return;
    }
    let joined = finalize(&chunk);
    let mut text = pystr::strip_chars(&joined, ASCII_WHITESPACE).to_string();
    if text.contains('\n') {
      let spaced = text.split('\n').collect::<Vec<_>>().join(" ");
      // Collapse ASCII whitespace runs ([ \t\r\f\v]+) to one space.
      let mut collapsed = String::with_capacity(spaced.len());
      let mut in_ws = false;
      for c in spaced.chars() {
        if matches!(c, ' ' | '\t' | '\r' | '\x0c' | '\x0b') {
          if !in_ws {
            collapsed.push(' ');
          }
          in_ws = true;
        } else {
          collapsed.push(c);
          in_ws = false;
        }
      }
      text = pystr::strip_chars(&collapsed, ASCII_WHITESPACE).to_string();
    }
    if text.contains('|') {
      text = Self::escape_cell_pipes(&text);
    }
    if let Some(cells) = self.top_mut().cells.as_mut() {
      cells.push(text);
    }
  }

  fn close_pre_frame(&mut self, ctx: &Frame, tag: &str) {
    let mut content: String = ctx.buffer.as_deref().unwrap_or(&[]).concat();
    let run = longest_backtick_run(&content);
    if tag == "code" {
      let delim = "`".repeat(run + 1);
      let pad = if content.is_empty() || content.starts_with('`') || content.ends_with('`') { " " } else { "" };
      self.emit(format!("{delim}{pad}{content}{pad}{delim}"));
      return;
    }
    let fence = "`".repeat(run.max(2) + 1);
    let prefix = self.ctx_prefix(ctx);
    let marker = if prefix.is_empty() {
      None
    } else {
      let kind = if ctx.quote_depth > 0 { MarkerKind::Quote } else { MarkerKind::Indent };
      content = content.split('\n').collect::<Vec<_>>().join(&format!("\n{}", prefix));
      Some(MdItem::Marker(kind, prefix))
    };
    self.emit(fence.clone());
    self.result.push(MdItem::BlockEnd);
    if let Some(m) = &marker {
      self.result.push(m.clone());
    }
    self.emit(content);
    self.result.push(MdItem::BlockEnd);
    if let Some(m) = &marker {
      self.result.push(m.clone());
    }
    self.emit(fence);
    self.result.push(MdItem::BlockEnd);
  }

  fn close_row_frame(&mut self, frame: &Frame) {
    let cells = frame.cells.clone().unwrap_or_default();
    if cells.is_empty() {
      return;
    }
    let is_header = match frame.table_frame {
      None => true,
      Some(t) => self.stack.get(t).map(|f| f.table_rows == 0).unwrap_or(true),
    };
    if let Some(t) = frame.table_frame
      && let Some(table) = self.stack.get_mut(t)
    {
      table.table_rows += 1;
    }
    let ctx = self.top().clone();
    self.open_line_boundary(&ctx, false);
    self.emit(format!("| {} |", cells.join(" | ")));
    if is_header {
      self.write_boundary(&ctx, false);
      self.emit(format!("| {} |", vec!["---"; cells.len()].join(" | ")));
    }
    if frame.table_frame.is_none() {
      self.write_boundary(&ctx, true);
    }
  }

  fn close_table_frame(&mut self, table: Option<&Frame>) {
    let parent = self.top().clone();
    if let Some(t) = table
      && t.table_rows == 0
      && t.table_do_store
    {
      return;
    }
    if parent.do_store {
      self.write_boundary(&parent, true);
    }
  }

  fn close_stale_cell(&mut self) {
    for i in (1..self.stack.len()).rev() {
      let tag = self.stack[i].tag.clone().unwrap_or_default();
      if tag == "tr" || tag == "table" {
        return;
      }
      if tag == "td" || tag == "th" {
        while self.stack.len() > i + 1 {
          let orphan = self.stack.pop().unwrap();
          if let Some(c) = self.tag_open_counts.get_mut(&orphan.tag) {
            *c = c.saturating_sub(1);
          }
        }
        let frame = self.stack[i].clone();
        self.pop_to(&tag);
        self.close_cell_frame(&frame);
        return;
      }
    }
  }

  fn close_stale_row(&mut self) {
    self.close_stale_cell();
    let top = self.top().clone();
    if top.tag_is("tr") {
      self.pop_to("tr");
      self.close_row_frame(&top);
    }
  }

  fn close(&mut self) {
    while self.stack.len() > 1 {
      let top = self.top().clone();
      let tag = top.tag.clone().unwrap_or_default();
      if !self.pop_to(&tag) {
        // Unreachable in practice; never loop forever on a bad count.
        self.stack.pop();
      }
      if top.emphasis_delim.is_some() {
        self.close_emphasis_frame(&top);
      } else if tag == "td" || tag == "th" {
        self.close_cell_frame(&top);
      } else if tag == "tr" {
        self.close_row_frame(&top);
      } else if tag == "table" {
        self.close_table_frame(Some(&top));
      } else if matches!(tag.as_str(), "code" | "pre" | "samp") && top.do_store {
        self.close_pre_frame(&top, &tag);
      }
    }
  }

  fn last_is_line_start(&self) -> bool {
    match self.result.last() {
      None => true,
      Some(MdItem::BlockEnd | MdItem::Marker(..) | MdItem::ParaBreak { .. }) => true,
      Some(MdItem::Text(s)) => s.ends_with('\n'),
    }
  }
}

fn longest_backtick_run(text: &str) -> usize {
  let mut longest = 0;
  let mut current = 0;
  for ch in text.chars() {
    if ch == '`' {
      current += 1;
      longest = longest.max(current);
    } else {
      current = 0;
    }
  }
  longest
}

/// Combine tokens into normalized Markdown lines (`HTMLMarkdownConverter._finalize`).
fn finalize(result: &[MdItem]) -> String {
  let mut out = String::new();
  let mut accum: Option<String> = None;
  let mut accum_is_marker = false;
  let mut marker_boundary_passed = false;
  let mut terminated = false;
  let mut para_break_seen = false;
  let mut para_break_prefix: Option<String> = None;
  let mut para_break_in_quote = false;
  let mut boundary_after_terminate = false;

  for item in result {
    if item.is_boundary() {
      if let MdItem::ParaBreak { prefix, in_quote } = item {
        if !prefix.is_empty() {
          para_break_prefix = Some(prefix.clone());
          para_break_in_quote = *in_quote;
        } else {
          para_break_seen = true;
          para_break_prefix = None;
          para_break_in_quote = false;
        }
      }
      if accum.is_none() {
        continue;
      }
      if accum_is_marker {
        marker_boundary_passed = true;
        continue;
      }
      if terminated {
        boundary_after_terminate = true;
      }
      terminated = true;
      continue;
    }

    if terminated {
      let sep = if let Some(prefix) = &para_break_prefix {
        if para_break_in_quote { format!("\n{}\n", pystr::rstrip_chars(prefix, ASCII_WHITESPACE)) } else { "\n\n".to_string() }
      } else if para_break_seen {
        "\n\n".to_string()
      } else {
        "\n".to_string()
      };
      let a = accum.take().unwrap_or_default();
      out.push_str(pystr::rstrip_chars(&a, ASCII_WHITESPACE));
      out.push_str(&sep);
      if let Some(prefix) = para_break_prefix.take() {
        accum = Some(prefix);
        accum_is_marker = true;
        marker_boundary_passed = boundary_after_terminate;
      } else {
        accum = None;
        accum_is_marker = false;
        marker_boundary_passed = false;
      }
      terminated = false;
      para_break_seen = false;
      para_break_in_quote = false;
      boundary_after_terminate = false;
    }

    if let Some(a) = accum.as_mut() {
      let item_is_marker = item.is_marker();
      if marker_boundary_passed && accum_is_marker && item_is_marker {
        *a = item.text().to_string();
        accum_is_marker = true;
      } else {
        a.push_str(item.text());
        accum_is_marker = accum_is_marker && item_is_marker;
      }
      marker_boundary_passed = false;
    } else {
      accum = Some(item.text().to_string());
      accum_is_marker = item.is_marker();
      para_break_seen = false;
      para_break_prefix = None;
      para_break_in_quote = false;
    }
  }
  if let Some(a) = accum
    && !accum_is_marker
  {
    out.push_str(pystr::rstrip_chars(&a, ASCII_WHITESPACE));
  }
  out
}

/// Escape text that could become block syntax at the start of a line.
fn escape_line_start(content: &str) -> String {
  let b = content.as_bytes();
  // An ordered-list marker: ^\d+\.(?=[ \t]|$)
  let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
  if digits > 0 && b.get(digits) == Some(&b'.') && matches!(b.get(digits + 1), None | Some(b' ' | b'\t')) {
    return format!("{}\\{}", &content[..digits], &content[digits..]);
  }
  // An unordered-list marker: ^[-+](?=[ \t]|$)
  if matches!(b.first(), Some(b'-' | b'+')) && matches!(b.get(1), None | Some(b' ' | b'\t')) {
    return format!("\\{}", content);
  }
  // A thematic break or setext underline.
  let trimmed = pystr::strip_chars(content, ASCII_WHITESPACE);
  if !trimmed.is_empty() && is_thematic(trimmed) {
    let first = trimmed.chars().next().unwrap();
    let idx = content.find(first).unwrap_or(0);
    return format!("{}\\{}", &content[..idx], &content[idx..]);
  }
  content.to_string()
}

// ^(?:-[ \t]*){3,}$|^(?:=[ \t]*){3,}$
fn is_thematic(s: &str) -> bool {
  for marker in ['-', '='] {
    let mut count = 0;
    let mut ok = true;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
      if c == marker {
        count += 1;
        while matches!(chars.peek(), Some(' ' | '\t')) {
          chars.next();
        }
      } else {
        ok = false;
        break;
      }
    }
    if ok && count >= 3 {
      return true;
    }
  }
  false
}

fn markdown_escape(s: &str) -> String {
  let mut out = String::with_capacity(s.len() + 8);
  for c in s.chars() {
    if matches!(c, '\\' | '`' | '*' | '_' | '~' | '#' | '[' | ']' | '(' | ')' | '!' | '<' | '>') {
      out.push('\\');
    }
    out.push(c);
  }
  out
}

impl HtmlHandler for MarkdownConverter {
  fn handle_data(&mut self, data: &str) {
    if !self.top().do_store {
      return;
    }
    if self.top().preserve_cr {
      if let Some(buf) = self.top_mut().buffer.as_mut() {
        buf.push(data.to_string());
      }
      return;
    }
    // Preserve deliberate non-breaking spaces even at block boundaries.
    if pystr::strip(data).is_empty() && data.contains('\u{a0}') {
      self.emit("\u{a0}".into());
      return;
    }
    let content = pystr::collapse_whitespace(data);
    let at_boundary = match self.result.last() {
      None => true,
      Some(i) => i.is_boundary() || i.is_marker(),
    };
    if pystr::strip(&content).is_empty() && at_boundary {
      return;
    }
    let is_line_start = self.last_is_line_start();
    let mut content = markdown_escape(&content);
    if is_line_start {
      content = escape_line_start(&content);
    }
    if self.top().emphasis_delim.is_some() && !self.top().emphasis_started {
      let stripped = pystr::lstrip_chars(&content, ASCII_WHITESPACE).to_string();
      if stripped.len() != content.len() {
        let leading = content[..content.len() - stripped.len()].to_string();
        let delim = self.result.pop();
        self.result.push(MdItem::Text(leading));
        if let Some(d) = delim {
          self.result.push(d);
        }
        content = stripped;
      }
      if content.is_empty() {
        return;
      }
      self.top_mut().emphasis_started = true;
    }
    self.emit(content);
  }

  fn handle_starttag(&mut self, tag: &str, attrs: &Attrs) {
    let ctx = self.top().clone();
    if ctx.preserve_cr {
      return;
    }

    if tag == "ul" || tag == "ol" {
      let mut counter: u128 = 1;
      if tag == "ol"
        && let Some(Some(start)) = attr(attrs, "start")
        && let Some(v) = py_int(start)
      {
        counter = v.max(0) as u128;
      }
      let mut frame = self.make_frame(tag);
      frame.do_store = false;
      frame.list_type = Some(if tag == "ol" { "ol" } else { "ul" });
      frame.depth = (ctx.depth + 1).min(LIST_DEPTH_MAX);
      frame.counter = if tag == "ol" { Some(counter) } else { None };
      if !(ctx.tag_is("ul") || ctx.tag_is("ol")) {
        frame.list_do_store = ctx.do_store;
      }
      if ctx.depth == 0 {
        frame.list_indent_base = ctx.list_indent.clone();
      }
      if ctx.do_store && !ctx.tag_is("li") {
        self.open_line_boundary(&ctx, true);
      }
      self.push_frame(frame);
      return;
    }

    if tag == "li" {
      let mut ctx = ctx;
      if ctx.list_type == Some("ol")
        && ctx.counter.is_some()
        && let Some(Some(value)) = attr(attrs, "value")
        && let Some(v) = py_int(value)
      {
        let v = v.max(0) as u128;
        self.top_mut().counter = Some(v);
        ctx.counter = Some(v);
      }
      let indent = self.indent(ctx.depth);
      let marker_text = match (ctx.list_type, ctx.counter) {
        (Some("ol"), Some(c)) => format!("{}{}. ", indent, c),
        _ => format!("{}- ", indent),
      };
      let do_store = ctx.list_do_store;
      let mut frame = self.make_frame(tag);
      frame.do_store = do_store;
      frame.content_seq_at_open = Some(self.content_seq);
      frame.list_indent = format!("{}{}", ctx.list_indent_base, " ".repeat(marker_text.chars().count()));
      if !self.push_frame(frame) {
        return;
      }
      if do_store {
        if !self.result.last().is_some_and(MdItem::is_marker) {
          self.result.push(MdItem::BlockEnd);
          if ctx.quote_depth > 0 {
            let p = self.quote_prefix(ctx.quote_depth);
            self.result.push(MdItem::Marker(MarkerKind::Quote, p));
          }
        }
        self.result.push(MdItem::Marker(MarkerKind::List, marker_text));
      }
      return;
    }

    if tag == "blockquote" {
      let depth = (ctx.quote_depth + 1).min(BLOCKQUOTE_DEPTH_MAX);
      let mut frame = self.make_frame(tag);
      frame.quote_depth = depth;
      if !self.push_frame(frame) {
        return;
      }
      if ctx.do_store {
        let last = self.result.last().cloned();
        match last {
          Some(MdItem::Marker(MarkerKind::Quote, _)) | Some(MdItem::ParaBreak { in_quote: true, .. }) => {
            if depth > ctx.quote_depth {
              self.result.push(MdItem::Marker(MarkerKind::Quote, "> ".into()));
            }
          }
          Some(MdItem::Marker(..)) => {
            let p = self.quote_prefix(depth);
            self.result.push(MdItem::Marker(MarkerKind::Quote, p));
          }
          Some(MdItem::Text(_)) => {
            self.write_boundary(&ctx, true);
            if depth > ctx.quote_depth {
              self.result.push(MdItem::Marker(MarkerKind::Quote, "> ".into()));
            }
          }
          _ => {
            let p = format!("{}{}", ctx.list_indent, self.quote_prefix(depth));
            self.result.push(MdItem::Marker(MarkerKind::Quote, p));
          }
        }
      }
      return;
    }

    if tag == "table" {
      let nested_in_cell = self.open_count("td") > 0 || self.open_count("th") > 0;
      if ctx.do_store && !nested_in_cell {
        self.open_line_boundary(&ctx, true);
      }
      let mut frame = self.make_frame(tag);
      frame.do_store = false;
      frame.table_do_store = if nested_in_cell { false } else { ctx.do_store };
      frame.table_rows = 0;
      self.push_frame(frame);
      return;
    }

    if tag == "tr" {
      self.close_stale_row();
      let row_ctx = self.top().clone();
      let is_table = row_ctx.tag_is("table");
      let mut frame = self.make_frame(tag);
      frame.do_store = false;
      frame.cells = Some(Vec::new());
      frame.table_frame = if is_table { Some(self.stack.len() - 1) } else { None };
      frame.table_do_store = if is_table { row_ctx.table_do_store } else { row_ctx.do_store };
      self.push_frame(frame);
      return;
    }

    if tag == "td" || tag == "th" {
      self.close_stale_cell();
      let cell_ctx = self.top().clone();
      if cell_ctx.tag_is("tr") {
        let mut frame = self.make_frame(tag);
        frame.do_store = cell_ctx.table_do_store;
        frame.cell_result_start = self.result.len();
        self.push_frame(frame);
        return;
      }
    }

    if matches!(tag, "code" | "pre" | "samp") {
      let mut frame = self.make_frame(tag);
      frame.preserve_cr = true;
      frame.buffer = Some(Vec::new());
      self.push_frame(frame);
    } else if tag == "body" {
      let mut frame = self.make_frame(tag);
      frame.do_store = true;
      self.push_frame(frame);
    } else if IGNORE_TAGS.contains(&tag) {
      let mut frame = self.make_frame(tag);
      frame.do_store = false;
      self.push_frame(frame);
    }

    if !ctx.do_store {
      return;
    }

    if MD_BLOCK_TAGS.contains(&tag) {
      self.open_line_boundary(&ctx, PARA_LIKE_TAGS.contains(&tag));
    }

    match tag {
      "br" => {
        let prefix = self.ctx_prefix(&ctx);
        self.emit(format!("  \n{}", prefix));
      }
      "hr" => {
        self.open_line_boundary(&ctx, true);
        self.emit("---".into());
        self.result.push(MdItem::BlockEnd);
      }
      "h1" => self.emit("# ".into()),
      "h2" => self.emit("## ".into()),
      "h3" => self.emit("### ".into()),
      "h4" => self.emit("#### ".into()),
      "h5" => self.emit("##### ".into()),
      "h6" => self.emit("###### ".into()),
      "strong" | "b" | "em" | "i" => {
        let delim = if tag == "strong" || tag == "b" { "**" } else { "*" };
        let mut frame = self.make_frame(tag);
        frame.emphasis_delim = Some(delim);
        frame.emphasis_started = false;
        frame.content_seq_at_open = Some(self.content_seq);
        frame.emphasis_result_start = self.result.len();
        if self.push_frame(frame) {
          self.result.push(MdItem::Text(delim.into()));
        }
      }
      "a" => {
        let Some(Some(href)) = attr(attrs, "href") else {
          return;
        };
        let href = sanitize_href(href);
        let mut safe = String::with_capacity(href.len() + 2);
        for c in href.chars() {
          if matches!(c, '\\' | '<' | '>') {
            safe.push('\\');
          }
          safe.push(c);
        }
        let mut frame = self.make_frame(tag);
        frame.href = Some(format!("<{}>", safe));
        if self.push_frame(frame) {
          self.emit("[".into());
        }
      }
      _ => {}
    }
  }

  fn handle_endtag(&mut self, tag: &str) {
    let ctx = self.top().clone();

    if tag == "a" {
      let href = self.stack.iter().rev().find(|f| f.tag_is("a")).and_then(|f| f.href.clone());
      self.pop_to(tag);
      if let Some(href) = href {
        self.emit(format!("]({})", href));
      }
      return;
    }

    if tag == "ul" || tag == "ol" {
      if !self.pop_to(tag) {
        return;
      }
      let parent = self.top().clone();
      if parent.do_store {
        self.write_boundary(&parent, !parent.tag_is("li"));
      }
      return;
    }

    if tag == "li" {
      if self.open_count(tag) == 0 {
        return;
      }
      let li_frame = self.stack.iter().rev().find(|f| f.tag_is("li")).cloned().unwrap_or_else(|| ctx.clone());
      let has_content = li_frame.do_store && self.content_seq != li_frame.content_seq_at_open.unwrap_or(self.content_seq);
      if li_frame.do_store {
        self.result.push(MdItem::BlockEnd);
      }
      self.pop_to(tag);
      let parent = self.top_mut();
      if parent.list_type == Some("ol")
        && has_content
        && let Some(c) = parent.counter.as_mut()
      {
        *c = c.saturating_add(1);
      }
      return;
    }

    if tag == "blockquote" {
      if !self.pop_to(tag) {
        return;
      }
      let parent = self.top().clone();
      if parent.do_store {
        self.write_boundary(&parent, true);
      }
      return;
    }

    if tag == "td" || tag == "th" {
      if ctx.tag_is(tag) {
        self.pop_to(tag);
        self.close_cell_frame(&ctx);
      }
      return;
    }
    if tag == "tr" {
      self.close_stale_cell();
      let row_ctx = self.top().clone();
      if row_ctx.tag_is("tr") {
        self.pop_to(tag);
        self.close_row_frame(&row_ctx);
      }
      return;
    }
    if tag == "table" {
      self.close_stale_row();
      let table_ctx = self.top().clone();
      if table_ctx.tag_is("table") {
        self.pop_to(tag);
        self.close_table_frame(Some(&table_ctx));
      }
      return;
    }

    if matches!(tag, "code" | "pre" | "samp") {
      if ctx.do_store && ctx.tag_is(tag) {
        self.close_pre_frame(&ctx, tag);
      }
      self.pop_to(tag);
      return;
    }

    if !ctx.do_store || ctx.preserve_cr {
      if tag == "body" || IGNORE_TAGS.contains(&tag) {
        self.pop_to(tag);
      }
      return;
    }

    if MD_BLOCK_TAGS.contains(&tag) {
      self.write_boundary(&ctx, PARA_LIKE_TAGS.contains(&tag));
    }

    if matches!(tag, "strong" | "b" | "em" | "i") {
      if ctx.tag_is(tag) && ctx.emphasis_delim.is_some() {
        self.pop_to(tag);
        self.close_emphasis_frame(&ctx);
      }
      return;
    }

    if tag == "body" || IGNORE_TAGS.contains(&tag) {
      self.pop_to(tag);
    }
  }
}

/// Remove controls browsers ignore and neutralize executable schemes.
fn sanitize_href(href: &str) -> String {
  let cleaned: String = href
    .chars()
    .filter(|&c| !(matches!(c, '\x00'..='\x1f' | '\x7f' | '\u{ad}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')))
    .collect();
  let cleaned = pystr::strip(&cleaned);
  // ^([a-zA-Z][a-zA-Z0-9+.-]*):
  let b = cleaned.as_bytes();
  if b.first().is_some_and(|c| c.is_ascii_alphabetic()) {
    let len = b.iter().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-')).count();
    if b.get(len) == Some(&b':') && UNSAFE_HREF_SCHEMES.contains(&cleaned[..len].to_ascii_lowercase().as_str()) {
      return "#".to_string();
    }
  }
  cleaned.to_string()
}

/// Convert HTML to CommonMark (upstream `html_to_markdown`).
pub fn html_to_markdown(content: &str) -> String {
  let mut parser = MarkdownConverter::new();
  pyhtml::parse(content, &mut parser);
  parser.close();
  let joined = finalize(&parser.result);
  pystr::strip(&joined).to_string()
}

// ─── Reduced tag sets ────────────────────────────────────────────────────────

/// Collapse source line breaks and nearby whitespace as HTML does:
/// `(?:(?<![ \t])[ \t]+)?[\r\n]+[ \t]*`. Returns `(start, end)` byte spans.
fn html_newline_spans(data: &str) -> Vec<(usize, usize)> {
  let b = data.as_bytes();
  let n = b.len();
  let mut spans = Vec::new();
  let mut i = 0;
  while i < n {
    let c = b[i];
    if c == b' ' || c == b'\t' {
      // A blank run only counts when it starts the run and reaches a line break.
      let run_start = i;
      let mut j = i;
      while j < n && (b[j] == b' ' || b[j] == b'\t') {
        j += 1;
      }
      if j < n && (b[j] == b'\r' || b[j] == b'\n') {
        let mut k = j;
        while k < n && (b[k] == b'\r' || b[k] == b'\n') {
          k += 1;
        }
        while k < n && (b[k] == b' ' || b[k] == b'\t') {
          k += 1;
        }
        spans.push((run_start, k));
        i = k;
      } else {
        i = j;
      }
      continue;
    }
    if c == b'\r' || c == b'\n' {
      let mut k = i;
      while k < n && (b[k] == b'\r' || b[k] == b'\n') {
        k += 1;
      }
      while k < n && (b[k] == b' ' || b[k] == b'\t') {
        k += 1;
      }
      spans.push((i, k));
      i = k;
      continue;
    }
    i += 1;
  }
  spans
}

/// Configuration and state shared by every tag reducer (upstream
/// `HTMLTagReducer`).
///
/// A reducer rewrites HTML so it only uses a small set of tags:
///
/// - Tags in [`inline_map`](Self::inline_map) are written as the tag they map to.
/// - Headings become bold text on their own line.
/// - List items become `- ` (or `1. `) lines and table rows become lines
///   with cells split by ` | `.
/// - Images are replaced by their alt text.
/// - Anything inside a [`skip_tags`](Self::skip_tags) tag is hidden.
/// - Any other tag is dropped and only its text is kept.
/// - Every opened tag is closed, so the result is always balanced.
///
/// Use it directly through [`HtmlTagReducer::reduce`], or wrap it in your own
/// type and implement [`TagReducer`] to change how individual tags are
/// written, as upstream's Telegram and Pushover reducers do.
#[derive(Debug, Clone)]
pub struct HtmlTagReducer {
  /// Source tag → the tag written in its place.
  pub inline_map: Vec<(&'static str, &'static str)>,
  /// Tags written as bold text on a line of their own.
  pub headings: Vec<&'static str>,
  /// Tags whose content is never shown.
  pub skip_tags: Vec<&'static str>,
  /// Page and block tags; whitespace right after one only lays out the
  /// HTML source, so its line breaks are not kept.
  pub layout_tags: Vec<&'static str>,
  /// What a line break looks like in the output.
  pub line_break: &'static str,
  /// Characters swapped in text before it is escaped.
  pub text_map: Vec<(char, &'static str)>,

  /// The rewritten message, one piece at a time.
  pub out: Vec<String>,
  /// Open output tags as `(source tag, written tag)` pairs.
  pub open_tags: Vec<(String, String)>,
  /// How many times each source tag is open.
  pub open_count: HashMap<String, usize>,
  /// One entry per open list: the next number for numbered lists, else `None`.
  pub lists: Vec<Option<u64>>,
  /// Depth of tags whose content is hidden.
  pub skip: usize,
  /// Number of cells written on the current table row.
  pub cells: usize,
  /// True when the last tag seen was a layout, heading or hidden tag.
  pub after_layout: bool,
}

impl Default for HtmlTagReducer {
  fn default() -> Self {
    HtmlTagReducer {
      inline_map: Vec::new(),
      headings: vec!["h1", "h2", "h3", "h4", "h5", "h6"],
      skip_tags: vec!["script", "style"],
      layout_tags: vec![
        "html",
        "head",
        "body",
        "meta",
        "link",
        "base",
        "title",
        "p",
        "div",
        "br",
        "hr",
        "blockquote",
        "pre",
        "ul",
        "ol",
        "li",
        "table",
        "thead",
        "tbody",
        "tfoot",
        "tr",
        "td",
        "th",
      ],
      line_break: "\n",
      text_map: Vec::new(),
      out: Vec::new(),
      open_tags: Vec::new(),
      open_count: HashMap::new(),
      lists: Vec::new(),
      skip: 0,
      cells: 0,
      after_layout: false,
    }
  }
}

impl HtmlTagReducer {
  /// Clear every piece of reducer state (the configuration is kept).
  pub fn clear(&mut self) {
    self.out.clear();
    self.open_tags.clear();
    self.open_count.clear();
    self.lists.clear();
    self.skip = 0;
    self.cells = 0;
    self.after_layout = false;
  }

  /// How many times `source` is currently open.
  pub fn is_open(&self, source: &str) -> bool {
    self.open_count.get(source).copied().unwrap_or(0) > 0
  }

  /// Write a supported tag and remember it so it gets closed.
  pub fn open(&mut self, source: &str, tag: &str, attrs: &str) {
    self.out.push(format!("<{}{}>", tag, attrs));
    self.open_tags.push((source.to_string(), tag.to_string()));
    *self.open_count.entry(source.to_string()).or_insert(0) += 1;
  }

  /// Close `source` and anything opened after it.
  pub fn close(&mut self, source: &str) {
    if !self.is_open(source) {
      return;
    }
    while let Some((opened, tag)) = self.open_tags.pop() {
      if let Some(c) = self.open_count.get_mut(&opened) {
        *c = c.saturating_sub(1);
      }
      self.out.push(format!("</{}>", tag));
      if opened == source {
        break;
      }
    }
  }

  /// Write one stretch of text with its special characters escaped.
  pub fn write(&mut self, data: &str) {
    let data = if self.out.last().is_none_or(|l| l == self.line_break) { pystr::lstrip(data) } else { data };
    if data.is_empty() {
      return;
    }
    let mapped: String = if self.text_map.is_empty() {
      data.to_string()
    } else {
      let mut s = String::with_capacity(data.len());
      for c in data.chars() {
        match self.text_map.iter().find(|(k, _)| *k == c) {
          Some((_, v)) => s.push_str(v),
          None => s.push(c),
        }
      }
      s
    };
    self.out.push(pystr::escape_html(&mapped));
  }

  /// Return `html` rewritten with only the supported tags.
  pub fn reduce(&mut self, html: &str) -> String {
    reduce_with(self, html)
  }
}

/// Hooks for a custom tag reducer (the overridable methods of upstream
/// `HTMLTagReducer`). Every hook has the base behaviour as its default; call
/// the matching `reducer_*` function to fall back to it from an override.
pub trait TagReducer {
  /// The shared state and configuration.
  fn state(&mut self) -> &mut HtmlTagReducer;

  /// Clear any extra state the implementation keeps (`reset()`).
  fn on_reset(&mut self) {}

  /// Every start tag, before skipped tags are filtered (`handle_starttag`).
  fn handle_starttag(&mut self, tag: &str, attrs: &Attrs) {
    reducer_handle_starttag(self, tag, attrs);
  }

  /// Write the markup for an opening tag (`_start`).
  fn start(&mut self, tag: &str, attrs: &Attrs) {
    reducer_start(self, tag, attrs);
  }

  /// Close a tag and end its line when it needs one (`_end`).
  fn end(&mut self, tag: &str) {
    reducer_end(self, tag);
  }

  /// Write text, keeping the line breaks written inside it (`_text`).
  fn text(&mut self, data: &str) {
    reducer_text(self, data);
  }

  /// Start a new line unless already on an empty one (`_line`).
  fn line(&mut self) {
    reducer_line(self);
  }
}

impl TagReducer for HtmlTagReducer {
  fn state(&mut self) -> &mut HtmlTagReducer {
    self
  }
}

/// Base `handle_starttag`: hide skipped tags, hand the rest to `start()`.
pub fn reducer_handle_starttag<R: TagReducer + ?Sized>(r: &mut R, tag: &str, attrs: &Attrs) {
  let s = r.state();
  s.after_layout = s.layout_tags.contains(&tag) || s.headings.contains(&tag) || s.skip_tags.contains(&tag);
  if s.skip_tags.contains(&tag) {
    s.skip += 1;
    return;
  }
  r.start(tag, attrs);
}

/// Base `_start`.
pub fn reducer_start<R: TagReducer + ?Sized>(r: &mut R, tag: &str, attrs: &Attrs) {
  let mapped = r.state().inline_map.iter().find(|(k, _)| *k == tag).map(|(_, v)| *v);
  if let Some(written) = mapped {
    r.state().open(tag, written, "");
  } else if r.state().headings.contains(&tag) {
    r.line();
    r.state().open(tag, "b", "");
  } else if tag == "ul" || tag == "ol" {
    r.line();
    r.state().lists.push(if tag == "ol" { Some(1) } else { None });
  } else if tag == "li" {
    r.line();
    let s = r.state();
    let number = s.lists.last().copied().flatten();
    let depth = s.lists.len().min(LIST_DEPTH_MAX);
    let indent = "  ".repeat(depth.saturating_sub(1));
    match number {
      None => s.out.push(format!("{}- ", indent)),
      Some(n) => {
        s.out.push(format!("{}{}. ", indent, n));
        if let Some(last) = s.lists.last_mut() {
          *last = Some(n + 1);
        }
      }
    }
  } else if tag == "tr" {
    r.line();
    r.state().cells = 0;
  } else if tag == "td" || tag == "th" {
    let s = r.state();
    if s.cells > 0 {
      s.out.push(" | ".into());
    }
    s.cells += 1;
  } else if tag == "img"
    && let Some(Some(alt)) = attr(attrs, "alt")
    && !alt.is_empty()
  {
    r.state().out.push(pystr::escape_html(alt));
  }
}

/// Base `_end`.
pub fn reducer_end<R: TagReducer + ?Sized>(r: &mut R, tag: &str) {
  r.state().close(tag);
  if (tag == "ul" || tag == "ol") && !r.state().lists.is_empty() {
    r.state().lists.pop();
  }
  if r.state().headings.contains(&tag) || matches!(tag, "ul" | "ol" | "li" | "tr") {
    r.line();
  }
}

/// Base `_line`.
pub fn reducer_line<R: TagReducer + ?Sized>(r: &mut R) {
  let s = r.state();
  if s.out.last().is_some_and(|l| l != s.line_break) {
    let lb = s.line_break.to_string();
    s.out.push(lb);
  }
}

/// Base `_text`.
pub fn reducer_text<R: TagReducer + ?Sized>(r: &mut R, data: &str) {
  let s = r.state();
  if s.after_layout && pystr::strip(data).is_empty() {
    // Whitespace after a layout tag only lays out the HTML source.
    let mut collapsed = String::with_capacity(data.len());
    let mut last = 0;
    for (a, b) in html_newline_spans(data) {
      collapsed.push_str(&data[last..a]);
      collapsed.push(' ');
      last = b;
    }
    collapsed.push_str(&data[last..]);
    s.write(&collapsed);
    return;
  }
  let mut pos = 0;
  for (a, b) in html_newline_spans(data) {
    s.write(&data[pos..a]);
    pos = b;
    let matched = data[a..b].replace("\r\n", "\n").replace('\r', "\n");
    let mut breaks = matched.matches('\n').count() as isize;
    if s.out.last().is_none_or(|l| l == s.line_break) {
      breaks -= 1;
    }
    for _ in 0..breaks.max(0) {
      let lb = s.line_break.to_string();
      s.out.push(lb);
    }
  }
  s.write(&data[pos..]);
}

struct ReducerDriver<'a, R: TagReducer + ?Sized>(&'a mut R);

impl<R: TagReducer + ?Sized> HtmlHandler for ReducerDriver<'_, R> {
  fn handle_starttag(&mut self, tag: &str, attrs: &Attrs) {
    self.0.handle_starttag(tag, attrs);
  }

  fn handle_startendtag(&mut self, tag: &str, attrs: &Attrs) {
    // Treat <br />-style tags like their opening tag.
    if matches!(tag, "br" | "hr" | "img") {
      self.0.handle_starttag(tag, attrs);
    }
  }

  fn handle_endtag(&mut self, tag: &str) {
    let s = self.0.state();
    s.after_layout = s.layout_tags.contains(&tag) || s.headings.contains(&tag) || s.skip_tags.contains(&tag);
    if s.skip_tags.contains(&tag) {
      s.skip = s.skip.saturating_sub(1);
      return;
    }
    self.0.end(tag);
  }

  fn handle_data(&mut self, data: &str) {
    if self.0.state().skip == 0 {
      self.0.text(data);
    }
  }
}

/// Run a reducer over `html` (upstream `HTMLTagReducer.reduce`).
pub fn reduce_with<R: TagReducer + ?Sized>(r: &mut R, html: &str) -> String {
  r.state().clear();
  r.on_reset();
  pyhtml::parse(html, &mut ReducerDriver(r));
  let s = r.state();
  // Drop line breaks and spaces left at the very end.
  while s.out.last().is_some_and(|l| l == s.line_break || pystr::strip(l).is_empty()) {
    s.out.pop();
  }
  // Close any tag still open so the message stays balanced.
  while let Some((_, tag)) = s.open_tags.pop() {
    s.out.push(format!("</{}>", tag));
  }
  s.out.concat()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_html_to_text_basics() {
    assert_eq!(html_to_text("<b>Hello</b> <i>World</i>"), "Hello World");
    assert_eq!(html_to_text("<p>First</p><p>Second</p>"), "First\nSecond");
    assert_eq!(html_to_text("<ul><li>One</li><li>Two</li></ul>"), "- One\n- Two");
    assert_eq!(html_to_text("Above<hr>Below"), "Above\n---\nBelow");
    assert_eq!(html_to_text("<pre>  a\n    b</pre>"), "a\n    b");
  }

  #[test]
  fn test_html_to_markdown_basics() {
    assert_eq!(html_to_markdown("<b>bold</b> and <i>it</i>"), "**bold** and *it*");
    assert_eq!(html_to_markdown("<h1>first</h1><h2>second</h2>"), "# first\n## second");
    assert_eq!(html_to_markdown("<a href=\"javascript:alert(1)\">x</a>"), "[x](<#>)");
  }
}
