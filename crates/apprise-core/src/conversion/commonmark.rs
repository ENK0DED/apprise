//! Scan and repair CommonMark before plugins translate it to service
//! dialects (upstream `apprise/conversion/commonmark.py`).
//!
//! Plugins that render CommonMark into a service dialect (Slack `mrkdwn`,
//! Telegram MarkdownV2, Google Chat, ...) walk the message one character at
//! a time. These helpers give them the CommonMark rules they need: where
//! code spans and link destinations end, which `*`/`_` runs pair up as
//! emphasis, and how to repair a message that was split in the middle of a
//! construct.
//!
//! Index-based helpers take the message as `&[char]` and use character
//! indices, exactly like upstream's Python string indices:
//!
//! ```
//! use apprise_core::conversion::*;
//!
//! let body: Vec<char> = "**hi** and `x*y`".chars().collect();
//! let runs = commonmark_index_backtick_runs(&body);
//! assert_eq!(commonmark_find_backtick_run(&runs, 12, 1), Some(15));
//!
//! // Swap emphasis runs for placeholders, then render them.
//! let sentinel = commonmark_pick_emphasis_sentinel("**hi**");
//! let text: Vec<char> = "**hi**".chars().collect();
//! let (mut out, mut delims) = (String::new(), Vec::new());
//! let mut i = 0;
//! while i < text.len() {
//!   if text[i] == '*' {
//!     i = commonmark_emphasis_run(&text, i, text.len(), &mut delims, &mut out, &sentinel);
//!   } else {
//!     out.push(text[i]);
//!     i += 1;
//!   }
//! }
//! let html = commonmark_render_emphasis_markers(&out, &mut delims, ("<b>", "</b>"), ("<i>", "</i>"), &sentinel);
//! assert_eq!(html, "<b>hi</b>");
//! ```

use std::collections::HashMap;

use super::pystr;

// CommonMark ASCII punctuation; Unicode categories cover other characters.
const CM_ASCII_PUNCTUATION: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";

// CommonMark expands tabs to the next four-column stop in block indentation.
const TAB_SIZE: usize = 4;

// Raw HTML blocks close at a matching raw-tag end rather than a blank line.
const HTML_RAW_TAGS: &[&str] = &["script", "pre", "style", "textarea"];

// These HTML block tags interrupt a CommonMark paragraph.
const HTML_BLOCK_TAGS: &[&str] = &[
  "address",
  "article",
  "aside",
  "base",
  "basefont",
  "blockquote",
  "body",
  "caption",
  "center",
  "col",
  "colgroup",
  "dd",
  "details",
  "dialog",
  "dir",
  "div",
  "dl",
  "dt",
  "fieldset",
  "figcaption",
  "figure",
  "footer",
  "form",
  "frame",
  "frameset",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "head",
  "header",
  "hr",
  "html",
  "iframe",
  "legend",
  "li",
  "link",
  "main",
  "menu",
  "menuitem",
  "nav",
  "noframes",
  "ol",
  "optgroup",
  "option",
  "p",
  "param",
  "pre",
  "script",
  "section",
  "source",
  "style",
  "summary",
  "table",
  "tbody",
  "td",
  "textarea",
  "tfoot",
  "th",
  "thead",
  "title",
  "tr",
  "track",
  "ul",
];

/// Allow several full-body destination scans while keeping total work linear.
pub const SCAN_BUDGET_MULTIPLIER: i64 = 4;

/// How deeply link labels may nest; a deeper `[` is kept as plain text so
/// heavily nested input cannot make the conversion slow.
pub const MAX_LINK_DEPTH: usize = 32;

/// After the shared scan budget is spent, allow a short probe for nearby URL
/// endings without permitting an unbounded search.
pub const MIN_SCAN_ALLOWANCE: usize = 256;

// Escape these CommonMark characters when they must remain literal.
const COMMONMARK_LITERAL_CHARS: &str = "\\`*_[]()<>";

// ─── Backtick runs ───────────────────────────────────────────────────────────

/// Where every run of backticks starts, grouped by run length.
pub type BacktickRuns = HashMap<usize, Vec<usize>>;

/// List where every run of backticks starts, grouped by its length.
///
/// Lets code-span lookups jump straight to a matching run of the same length
/// instead of rescanning the text. Escaped backticks (`` \` ``) are skipped
/// because they are plain text.
///
/// Example: ``"a`b``c"`` returns `{1: [1], 2: [3]}`.
pub fn commonmark_index_backtick_runs(text: &[char]) -> BacktickRuns {
  let mut index: BacktickRuns = HashMap::new();
  let n = text.len();
  let mut i = 0;
  while i < n {
    let ch = text[i];
    if ch == '\\' && i + 1 < n {
      i += 2;
      continue;
    }
    if ch == '`' {
      let mut j = i;
      while j < n && text[j] == '`' {
        j += 1;
      }
      index.entry(j - i).or_default().push(i);
      i = j;
      continue;
    }
    i += 1;
  }
  index
}

/// Find the next backtick run of exactly `run` backticks starting at or
/// after `start`. `index` comes from [`commonmark_index_backtick_runs`].
///
/// Example: with `{1: [0, 4]}`, `start = 1` and `run = 1` return `Some(4)`.
pub fn commonmark_find_backtick_run(index: &BacktickRuns, start: usize, run: usize) -> Option<usize> {
  let positions = index.get(&run)?;
  let pos = positions.partition_point(|&p| p < start);
  positions.get(pos).copied()
}

// ─── Character classes ───────────────────────────────────────────────────────

fn cm_is_whitespace(ch: Option<char>) -> bool {
  match ch {
    None => true,
    Some(c) => matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b') || pystr::is_space_separator(c),
  }
}

fn cm_is_punctuation(ch: Option<char>) -> bool {
  match ch {
    None => false,
    Some(c) => CM_ASCII_PUNCTUATION.contains(c) || pystr::is_punct_or_symbol(c),
  }
}

fn cm_flanking(prev: Option<char>, next: Option<char>) -> (bool, bool) {
  let left = !cm_is_whitespace(next) && (!cm_is_punctuation(next) || cm_is_whitespace(prev) || cm_is_punctuation(prev));
  let right = !cm_is_whitespace(prev) && (!cm_is_punctuation(prev) || cm_is_whitespace(next) || cm_is_punctuation(next));
  (left, right)
}

/// Whether a `*` or `_` run can start emphasis, by the CommonMark flanking
/// rules. `prev_ch` and `next_ch` are the characters around the run (`None`
/// at the start or end of the text).
///
/// Example: `commonmark_can_open_emphasis('*', Some(' '), Some('x'))` is true.
pub fn commonmark_can_open_emphasis(delim_char: char, prev_ch: Option<char>, next_ch: Option<char>) -> bool {
  let (left, right) = cm_flanking(prev_ch, next_ch);
  if !left {
    return false;
  }
  if delim_char != '_' {
    return true;
  }
  // Prevent right-flanking intraword underscores from opening emphasis.
  !right || cm_is_punctuation(prev_ch)
}

/// Whether a `*` or `_` run can end emphasis, by the CommonMark flanking
/// rules.
///
/// Example: `commonmark_can_close_emphasis('*', Some('x'), Some(' '))` is true.
pub fn commonmark_can_close_emphasis(delim_char: char, prev_ch: Option<char>, next_ch: Option<char>) -> bool {
  let (left, right) = cm_flanking(prev_ch, next_ch);
  if !right {
    return false;
  }
  if delim_char != '_' {
    return true;
  }
  // Prevent left-flanking intraword underscores from closing emphasis.
  !left || cm_is_punctuation(next_ch)
}

// ─── Line and container helpers ──────────────────────────────────────────────

fn is_blank(line: &[char]) -> bool {
  line.iter().all(|&c| pystr::is_space(c))
}

fn leading_width(line: &[char], start_col: usize) -> usize {
  let mut col = start_col;
  for &ch in line {
    match ch {
      ' ' => col += 1,
      '\t' => col += TAB_SIZE - (col % TAB_SIZE),
      _ => break,
    }
  }
  col - start_col
}

fn text_width(text: &[char], start_col: usize) -> usize {
  let mut col = start_col;
  for &ch in text {
    if ch == '\t' {
      col += TAB_SIZE - (col % TAB_SIZE);
    } else {
      col += 1;
    }
  }
  col - start_col
}

fn chars_for_width(line: &[char], width: usize, start_col: usize) -> usize {
  let mut col = start_col;
  for (i, &ch) in line.iter().enumerate() {
    if col - start_col >= width {
      return i;
    }
    match ch {
      ' ' => col += 1,
      '\t' => col += TAB_SIZE - (col % TAB_SIZE),
      _ => return i,
    }
  }
  line.len()
}

fn strip_cr(line: &[char]) -> &[char] {
  if line.last() == Some(&'\r') { &line[..line.len() - 1] } else { line }
}

fn find_char(text: &[char], c: char, from: usize, end: usize) -> Option<usize> {
  let end = end.min(text.len());
  if from >= end {
    return None;
  }
  text[from..end].iter().position(|&x| x == c).map(|p| p + from)
}

fn find_seq(text: &[char], pat: &str, from: usize, end: usize) -> Option<usize> {
  let pat: Vec<char> = pat.chars().collect();
  let end = end.min(text.len());
  if pat.is_empty() || from + pat.len() > end {
    return None;
  }
  (from..=end - pat.len()).find(|&i| text[i..i + pat.len()] == pat[..])
}

fn starts_with(text: &[char], at: usize, pat: &str) -> bool {
  (at..).zip(pat.chars()).all(|(i, c)| text.get(i) == Some(&c))
}

fn next_line(text: &[char], pos: usize, end: usize) -> (&[char], usize) {
  let line_end = find_char(text, '\n', pos, end).unwrap_or(end);
  (strip_cr(&text[pos.min(line_end)..line_end]), line_end)
}

