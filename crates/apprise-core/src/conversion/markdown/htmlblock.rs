//! Python-Markdown's `HtmlBlockPreprocessor`: lift raw HTML blocks out of
//! the source into the stash, leaving placeholders, while inline HTML stays
//! in the text. It runs `HTMLExtractor`, Python-Markdown's subclass of a
//! patched `HTMLParser` (`convert_charrefs=False`, backticks end tag names,
//! no whitespace around `=`, malformed tags kept as text).

use super::super::pyhtml::{attrfind_tolerant, locatetagend, tagfind_tolerant};
use super::super::pystr;
use super::{is_block_level, store_html};

const CDATA_CONTENT_ELEMENTS: &[&str] = &["script", "style", "xmp", "iframe", "noembed", "noframes"];
const VOID_TAGS: &[&str] = &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

/// Run the extractor over `source`; returns the cleaned document.
pub(super) fn extract(source: &str, stash: &mut Vec<String>) -> String {
  let mut x = Extractor {
    raw: source,
    stash,
    inraw: false,
    intail: false,
    stack: Vec::new(),
    inline_stack: Vec::new(),
    cache: Vec::new(),
    cleandoc: Vec::new(),
    cdata_elem: None,
    starttag_text: String::new(),
  };
  // feed(), then close() on whatever feed() could not finish.
  let i = x.goahead(0, false);
  x.goahead(i, true);
  if !x.cache.is_empty() {
    let html = x.cache.concat();
    x.cleandoc.push(store_html(x.stash, html));
    x.cache.clear();
  }
  x.cleandoc.concat()
}

struct Extractor<'a> {
  raw: &'a str,
  stash: &'a mut Vec<String>,
  inraw: bool,
  intail: bool,
  stack: Vec<String>,
  inline_stack: Vec<String>,
  cache: Vec<String>,
  cleandoc: Vec<String>,
  cdata_elem: Option<String>,
  starttag_text: String,
}

fn is_tag_ws(b: u8) -> bool {
  matches!(b, b'\t' | b'\n' | b'\r' | b'\x0c' | b' ')
}

// `^([ ]*\n){2}`
fn blank_line_follows(s: &str) -> bool {
  let mut rest = s;
  for _ in 0..2 {
    let spaces = rest.bytes().take_while(|&b| b == b' ').count();
    if rest.as_bytes().get(spaces) != Some(&b'\n') {
      return false;
    }
    rest = &rest[spaces + 1..];
  }
  true
}

// `\n[ \t]*\n`
fn find_inner_blank_line(s: &str) -> Option<usize> {
  let b = s.as_bytes();
  let mut i = 0;
  while let Some(p) = s[i..].find('\n') {
    let at = i + p;
    let mut j = at + 1;
    while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
      j += 1;
    }
    if j < b.len() && b[j] == b'\n' {
      return Some(at);
    }
    i = at + 1;
  }
  None
}

