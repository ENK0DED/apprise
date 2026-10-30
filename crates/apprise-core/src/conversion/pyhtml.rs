//! A port of Python 3.13's `html.parser.HTMLParser` tokenizer.
//!
//! Upstream's converters subclass `HTMLParser` with `convert_charrefs=True`,
//! so their output depends on exactly how it splits text, which malformed
//! tags it keeps as text and which it drops. This module reproduces that
//! behaviour for one `feed()` followed by `close()`, which is how every
//! upstream converter uses it.

use super::pystr;

/// Attributes of a start tag: lower-cased names, values with character
/// references decoded. `None` means the attribute had no `=value`.
pub type Attrs = [(String, Option<String>)];

/// Callbacks for the parsed tokens (the `handle_*` methods of `HTMLParser`).
pub trait HtmlHandler {
  /// An opening tag such as `<b>`.
  fn handle_starttag(&mut self, tag: &str, attrs: &Attrs);
  /// A self-closing tag such as `<br/>`. Defaults to a start then an end tag.
  fn handle_startendtag(&mut self, tag: &str, attrs: &Attrs) {
    self.handle_starttag(tag, attrs);
    self.handle_endtag(tag);
  }
  /// A closing tag such as `</b>`.
  fn handle_endtag(&mut self, tag: &str);
  /// Text, with character references already decoded.
  fn handle_data(&mut self, data: &str);
}

/// Parse `html` and report every token to `handler`.
pub fn parse(html: &str, handler: &mut dyn HtmlHandler) {
  let mut parser = Parser { cdata_elem: None, escapable: true };
  // feed(): parse what can be parsed without knowing the input has ended.
  let consumed = parser.goahead(html, false, handler);
  // close(): finish whatever feed() left buffered.
  parser.goahead(&html[consumed..], true, handler);
}

struct Parser {
  // Set while inside script, style, title, ... whose text is not markup.
  cdata_elem: Option<String>,
  // Whether character references are decoded in the current text.
  escapable: bool,
}

const CDATA_CONTENT_ELEMENTS: &[&str] = &["script", "style", "xmp", "iframe", "noembed", "noframes"];
const RCDATA_CONTENT_ELEMENTS: &[&str] = &["textarea", "title"];

fn is_tag_ws(b: u8) -> bool {
  matches!(b, b'\t' | b'\n' | b'\r' | b'\x0c' | b' ')
}

impl Parser {
  fn set_cdata_mode(&mut self, elem: &str, escapable: bool) {
    self.cdata_elem = Some(elem.to_lowercase());
    self.escapable = escapable;
  }

  fn clear_cdata_mode(&mut self) {
    self.cdata_elem = None;
    self.escapable = true;
  }

  fn data(&self, h: &mut dyn HtmlHandler, text: &str) {
    if self.escapable {
      h.handle_data(&pystr::unescape(text));
    } else {
      h.handle_data(text);
    }
  }

  /// Find where the text of the current CDATA element ends.
  fn cdata_end(&self, raw: &str, from: usize) -> Option<usize> {
    let elem = self.cdata_elem.as_deref()?;
    if elem == "plaintext" {
      return Some(raw.len());
    }
    let b = raw.as_bytes();
    let pat = elem.as_bytes();
    let mut i = from;
    while let Some(p) = raw[i..].find("</") {
      let at = i + p;
      let name_start = at + 2;
      let name_end = name_start + pat.len();
      if name_end < b.len() && b[name_start..name_end].eq_ignore_ascii_case(pat) && (is_tag_ws(b[name_end]) || b[name_end] == b'/' || b[name_end] == b'>') {
        return Some(at);
      }
      i = at + 1;
    }
    None
  }