fn match_quote_prefix(line: &[char], quote_depth: usize, mut pos: usize, mut col: usize) -> Option<(usize, usize)> {
  let n = line.len();
  for _ in 0..quote_depth {
    let mut leading = 0;
    while pos < n && leading < 3 && line[pos] == ' ' {
      pos += 1;
      col += 1;
      leading += 1;
    }
    if pos >= n || line[pos] != '>' {
      return None;
    }
    pos += 1;
    col += 1;
    if pos < n && (line[pos] == ' ' || line[pos] == '\t') {
      pos += 1;
      col += 1;
    }
  }
  Some((pos, col))
}

fn scan_quote_depth(line: &[char], mut pos: usize, mut col: usize) -> (usize, usize, usize) {
  let mut depth = 0;
  let n = line.len();
  loop {
    let (mut p, mut c) = (pos, col);
    let mut leading = 0;
    while p < n && leading < 3 && line[p] == ' ' {
      p += 1;
      c += 1;
      leading += 1;
    }
    if p >= n || line[p] != '>' {
      break;
    }
    p += 1;
    c += 1;
    if p < n && (line[p] == ' ' || line[p] == '\t') {
      p += 1;
      c += 1;
    }
    pos = p;
    col = c;
    depth += 1;
  }
  (depth, pos, col)
}

// ^ {0,3}(?:[-*+]|[0-9]{1,9}[.)])
fn list_marker_len(s: &[char]) -> Option<usize> {
  let mut i = 0;
  while i < 3 && s.get(i) == Some(&' ') {
    i += 1;
  }
  match s.get(i) {
    Some('-' | '*' | '+') => Some(i + 1),
    Some(c) if c.is_ascii_digit() => {
      let digits = s[i..].iter().take(10).take_while(|c| c.is_ascii_digit()).count();
      if digits > 9 {
        return None;
      }
      match s.get(i + digits) {
        Some('.' | ')') => Some(i + digits + 1),
        _ => None,
      }
    }
    _ => None,
  }
}

fn match_list_marker(line: &[char], pos: usize, col: usize) -> Option<(usize, usize)> {
  let m = list_marker_len(&line[pos.min(line.len())..])?;
  let marker_end = pos + m;
  let marker_col = col + text_width(&line[pos..marker_end], col);
  let rest = &line[marker_end..];
  if rest.is_empty() {
    return Some((marker_end, marker_col + 1));
  }
  if rest[0] != ' ' && rest[0] != '\t' {
    return None;
  }
  let pad_width = leading_width(rest, marker_col);
  let after_pad = &rest[chars_for_width(rest, pad_width, marker_col)..];
  if after_pad.is_empty() || pad_width > 4 {
    return Some((marker_end, marker_col + 1));
  }
  let content_pos = marker_end + chars_for_width(rest, pad_width, marker_col);
  Some((content_pos, marker_col + pad_width))
}

// (quote_depth, quote_end_col, marker_width, end_pos, end_col)
fn scan_opening_prefix(line: &[char]) -> (usize, usize, usize, usize, usize) {
  if let Some((content_pos, content_col)) = match_list_marker(line, 0, 0) {
    let marker_width = content_col;
    let (quote_depth, end_pos, end_col) = scan_quote_depth(line, content_pos, content_col);
    return (quote_depth, end_col, marker_width, end_pos, end_col);
  }
  let (quote_depth, q_end_pos, q_end_col) = scan_quote_depth(line, 0, 0);
  if let Some((content_pos, content_col)) = match_list_marker(line, q_end_pos, q_end_col) {
    let marker_width = content_col - q_end_col;
    return (quote_depth, q_end_col, marker_width, content_pos, content_col);
  }
  (quote_depth, q_end_col, 0, q_end_pos, q_end_col)
}

fn match_container(line: &[char], quote_depth: usize, list_width: usize) -> Option<usize> {
  if quote_depth == 0 {
    if leading_width(line, 0) >= list_width {
      return Some(chars_for_width(line, list_width, 0));
    }
    return None;
  }
  if leading_width(line, 0) >= list_width {
    let rest_start = chars_for_width(line, list_width, 0);
    if let Some((p, _)) = match_quote_prefix(line, quote_depth, rest_start, list_width) {
      return Some(p);
    }
  }
  if let Some((end_pos, end_col)) = match_quote_prefix(line, quote_depth, 0, 0) {
    let rest = &line[end_pos..];
    if leading_width(rest, end_col) >= list_width {
      return Some(end_pos + chars_for_width(rest, list_width, end_col));
    }
  }
  None
}

fn find_fence_end(text: &[char], search_from: usize, quote_depth: usize, list_width: usize, fence_char: char, fence_len: usize) -> usize {
  let n = text.len();
  let has_container = quote_depth > 0 || list_width > 0;
  let mut pos = search_from;
  while pos < n {
    let (line, line_end) = next_line(text, pos, n);
    if is_blank(line) {
      if quote_depth > 0 && match_quote_prefix(line, quote_depth, 0, 0).is_none() {
        return pos;
      }
      pos = line_end + 1;
      continue;
    }
    let consumed = if has_container {
      match match_container(line, quote_depth, list_width) {
        Some(c) => c,
        None => return pos,
      }
    } else {
      0
    };
    let rest = &line[consumed..];
    let extra = leading_width(rest, consumed);
    if extra <= 3 {
      let candidate = &rest[chars_for_width(rest, extra, consumed)..];
      let mut end = candidate.len();
      while end > 0 && (candidate[end - 1] == ' ' || candidate[end - 1] == '\t') {
        end -= 1;
      }
      let stripped = &candidate[..end];
      if stripped.len() >= fence_len && stripped.iter().all(|&c| c == fence_char) {
        return (line_end + 1).min(n);
      }
    }
    pos = line_end + 1;
  }
  n
}

// ^(?:(`{3,})[^\n`]*|(~{3,})[^\n]*)$
fn fence_shape(candidate: &[char]) -> Option<(char, usize)> {
  let first = *candidate.first()?;
  if first != '`' && first != '~' {
    return None;
  }
  let run = candidate.iter().take_while(|&&c| c == first).count();
  if run < 3 {
    return None;
  }
  let info = &candidate[run..];
  if info.contains(&'\n') || (first == '`' && info.contains(&'`')) {
    return None;
  }
  Some((first, run))
}

fn match_fence_open(rest: &[char], start_col: usize) -> Option<(char, usize)> {
  let slack = leading_width(rest, start_col);
  if slack > 3 {
    return None;
  }
  fence_shape(&rest[chars_for_width(rest, slack, start_col)..])
}

fn skip_up_to_3_spaces(line: &[char]) -> usize {
  let mut i = 0;
  while i < 3 && line.get(i) == Some(&' ') {
    i += 1;
  }
  i
}

// ^ {0,3}#{1,6}(?:[ \t]|$)
fn atx_heading_start(line: &[char]) -> bool {
  let i = skip_up_to_3_spaces(line);
  let hashes = line[i..].iter().take(7).take_while(|&&c| c == '#').count();
  if hashes == 0 || hashes > 6 {
    return false;
  }
  matches!(line.get(i + hashes), None | Some(' ' | '\t'))
}

// ^ {0,3}>
fn quote_start(line: &[char]) -> bool {
  let i = skip_up_to_3_spaces(line);
  line.get(i) == Some(&'>')
}