impl Extractor<'_> {
  fn line_start(&self, pos: usize) -> usize {
    self.raw[..pos].rfind('\n').map(|p| p + 1).unwrap_or(0)
  }

  fn at_line_start(&self, pos: usize) -> bool {
    let start = self.line_start(pos);
    let offset = pos - start;
    if offset == 0 {
      return true;
    }
    if offset > 3 {
      return false;
    }
    pystr::strip(&self.raw[start..pos]).is_empty()
  }

  fn store(&mut self, html: String) -> String {
    store_html(self.stash, html)
  }

  fn handle_data(&mut self, data: &str) {
    if self.intail && data.contains('\n') {
      self.intail = false;
    }
    if self.inraw {
      self.cache.push(data.to_string());
    } else {
      if find_inner_blank_line(data).is_some() {
        self.inline_stack.clear();
      }
      self.cleandoc.push(data.to_string());
    }
  }

  fn handle_empty_tag(&mut self, data: String, is_block: bool, pos: usize) {
    if self.inraw || self.intail {
      self.cache.push(data);
    } else if self.at_line_start(pos) && is_block {
      let mut data = data;
      let after = (pos + data.len()).min(self.raw.len());
      if blank_line_follows(&self.raw[after..]) {
        data.push('\n');
      } else {
        self.intail = true;
      }
      let item = self.cleandoc.last().map(String::as_str).unwrap_or("");
      if !item.ends_with("\n\n") && item.ends_with('\n') {
        self.cleandoc.push("\n".into());
      }
      let ph = self.store(data);
      self.cleandoc.push(ph);
      self.cleandoc.push("\n\n".into());
    } else {
      self.cleandoc.push(data);
    }
  }

  fn handle_starttag(&mut self, tag: &str, pos: usize) {
    if tag == "hr" {
      self.handle_startendtag(tag, pos);
      return;
    }
    if is_block_level(tag) && (self.intail || (self.at_line_start(pos) && !self.inraw)) {
      self.inraw = true;
      self.cleandoc.push("\n".into());
    }
    let text = self.starttag_text.clone();
    if self.inraw {
      self.stack.push(tag.to_string());
      self.cache.push(text);
    } else {
      self.cleandoc.push(text);
      if !is_block_level(tag) && !VOID_TAGS.contains(&tag) {
        self.inline_stack.push(tag.to_string());
      }
      if CDATA_CONTENT_ELEMENTS.contains(&tag) {
        self.cdata_elem = None;
      }
    }
  }

  fn handle_startendtag(&mut self, tag: &str, pos: usize) {
    let text = self.starttag_text.clone();
    self.handle_empty_tag(text, is_block_level(tag), pos);
  }

  fn handle_endtag(&mut self, tag: &str, pos: usize) {
    let text = match self.raw[pos..].find('>') {
      Some(p) => self.raw[pos..pos + p + 1].to_string(),
      None => format!("</{}>", tag),
    };
    if self.inraw {
      let after = (pos + text.len()).min(self.raw.len());
      self.cache.push(text);
      if self.stack.iter().any(|t| t == tag) {
        while let Some(t) = self.stack.pop() {
          if t == tag {
            break;
          }
        }
      }
      if self.stack.is_empty() {
        if blank_line_follows(&self.raw[after..]) {
          self.cache.push("\n".into());
        } else {
          self.intail = true;
        }
        self.inraw = false;
        let html = self.cache.concat();
        self.cache.clear();
        let ph = self.store(html);
        self.cleandoc.push(ph);
        self.cleandoc.push("\n\n".into());
      }
    } else {
      self.cleandoc.push(text);
      if self.inline_stack.iter().any(|t| t == tag) {
        while let Some(t) = self.inline_stack.pop() {
          if t == tag {
            break;
          }
        }
      }
    }
  }

  fn inline_close_follows(&self, text: &str, pos: usize) -> bool {
    if self.inline_stack.is_empty() {
      return false;
    }
    let after = (pos + text.len()).min(self.raw.len());
    let rest = &self.raw[after..];
    let rest = match find_inner_blank_line(rest) {
      Some(p) => &rest[..p],
      None => rest,
    };
    let lower = rest.to_lowercase();
    self.inline_stack.iter().any(|tag| {
      // </\s*tag\s*>
      let tag = tag.to_lowercase();
      let mut from = 0;
      while let Some(p) = lower[from..].find("</") {
        let at = from + p + 2;
        let s = lower[at..].trim_start_matches(|c: char| pystr::is_space(c));
        if let Some(r) = s.strip_prefix(tag.as_str())
          && r.trim_start_matches(|c: char| pystr::is_space(c)).starts_with('>')
        {
          return true;
        }
        from = at;
      }
      false
    })
  }

  fn handle_comment(&mut self, data: &str, pos: usize) {
    let text = format!("<!--{}-->", data);
    let is_block = find_inner_blank_line(data).is_some() || !self.inline_close_follows(&text, pos);
    self.handle_empty_tag(text, is_block, pos);
  }

  fn unknown_decl(&mut self, data: &str, pos: usize) {
    let end = if data.starts_with("CDATA[") { "]]>" } else { "]>" };
    self.handle_empty_tag(format!("<![{}{}", data, end), true, pos);
  }

  fn cdata_end(&self, from: usize) -> Option<usize> {
    let elem = self.cdata_elem.as_deref()?;
    let b = self.raw.as_bytes();
    let pat = elem.as_bytes();
    let mut i = from;
    while let Some(p) = self.raw[i..].find("</") {
      let at = i + p;
      let name_end = at + 2 + pat.len();
      if name_end < b.len() && b[at + 2..name_end].eq_ignore_ascii_case(pat) && (is_tag_ws(b[name_end]) || b[name_end] == b'/' || b[name_end] == b'>') {
        return Some(at);
      }
      i = at + 1;
    }
    None
  }

  fn goahead(&mut self, start: usize, end: bool) -> usize {
    let raw = self.raw;
    let b = raw.as_bytes();
    let n = b.len();
    let mut i = start;
    while i < n {
      let j = if self.cdata_elem.is_some() {
        match self.cdata_end(i) {
          Some(p) => p,
          None => break,
        }
      } else {
        raw[i..].find(['&', '<']).map(|p| i + p).unwrap_or(n)
      };
      if i < j {
        let data = raw[i..j].to_string();
        self.handle_data(&data);
      }
      i = j;
      if i == n {
        break;
      }
      let rest = &raw[i..];
      if rest.starts_with('<') {
        let starttagopen = rest.starts_with("</>") || b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic());
        let k: Option<usize> = if starttagopen {
          Some(self.parse_starttag(i))
        } else if rest.starts_with("</") {
          Some(self.parse_endtag(i))
        } else if rest.starts_with("<!--") {
          Some(self.parse_comment(i))
        } else if rest.starts_with("<?") {
          self.parse_pi(i)
        } else if rest.starts_with("<!") {
          self.parse_html_declaration(i)
        } else if i + 1 < n || end {
          self.handle_data("<");
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
            if rest.starts_with("<!--") {
              let mut stop = n;
              for suffix in ["--!", "--", "-"] {
                if raw[i + 4..].ends_with(suffix) {
                  stop -= suffix.len();
                  break;
                }
              }
              let data = raw[i + 4..stop.max(i + 4)].to_string();
              self.handle_comment(&data, i);
            } else if rest.starts_with("<![CDATA[") {
              let data = raw[i + 3..].to_string();
              self.unknown_decl(&data, i);
            } else if rest.len() >= 9 && rest.as_bytes()[..9].eq_ignore_ascii_case(b"<!doctype") {
              self.handle_empty_tag(format!("<!{}>", &raw[i + 2..]), true, i);
            } else if rest.starts_with("<!") {
              let data = raw[i + 2..].to_string();
              self.handle_comment(&data, i);
            } else if rest.starts_with("<?") {
              self.handle_empty_tag(format!("<?{}?>", &raw[i + 2..]), true, i);
            }
            i = n;
          }
        }
      } else if let Some(after) = rest.strip_prefix("&#") {
        // &#(?:[0-9]+|[xX][0-9a-fA-F]+)[^0-9a-fA-F]
        let digits = after.bytes().take_while(|c| c.is_ascii_digit()).count();
        let hex = if matches!(b.get(i + 2), Some(b'x' | b'X')) { rest[3..].bytes().take_while(|c| c.is_ascii_hexdigit()).count() } else { 0 };
        let full = if digits > 0 && b.get(i + 2 + digits).is_some_and(|c| !c.is_ascii_hexdigit()) {
          Some(i + 2 + digits)
        } else if hex > 0 && b.get(i + 3 + hex).is_some_and(|c| !c.is_ascii_hexdigit()) {
          Some(i + 3 + hex)
        } else {
          None
        };
        if let Some(name_end) = full {
          let name = raw[i + 2..name_end].to_string();
          self.handle_empty_tag(format!("&#{};", name), false, i);
          i = if b[name_end] == b';' { name_end + 1 } else { name_end };
          continue;
        }
        // &#(?:[0-9]|[xX][0-9a-fA-F])
        let incomplete = digits > 0 || hex > 0;
        if incomplete {
          if end {
            let name = raw[i + 2..].to_string();
            self.handle_empty_tag(format!("&#{};", name), false, i);
            i = n;
          }
          break;
        } else if i + 3 < n {
          self.handle_data("&#");
          i += 2;
        } else {
          break;
        }
      } else {
        // &([a-zA-Z][-.a-zA-Z0-9]*);
        let name_len = if b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
          1 + rest[2..].bytes().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.')).count()
        } else {
          0
        };
        if name_len > 0 && b.get(i + 1 + name_len) == Some(&b';') {
          let name = raw[i + 1..i + 1 + name_len].to_string();
          self.handle_empty_tag(format!("&{};", name), false, i);
          i += name_len + 2;
          continue;
        }
        if i + 1 < n {
          self.handle_data("&");
          i += 1;
        } else {
          break;
        }
      }
    }
    if end && i < n {
      let data = raw[i..n].to_string();
      self.handle_data(&data);
      i = n;
    }
    i
  }

  fn parse_starttag(&mut self, i: usize) -> usize {
    let raw = self.raw;
    let b = raw.as_bytes();
    if raw[i..].starts_with("</>") {
      self.handle_data("</>");
      return i + 3;
    }
    let endpos = locatetagend(b, i + 1, true);
    if b[endpos - 1] != b'>' {
      self.handle_data(&raw[i..i + 1]);
      return i + 1;
    }
    self.starttag_text = raw[i..endpos].to_string();
    let (name_end, mut k) = tagfind_tolerant(b, i + 1);
    let tag = raw[i + 1..name_end].to_lowercase();
    while k < endpos {
      let Some(m) = attrfind_tolerant(b, k, false) else {
        break;
      };
      k = m.end;
    }
    let tail = if k <= endpos { pystr::strip(&raw[k..endpos]) } else { "" };
    if tail != ">" && tail != "/>" {
      let data = raw[i..endpos].to_string();
      self.handle_data(&data);
      return endpos;
    }
    if tail.ends_with("/>") {
      self.handle_startendtag(&tag, i);
    } else {
      if CDATA_CONTENT_ELEMENTS.contains(&tag.as_str()) {
        self.cdata_elem = Some(tag.clone());
      }
      self.handle_starttag(&tag, i);
    }
    endpos
  }

  fn parse_endtag(&mut self, i: usize) -> usize {
    let raw = self.raw;
    let b = raw.as_bytes();
    let is_data = !(b.get(i + 2).is_some_and(|c| c.is_ascii_alphabetic()));
    let pos = if is_data { None } else { self.stdlib_parse_endtag(i) };
    match pos {
      Some(p) => p,
      None => {
        let end = (i + 2).min(raw.len());
        let data = raw[i..end].to_string();
        self.handle_data(&data);
        i + 2
      }
    }
  }

  fn stdlib_parse_endtag(&mut self, i: usize) -> Option<usize> {
    let raw = self.raw;
    let b = raw.as_bytes();
    raw[i + 2..].find('>')?;
    let j = locatetagend(b, i + 2, true);
    if b[j - 1] != b'>' {
      return None;
    }
    let (name_end, _) = tagfind_tolerant(b, i + 2);
    let tag = raw[i + 2..name_end].to_lowercase();
    self.handle_endtag(&tag, i);
    self.cdata_elem = None;
    Some(j)
  }

  fn parse_comment(&mut self, i: usize) -> usize {
    let raw = self.raw;
    let b = raw.as_bytes();
    // --!?>
    let mut p = i + 4;
    while let Some(off) = raw[p.min(raw.len())..].find("--") {
      let at = p + off;
      let close = if b.get(at + 2) == Some(&b'>') {
        Some(at + 3)
      } else if b.get(at + 2) == Some(&b'!') && b.get(at + 3) == Some(&b'>') {
        Some(at + 4)
      } else {
        None
      };
      if let Some(end) = close {
        let data = raw[i + 4..at].to_string();
        self.handle_comment(&data, i);
        return end;
      }
      p = at + 1;
    }
    self.handle_data("<");
    i + 1
  }

  fn parse_pi(&mut self, i: usize) -> Option<usize> {
    if self.at_line_start(i) || self.intail {
      let p = self.raw[i + 2..].find("?>")?;
      let data = self.raw[i + 2..i + 2 + p].to_string();
      self.handle_empty_tag(format!("<?{}?>", data), true, i);
      return Some(i + 2 + p + 2);
    }
    self.handle_data("<?");
    Some(i + 2)
  }

  fn parse_bogus_comment(&mut self, i: usize) -> Option<usize> {
    let p = self.raw[i + 2..].find('>')?;
    let pos = i + 2 + p + 1;
    let data = self.raw[i..pos].to_string();
    self.handle_empty_tag(data, false, i);
    Some(pos)
  }

  fn parse_html_declaration(&mut self, i: usize) -> Option<usize> {
    let raw = self.raw;
    if self.at_line_start(i) || self.intail {
      let rest = &raw[i..];
      if rest.starts_with("<![") && !rest.starts_with("<![CDATA[") {
        return match self.parse_bogus_comment(i) {
          Some(p) => Some(p),
          None => {
            self.handle_data("<");
            Some(i + 1)
          }
        };
      }
      if rest.starts_with("<!--") {
        return Some(self.parse_comment(i));
      }
      if rest.starts_with("<![CDATA[") {
        let p = raw[i + 9..].find("]]>")?;
        let data = raw[i + 3..i + 9 + p].to_string();
        self.unknown_decl(&data, i);
        return Some(i + 9 + p + 3);
      }
      if rest.len() >= 9 && rest.as_bytes()[..9].eq_ignore_ascii_case(b"<!doctype") {
        let p = raw[i + 9..].find('>')?;
        let data = raw[i + 2..i + 9 + p].to_string();
        self.handle_empty_tag(format!("<!{}>", data), true, i);
        return Some(i + 9 + p + 1);
      }
      return self.parse_bogus_comment(i);
    }
    self.handle_data("<!");
    Some(i + 2)
  }
}