  /// `HTMLParser.goahead()`; returns how many bytes were consumed.
  fn goahead(&mut self, raw: &str, end: bool, h: &mut dyn HtmlHandler) -> usize {
    let b = raw.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n {
      let j;
      if self.cdata_elem.is_none() {
        match raw[i..].find('<') {
          Some(p) => j = i + p,
          None => {
            // Wait for more input when the text ends inside what may be a
            // character reference.
            let lo = i.max(chars_back(raw, 34));
            if let Some(a) = raw[lo..].rfind('&') {
              let amppos = lo + a;
              if !b[amppos..].iter().any(|&c| is_tag_ws(c) || c == b';') {
                break;
              }
            }
            j = n;
          }
        }
      } else {
        match self.cdata_end(raw, i) {
          Some(p) => j = p,
          None => break,
        }
      }
      if i < j {
        self.data(h, &raw[i..j]);
      }
      i = j;
      if i == n {
        break;
      }

      let rest = &raw[i..];
      let k: Option<usize> = if starttagopen(b, i) {
        self.parse_starttag(raw, i, h)
      } else if rest.starts_with("</") {
        self.parse_endtag(raw, i, h)
      } else if rest.starts_with("<!--") {
        parse_comment(raw, i)
      } else if rest.starts_with("<?") {
        raw[i + 2..].find('>').map(|p| i + 2 + p + 1)
      } else if rest.starts_with("<!") {
        parse_html_declaration(raw, i)
      } else if i + 1 < n || end {
        h.handle_data("<");
        Some(i + 1)
      } else {
        break;
      };

      match k {
        Some(k) => i = k,
        None => {
          if !end {
            break;
          }
          // The input ended inside a construct. Only an unfinished "</"
          // survives as text; everything else is dropped.
          if !starttagopen(b, i) && rest.starts_with("</") && i + 2 == n {
            h.handle_data("</");
          }
          i = n;
        }
      }
    }
    if end && i < n {
      self.data(h, &raw[i..n]);
      i = n;
    }
    i
  }

  fn parse_starttag(&mut self, raw: &str, i: usize, h: &mut dyn HtmlHandler) -> Option<usize> {
    let b = raw.as_bytes();
    let endpos = locatetagend(b, i + 1, false);
    if b[endpos - 1] != b'>' {
      return None;
    }
    let (name_end, mut k) = tagfind_tolerant(b, i + 1);
    let tag = raw[i + 1..name_end].to_lowercase();
    let mut attrs: Vec<(String, Option<String>)> = Vec::new();
    while k < endpos {
      let Some(m) = attrfind_tolerant(b, k, false) else {
        break;
      };
      let name = raw[m.name.0..m.name.1].to_lowercase();
      let value = m.value.map(|(vs, ve)| {
        let mut v = &raw[vs..ve];
        let vb = v.as_bytes();
        if vb.len() >= 2 && (vb[0] == b'\'' || vb[0] == b'"') && vb[vb.len() - 1] == vb[0] {
          v = &v[1..v.len() - 1];
        } else if vb.len() == 1 && (vb[0] == b'\'' || vb[0] == b'"') {
          // Python's attrvalue[:1] == q == attrvalue[-1:] holds for one quote.
          v = "";
        }
        if v.is_empty() { String::new() } else { unescape_attrvalue(v) }
      });
      attrs.push((name, value));
      k = m.end;
    }
    let tail = if k <= endpos { pystr::strip(&raw[k..endpos]) } else { "" };
    if tail != ">" && tail != "/>" {
      self.data_raw(h, &raw[i..endpos]);
      return Some(endpos);
    }
    if tail.ends_with("/>") {
      h.handle_startendtag(&tag, &attrs);
    } else {
      h.handle_starttag(&tag, &attrs);
      if CDATA_CONTENT_ELEMENTS.contains(&tag.as_str()) || tag == "plaintext" {
        self.set_cdata_mode(&tag, false);
      } else if RCDATA_CONTENT_ELEMENTS.contains(&tag.as_str()) {
        self.set_cdata_mode(&tag, true);
      }
    }
    Some(endpos)
  }