fn thematic_break(line: &[char]) -> bool {
  let i = skip_up_to_3_spaces(line);
  let rest = &line[i..];
  for marker in ['-', '_', '*'] {
    let mut count = 0;
    let mut ok = !rest.is_empty();
    let mut j = 0;
    while j < rest.len() {
      if rest[j] == marker {
        count += 1;
        j += 1;
        while j < rest.len() && (rest[j] == ' ' || rest[j] == '\t') {
          j += 1;
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

// ^ {0,3}(?:=+|-+)[ \t]*$
fn setext_underline(line: &[char]) -> bool {
  let i = skip_up_to_3_spaces(line);
  let Some(&first) = line.get(i) else {
    return false;
  };
  if first != '=' && first != '-' {
    return false;
  }
  let run = line[i..].iter().take_while(|&&c| c == first).count();
  line[i + run..].iter().all(|&c| c == ' ' || c == '\t')
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum HtmlBlockKind {
  Comment,
  Pi,
  Decl,
  Cdata,
  Tag { raw: bool },
}

// Match the start of a CommonMark HTML block (types 1-6). Returns the kind
// and the match end within `line`.
fn match_html_block_start(line: &[char]) -> Option<(HtmlBlockKind, usize)> {
  let i = skip_up_to_3_spaces(line);
  if line.get(i) != Some(&'<') {
    return None;
  }
  let lower_at = |k: usize| line.get(k).map(|c| c.to_ascii_lowercase());
  if starts_with(line, i, "<!--") {
    return Some((HtmlBlockKind::Comment, i + 4));
  }
  if starts_with(line, i, "<?") {
    return Some((HtmlBlockKind::Pi, i + 2));
  }
  if line.get(i + 1) == Some(&'!') {
    if line.get(i + 2).is_some_and(|c| c.is_ascii_alphabetic()) {
      return Some((HtmlBlockKind::Decl, i + 3));
    }
    // CDATA must be written exactly; a case-folded match is rejected.
    if starts_with(line, i, "<![CDATA[") {
      return Some((HtmlBlockKind::Cdata, i + 9));
    }
    return None;
  }
  let mut j = i + 1;
  if line.get(j) == Some(&'/') {
    j += 1;
  }
  let name_len = line[j..].iter().take_while(|c| c.is_ascii_alphanumeric()).count();
  if name_len == 0 {
    return None;
  }
  let name: String = line[j..j + name_len].iter().map(|c| c.to_ascii_lowercase()).collect();
  if !HTML_BLOCK_TAGS.contains(&name.as_str()) {
    return None;
  }
  let after = j + name_len;
  let end = match lower_at(after) {
    None => after,
    Some(' ' | '\t' | '>') => after + 1,
    Some('/') if line.get(after + 1) == Some(&'>') => after + 2,
    _ => return None,
  };
  Some((HtmlBlockKind::Tag { raw: HTML_RAW_TAGS.contains(&name.as_str()) }, end))
}

// Type 7: another complete HTML tag alone on its line.
fn html_custom_tag(line: &[char]) -> bool {
  let n = line.len();
  let mut i = skip_up_to_3_spaces(line);
  if line.get(i) != Some(&'<') {
    return false;
  }
  i += 1;
  let tag_name = |i: &mut usize| -> bool {
    if !line.get(*i).is_some_and(|c| c.is_ascii_alphabetic()) {
      return false;
    }
    *i += 1;
    while *i < n && (line[*i].is_ascii_alphanumeric() || line[*i] == '-') {
      *i += 1;
    }
    true
  };
  let skip_blank = |i: &mut usize| {
    while *i < n && (line[*i] == ' ' || line[*i] == '\t') {
      *i += 1;
    }
  };
  if line.get(i) == Some(&'/') {
    i += 1;
    if !tag_name(&mut i) {
      return false;
    }
    skip_blank(&mut i);
  } else {
    if !tag_name(&mut i) {
      return false;
    }
    // Attributes: [ \t]+name(?:[ \t]*=[ \t]*value)?
    loop {
      let mut k = i;
      skip_blank(&mut k);
      if k == i || !line.get(k).is_some_and(|&c| c.is_ascii_alphabetic() || c == '_' || c == ':') {
        break;
      }
      k += 1;
      while k < n && (line[k].is_ascii_alphanumeric() || matches!(line[k], '_' | '.' | ':' | '-')) {
        k += 1;
      }
      i = k;
      let mut v = k;
      skip_blank(&mut v);
      if line.get(v) == Some(&'=') {
        v += 1;
        skip_blank(&mut v);
        match line.get(v) {
          Some(&q @ ('"' | '\'')) => {
            if let Some(p) = line[v + 1..].iter().position(|&c| c == q) {
              i = v + 1 + p + 1;
            }
          }
          Some(_) => {
            let len = line[v..].iter().take_while(|&&c| !pystr::is_space(c) && !matches!(c, '"' | '\'' | '=' | '<' | '>' | '`')).count();
            if len > 0 {
              i = v + len;
            }
          }
          None => {}
        }
      }
    }
    skip_blank(&mut i);
    if line.get(i) == Some(&'/') {
      i += 1;
    }
  }
  if line.get(i) != Some(&'>') {
    return false;
  }
  line[i + 1..].iter().all(|&c| c == ' ' || c == '\t')
}

fn looks_like_new_block(line: &[char], require_nonempty_list_item: bool) -> bool {
  let has_list_marker = match match_list_marker(line, 0, 0) {
    Some((p, _)) => !require_nonempty_list_item || !is_blank(&line[p..]),
    None => false,
  };
  has_list_marker
    || match_fence_open(line, 0).is_some()
    || atx_heading_start(line)
    || quote_start(line)
    || thematic_break(line)
    || match_html_block_start(line).is_some()
}

fn commonmark_code_spans_in_range(text: &[char], start: usize, end: usize) -> Vec<(usize, usize)> {
  let mut spans = Vec::new();
  let scoped = &text[start..end];
  let runs = commonmark_index_backtick_runs(scoped);
  let m = scoped.len();
  let mut i = 0;
  while i < m {
    if scoped[i] == '\\' && i + 1 < m {
      i += 2;
      continue;
    }
    if scoped[i] == '`' {
      let mut j = i;
      while j < m && scoped[j] == '`' {
        j += 1;
      }
      let run = j - i;
      if let Some(close) = commonmark_find_backtick_run(&runs, j, run) {
        spans.push((start + i, start + close + run));
        i = close + run;
        continue;
      }
      i = j;
      continue;
    }
    i += 1;
  }
  spans
}

// Complete backtick spans, paired only within one paragraph.
fn commonmark_code_spans(text: &[char]) -> Vec<(usize, usize)> {
  let mut spans = Vec::new();
  let n = text.len();
  let mut pos = 0;
  while pos < n {
    let (line, line_end) = next_line(text, pos, n);
    if is_blank(line) || looks_like_new_block(line, false) {
      pos = line_end + 1;
      continue;
    }
    let mut para_end = line_end;
    let mut scan_pos = line_end + 1;
    while scan_pos < n {
      let (nl, nl_end) = next_line(text, scan_pos, n);
      if is_blank(nl) || looks_like_new_block(nl, false) {
        break;
      }
      para_end = nl_end;
      scan_pos = nl_end + 1;
    }
    spans.extend(commonmark_code_spans_in_range(text, pos, para_end));
    pos = scan_pos;
  }
  spans
}

// Track nested list widths while scanning each message line once.
struct ContainerScanner {
  pos: usize,
  stack: Vec<usize>,
  paragraph_open: bool,
}

impl ContainerScanner {
  fn new() -> Self {
    ContainerScanner { pos: 0, stack: Vec::new(), paragraph_open: false }
  }

  fn width_at(&mut self, text: &[char], offset: usize) -> usize {
    let n = text.len();
    let mut pos = self.pos;
    let stack = &mut self.stack;
    let mut paragraph_open = self.paragraph_open;
    while pos < offset {
      let (line, line_end) = next_line(text, pos, n);
      if is_blank(line) {
        paragraph_open = false;
        pos = line_end + 1;
        continue;
      }
      let leading = leading_width(line, 0);
      let satisfied = stack.partition_point(|&w| w <= leading);
      if satisfied < stack.len() && paragraph_open && !looks_like_new_block(line, true) {
        pos = line_end + 1;
        continue;
      }
      if satisfied < stack.len() {
        stack.truncate(satisfied);
      }
      let matched_width = stack.last().copied().unwrap_or(0);
      let rest = &line[chars_for_width(line, matched_width, 0)..];
      if let Some((content_pos, content_col)) = match_list_marker(rest, 0, matched_width) {
        stack.push(content_col);
        let item_content = &rest[content_pos..];
        paragraph_open = !item_content.is_empty() && !looks_like_new_block(item_content, false);
        pos = line_end + 1;
        continue;
      }
      if let Some((fence_char, fence_len)) = match_fence_open(rest, matched_width) {
        pos = find_fence_end(text, line_end + 1, 0, matched_width, fence_char, fence_len);
        paragraph_open = false;
        continue;
      }
      if paragraph_open && setext_underline(rest) {
        paragraph_open = false;
      } else if paragraph_open {
        paragraph_open = !looks_like_new_block(rest, true);
      } else {
        paragraph_open = !rest.is_empty() && !looks_like_new_block(rest, false);
      }
      pos = line_end + 1;
    }
    let (line, _) = next_line(text, offset, n);
    let leading = leading_width(line, 0);
    let satisfied = stack.partition_point(|&w| w <= leading);
    if satisfied < stack.len() {
      stack.truncate(satisfied);
    }
    self.pos = pos;
    self.paragraph_open = paragraph_open;
    stack.last().copied().unwrap_or(0)
  }
}

fn container_drop_pos(text: &[char], search_from: usize, quote_depth: usize, list_width: usize) -> Option<usize> {
  if quote_depth == 0 && list_width == 0 {
    return None;
  }
  let n = text.len();
  let mut pos = search_from;
  while pos < n {
    let (line, line_end) = next_line(text, pos, n);
    if is_blank(line) {
      if quote_depth > 0 && match_quote_prefix(line, quote_depth, 0, 0).is_none() {
        return Some(pos);
      }
      pos = line_end + 1;
      continue;
    }
    if match_container(line, quote_depth, list_width).is_none() {
      return Some(pos);
    }
    pos = line_end + 1;
  }
  None
}

fn blank_line_end(text: &[char], search_from: usize, quote_depth: usize, list_width: usize) -> usize {
  let n = text.len();
  let has_container = quote_depth > 0 || list_width > 0;
  let mut pos = search_from;
  while pos < n {
    let (line, line_end) = next_line(text, pos, n);
    if is_blank(line) {
      return pos;
    }
    if !has_container {
      pos = line_end + 1;
      continue;
    }
    match match_container(line, quote_depth, list_width) {
      None => return pos,
      Some(consumed) if is_blank(&line[consumed..]) => return pos,
      Some(_) => {}
    }
    pos = line_end + 1;
  }
  n
}

fn html_block_end(text: &[char], abs_rest_start: usize, line_end: usize, kind: HtmlBlockKind, m_end: usize, quote_depth: usize, list_width: usize) -> usize {
  let n = text.len();
  let search_from = abs_rest_start + m_end;
  if let HtmlBlockKind::Tag { raw: false } = kind {
    return blank_line_end(text, line_end + 1, quote_depth, list_width);
  }
  let limit = container_drop_pos(text, line_end + 1, quote_depth, list_width).unwrap_or(n);
  let terminator = match kind {
    HtmlBlockKind::Comment => find_seq(text, "-->", search_from, limit),
    HtmlBlockKind::Pi => find_seq(text, "?>", search_from, limit),
    HtmlBlockKind::Decl => find_char(text, '>', search_from, limit),
    HtmlBlockKind::Cdata => find_seq(text, "]]>", search_from, limit),
    HtmlBlockKind::Tag { .. } => raw_tag_close(text, search_from, limit),
  };
  let Some(terminator) = terminator else {
    return limit;
  };
  let (_, term_line_end) = next_line(text, terminator, n);
  if term_line_end < n { (term_line_end + 1).min(limit) } else { limit }
}

// Any complete raw-tag closer ends the block: </(script|pre|style|textarea)>.
fn raw_tag_close(text: &[char], from: usize, limit: usize) -> Option<usize> {
  let limit = limit.min(text.len());
  let mut i = from;
  while i + 2 < limit {
    if text[i] == '<' && text[i + 1] == '/' {
      for tag in HTML_RAW_TAGS {
        let len = tag.len();
        if i + 2 + len < limit && text[i + 2..i + 2 + len].iter().zip(tag.chars()).all(|(a, b)| a.to_ascii_lowercase() == b) && text[i + 2 + len] == '>' {
          return Some(i);
        }
      }
    }
    i += 1;
  }
  None
}

fn resolve_prefix_widths(
  text: &[char],
  start: usize,
  quote_depth: usize,
  marker_width: usize,
  slack_width: usize,
  scanner: &mut Option<ContainerScanner>,
) -> (usize, usize, usize) {
  if marker_width > 0 {
    (quote_depth, marker_width, slack_width)
  } else if quote_depth > 0 || slack_width == 0 {
    (quote_depth, 0, slack_width)
  } else {
    let active = scanner.get_or_insert_with(ContainerScanner::new).width_at(text, start);
    if active <= slack_width && slack_width <= active + 3 { (quote_depth, active, slack_width - active) } else { (quote_depth, 0, slack_width) }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LeafKind {
  Fence,
  Html,
}

// Fenced code and HTML block ranges, found in a single forward pass.
fn commonmark_leaf_block_spans(text: &[char]) -> Vec<(usize, usize, LeafKind)> {
  let mut spans = Vec::new();
  let n = text.len();
  let mut pos = 0;
  let mut scanner: Option<ContainerScanner> = None;
  let mut paragraph_open = false;
  while pos < n {
    let (line, line_end) = next_line(text, pos, n);
    if is_blank(line) {
      paragraph_open = false;
      pos = line_end + 1;
      continue;
    }
    let (quote_depth, _quote_end_col, marker_width, prefix_end_pos, prefix_end_col) = scan_opening_prefix(line);
    let slack_width = leading_width(&line[prefix_end_pos..], prefix_end_col);
    let (quote_depth, list_width, extra_slack) = resolve_prefix_widths(text, pos, quote_depth, marker_width, slack_width, &mut scanner);
    if extra_slack > 3 {
      pos = line_end + 1;
      continue;
    }
    let slack_end_pos = prefix_end_pos + chars_for_width(&line[prefix_end_pos..], slack_width, prefix_end_col);
    let rest = &line[slack_end_pos..];
    let rest_col = prefix_end_col + slack_width;

    if let Some((fence_char, fence_len)) = match_fence_open(rest, rest_col) {
      let end = find_fence_end(text, line_end + 1, quote_depth, list_width, fence_char, fence_len);
      spans.push((pos, end, LeafKind::Fence));
      paragraph_open = false;
      pos = end;
      continue;
    }
    if let Some((kind, m_end)) = match_html_block_start(rest) {
      let end = html_block_end(text, pos + slack_end_pos, line_end, kind, m_end, quote_depth, list_width);
      spans.push((pos, end, LeafKind::Html));
      paragraph_open = false;
      pos = end;
      continue;
    }
    if !paragraph_open && html_custom_tag(rest) {
      let end = blank_line_end(text, line_end + 1, quote_depth, list_width);
      spans.push((pos, end, LeafKind::Html));
      paragraph_open = false;
      pos = end;
      continue;
    }
    if paragraph_open && setext_underline(rest) {
      paragraph_open = false;
    } else if paragraph_open {
      let has_nonempty_marker = marker_width > 0 && !is_blank(rest);
      paragraph_open = !(has_nonempty_marker || looks_like_new_block(rest, false));
    } else {
      paragraph_open = !rest.is_empty() && !looks_like_new_block(rest, false);
    }
    pos = line_end + 1;
  }
  spans
}

// ─── Headings ────────────────────────────────────────────────────────────────

// #{1,6}(?:[ \t]+(?P<content>.*))?[ \t]*$ ; returns the content.
fn atx_heading_shape(s: &[char]) -> Option<&[char]> {
  let hashes = s.iter().take_while(|&&c| c == '#').count();
  if hashes == 0 || hashes > 6 {
    return None;
  }
  let rest = &s[hashes..];
  match rest.first() {
    None => Some(&rest[..0]),
    Some(' ' | '\t') => {
      let ws = rest.iter().take_while(|&&c| c == ' ' || c == '\t').count();
      Some(&rest[ws..])
    }
    _ => None,
  }
}

// Remove a closing `(?<![ \t])[ \t]+#+[ \t]*$` sequence.
fn strip_closing_sequence(s: &[char]) -> &[char] {
  let mut end = s.len();
  while end > 0 && (s[end - 1] == ' ' || s[end - 1] == '\t') {
    end -= 1;
  }
  let hash_end = end;
  while end > 0 && s[end - 1] == '#' {
    end -= 1;
  }
  if end == hash_end {
    return s;
  }
  let ws_end = end;
  while end > 0 && (s[end - 1] == ' ' || s[end - 1] == '\t') {
    end -= 1;
  }
  if end == ws_end {
    return s;
  }
  &s[..end]
}

fn chars_strip(s: &[char]) -> &[char] {
  let mut a = 0;
  let mut b = s.len();
  while a < b && pystr::is_space(s[a]) {
    a += 1;
  }
  while b > a && pystr::is_space(s[b - 1]) {
    b -= 1;
  }
  &s[a..b]
}

/// Turn `# Heading` lines into bold text, for services that have bold but no
/// headings. Code blocks, code spans and HTML are left alone; list and quote
/// prefixes such as `> ` are kept.
///
/// Example: `"# Alert"` becomes `"**Alert**"`.
pub fn commonmark_headings_to_bold(body: &str) -> String {
  let text: Vec<char> = body.chars().collect();
  let mut protected = commonmark_code_spans(&text);
  protected.extend(commonmark_leaf_block_spans(&text).into_iter().map(|(s, e, _)| (s, e)));
  protected.sort();
  let mut span_idx = 0;
  let mut scanner: Option<ContainerScanner> = None;
  let n = text.len();
  let mut out = String::with_capacity(body.len());
  let mut pos = 0;
  while pos < n {
    let line_end = find_char(&text, '\n', pos, n).unwrap_or(n);
    let raw_line = &text[pos..line_end];
    let line = strip_cr(raw_line);
    while span_idx < protected.len() && protected[span_idx].1 <= pos {
      span_idx += 1;
    }
    let in_protected = span_idx < protected.len() && protected[span_idx].0 <= pos;
    let mut replacement: Option<String> = None;
    if !in_protected {
      let (quote_depth, _q, marker_width, prefix_end_pos, prefix_end_col) = scan_opening_prefix(line);
      let slack_width = leading_width(&line[prefix_end_pos..], prefix_end_col);
      let (_, _, extra_slack) = resolve_prefix_widths(&text, pos, quote_depth, marker_width, slack_width, &mut scanner);
      if extra_slack <= 3 {
        let slack_end_pos = prefix_end_pos + chars_for_width(&line[prefix_end_pos..], slack_width, prefix_end_col);
        if let Some(content) = atx_heading_shape(&line[slack_end_pos..]) {
          let heading: String = chars_strip(strip_closing_sequence(content)).iter().collect();
          let prefix: String = line[..slack_end_pos].iter().collect();
          replacement = Some(if heading.is_empty() { prefix } else { format!("{prefix}**{heading}**") });
        }
      }
    }
    match replacement {
      Some(r) => out.push_str(&r),
      None => out.extend(raw_line),
    }
    if line_end < n {
      out.push('\n');
    }
    pos = line_end + 1;
  }
  out
}

// ─── Escapes and link destinations ───────────────────────────────────────────

/// Remove the backslash from escaped punctuation. Use it on text that is not
/// Markdown, such as a link URL or label. Only punctuation CommonMark allows
/// to be escaped is changed, so Windows paths keep their backslashes.
///
/// Example: `r"a\_b"` becomes `"a_b"`, but `r"C:\Users"` stays the same.
pub fn commonmark_decode_backslash_escapes(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  let mut chars = text.chars().peekable();
  while let Some(ch) = chars.next() {
    if ch == '\\'
      && let Some(&next) = chars.peek()
      && CM_ASCII_PUNCTUATION.contains(next)
    {
      out.push(next);
      chars.next();
      continue;
    }
    out.push(ch);
  }
  out
}

/// Make a Markdown link URL safe inside a `<url|label>` style link: remove
/// Markdown backslash escapes, write `&`, `<` and `>` as HTML entities, and
/// write `|` as `%7C` so it is not read as the label separator.
///
/// Example: `r"a\|b&c"` becomes `"a%7Cb&amp;c"`.
pub fn commonmark_escape_link_url(url: &str) -> String {
  let url = commonmark_decode_backslash_escapes(url);
  url.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('|', "%7C")
}

/// A work limit shared by the link scanners for one message. Once spent,
/// scans only look a short distance ahead, so a message full of broken
/// links cannot make scanning slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanBudget(pub i64);

impl ScanBudget {
  fn spent(&self) -> bool {
    self.0 <= 0
  }
}

/// Create the [`ScanBudget`] for one message, to pass to
/// [`commonmark_scan_angle_dest`] and [`commonmark_scan_paren_dest`].
///
/// Example: a three-character body gets a budget of 12.
pub fn commonmark_new_scan_budget(body: &[char]) -> ScanBudget {
  ScanBudget(body.len() as i64 * SCAN_BUDGET_MULTIPLIER)
}

/// Start buffering a link label at a `[`.
///
/// - Records where the label starts in `out` (a byte offset) and adds the
///   `[` to `out`.
/// - Returns `false` once labels nest deeper than [`MAX_LINK_DEPTH`]; the
///   caller then keeps the `[` as plain text.
///
/// Example: with `out = "a "`, the stack becomes `[2]` and `out` `"a ["`.
pub fn commonmark_open_link_label(out: &mut String, link_stack: &mut Vec<usize>) -> bool {
  if link_stack.len() >= MAX_LINK_DEPTH {
    return false;
  }
  link_stack.push(out.len());
  out.push('[');
  true
}

/// Remove the innermost buffered link label from `out`.
///
/// - Drops the label's opening `[` and everything after it.
/// - Returns the label text, without the `[`. With no label open, `out` is
///   left alone and the text is empty.
///
/// Example: with `out = "a [label"` and the stack `[2]`, returns `"label"`
/// and leaves `out` as `"a "`.
pub fn commonmark_take_link_label(out: &mut String, link_stack: &mut Vec<usize>) -> String {
  let Some(open) = link_stack.pop() else { return String::new() };
  let text = out[open + 1..].to_string();
  out.truncate(open);
  text
}

/// Find where a `](<url>)` link ends. `i` is the index of the `]`. Returns
/// the index of the closing `>`, or `None` if not found. Escaped characters
/// such as `\>` are skipped.
///
/// Example: in `"[x](<https://a>)"`, `i = 2` returns `Some(14)`.
pub fn commonmark_scan_angle_dest(body: &[char], i: usize, n: usize, mut budget: Option<&mut ScanBudget>) -> Option<usize> {
  let start = i + 3;
  let mut k = start;
  let limit = match &budget {
    Some(b) if b.spent() => n.min(start + MIN_SCAN_ALLOWANCE + 2),
    _ => n,
  };
  while k + 1 < limit {
    if body[k] == '\\' && k + 1 < n {
      k += 2;
      continue;
    }
    if body[k] == '>' && body[k + 1] == ')' {
      if let Some(b) = budget.as_deref_mut() {
        b.0 -= (k - start) as i64;
      }
      return Some(k);
    }
    k += 1;
  }
  if let Some(b) = budget {
    b.0 -= k.saturating_sub(start) as i64;
  }
  None
}

/// Find where a `](url)` link ends. `i` is the index of the `(`. Returns the
/// index of the matching `)`, or `None` if the URL is not valid (for
/// example, it contains a space or `<`). Balanced brackets inside the URL
/// are allowed.
///
/// Example: in `"[x](a_(b))"`, `i = 3` returns `Some(9)`.
pub fn commonmark_scan_paren_dest(text: &[char], i: usize, n: usize, mut budget: Option<&mut ScanBudget>) -> Option<usize> {
  let mut depth = 1;
  let start = i + 1;
  let mut k = start;
  let limit = match &budget {
    Some(b) if b.spent() => n.min(start + MIN_SCAN_ALLOWANCE + 1),
    _ => n,
  };
  let charge = |b: &mut Option<&mut ScanBudget>, k: usize| {
    if let Some(b) = b.as_deref_mut() {
      b.0 -= k.saturating_sub(start) as i64;
    }
  };
  while k < limit {
    let ch = text[k];
    if ch == '\\' && k + 1 < n {
      k += 2;
      continue;
    }
    if ch == '(' {
      depth += 1;
    } else if ch == ')' {
      depth -= 1;
      if depth == 0 {
        charge(&mut budget, k);
        return Some(k);
      }
    } else {
      let is_control = (ch as u32) < 0x20 || ch as u32 == 0x7f;
      if ch == '<' || is_control || cm_is_whitespace(Some(ch)) {
        charge(&mut budget, k);
        return None;
      }
    }
    k += 1;
  }
  charge(&mut budget, k);
  None
}

/// Check whether `<scheme:...>` at `i` (the index of the `<`) is an
/// autolink. Returns `(end, still_valid)`: `end` is the index of the
/// closing `>`; `still_valid` is false once a space or other disallowed
/// character is seen, and stays true if the text simply ends first.
///
/// Examples: `"<https://a*b>"` returns `(Some(12), true)`;
/// `"<https://a b>"` returns `(None, false)`.
pub fn commonmark_scan_autolink_dest(text: &[char], i: usize, n: usize) -> (Option<usize>, bool) {
  // [A-Za-z][A-Za-z0-9+.\-]{1,31}:
  let s = i + 1;
  if !text.get(s).is_some_and(|c| c.is_ascii_alphabetic()) {
    return (None, false);
  }
  let run = text[s + 1..].iter().take(32).take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')).count();
  if !(1..=31).contains(&run) || text.get(s + 1 + run) != Some(&':') {
    return (None, false);
  }
  let mut k = s + 1 + run + 1;
  while k < n {
    let ch = text[k];
    if ch == '>' {
      return (Some(k), true);
    }
    let is_control = (ch as u32) < 0x20 || ch as u32 == 0x7f;
    if ch == '<' || is_control || cm_is_whitespace(Some(ch)) {
      return (None, false);
    }
    k += 1;
  }
  (None, true)
}

// ─── Emphasis ────────────────────────────────────────────────────────────────

const PRIVATE_USE_RANGES: [(u32, u32); 3] = [(0xE000, 0xF8FF), (0xF0000, 0xFFFFD), (0x100000, 0x10FFFD)];

fn is_private_use(c: char) -> bool {
  let cp = c as u32;
  PRIVATE_USE_RANGES.iter().any(|&(a, b)| (a..=b).contains(&cp))
}

fn private_use_chars() -> impl Iterator<Item = char> {
  PRIVATE_USE_RANGES.into_iter().flat_map(|(a, b)| (a..=b).filter_map(char::from_u32))
}

/// Pick a marker that does not appear anywhere in `body`, for emphasis
/// placeholders (see [`commonmark_emphasis_run`]). It comes from Unicode's
/// Private Use area, which normal text almost never uses, and is usually
/// `"\u{e000}"`. If the message uses every Private Use character, a
/// two-character marker is returned instead.
///
/// Examples: `"hi *there*"` gives `"\u{e000}"`; `"x\u{e000}"` gives `"\u{e001}"`.
pub fn commonmark_pick_emphasis_sentinel(body: &str) -> String {
  if !body.chars().any(is_private_use) {
    return '\u{e000}'.to_string();
  }
  let private: Vec<char> = body.chars().filter(|&c| is_private_use(c)).collect();
  let present: std::collections::HashSet<char> = private.iter().copied().collect();
  if let Some(c) = private_use_chars().find(|c| !present.contains(c)) {
    return c.to_string();
  }
  let total = PRIVATE_USE_RANGES.iter().map(|&(a, b)| (b - a + 1) as usize).sum::<usize>();
  let mut counts: HashMap<char, usize> = HashMap::new();
  for &c in &private {
    *counts.entry(c).or_insert(0) += 1;
  }
  let first = private_use_chars().find(|c| counts.get(c).copied().unwrap_or(0) < total - 1).unwrap_or('\u{e000}');
  let chars: Vec<char> = body.chars().collect();
  let mut taken: std::collections::HashSet<char> = chars.windows(2).filter(|w| w[0] == first && is_private_use(w[1])).map(|w| w[1]).collect();
  taken.insert(first);
  let second = private_use_chars().find(|c| !taken.contains(c)).unwrap_or('\u{e001}');
  format!("{first}{second}")
}

/// One open or close step in an emphasis match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmphasisEvent {
  /// `true` for an opening marker, `false` for a closing one.
  pub open: bool,
  /// `true` for strong emphasis (two delimiters), `false` for regular.
  pub strong: bool,
}

/// One `*` or `_` delimiter run recorded by [`commonmark_emphasis_run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmphasisDelimiter {
  /// The delimiter character, `*` or `_`.
  pub ch: char,
  /// Delimiters not yet consumed by a match.
  pub numdelims: usize,
  /// The run's original length.
  pub origdelims: usize,
  /// Whether the run can open emphasis.
  pub can_open: bool,
  /// Whether the run can close emphasis.
  pub can_close: bool,
  /// Open and close steps, in the order the matcher recorded them.
  pub events: Vec<EmphasisEvent>,
  /// Set by [`commonmark_match_emphasis`]: the run was not fully consumed.
  pub extendable: bool,
}

impl EmphasisDelimiter {
  /// Describe a run of `numdelims` copies of `ch` between `prev_ch` and `next_ch`.
  pub fn new(ch: char, numdelims: usize, prev_ch: Option<char>, next_ch: Option<char>) -> Self {
    EmphasisDelimiter {
      ch,
      numdelims,
      origdelims: numdelims,
      can_open: commonmark_can_open_emphasis(ch, prev_ch, next_ch),
      can_close: commonmark_can_close_emphasis(ch, prev_ch, next_ch),
      events: Vec::new(),
      extendable: false,
    }
  }
}

/// The placeholder written for delimiter number `index`.
fn placeholder(sentinel: &str, index: usize) -> String {
  format!("{sentinel}{index}{sentinel}")
}

/// Swap the `*` or `_` run at `i` for a numbered placeholder.
///
/// Records the run in `delimiters`, appends `sentinel + index + sentinel` to
/// `out` and returns the index just past the run. Call
/// [`commonmark_render_emphasis_markers`] when the whole body is scanned.
///
/// Example: for `"**bold**"` at `i = 0` with sentinel `"S"`, `out` gets
/// `"S0S"` and `2` is returned.
pub fn commonmark_emphasis_run(body: &[char], i: usize, n: usize, delimiters: &mut Vec<EmphasisDelimiter>, out: &mut String, sentinel: &str) -> usize {
  let ch = body[i];
  let mut j = i;
  while j < n && body[j] == ch {
    j += 1;
  }
  let prev_ch = if i > 0 { Some(body[i - 1]) } else { None };
  let next_ch = if j < n { Some(body[j]) } else { None };
  delimiters.push(EmphasisDelimiter::new(ch, j - i, prev_ch, next_ch));
  out.push_str(&placeholder(sentinel, delimiters.len() - 1));
  j
}

/// Build the markers to output for one run's `events`, as
/// `(close_part, open_part)`. Closing markers come first. `strong_markers`
/// and `regular_markers` are `(open, close)` pairs.
///
/// Example: one strong open event with `("<b>", "</b>")` gives `("", "<b>")`.
pub fn commonmark_render_emphasis_events(events: &[EmphasisEvent], strong_markers: (&str, &str), regular_markers: (&str, &str)) -> (String, String) {
  let pick = |strong: bool| if strong { strong_markers } else { regular_markers };
  let close_part: String = events.iter().filter(|e| !e.open).map(|e| pick(e.strong).1).collect();
  // Open markers nest around later content, so the first one renders last.
  let open_part: String = events.iter().rev().filter(|e| e.open).map(|e| pick(e.strong).0).collect();
  (close_part, open_part)
}

/// Replace every `sentinel + index + sentinel` placeholder in `text` using
/// `replace(index)`.
fn substitute_placeholders(text: &str, sentinel: &str, mut replace: impl FnMut(usize) -> String) -> String {
  if sentinel.is_empty() {
    return text.to_string();
  }
  let mut out = String::with_capacity(text.len());
  let mut rest = text;
  while let Some(p) = rest.find(sentinel) {
    out.push_str(&rest[..p]);
    let after = &rest[p + sentinel.len()..];
    let digits = after.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && after[digits..].starts_with(sentinel) {
      let index: usize = after[..digits].parse().unwrap_or(usize::MAX);
      out.push_str(&replace(index));
      rest = &after[digits + sentinel.len()..];
    } else {
      out.push_str(sentinel);
      rest = after;
    }
  }
  out.push_str(rest);
  out
}

/// Replace the placeholders from [`commonmark_emphasis_run`] with the
/// service's bold and italic markers. A run with no partner is put back as
/// its original `*` or `_` characters.
///
/// Example: `"**hi** and *x*"` with `("<b>", "</b>")` and `("<i>", "</i>")`
/// becomes `"<b>hi</b> and <i>x</i>"`.
pub fn commonmark_render_emphasis_markers(
  text: &str,
  delimiters: &mut [EmphasisDelimiter],
  strong_markers: (&str, &str),
  regular_markers: (&str, &str),
  sentinel: &str,
) -> String {
  commonmark_match_emphasis(delimiters);
  substitute_placeholders(text, sentinel, |index| match delimiters.get(index) {
    Some(d) => {
      let leftover: String = std::iter::repeat_n(d.ch, d.numdelims).collect();
      let (close, open) = commonmark_render_emphasis_events(&d.events, strong_markers, regular_markers);
      format!("{close}{leftover}{open}")
    }
    None => String::new(),
  })
}

/// Pair up opening and closing `*` and `_` runs by the CommonMark rules,
/// recording `open`/`close` events on each run and setting `extendable` on
/// the runs that were not fully consumed.
///
/// Example: for `"*text*"` the first run gets one regular open event and the
/// second one regular close event.
pub fn commonmark_match_emphasis(delimiters: &mut [EmphasisDelimiter]) {
  let total = delimiters.len();
  // Doubly linked list of surviving runs; `total` is the end sentinel and
  // `usize::MAX` stands for "before the first".
  const NONE: usize = usize::MAX;
  let mut prev_active: Vec<usize> = (0..total).map(|i| if i == 0 { NONE } else { i - 1 }).collect();
  let mut next_active: Vec<usize> = (1..=total).collect();
  let mut head = 0usize;

  let unlink = |index: usize, prev_active: &mut Vec<usize>, next_active: &mut Vec<usize>, head: &mut usize| {
    let before = prev_active[index];
    let after = next_active[index];
    if before == NONE {
      *head = after;
    } else {
      next_active[before] = after;
    }
    if after != total {
      prev_active[after] = before;
    }
  };

  let mut search_bound: HashMap<(char, bool, usize), isize> = HashMap::new();
  let mut closer_index = head;
  while closer_index < total {
    if !delimiters[closer_index].can_close {
      closer_index = next_active[closer_index];
      continue;
    }
    let closer = &delimiters[closer_index];
    let bucket = (closer.ch, closer.can_open, closer.origdelims % 3);
    let bound = search_bound.get(&bucket).copied().unwrap_or(-1);
    let mut opener_index = prev_active[closer_index];
    let mut found: Option<usize> = None;
    while opener_index != NONE && opener_index as isize > bound {
      let opener = &delimiters[opener_index];
      if opener.ch == closer.ch && opener.can_open {
        let ambiguous =
          (closer.can_open || opener.can_close) && !closer.origdelims.is_multiple_of(3) && (opener.origdelims + closer.origdelims).is_multiple_of(3);
        if !ambiguous {
          found = Some(opener_index);
          break;
        }
      }
      opener_index = prev_active[opener_index];
    }

    let Some(found) = found else {
      search_bound.insert(bucket, closer_index as isize - 1);
      if !delimiters[closer_index].can_open {
        let next_pos = next_active[closer_index];
        unlink(closer_index, &mut prev_active, &mut next_active, &mut head);
        closer_index = next_pos;
      } else {
        closer_index = next_active[closer_index];
      }
      continue;
    };

    let use_delims = if delimiters[closer_index].numdelims >= 2 && delimiters[found].numdelims >= 2 { 2 } else { 1 };
    delimiters[found].numdelims -= use_delims;
    delimiters[closer_index].numdelims -= use_delims;
    delimiters[found].events.push(EmphasisEvent { open: true, strong: use_delims == 2 });
    delimiters[closer_index].events.push(EmphasisEvent { open: false, strong: use_delims == 2 });

    if found + 1 != closer_index {
      next_active[found] = closer_index;
      prev_active[closer_index] = found;
    }
    if delimiters[found].numdelims == 0 {
      unlink(found, &mut prev_active, &mut next_active, &mut head);
    }
    if delimiters[closer_index].numdelims == 0 {
      let next_pos = next_active[closer_index];
      unlink(closer_index, &mut prev_active, &mut next_active, &mut head);
      closer_index = next_pos;
    }
  }

  let mut still_active = vec![false; total];
  let mut pos = head;
  while pos < total {
    still_active[pos] = true;
    pos = next_active[pos];
  }
  for (d, active) in delimiters.iter_mut().zip(still_active) {
    d.extendable = active;
  }
}

fn escape_literal(text: &[char]) -> String {
  let mut out = String::with_capacity(text.len() * 2);
  for &ch in text {
    if COMMONMARK_LITERAL_CHARS.contains(ch) {
      out.push('\\');
    }
    out.push(ch);
  }
  out
}

fn escape_literal_str(text: &str) -> String {
  let chars: Vec<char> = text.chars().collect();
  escape_literal(&chars)
}

// Find `>)` or report a trailing `>` split from the next chunk.
fn scan_angle_terminator(text: &[char], start: usize, n: usize) -> (Option<usize>, bool) {
  let mut k = start;
  while k < n {
    if text[k] == '\\' && k + 1 < n {
      k += 2;
      continue;
    }
    if text[k] == '>' {
      if k + 1 < n && text[k + 1] == ')' {
        return (Some(k), false);
      }
      if k + 1 == n {
        return (None, true);
      }
    }
    k += 1;
  }
  (None, false)
}

/// Measure the `*` or `_` run at `i`. Returns `(end, prev_ch, next_ch)`:
/// the index after the run and the characters on either side. The boundary
/// characters stand in for the neighbours when `text` is only part of a
/// longer message.
///
/// Example: `"**word"` at `0` returns `(2, None, Some('w'))`.
pub fn commonmark_scan_delimiter_run(
  text: &[char],
  i: usize,
  boundary_prev_ch: Option<char>,
  boundary_next_ch: Option<char>,
) -> (usize, Option<char>, Option<char>) {
  let ch = text[i];
  let n = text.len();
  let mut j = i;
  while j < n && text[j] == ch {
    j += 1;
  }
  let prev_ch = if i > 0 { Some(text[i - 1]) } else { boundary_prev_ch };
  let next_ch = if j < n { Some(text[j]) } else { boundary_next_ch };
  (j, prev_ch, next_ch)
}

/// The widest `*` and `_` closing runs (up to 3) found in following text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CloserWidths {
  /// Widest closing `*` run.
  pub star: usize,
  /// Widest closing `_` run.
  pub underscore: usize,
}

impl CloserWidths {
  /// The width recorded for `ch`.
  pub fn get(&self, ch: char) -> usize {
    match ch {
      '*' => self.star,
      '_' => self.underscore,
      _ => 0,
    }
  }

  fn raise(&mut self, ch: char, width: usize) {
    let slot = if ch == '*' { &mut self.star } else { &mut self.underscore };
    if width > *slot {
      *slot = width;
    }
  }
}

/// Find which emphasis runs could be closed by the text that follows a
/// chunk. Markers inside code spans or links do not count.
///
/// Examples: `"tail** end"` gives a `*` width of 2; ``"`**` x"`` gives none.
pub fn commonmark_lookahead_closer_widths(next_chunk: &[char], boundary_prev_ch: Option<char>, boundary_next_ch: Option<char>) -> CloserWidths {
  let mut widths = CloserWidths::default();
  let n = next_chunk.len();
  if n == 0 {
    return widths;
  }
  let runs = commonmark_index_backtick_runs(next_chunk);
  let mut i = 0;
  while i < n {
    let ch = next_chunk[i];
    if ch == '\\' && i + 1 < n {
      i += 2;
      continue;
    }
    if ch == '`' {
      let mut j = i;
      while j < n && next_chunk[j] == ch {
        j += 1;
      }
      let run = j - i;
      if let Some(close) = commonmark_find_backtick_run(&runs, j, run) {
        i = close + run;
        continue;
      }
      break;
    }
    if ch == ']' && starts_with(next_chunk, i, "](<") {
      if let Some(close) = commonmark_scan_angle_dest(next_chunk, i, n, None) {
        i = close + 2;
        continue;
      }
      break;
    }
    if ch == ']' && starts_with(next_chunk, i, "](") {
      if let Some(close) = commonmark_scan_paren_dest(next_chunk, i + 1, n, None) {
        i = close + 1;
        continue;
      }
      break;
    }
    if ch == '<' {
      let (close, still_valid) = commonmark_scan_autolink_dest(next_chunk, i, n);
      if let Some(close) = close {
        i = close + 1;
        continue;
      }
      if still_valid {
        break;
      }
    }
    if ch == '*' || ch == '_' {
      let (j, prev_ch, next_ch) = commonmark_scan_delimiter_run(next_chunk, i, boundary_prev_ch, boundary_next_ch);
      if commonmark_can_close_emphasis(ch, prev_ch, next_ch) {
        widths.raise(ch, (j - i).min(3));
      }
      i = j;
      continue;
    }
    i += 1;
  }
  widths
}

// ─── Chunk repair ────────────────────────────────────────────────────────────

/// Styles and constructs still open at the end of a repaired chunk, carried
/// into the next call of [`commonmark_repair_chunk`]. Start with
/// `RepairState::default()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairState {
  /// Forced-closed `*` emphasis expected to close later.
  pub star: usize,
  /// Forced-closed `**` emphasis expected to close later.
  pub star2: usize,
  /// Forced-closed `_` emphasis expected to close later.
  pub underscore: usize,
  /// Forced-closed `__` emphasis expected to close later.
  pub underscore2: usize,
  /// Width of a code span left open by the previous chunk.
  pub in_code: Option<usize>,
  /// A `](<url>)` destination continues in the next chunk.
  pub in_link_dest: bool,
  /// That destination's chunk ended with a `>` that may close it.
  pub dest_gt: bool,
  /// An autolink continues in the next chunk.
  pub in_autolink: bool,
  /// The previous chunk ended with a lone backslash.
  pub in_escape: bool,
}

impl RepairState {
  fn marker_mut(&mut self, marker: &str) -> &mut usize {
    match marker {
      "**" => &mut self.star2,
      "*" => &mut self.star,
      "__" => &mut self.underscore2,
      _ => &mut self.underscore,
    }
  }

  /// The pending count for one marker (`"*"`, `"**"`, `"_"` or `"__"`).
  pub fn marker(&self, marker: &str) -> usize {
    match marker {
      "**" => self.star2,
      "*" => self.star,
      "__" => self.underscore2,
      "_" => self.underscore,
      _ => 0,
    }
  }
}

/// Fix one piece of a split Markdown message so it displays correctly.
///
/// When a long message is split, bold or italic text can be cut in half;
/// this closes it at the end of the piece. Code spans, link destinations,
/// autolinks and escapes cut at the edge are made literal. `pending` tracks
/// styles still open from earlier pieces; pass the returned state to the
/// next call. `next_chunk` is the text that comes next, used to check that a
/// style really does close later, and `next_chunk_boundary_ch` the character
/// after that lookahead.
///
/// Example: `("**hello", default, Some(" world**"))` returns `"**hello**"`
/// with one `**` pending.
pub fn commonmark_repair_chunk(text: &str, pending: &RepairState, next_chunk: Option<&str>, next_chunk_boundary_ch: Option<char>) -> (String, RepairState) {
  let chars: Vec<char> = text.chars().collect();
  let next: Option<Vec<char>> = next_chunk.map(|s| s.chars().collect());
  repair_chars(&chars, pending, next.as_deref(), next_chunk_boundary_ch)
}

pub(crate) fn repair_chars(text: &[char], pending: &RepairState, next_chunk: Option<&[char]>, next_chunk_boundary_ch: Option<char>) -> (String, RepairState) {
  let mut out: Vec<String> = Vec::new();
  let mut delimiters: Vec<EmphasisDelimiter> = Vec::new();
  let mut pending = pending.clone();
  let mut link_stack: Vec<usize> = Vec::new();
  let n = text.len();
  let mut i = 0;
  let runs = commonmark_index_backtick_runs(text);
  let text_str: String = text.iter().collect();
  let sentinel = commonmark_pick_emphasis_sentinel(&text_str);

  let in_code_width = pending.in_code.take();
  if let Some(width) = in_code_width.filter(|w| *w > 0) {
    match commonmark_find_backtick_run(&runs, 0, width) {
      Some(close) => {
        out.push(escape_literal(&text[..close]));
        i = close + width;
      }
      None => {
        out.push(escape_literal(text));
        pending.in_code = Some(width);
        i = n;
      }
    }
  } else if std::mem::take(&mut pending.in_link_dest) {
    if std::mem::take(&mut pending.dest_gt) && n > 0 && text[0] == ')' {
      out.push("\\)".into());
      i = 1;
    } else {
      let (close, trailing_gt) = scan_angle_terminator(text, 0, n);
      match close {
        Some(close) => {
          out.push(escape_literal(&text[..close]));
          out.push("\\>\\)".into());
          i = close + 2;
        }
        None => {
          out.push(escape_literal(text));
          pending.in_link_dest = true;
          if trailing_gt {
            pending.dest_gt = true;
          }
          i = n;
        }
      }
    }
  } else if std::mem::take(&mut pending.in_autolink) {
    match text.iter().position(|&c| c == '>') {
      Some(close) => {
        out.push(escape_literal(&text[..close]));
        out.push("\\>".into());
        i = close + 1;
      }
      None => {
        out.push(escape_literal(text));
        pending.in_autolink = true;
        i = n;
      }
    }
  } else if std::mem::take(&mut pending.in_escape) {
    if n > 0 {
      out.push(text[0].to_string());
      i = 1;
    } else {
      pending.in_escape = true;
    }
  }

  while i < n {
    let start = i;
    let ch = text[i];

    if ch == '\\' {
      if i + 1 < n {
        out.push(text[i..i + 2].iter().collect());
        i += 2;
        continue;
      }
      out.push(ch.to_string());
      pending.in_escape = true;
      i = n;
      continue;
    }

    if ch == '`' {
      let mut j = i;
      while j < n && text[j] == '`' {
        j += 1;
      }
      let run = j - i;
      if let Some(close) = commonmark_find_backtick_run(&runs, j, run) {
        out.push(text[i..close + run].iter().collect());
        i = close + run;
        continue;
      }
      pending.in_code = Some(run);
      out.push(escape_literal(&text[j..]));
      i = n;
      continue;
    }

    if ch == '<' {
      let (close, still_valid) = commonmark_scan_autolink_dest(text, i, n);
      if let Some(close) = close {
        out.push(text[i..close + 1].iter().collect());
        i = close + 1;
        continue;
      }
      if still_valid {
        out.push(escape_literal(&text[i..]));
        pending.in_autolink = true;
        i = n;
        continue;
      }
    }

    if ch == '*' || ch == '_' {
      let boundary_next = next_chunk.and_then(|c| c.first().copied());
      let (j, prev_ch, next_ch) = commonmark_scan_delimiter_run(text, i, None, boundary_next);
      let mut run = j - i;
      while run > 0 {
        let marker = if run >= 2 {
          if ch == '*' { "**" } else { "__" }
        } else if ch == '*' {
          "*"
        } else {
          "_"
        };
        let slot = pending.marker_mut(marker);
        if *slot > 0 {
          *slot -= 1;
          run -= marker.len();
        } else {
          break;
        }
      }
      if run > 0 {
        delimiters.push(EmphasisDelimiter::new(ch, run, prev_ch, next_ch));
        out.push(placeholder(&sentinel, delimiters.len() - 1));
      }
      i = j;
      continue;
    }

    if ch == '[' {
      link_stack.push(out.len());
      out.push("[".into());
      i += 1;
      continue;
    }

    if starts_with(text, i, "](<") {
      let close = commonmark_scan_angle_dest(text, i, n, None);
      if let Some(close) = close
        && !link_stack.is_empty()
      {
        link_stack.pop();
        out.push(text[i..close + 2].iter().collect());
        i = close + 2;
        continue;
      }
      link_stack.pop();
      match close {
        Some(close) => {
          out.push("\\]\\(".into());
          out.push(escape_literal(&text[i + 3..close]));
          out.push("\\>\\)".into());
          i = close + 2;
        }
        None => {
          out.push("\\]\\(".into());
          out.push(escape_literal(&text[i + 3..]));
          pending.in_link_dest = true;
          let (_, trailing_gt) = scan_angle_terminator(text, i + 3, n);
          if trailing_gt {
            pending.dest_gt = true;
          }
          i = n;
        }
      }
      continue;
    }

    if starts_with(text, i, "](") {
      let close = commonmark_scan_paren_dest(text, i + 1, n, None);
      if let Some(close) = close
        && !link_stack.is_empty()
      {
        link_stack.pop();
        out.push(text[i..close + 1].iter().collect());
        i = close + 1;
        continue;
      }
      link_stack.pop();
      if let Some(close) = close {
        out.push("\\]\\(".into());
        out.push(escape_literal(&text[i + 2..close]));
        out.push("\\)".into());
        i = close + 1;
        continue;
      }
      out.push(text[i..].iter().collect());
      i = n;
      continue;
    }

    let special = |c: char| matches!(c, '\\' | '`' | '<' | '*' | '_' | '[' | ']');
    if special(ch) {
      i += 1;
    } else {
      while i < n && !special(text[i]) {
        i += 1;
      }
    }
    out.push(text[start..i].iter().collect());
  }

  for &open_index in &link_stack {
    out[open_index] = format!("\\{}", out[open_index]);
  }

  let last_char = text.last().copied();
  resolve_emphasis(out, delimiters, &sentinel, n, last_char, pending, next_chunk, next_chunk_boundary_ch)
}

#[allow(clippy::too_many_arguments)]
fn resolve_emphasis(
  mut out: Vec<String>,
  mut delimiters: Vec<EmphasisDelimiter>,
  sentinel: &str,
  text_len: usize,
  last_char: Option<char>,
  pending: RepairState,
  next_chunk: Option<&[char]>,
  next_chunk_boundary_ch: Option<char>,
) -> (String, RepairState) {
  commonmark_match_emphasis(&mut delimiters);
  let mut new_pending = pending;
  let last_index = delimiters.len().wrapping_sub(1);
  let trailing_empty = !delimiters.is_empty() && out.last().is_some_and(|l| *l == placeholder(sentinel, last_index));
  let widths = commonmark_lookahead_closer_widths(next_chunk.unwrap_or(&[]), last_char, next_chunk_boundary_ch);

  let mut forced_text: Vec<Option<String>> = vec![None; delimiters.len()];
  let mut forced_open_groups: Vec<Vec<String>> = Vec::new();
  for (index, d) in delimiters.iter_mut().enumerate() {
    let available = widths.get(d.ch);
    if !d.can_open || !d.extendable || d.numdelims == 0 || available == 0 {
      continue;
    }
    let whole_chunk_is_this_run = text_len == d.origdelims;
    if trailing_empty && index == last_index && !whole_chunk_is_this_run {
      d.numdelims %= 2;
      continue;
    }
    let single = d.ch.to_string();
    let double = format!("{0}{0}", d.ch);
    let mut opened: Vec<String> = Vec::new();
    let mut to_allocate = d.numdelims;
    if available >= 2 && to_allocate >= 2 {
      opened.push(double.clone());
      to_allocate -= 2;
    }
    if available != 2 && to_allocate >= 1 {
      opened.push(single.clone());
      to_allocate -= 1;
    }
    let excess = escape_literal(&vec![d.ch; to_allocate]);
    d.numdelims = 0;
    let (real_close_part, _) = commonmark_render_emphasis_events(&d.events, (&double, &double), (&single, &single));
    forced_text[index] = Some(format!("{}{}{}", real_close_part, excess, opened.concat()));
    for marker in &opened {
      *new_pending.marker_mut(marker) += 1;
    }
    forced_open_groups.push(opened);
  }
  for opened in forced_open_groups.into_iter().rev() {
    out.extend(opened);
  }

  let joined = out.concat();
  let resolved = substitute_placeholders(&joined, sentinel, |index| {
    if let Some(Some(text)) = forced_text.get(index) {
      return text.clone();
    }
    let Some(d) = delimiters.get(index) else {
      return String::new();
    };
    let single = d.ch.to_string();
    let double = format!("{0}{0}", d.ch);
    let leftover = escape_literal_str(&single.repeat(d.numdelims));
    let (close, open) = commonmark_render_emphasis_events(&d.events, (&double, &double), (&single, &single));
    format!("{close}{leftover}{open}")
  });
  (resolved, new_pending)
}

// ─── Sections ────────────────────────────────────────────────────────────────

fn section_text_end(text: &[char], start: usize, tail: usize) -> usize {
  let mut hash_idx = find_char(text, '#', start, tail);
  while let Some(h) = hash_idx {
    let mut ws_idx = h;
    while ws_idx > start && pystr::is_space(text[ws_idx - 1]) {
      ws_idx -= 1;
    }
    for (idx, &c) in text.iter().enumerate().take(h).skip(ws_idx) {
      if c == '\r' || c == '\n' {
        return idx;
      }
    }
    hash_idx = find_char(text, '#', h + 1, tail);
  }
  start.max(tail)
}

fn rstrip_len(text: &[char]) -> usize {
  let mut n = text.len();
  while n > 0 && pystr::is_space(text[n - 1]) {
    n -= 1;
  }
  n
}

/// Split Markdown into intro text and `(heading, text)` sections, for
/// services that show messages as titled fields. Headings are cut to
/// `name_maxlen` characters; section text longer than `value_maxlen`
/// continues in extra sections with the same heading. A heading with no
/// text under it gets `""`.
///
/// Example: `"Intro\n# A\nsome text\n# B"` returns
/// `("Intro", [("A", "some text"), ("B", "")])`.
pub fn commonmark_sections(markdown: &str, name_maxlen: usize, value_maxlen: usize) -> (String, Vec<(String, String)>) {
  let mut text: Vec<char> = markdown.chars().collect();
  let mut description = String::new();
  let n = text.len();
  let mut idx = 0;
  while idx < n && pystr::is_space(text[idx]) {
    idx += 1;
  }
  if idx < n && text[idx] != '#' {
    let tail = rstrip_len(&text);
    let end = section_text_end(&text, idx + 1, tail);
    let slice: String = text[idx..end.max(idx)].iter().collect();
    description = pystr::strip(&slice).to_string();
  }
  if !description.is_empty() {
    let s: String = text.iter().collect();
    text = s.replacen(&description, "", 1).chars().collect();
  }

  let n = text.len();
  let tail = rstrip_len(&text);
  let mut sections: Vec<(String, String)> = Vec::new();
  let mut pos = 0;
  while let Some(hash_idx) = find_char(&text, '#', pos, n) {
    let line_end = find_char(&text, '\n', hash_idx, n).unwrap_or(n);
    let mut idx = hash_idx + 1;
    while idx < line_end && matches!(text[idx], '#' | ' ' | '\t' | '\x0b') {
      idx += 1;
    }
    if idx == line_end {
      if idx == hash_idx + 1 {
        pos = hash_idx + 1;
        continue;
      }
      idx -= 1;
    }
    let raw_name: String = text[idx..line_end].iter().collect();
    let name: String = pystr::strip_chars(&raw_name, "#`* \r\n\t\x0b").chars().take(name_maxlen).collect();

    let mut idx = (line_end + 1).min(n);
    while idx < n && pystr::is_space(text[idx]) {
      idx += 1;
    }
    let mut section = String::new();
    pos = idx;
    if idx + 1 < n && text[idx] != '#' {
      pos = section_text_end(&text, idx + 2, tail);
      let slice: String = text[idx..pos.max(idx)].iter().collect();
      section = pystr::strip(&slice).to_string();
    }
    if section.is_empty() {
      sections.push((name, String::new()));
      continue;
    }
    for part in crate::utils::format::smart_split(&section, value_maxlen, &crate::types::NotifyFormat::Text) {
      sections.push((name.clone(), part));
    }
  }
  (description, sections)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
  }

  #[test]
  fn test_docstring_examples() {
    let runs = commonmark_index_backtick_runs(&chars("a`b``c"));
    assert_eq!(runs.get(&1), Some(&vec![1]));
    assert_eq!(runs.get(&2), Some(&vec![3]));
    assert_eq!(commonmark_headings_to_bold("# Alert"), "**Alert**");
    assert_eq!(commonmark_decode_backslash_escapes(r"a\_b"), "a_b");
    assert_eq!(commonmark_decode_backslash_escapes(r"C:\Users"), r"C:\Users");
    assert_eq!(commonmark_escape_link_url(r"a\|b&c"), "a%7Cb&amp;c");
    assert_eq!(commonmark_new_scan_budget(&chars("abc")), ScanBudget(12));
    let (mut out, mut stack) = ("a ".to_string(), Vec::new());
    assert!(commonmark_open_link_label(&mut out, &mut stack));
    assert_eq!((out.as_str(), stack.as_slice()), ("a [", &[2][..]));
    out.push_str("label");
    assert_eq!(commonmark_take_link_label(&mut out, &mut stack), "label");
    assert_eq!(out, "a ");
    assert_eq!(commonmark_scan_angle_dest(&chars("[x](<https://a>)"), 2, 16, None), Some(14));
    assert_eq!(commonmark_scan_paren_dest(&chars("[x](a_(b))"), 3, 10, None), Some(9));
    assert_eq!(commonmark_scan_autolink_dest(&chars("<https://a*b>"), 0, 13), (Some(12), true));
    assert_eq!(commonmark_scan_autolink_dest(&chars("<https://a b>"), 0, 13), (None, false));
    assert_eq!(commonmark_pick_emphasis_sentinel("hi *there*"), "\u{e000}");
    assert_eq!(commonmark_pick_emphasis_sentinel("x\u{e000}"), "\u{e001}");
    assert_eq!(commonmark_scan_delimiter_run(&chars("**word"), 0, None, None), (2, None, Some('w')));
    assert_eq!(commonmark_lookahead_closer_widths(&chars("tail** end"), None, None), CloserWidths { star: 2, underscore: 0 });
    assert_eq!(commonmark_lookahead_closer_widths(&chars("`**` x"), None, None), CloserWidths::default());
    let (fixed, state) = commonmark_repair_chunk("**hello", &RepairState::default(), Some(" world**"), None);
    assert_eq!(fixed, "**hello**");
    assert_eq!(state.star2, 1);
    let (desc, sections) = commonmark_sections("Intro\n# A\nsome text\n# B", 256, 1024);
    assert_eq!(desc, "Intro");
    assert_eq!(sections, vec![("A".to_string(), "some text".to_string()), ("B".to_string(), String::new())]);
  }
}