  // handle_data() called directly (no reference decoding), as parse_starttag does.
  fn data_raw(&self, h: &mut dyn HtmlHandler, text: &str) {
    h.handle_data(text);
  }

  fn parse_endtag(&mut self, raw: &str, i: usize, h: &mut dyn HtmlHandler) -> Option<usize> {
    let b = raw.as_bytes();
    raw[i + 2..].find('>')?;
    if !(i + 2 < b.len() && b[i + 2].is_ascii_alphabetic()) {
      if b.get(i + 2) == Some(&b'>') {
        return Some(i + 3);
      }
      // A bogus comment such as "</ x>" or "</1>".
      return raw[i + 2..].find('>').map(|p| i + 2 + p + 1);
    }
    let j = locatetagend(b, i + 2, false);
    if b[j - 1] != b'>' {
      return None;
    }
    let (name_end, _) = tagfind_tolerant(b, i + 2);
    let tag = raw[i + 2..name_end].to_lowercase();
    h.handle_endtag(&tag);
    self.clear_cdata_mode();
    Some(j)
  }
}

/// Byte offset of the character `count` characters before the end.
fn chars_back(s: &str, count: usize) -> usize {
  s.char_indices().rev().nth(count.saturating_sub(1)).map(|(i, _)| i).unwrap_or(0)
}

fn starttagopen(b: &[u8], i: usize) -> bool {
  b[i] == b'<' && i + 1 < b.len() && b[i + 1].is_ascii_alphabetic()
}

fn parse_comment(raw: &str, i: usize) -> Option<usize> {
  let b = raw.as_bytes();
  // "--!?>" anywhere after the opener.
  let mut p = i + 4;
  while let Some(off) = raw[p..].find("--") {
    let at = p + off;
    if b.get(at + 2) == Some(&b'>') {
      return Some(at + 3);
    }
    if b.get(at + 2) == Some(&b'!') && b.get(at + 3) == Some(&b'>') {
      return Some(at + 4);
    }
    p = at + 1;
  }
  // An abruptly closed comment: "<!-->" or "<!--->".
  match (b.get(i + 4), b.get(i + 5)) {
    (Some(b'>'), _) => Some(i + 5),
    (Some(b'-'), Some(b'>')) => Some(i + 6),
    _ => None,
  }
}

fn parse_html_declaration(raw: &str, i: usize) -> Option<usize> {
  let rest = &raw[i..];
  if rest.starts_with("<!--") {
    return parse_comment(raw, i);
  }
  if rest.starts_with("<![CDATA[") {
    return raw[i + 9..].find("]]>").map(|p| i + 9 + p + 3);
  }
  if rest.len() >= 9 && rest.as_bytes()[..9].eq_ignore_ascii_case(b"<!doctype") {
    return raw[i + 9..].find('>').map(|p| i + 9 + p + 1);
  }
  raw[i + 2..].find('>').map(|p| i + 2 + p + 1)
}

/// `tagfind_tolerant`: `([a-zA-Z][^\t\n\r\f />]*)(?:[\t\n\r\f ]|/(?!>))*`.
/// Returns the end of the name and the end of the whole match.
pub(crate) fn tagfind_tolerant(b: &[u8], start: usize) -> (usize, usize) {
  let mut i = start + 1;
  while i < b.len() && !is_tag_ws(b[i]) && b[i] != b'/' && b[i] != b'>' {
    i += 1;
  }
  let name_end = i;
  // (?:[\t\n\r\f ]|/(?!>))*
  while i < b.len() && (is_tag_ws(b[i]) || (b[i] == b'/' && b.get(i + 1) != Some(&b'>'))) {
    i += 1;
  }
  (name_end, i)
}

/// An attribute matched by `attrfind_tolerant`.
pub(crate) struct AttrMatch {
  pub name: (usize, usize),
  pub value: Option<(usize, usize)>,
  pub end: usize,
}

fn lookbehind_ok(b: &[u8], i: usize) -> bool {
  i > 0 && matches!(b[i - 1], b'\'' | b'"' | b'\t' | b'\n' | b'\r' | b'\x0c' | b' ' | b'/')
}

/// Match an attribute name starting at `i` (the lookbehind is checked).
fn attr_name(b: &[u8], i: usize, no_backtick: bool) -> Option<usize> {
  if i >= b.len() || !lookbehind_ok(b, i) {
    return None;
  }
  let c = b[i];
  if is_tag_ws(c) || c == b'/' || c == b'>' || (no_backtick && c == b'`') {
    return None;
  }
  let mut j = i + 1;
  while j < b.len() && !is_tag_ws(b[j]) && b[j] != b'/' && b[j] != b'=' && b[j] != b'>' {
    j += 1;
  }
  Some(j)
}

/// Match `[ws]*=[ws]*(value)` after an attribute name, reproducing the
/// regex backtracking. `ws_around_eq` is false for Python-Markdown's
/// patched pattern, which allows no whitespace around `=`.
fn attr_value(b: &[u8], i: usize, ws_around_eq: bool) -> Option<(usize, usize)> {
  let n = b.len();
  let mut q = i;
  if ws_around_eq {
    while q < n && is_tag_ws(b[q]) {
      q += 1;
    }
  }
  if q >= n || b[q] != b'=' {
    return None;
  }
  let r = q + 1;
  let mut s = r;
  if ws_around_eq {
    while s < n && is_tag_ws(b[s]) {
      s += 1;
    }
  }
  if s < n && (b[s] == b'\'' || b[s] == b'"') {
    let quote = b[s];
    if let Some(p) = b[s + 1..].iter().position(|&c| c == quote) {
      return Some((s, s + 1 + p + 1));
    }
    // No closing quote: give back one whitespace so an empty bare value
    // matches, as the regex engine does.
    if s > r {
      return Some((s - 1, s - 1));
    }
    return None;
  }
  let mut e = s;
  while e < n && b[e] != b'>' && !is_tag_ws(b[e]) {
    e += 1;
  }
  Some((s, e))
}

/// `attrfind_tolerant` at `k`.
pub(crate) fn attrfind_tolerant(b: &[u8], k: usize, md: bool) -> Option<AttrMatch> {
  let name_end = attr_name(b, k, md)?;
  let value = attr_value(b, name_end, !md);
  let mut i = value.map(|(_, e)| e).unwrap_or(name_end);
  // (?:[\t\n\r\f ]|/(?!>))*
  while i < b.len() && (is_tag_ws(b[i]) || (b[i] == b'/' && b.get(i + 1) != Some(&b'>'))) {
    i += 1;
  }
  Some(AttrMatch { name: (k, name_end), value, end: i })
}

/// `locatetagend` matched at `start` (the first letter of the tag name).
/// `md` selects Python-Markdown's patched pattern, which stops tag and
/// attribute names at a backtick and allows no whitespace around `=`.
pub(crate) fn locatetagend(b: &[u8], start: usize, md: bool) -> usize {
  let n = b.len();
  let mut i = start + 1;
  while i < n && !is_tag_ws(b[i]) && b[i] != b'/' && b[i] != b'>' && !(md && b[i] == b'`') {
    i += 1;
  }
  let skip_ws_slash = |mut i: usize| {
    while i < n && (is_tag_ws(b[i]) || b[i] == b'/') {
      i += 1;
    }
    i
  };
  i = skip_ws_slash(i);
  while let Some(name_end) = attr_name(b, i, md) {
    let after = attr_value(b, name_end, !md).map(|(_, e)| e).unwrap_or(name_end);
    i = skip_ws_slash(after);
  }
  if i < n && b[i] == b'>' {
    i += 1;
  }
  i
}

/// Python `html.parser._unescape_attrvalue`.
fn unescape_attrvalue(s: &str) -> String {
  if !s.contains('&') {
    return s.to_string();
  }
  let b = s.as_bytes();
  let mut out = String::with_capacity(s.len());
  let mut last = 0;
  let mut i = 0;
  while i < b.len() {
    if b[i] != b'&' {
      i += 1;
      continue;
    }
    // &(#[0-9]+|#[xX][0-9a-fA-F]+|[a-zA-Z][a-zA-Z0-9]*)[;=]?
    let mut j = i + 1;
    let numeric;
    if b.get(j) == Some(&b'#') {
      numeric = true;
      let digits = b[j + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
      if digits > 0 {
        j += 1 + digits;
      } else if matches!(b.get(j + 1), Some(b'x' | b'X')) {
        let hex = b[j + 2..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
        if hex == 0 {
          i += 1;
          continue;
        }
        j += 2 + hex;
      } else {
        i += 1;
        continue;
      }
    } else if b.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
      numeric = false;
      j += 1;
      while j < b.len() && b[j].is_ascii_alphanumeric() {
        j += 1;
      }
    } else {
      i += 1;
      continue;
    }
    if matches!(b.get(j), Some(b';' | b'=')) {
      j += 1;
    }
    let reference = &s[i..j];
    out.push_str(&s[last..i]);
    if numeric || (!reference.ends_with('=') && pystr::html5_entity(&reference[1..]).is_some()) {
      out.push_str(&pystr::unescape(reference));
    } else {
      out.push_str(reference);
    }
    last = j;
    i = j;
  }
  out.push_str(&s[last..]);
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Default)]
  struct Recorder(Vec<String>);

  impl HtmlHandler for Recorder {
    fn handle_starttag(&mut self, tag: &str, attrs: &Attrs) {
      self.0.push(format!("start {} {:?}", tag, attrs));
    }
    fn handle_startendtag(&mut self, tag: &str, attrs: &Attrs) {
      self.0.push(format!("startend {} {:?}", tag, attrs));
    }
    fn handle_endtag(&mut self, tag: &str) {
      self.0.push(format!("end {}", tag));
    }
    fn handle_data(&mut self, data: &str) {
      self.0.push(format!("data {:?}", data));
    }
  }

  fn events(html: &str) -> Vec<String> {
    let mut r = Recorder::default();
    parse(html, &mut r);
    r.0
  }

  #[test]
  fn test_events_match_python() {
    // Expected events were recorded from Python 3.13's HTMLParser.
    assert_eq!(events("a < b"), vec![r#"data "a ""#, r#"data "<""#, r#"data " b""#]);
    assert_eq!(events("<b>x</b>"), vec!["start b []", r#"data "x""#, "end b"]);
    assert_eq!(events("<br/>"), vec!["startend br []"]);
    assert_eq!(events("x <b"), vec![r#"data "x ""#]);
    assert_eq!(events("x </"), vec![r#"data "x ""#, r#"data "</""#]);
    assert_eq!(events("q&a"), vec![r#"data "q&a""#]);
    assert_eq!(events("&lt;b&gt;"), vec![r#"data "<b>""#]);
    assert_eq!(events("<a href=\"x&amp;y\" B=1 c>"), vec![r#"start a [("href", Some("x&y")), ("b", Some("1")), ("c", None)]"#]);
    assert_eq!(events("<script>a<b>&amp;</script>"), vec!["start script []", r#"data "a<b>&amp;""#, "end script"]);
    assert_eq!(events("<title>&amp;</title>"), vec!["start title []", r#"data "&""#, "end title"]);
    assert_eq!(events("<!-- c -->x"), vec![r#"data "x""#]);
    assert_eq!(events("</>x"), vec![r#"data "x""#]);
    assert_eq!(events("<a b='c' d=\"e>"), Vec::<String>::new());
  }
}
