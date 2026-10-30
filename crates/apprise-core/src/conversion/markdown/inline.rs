//! Python-Markdown's `InlineProcessor` tree processor and inline patterns.
//!
//! Patterns run in priority order over each text; a match is replaced by a
//! placeholder that refers to a stashed element or string, and the
//! placeholders are expanded back into the tree afterwards. Matching happens
//! on byte offsets, but every decision Python-Markdown makes on characters
//! is made on whole characters here.

use std::collections::HashMap;

use super::super::pystr;
use super::blocks::code_escape;
use super::emphasis::Emphasis;
use super::{AMP_SUBSTITUTE, ETX, INLINE_PLACEHOLDER_PREFIX, Id, Markdown, STX, Text, serialize, store_html, unescape_placeholders};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Pat {
  Backtick,
  Escape,
  Reference,
  Link,
  ImageLink,
  ImageReference,
  ShortReference,
  ShortImageRef,
  Autolink,
  Automail,
  Linebreak,
  Html,
  Entity,
  EmStrong,
  Nl,
}

#[derive(Debug, Clone)]
pub(super) enum Stashed {
  El(Id),
  Str(String),
}

/// What a pattern's `handleMatch` returned.
enum Handled {
  /// `(None, None, None)`: no match here; keep scanning after it.
  Skip,
  /// A node (or nothing) replacing `data[start..end]`.
  Node(Option<Stashed>, usize, usize),
}

/// One finditer match: its span and, for backticks, the escaped-backslash run.
struct Found {
  start: usize,
  end: usize,
  group: Option<(usize, usize)>,
}

pub(super) struct Inline<'a> {
  pub md: &'a mut Markdown,
  pub stash: HashMap<String, Stashed>,
  patterns: Vec<Pat>,
  emphasis: Emphasis,
}

/// Run the inline processor over the tree rooted at `root`.
pub(super) fn run(md: &mut Markdown, root: Id, nl2br: bool) {
  let mut patterns = vec![
    Pat::Backtick,
    Pat::Escape,
    Pat::Reference,
    Pat::Link,
    Pat::ImageLink,
    Pat::ImageReference,
    Pat::ShortReference,
    Pat::ShortImageRef,
    Pat::Autolink,
    Pat::Automail,
    Pat::Linebreak,
    Pat::Html,
    Pat::Entity,
    Pat::EmStrong,
  ];
  if nl2br {
    patterns.push(Pat::Nl);
  }
  let mut inline = Inline { md, stash: HashMap::new(), patterns, emphasis: Emphasis::new() };
  inline.run(root);
}

fn char_at(s: &str, i: usize) -> Option<char> {
  s.get(i..).and_then(|r| r.chars().next())
}

/// `INLINE_PLACEHOLDER_RE` search from `from`: (id, start, end).
fn find_placeholder_re(data: &str, from: usize) -> Option<(String, usize, usize)> {
  let mut i = from;
  while let Some(p) = data.get(i..).and_then(|r| r.find(INLINE_PLACEHOLDER_PREFIX)) {
    let at = i + p;
    let rest = &data[at + INLINE_PLACEHOLDER_PREFIX.len()..];
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && rest[digits..].starts_with(ETX) {
      return Some((rest[..digits].to_string(), at, at + INLINE_PLACEHOLDER_PREFIX.len() + digits + 1));
    }
    i = at + 1;
  }
  None
}

impl Inline<'_> {
  // ── Placeholders ─────────────────────────────────────────────────────

  fn stash_node(&mut self, node: Stashed) -> String {
    let id = format!("{:04}", self.stash.len());
    let placeholder = format!("{}{}{}", INLINE_PLACEHOLDER_PREFIX, id, ETX);
    self.stash.insert(id, node);
    placeholder
  }

  fn handle_inline(&mut self, data: Text, mut pattern_index: usize) -> Text {
    if data.atomic {
      return data;
    }
    let mut data = data.s;
    let mut start_index = 0;
    while pattern_index < self.patterns.len() {
      let (new, matched, next) = self.apply_pattern(pattern_index, data, start_index);
      data = new;
      start_index = next;
      if !matched {
        pattern_index += 1;
      }
    }
    Text::plain(data)
  }

  fn link_text(&mut self, text: &str, result: &[Id], parent: Id, is_text: bool) {
    self.link_text_atomic(text, false, result, parent, is_text);
  }

  /// `linkText`: assigning keeps an `AtomicString`; appending makes a plain one.
  fn link_text_atomic(&mut self, text: &str, atomic: bool, result: &[Id], parent: Id, is_text: bool) {
    if text.is_empty() {
      return;
    }
    let tree = &mut self.md.tree;
    let target = if let Some(&last) = result.last() {
      &mut tree.nodes[last].tail
    } else if !is_text {
      &mut tree.nodes[parent].tail
    } else {
      &mut tree.nodes[parent].text
    };
    *target = Some(match target.take() {
      Some(t) if !t.s.is_empty() => Text::plain(format!("{}{}", t.s, text)),
      _ => Text { s: text.to_string(), atomic },
    });
  }

  fn process_element_text(&mut self, node: Id, subnode: Id, is_text: bool) {
    let text = if is_text { self.md.tree.nodes[subnode].text.take() } else { self.md.tree.nodes[subnode].tail.take() };
    let mut child_result = self.process_placeholders(text, subnode, is_text);
    let pos = if !is_text && node != subnode { self.md.tree.nodes[node].children.iter().position(|&c| c == subnode).map(|p| p + 1).unwrap_or(0) } else { 0 };
    child_result.reverse();
    for new_child in child_result {
      self.md.tree.insert(node, pos, new_child);
    }
  }

  fn process_placeholders(&mut self, data: Option<Text>, parent: Id, is_text: bool) -> Vec<Id> {
    let mut result: Vec<Id> = Vec::new();
    let Some(data) = data else {
      return result;
    };
    let atomic = data.atomic;
    let data = data.s;
    let mut start = 0;
    while !data.is_empty() {
      match data.get(start..).and_then(|r| r.find(INLINE_PLACEHOLDER_PREFIX)) {
        Some(p) => {
          let index = start + p;
          let (id, ph_end) = match find_placeholder_re(&data, index) {
            Some((id, _, end)) => (Some(id), end),
            None => (None, index + 1),
          };
          let node = id.as_ref().and_then(|i| self.stash.get(i)).cloned();
          match node {
            Some(node) => {
              if index > 0 {
                let text = data[start..index].to_string();
                self.link_text(&text, &result, parent, is_text);
              }
              match node {
                Stashed::El(n) => {
                  let mut children = vec![n];
                  children.extend(self.md.tree.nodes[n].children.clone());
                  for child in children {
                    let tail = self.md.tree.tail_str(child);
                    if !tail.is_empty() && !pystr::strip(tail).is_empty() {
                      self.process_element_text(n, child, false);
                    }
                    let text = self.md.tree.text_str(child);
                    if !text.is_empty() && !pystr::strip(text).is_empty() {
                      self.process_element_text(child, child, true);
                    }
                  }
                  start = ph_end;
                  result.push(n);
                }
                Stashed::Str(s) => {
                  self.link_text(&s, &result, parent, is_text);
                  start = ph_end;
                }
              }
            }
            None => {
              let end = index + INLINE_PLACEHOLDER_PREFIX.len();
              let text = data[start..end].to_string();
              self.link_text(&text, &result, parent, is_text);
              start = end;
            }
          }
        }
        None => {
          let text = data[start..].to_string();
          self.link_text_atomic(&text, atomic, &result, parent, is_text);
          break;
        }
      }
    }
    result
  }

  fn apply_pattern(&mut self, pattern_index: usize, data: String, start_index: usize) -> (String, bool, usize) {
    let pat = self.patterns[pattern_index];
    let mut from = start_index;
    let mut chosen: Option<(Option<Stashed>, usize, usize)> = None;
    while let Some(found) = self.find(pat, &data, from) {
      match self.handle(pat, &found, &data) {
        Handled::Skip => from = found.end,
        Handled::Node(node, s, e) => {
          chosen = Some((node, s, e));
          break;
        }
      }
    }
    let Some((node, start, end)) = chosen else {
      return (data, false, 0);
    };
    let Some(node) = node else {
      return (data, true, end);
    };
    if let Stashed::El(id) = node
      && !self.md.tree.nodes[id].text.as_ref().is_some_and(|t| t.atomic)
    {
      let mut children = vec![id];
      children.extend(self.md.tree.nodes[id].children.clone());
      for child in children {
        if let Some(t) = self.md.tree.nodes[child].text.clone()
          && !t.s.is_empty()
        {
          let t = self.handle_inline(t, pattern_index + 1);
          self.md.tree.nodes[child].text = Some(t);
        }
        if let Some(t) = self.md.tree.nodes[child].tail.clone()
          && !t.s.is_empty()
        {
          let t = self.handle_inline(t, pattern_index);
          self.md.tree.nodes[child].tail = Some(t);
        }
      }
    }
    let placeholder = self.stash_node(node);
    let len = placeholder.len();
    // Python slicing clamps out-of-range positions.
    let clamp = |x: usize| {
      let mut x = x.min(data.len());
      while !data.is_char_boundary(x) {
        x -= 1;
      }
      x
    };
    let (start, end) = (clamp(start), clamp(end));
    (format!("{}{}{}", &data[..start], placeholder, &data[end..]), true, start + len)
  }

  fn run(&mut self, root: Id) {
    let mut stack: std::collections::VecDeque<Id> = std::collections::VecDeque::from([root]);
    while let Some(curr) = stack.pop_front() {
      let mut insert_queue: Vec<(Id, Vec<Id>)> = Vec::new();
      let mut i = 0;
      // Children inserted after the current one are visited too, as
      // ElementTree's index-based iteration does.
      while i < self.md.tree.nodes[curr].children.len() {
        let child = self.md.tree.nodes[curr].children[i];
        if let Some(t) = self.md.tree.nodes[child].text.clone()
          && !t.s.is_empty()
          && !t.atomic
        {
          self.md.tree.nodes[child].text = None;
          let handled = self.handle_inline(t, 0);
          let lst = self.process_placeholders(Some(handled), child, true);
          stack.extend(lst.iter().copied());
          insert_queue.push((child, lst));
        }
        if let Some(t) = self.md.tree.nodes[child].tail.clone()
          && !t.s.is_empty()
        {
          let tail = self.handle_inline(t, 0);
          let dumby = self.md.tree.element("d");
          self.md.tree.nodes[child].tail = None;
          let mut tail_result = self.process_placeholders(Some(tail), dumby, false);
          if let Some(dt) = self.md.tree.nodes[dumby].tail.clone()
            && !dt.s.is_empty()
          {
            self.md.tree.nodes[child].tail = Some(dt);
          }
          let pos = self.md.tree.nodes[curr].children.iter().position(|&c| c == child).map(|p| p + 1).unwrap_or(0);
          tail_result.reverse();
          for new_child in tail_result {
            self.md.tree.insert(curr, pos, new_child);
          }
        }
        if self.md.tree.len(child) > 0 {
          stack.push_back(child);
        }
        i += 1;
      }
      for (element, lst) in insert_queue {
        for (k, new_child) in lst.into_iter().enumerate() {
          self.md.tree.insert(element, k, new_child);
        }
      }
    }
  }

  // ── Matching ─────────────────────────────────────────────────────────

  fn find(&self, pat: Pat, data: &str, from: usize) -> Option<Found> {
    let b = data.as_bytes();
    let n = b.len();
    let simple = |needle: &str| data.get(from..).and_then(|r| r.find(needle)).map(|p| Found { start: from + p, end: from + p + needle.len(), group: None });
    match pat {
      Pat::Backtick => {
        let mut i = from;
        while i < n {
          let prev_bs = i > 0 && b[i - 1] == b'\\';
          if b[i] == b'\\' && !prev_bs {
            let run = b[i..].iter().take_while(|&&c| c == b'\\').count();
            if run.is_multiple_of(2) && b.get(i + run) == Some(&b'`') {
              return Some(Found { start: i, end: i + run, group: Some((i, i + run)) });
            }
          } else if b[i] == b'`' && !prev_bs {
            return Some(Found { start: i, end: i + 1, group: None });
          }
          i += 1;
        }
        None
      }
      Pat::Escape => {
        let mut i = from;
        while let Some(p) = data.get(i..).and_then(|r| r.find('\\')) {
          let at = i + p;
          if let Some(c) = char_at(data, at + 1) {
            return Some(Found { start: at, end: at + 1 + c.len_utf8(), group: None });
          }
          i = at + 1;
        }
        None
      }
      Pat::Reference | Pat::Link | Pat::ShortReference => {
        let mut i = from;
        while let Some(p) = data.get(i..).and_then(|r| r.find('[')) {
          let at = i + p;
          if at == 0 || b[at - 1] != b'!' {
            return Some(Found { start: at, end: at + 1, group: None });
          }
          i = at + 1;
        }
        None
      }
      Pat::ImageLink | Pat::ImageReference | Pat::ShortImageRef => simple("!["),
      Pat::Autolink => {
        let mut i = from;
        while let Some(p) = data.get(i..).and_then(|r| r.find('<')) {
          let at = i + p;
          if let Some(end) = autolink_at(data, at) {
            return Some(Found { start: at, end, group: Some((at + 1, end - 1)) });
          }
          i = at + 1;
        }
        None
      }
      Pat::Automail => {
        let mut i = from;
        while let Some(p) = data.get(i..).and_then(|r| r.find('<')) {
          let at = i + p;
          if let Some(end) = automail_at(data, at) {
            return Some(Found { start: at, end, group: Some((at + 1, end - 1)) });
          }
          i = at + 1;
        }
        None
      }
      Pat::Linebreak => simple("  \n"),
      Pat::Html => {
        let mut i = from;
        while let Some(p) = data.get(i..).and_then(|r| r.find('<')) {
          let at = i + p;
          if let Some(end) = html_at(data, at) {
            return Some(Found { start: at, end, group: Some((at, end)) });
          }
          i = at + 1;
        }
        None
      }
      Pat::Entity => {
        let mut i = from;
        while let Some(p) = data.get(i..).and_then(|r| r.find('&')) {
          let at = i + p;
          if let Some(end) = entity_at(data, at) {
            return Some(Found { start: at, end, group: Some((at, end)) });
          }
          i = at + 1;
        }
        None
      }
      Pat::EmStrong => {
        let p = data.get(from..)?.find(['*', '_'])?;
        Some(Found { start: from + p, end: from + p + 1, group: None })
      }
      Pat::Nl => simple("\n"),
    }
  }

  fn handle(&mut self, pat: Pat, m: &Found, data: &str) -> Handled {
    match pat {
      Pat::Backtick => {
        if let Some((a, z)) = m.group {
          let escaped = format!("{}92{}", STX, ETX);
          return Handled::Node(Some(Stashed::Str(data[a..z].replace("\\\\", &escaped))), m.start, m.end);
        }
        // Python's arithmetic here is on characters.
        let (chars, offsets) = char_table(data);
        let to_byte = |i: usize| offsets.get(i).copied().unwrap_or(data.len());
        let begin = offsets.partition_point(|&o| o < m.start);
        match find_code_spans(begin, &chars) {
          Some((start, end)) => {
            let el = self.md.tree.element("code");
            let content: String = if start < end { chars[start..end].iter().collect() } else { String::new() };
            self.md.tree.nodes[el].text = Some(Text::atomic(code_escape(pystr::strip(&content))));
            Handled::Node(Some(Stashed::El(el)), to_byte(begin), to_byte(end + (start - begin)))
          }
          None => Handled::Skip,
        }
      }
      Pat::Escape => {
        let c = char_at(data, m.start + 1).unwrap_or(' ');
        if c.is_ascii_punctuation() {
          Handled::Node(Some(Stashed::Str(format!("{}{}{}", STX, c as u32, ETX))), m.start, m.end)
        } else {
          Handled::Node(None, m.start, m.end)
        }
      }
      Pat::Link | Pat::ImageLink => {
        let Some((text, index)) = get_text(data, m.end) else {
          return Handled::Skip;
        };
        let Some((href, title, index)) = self.get_link(data, index) else {
          return Handled::Skip;
        };
        let el;
        if pat == Pat::Link {
          el = self.md.tree.element("a");
          self.md.tree.nodes[el].text = Some(Text::plain(text));
          self.md.tree.set_attr(el, "href", href);
          if let Some(t) = title {
            self.md.tree.set_attr(el, "title", t);
          }
        } else {
          el = self.md.tree.element("img");
          self.md.tree.set_attr(el, "src", href);
          if let Some(t) = title {
            self.md.tree.set_attr(el, "title", t);
          }
          let alt = self.pattern_unescape(&text);
          self.md.tree.set_attr(el, "alt", alt);
        }
        Handled::Node(Some(Stashed::El(el)), m.start, index)
      }
      Pat::Reference | Pat::ShortReference | Pat::ImageReference | Pat::ShortImageRef => {
        let Some((text, index)) = get_text(data, m.end) else {
          return Handled::Skip;
        };
        let short = matches!(pat, Pat::ShortReference | Pat::ShortImageRef);
        let (id, end) = if short {
          (text.to_lowercase(), index)
        } else {
          // \s?\[([^\]]*)\]
          let mut p = index;
          if let Some(c) = char_at(data, p)
            && pystr::is_space(c)
          {
            p += c.len_utf8();
          }
          if data.as_bytes().get(p) != Some(&b'[') {
            return Handled::Skip;
          }
          let Some(close) = data[p + 1..].find(']') else {
            return Handled::Skip;
          };
          let mut id = data[p + 1..p + 1 + close].to_lowercase();
          if id.is_empty() {
            id = text.to_lowercase();
          }
          (id, p + 1 + close + 1)
        };
        let id = pystr::collapse_whitespace(&id);
        let Some((href, title)) = self.md.references.get(&id).cloned() else {
          return Handled::Node(None, m.start, end);
        };
        let el;
        if matches!(pat, Pat::Reference | Pat::ShortReference) {
          el = self.md.tree.element("a");
          self.md.tree.set_attr(el, "href", href);
          if let Some(t) = title.filter(|t| !t.is_empty()) {
            self.md.tree.set_attr(el, "title", t);
          }
          self.md.tree.nodes[el].text = Some(Text::plain(text));
        } else {
          el = self.md.tree.element("img");
          self.md.tree.set_attr(el, "src", href);
          if let Some(t) = title.filter(|t| !t.is_empty()) {
            self.md.tree.set_attr(el, "title", t);
          }
          let alt = self.pattern_unescape(&text);
          self.md.tree.set_attr(el, "alt", alt);
        }
        Handled::Node(Some(Stashed::El(el)), m.start, end)
      }
      Pat::Autolink => {
        let (a, z) = m.group.unwrap();
        let el = self.md.tree.element("a");
        let href = self.pattern_unescape(&data[a..z]);
        self.md.tree.set_attr(el, "href", href);
        self.md.tree.nodes[el].text = Some(Text::atomic(&data[a..z]));
        Handled::Node(Some(Stashed::El(el)), m.start, m.end)
      }
      Pat::Automail => {
        let (a, z) = m.group.unwrap();
        let el = self.md.tree.element("a");
        let mut email = self.pattern_unescape(&data[a..z]);
        if let Some(rest) = email.strip_prefix("mailto:") {
          email = rest.to_string();
        }
        let letters: String = email
          .chars()
          .map(|c| match codepoint2name(c as u32) {
            Some(name) => format!("{}{};", AMP_SUBSTITUTE, name),
            None => format!("{}#{};", AMP_SUBSTITUTE, c as u32),
          })
          .collect();
        self.md.tree.nodes[el].text = Some(Text::atomic(letters));
        let mailto: String = format!("mailto:{}", email).chars().map(|c| format!("{}#{};", AMP_SUBSTITUTE, c as u32)).collect();
        self.md.tree.set_attr(el, "href", mailto);
        Handled::Node(Some(Stashed::El(el)), m.start, m.end)
      }
      Pat::Linebreak | Pat::Nl => {
        let el = self.md.tree.element("br");
        Handled::Node(Some(Stashed::El(el)), m.start, m.end)
      }
      Pat::Html | Pat::Entity => {
        let (a, z) = m.group.unwrap();
        let raw = self.html_unescape(&data[a..z]);
        let raw = unescape_placeholders(&raw);
        let placeholder = store_html(&mut self.md.stash, raw);
        Handled::Node(Some(Stashed::Str(placeholder)), m.start, m.end)
      }
      Pat::EmStrong => {
        // The processor works on character indices, as Python does.
        let (chars, offsets) = char_table(data);
        let m_start = offsets.partition_point(|&o| o < m.start);
        let (el, start, end) = self.emphasis.handle_match(&mut self.md.tree, m_start, &chars);
        let to_byte = |i: usize| offsets.get(i).copied().unwrap_or(data.len());
        Handled::Node(el.map(Stashed::El), to_byte(start), to_byte(end))
      }
    }
  }

  /// `Pattern.unescape`: expand inline placeholders to their text.
  fn pattern_unescape(&self, text: &str) -> String {
    self.expand_placeholders(text, |this, node| match node {
      Stashed::Str(s) => s.clone(),
      Stashed::El(id) => this.itertext(*id),
    })
  }

  /// `HtmlInlineProcessor.unescape`: expand placeholders back to markup.
  fn html_unescape(&self, text: &str) -> String {
    self.expand_placeholders(text, |this, node| match node {
      Stashed::Str(s) => format!("\\{}", s),
      Stashed::El(id) => this.html_unescape(&serialize(&this.md.tree, *id)),
    })
  }

  fn expand_placeholders(&self, text: &str, f: impl Fn(&Self, &Stashed) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut from = 0;
    while let Some((id, start, end)) = find_placeholder_re(text, from) {
      out.push_str(&text[last..start]);
      if let Some(node) = self.stash.get(&id) {
        out.push_str(&f(self, node));
      }
      last = end;
      from = end;
    }
    out.push_str(&text[last..]);
    out
  }

  fn itertext(&self, id: Id) -> String {
    let mut out = String::new();
    out.push_str(self.md.tree.text_str(id));
    for &c in &self.md.tree.nodes[id].children {
      out.push_str(&self.itertext(c));
      out.push_str(self.md.tree.tail_str(c));
    }
    out
  }

  /// `LinkInlineProcessor.getLink`: (href, title, index after the link).
  fn get_link(&self, data: &str, index: usize) -> Option<(String, Option<String>, usize)> {
    let b = data.as_bytes();
    if b.get(index) != Some(&b'(') {
      return None;
    }
    let mut href = String::new();
    let mut title: Option<String> = None;
    let skip_ws = |mut p: usize| {
      while let Some(c) = char_at(data, p) {
        if !pystr::is_space(c) {
          break;
        }
        p += c.len_utf8();
      }
      p
    };
    let after_paren = skip_ws(index + 1);
    // (<[^<>]*>)\s*(?:('[^']*'|"[^"]*")\s*)?\)
    // (href start, href end, title span, end of the link)
    type Angle = (usize, usize, Option<(usize, usize)>, usize);
    let mut angle: Option<Angle> = None;
    if b.get(after_paren) == Some(&b'<')
      && let Some(close) = data[after_paren + 1..].find(['<', '>'])
      && b[after_paren + 1 + close] == b'>'
    {
      let gt = after_paren + 1 + close;
      let p = skip_ws(gt + 1);
      let mut title_span = None;
      let mut q = p;
      if let Some(&quote @ (b'\'' | b'"')) = b.get(p)
        && let Some(end) = data[p + 1..].find(quote as char)
      {
        let after = skip_ws(p + 1 + end + 1);
        if b.get(after) == Some(&b')') {
          title_span = Some((p, p + 1 + end + 1));
          q = after;
        }
      }
      if b.get(q) == Some(&b')') {
        angle = Some((after_paren, gt + 1, title_span, q + 1));
      }
    }
    let index = if let Some((hs, he, ts, end)) = angle {
      href = pystr::strip(&data[hs + 1..he - 1]).to_string();
      if let Some((a, z)) = ts {
        title = Some(data[a + 1..z - 1].to_string());
      }
      end
    } else {
      let mut bracket_count: i64 = 1;
      let mut backtrack_count: i64 = 1;
      let start_index = after_paren;
      let mut index = start_index;
      let mut last_bracket: usize = 0;
      let mut quote: Option<char> = None;
      let mut start_quote: usize = 0;
      let mut exit_quote: Option<usize> = None;
      let mut ignore_matches = false;
      let mut alt_quote: Option<char> = None;
      let mut start_alt_quote: usize = 0;
      let mut exit_alt_quote: Option<usize> = None;
      let mut last: Option<char> = None;
      let mut have_last_bracket = false;
      for (off, c) in data[start_index..].char_indices() {
        let pos = start_index + off;
        if c == '(' {
          if !ignore_matches {
            bracket_count += 1;
          } else if backtrack_count > 0 {
            backtrack_count -= 1;
          }
        } else if c == ')' {
          if (exit_quote.is_some() && quote == last) || (exit_alt_quote.is_some() && alt_quote == last) {
            bracket_count = 0;
          } else if !ignore_matches {
            bracket_count -= 1;
          } else if backtrack_count > 0 {
            backtrack_count -= 1;
            if backtrack_count == 0 {
              last_bracket = pos + 1;
              have_last_bracket = true;
            }
          }
        } else if c == '\'' || c == '"' {
          if quote.is_none() {
            ignore_matches = true;
            backtrack_count = bracket_count;
            bracket_count = 1;
            start_quote = pos + 1;
            quote = Some(c);
          } else if Some(c) != quote && alt_quote.is_none() {
            start_alt_quote = pos + 1;
            alt_quote = Some(c);
          } else if Some(c) == quote {
            exit_quote = Some(pos + 1);
          } else if alt_quote == Some(c) {
            exit_alt_quote = Some(pos + 1);
          }
        }
        index = pos + c.len_utf8();
        if bracket_count == 0 {
          if let Some(exit) = exit_quote.filter(|_| quote == last) {
            href = data[start_index..start_quote - 1].to_string();
            title = Some(data[start_quote..exit - 1].to_string());
          } else if let Some(exit) = exit_alt_quote.filter(|_| alt_quote == last) {
            href = data[start_index..start_alt_quote - 1].to_string();
            title = Some(data[start_alt_quote..exit - 1].to_string());
          } else {
            href = data[start_index..index - 1].to_string();
          }
          break;
        }
        if c != ' ' {
          last = Some(c);
        }
      }
      if bracket_count != 0 && backtrack_count == 0 {
        if have_last_bracket {
          href = data[start_index..(last_bracket - 1).max(start_index)].to_string();
          index = last_bracket;
        } else {
          // Python's last_bracket stays -1: data[start:-2] and index -1.
          let ends: Vec<usize> = data.char_indices().map(|(i, _)| i).collect();
          let second_last = if ends.len() >= 2 { ends[ends.len() - 2] } else { 0 };
          href = data.get(start_index..second_last.max(start_index)).unwrap_or("").to_string();
          index = ends.last().copied().unwrap_or(0);
        }
        bracket_count = 0;
      }
      if bracket_count != 0 {
        return None;
      }
      index
    };
    let title = title.map(|t| {
      let t = self.pattern_unescape(pystr::strip(&t));
      let t = dequote(&t);
      t.chars().map(|c| if pystr::is_space(c) { ' ' } else { c }).collect::<String>()
    });
    let href = pystr::strip(&self.pattern_unescape(&href)).to_string();
    Some((href, title, index))
  }
}

fn dequote(s: &str) -> String {
  if s.len() >= 2 && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\''))) {
    s[1..s.len() - 1].to_string()
  } else {
    s.to_string()
  }
}

/// `LinkInlineProcessor.getText`: the bracketed label from `index`.
fn get_text(data: &str, index: usize) -> Option<(String, usize)> {
  let mut bracket_count = 1;
  let mut text = String::new();
  let mut idx = index;
  for (off, c) in data[index..].char_indices() {
    if c == ']' {
      bracket_count -= 1;
    } else if c == '[' {
      bracket_count += 1;
    }
    idx = index + off + c.len_utf8();
    if bracket_count == 0 {
      break;
    }
    text.push(c);
  }
  if bracket_count == 0 { Some((text, idx)) } else { None }
}

/// Characters of `s` and the byte offset of each.
fn char_table(s: &str) -> (Vec<char>, Vec<usize>) {
  s.char_indices().map(|(i, c)| (c, i)).unzip()
}

/// `BacktickInlineProcessor.find_code_spans`, on character indices.
fn find_code_spans(start: usize, b: &[char]) -> Option<(usize, usize)> {
  let last = b.len();
  let mut start = start;
  let mut max_ticks = 0;
  while start < last && b[start] == '`' {
    max_ticks += 1;
    start += 1;
  }
  if max_ticks == 0 {
    return None;
  }
  let mut longest_span = 0;
  let mut end = 0;
  let mut i = start;
  while i < last {
    let mut span_length = 0;
    while i < last && b[i] == '`' {
      span_length += 1;
      i += 1;
    }
    if span_length == 0 {
      i += 1;
      continue;
    }
    if max_ticks == span_length {
      return Some((start, i - span_length));
    }
    if span_length > longest_span {
      longest_span = span_length;
      end = i;
    }
  }
  if longest_span > 0 {
    // Python's arithmetic: the start moves right when the closing run is longer.
    let s = start as isize - (max_ticks as isize - longest_span as isize);
    return Some((s.max(0) as usize, end - longest_span));
  }
  None
}

// <((?:[Ff]|[Hh][Tt])[Tt][Pp][Ss]?://[^<>]*)>
fn autolink_at(data: &str, at: usize) -> Option<usize> {
  let b = data.as_bytes();
  let mut p = at + 1;
  let lower = |i: usize| b.get(i).map(|c| c.to_ascii_lowercase());
  if lower(p) == Some(b'f') {
    p += 1;
  } else if lower(p) == Some(b'h') && lower(p + 1) == Some(b't') {
    p += 2;
  } else {
    return None;
  }
  if lower(p) != Some(b't') || lower(p + 1) != Some(b'p') {
    return None;
  }
  p += 2;
  if lower(p) == Some(b's') {
    p += 1;
  }
  if !data[p..].starts_with("://") {
    return None;
  }
  p += 3;
  let close = data[p..].find(['<', '>'])?;
  if b[p + close] == b'>' { Some(p + close + 1) } else { None }
}

// <([^<> !]+@[^@<> ]+)>
fn automail_at(data: &str, at: usize) -> Option<usize> {
  let b = data.as_bytes();
  let start = at + 1;
  let q = start + data[start..].find(['<', '>', ' '])?;
  if b[q] != b'>' {
    return None;
  }
  let seg = &data[start..q];
  let a = seg.rfind('@')?;
  if a == 0 || seg[..a].contains('!') || a + 1 >= seg.len() {
    return None;
  }
  Some(q + 1)
}

// (&(?:\#[0-9]+|\#x[0-9a-fA-F]+|[a-zA-Z0-9]+);)
fn entity_at(data: &str, at: usize) -> Option<usize> {
  let b = data.as_bytes();
  let mut p = at + 1;
  if b.get(p) == Some(&b'#') {
    let digits = b[p + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && b.get(p + 1 + digits) == Some(&b';') {
      return Some(p + 1 + digits + 1);
    }
    if b.get(p + 1) == Some(&b'x') {
      let hex = b[p + 2..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
      if hex > 0 && b.get(p + 2 + hex) == Some(&b';') {
        return Some(p + 2 + hex + 1);
      }
    }
    return None;
  }
  let name = b[p..].iter().take_while(|c| c.is_ascii_alphanumeric()).count();
  p += name;
  if name > 0 && b.get(p) == Some(&b';') { Some(p + 1) } else { None }
}

/// `HTML_RE` at `at` (a `<`); returns the end of the match.
fn html_at(data: &str, at: usize) -> Option<usize> {
  let b = data.as_bytes();
  let n = b.len();
  let rest = &data[at + 1..];
  // Comment: !--(?:(?!<!--|-->).)*-->
  if rest.starts_with("!--") {
    return first_terminator(data, at + 4, "-->", "<!--").map(|p| p + 3);
  }
  // Processing instruction: [?](?:(?!<[?]|[?]>).)*[?]>
  if rest.starts_with('?') {
    return first_terminator(data, at + 2, "?>", "<?").map(|p| p + 2);
  }
  // CDATA: !\[CDATA\[(?:(?!<!\[CDATA\[|\]\]>).)*\]\]>
  if rest.starts_with("![CDATA[") {
    return first_terminator(data, at + 9, "]]>", "<![CDATA[").map(|p| p + 3);
  }
  // Tag: \/?+[a-zA-Z][^\s"'<>@]*+(?:\s+[^\s"'=<>]++(?:\s*+=\s*+(?:"[^"]*+"|'[^']*+'|[^\s"'=<>]++))?+)*+\s*+/?>
  let mut p = at + 1;
  if b.get(p) == Some(&b'/') {
    p += 1;
  }
  if !b.get(p).is_some_and(|c| c.is_ascii_alphabetic()) {
    return None;
  }
  p += 1;
  let take = |p: &mut usize, ok: &dyn Fn(char) -> bool| {
    while let Some(c) = char_at(data, *p) {
      if !ok(c) {
        break;
      }
      *p += c.len_utf8();
    }
  };
  take(&mut p, &|c| !pystr::is_space(c) && !matches!(c, '"' | '\'' | '<' | '>' | '@'));
  let name_char = |c: char| !pystr::is_space(c) && !matches!(c, '"' | '\'' | '=' | '<' | '>');
  loop {
    let mut q = p;
    take(&mut q, &|c| pystr::is_space(c));
    if q == p || !char_at(data, q).is_some_and(name_char) {
      break;
    }
    take(&mut q, &name_char);
    // (?:\s*+=\s*+(?:"[^"]*+"|'[^']*+'|[^\s"'=<>]++))?+
    let mut v = q;
    take(&mut v, &|c| pystr::is_space(c));
    if b.get(v) == Some(&b'=') {
      v += 1;
      take(&mut v, &|c| pystr::is_space(c));
      match b.get(v) {
        Some(&quote @ (b'"' | b'\'')) => {
          if let Some(end) = data[v + 1..].find(quote as char) {
            q = v + 1 + end + 1;
          }
        }
        Some(_) => {
          let start = v;
          take(&mut v, &name_char);
          if v > start {
            q = v;
          }
        }
        None => {}
      }
    }
    p = q;
  }
  take(&mut p, &|c| pystr::is_space(c));
  if b.get(p) == Some(&b'/') {
    p += 1;
  }
  if p < n && b[p] == b'>' { Some(p + 1) } else { None }
}

/// The first `terminator` or `opener` at or after `from`; the position of
/// the terminator when it comes first.
fn first_terminator(data: &str, from: usize, terminator: &str, opener: &str) -> Option<usize> {
  let rest = data.get(from..)?;
  let t = rest.find(terminator);
  let o = rest.find(opener);
  match (t, o) {
    (Some(t), Some(o)) if o < t => None,
    (Some(t), _) => Some(from + t),
    _ => None,
  }
}

fn codepoint2name(code: u32) -> Option<&'static str> {
  CODEPOINT2NAME.binary_search_by_key(&code, |&(k, _)| k).ok().map(|i| CODEPOINT2NAME[i].1)
}

// Python `html.entities.codepoint2name` (HTML 4 entity names).
static CODEPOINT2NAME: &[(u32, &str)] = &[
  (34, "quot"),
  (38, "amp"),
  (60, "lt"),
  (62, "gt"),
  (160, "nbsp"),
  (161, "iexcl"),
  (162, "cent"),
  (163, "pound"),
  (164, "curren"),
  (165, "yen"),
  (166, "brvbar"),
  (167, "sect"),
  (168, "uml"),
  (169, "copy"),
  (170, "ordf"),
  (171, "laquo"),
  (172, "not"),
  (173, "shy"),
  (174, "reg"),
  (175, "macr"),
  (176, "deg"),
  (177, "plusmn"),
  (178, "sup2"),
  (179, "sup3"),
  (180, "acute"),
  (181, "micro"),
  (182, "para"),
  (183, "middot"),
  (184, "cedil"),
  (185, "sup1"),
  (186, "ordm"),
  (187, "raquo"),
  (188, "frac14"),
  (189, "frac12"),
  (190, "frac34"),
  (191, "iquest"),
  (192, "Agrave"),
  (193, "Aacute"),
  (194, "Acirc"),
  (195, "Atilde"),
  (196, "Auml"),
  (197, "Aring"),
  (198, "AElig"),
  (199, "Ccedil"),
  (200, "Egrave"),
  (201, "Eacute"),
  (202, "Ecirc"),
  (203, "Euml"),
  (204, "Igrave"),
  (205, "Iacute"),
  (206, "Icirc"),
  (207, "Iuml"),
  (208, "ETH"),
  (209, "Ntilde"),
  (210, "Ograve"),
  (211, "Oacute"),
  (212, "Ocirc"),
  (213, "Otilde"),
  (214, "Ouml"),
  (215, "times"),
  (216, "Oslash"),
  (217, "Ugrave"),
  (218, "Uacute"),
  (219, "Ucirc"),
  (220, "Uuml"),
  (221, "Yacute"),
  (222, "THORN"),
  (223, "szlig"),
  (224, "agrave"),
  (225, "aacute"),
  (226, "acirc"),
  (227, "atilde"),
  (228, "auml"),
  (229, "aring"),
  (230, "aelig"),
  (231, "ccedil"),
  (232, "egrave"),
  (233, "eacute"),
  (234, "ecirc"),
  (235, "euml"),
  (236, "igrave"),
  (237, "iacute"),
  (238, "icirc"),
  (239, "iuml"),
  (240, "eth"),
  (241, "ntilde"),
  (242, "ograve"),
  (243, "oacute"),
  (244, "ocirc"),
  (245, "otilde"),
  (246, "ouml"),
  (247, "divide"),
  (248, "oslash"),
  (249, "ugrave"),
  (250, "uacute"),
  (251, "ucirc"),
  (252, "uuml"),
  (253, "yacute"),
  (254, "thorn"),
  (255, "yuml"),
  (338, "OElig"),
  (339, "oelig"),
  (352, "Scaron"),
  (353, "scaron"),
  (376, "Yuml"),
  (402, "fnof"),
  (710, "circ"),
  (732, "tilde"),
  (913, "Alpha"),
  (914, "Beta"),
  (915, "Gamma"),
  (916, "Delta"),
  (917, "Epsilon"),
  (918, "Zeta"),
  (919, "Eta"),
  (920, "Theta"),
  (921, "Iota"),
  (922, "Kappa"),
  (923, "Lambda"),
  (924, "Mu"),
  (925, "Nu"),
  (926, "Xi"),
  (927, "Omicron"),
  (928, "Pi"),
  (929, "Rho"),
  (931, "Sigma"),
  (932, "Tau"),
  (933, "Upsilon"),
  (934, "Phi"),
  (935, "Chi"),
  (936, "Psi"),
  (937, "Omega"),
  (945, "alpha"),
  (946, "beta"),
  (947, "gamma"),
  (948, "delta"),
  (949, "epsilon"),
  (950, "zeta"),
  (951, "eta"),
  (952, "theta"),
  (953, "iota"),
  (954, "kappa"),
  (955, "lambda"),
  (956, "mu"),
  (957, "nu"),
  (958, "xi"),
  (959, "omicron"),
  (960, "pi"),
  (961, "rho"),
  (962, "sigmaf"),
  (963, "sigma"),
  (964, "tau"),
  (965, "upsilon"),
  (966, "phi"),
  (967, "chi"),
  (968, "psi"),
  (969, "omega"),
  (977, "thetasym"),
  (978, "upsih"),
  (982, "piv"),
  (8194, "ensp"),
  (8195, "emsp"),
  (8201, "thinsp"),
  (8204, "zwnj"),
  (8205, "zwj"),
  (8206, "lrm"),
  (8207, "rlm"),
  (8211, "ndash"),
  (8212, "mdash"),
  (8216, "lsquo"),
  (8217, "rsquo"),
  (8218, "sbquo"),
  (8220, "ldquo"),
  (8221, "rdquo"),
  (8222, "bdquo"),
  (8224, "dagger"),
  (8225, "Dagger"),
  (8226, "bull"),
  (8230, "hellip"),
  (8240, "permil"),
  (8242, "prime"),
  (8243, "Prime"),
  (8249, "lsaquo"),
  (8250, "rsaquo"),
  (8254, "oline"),
  (8260, "frasl"),
  (8364, "euro"),
  (8465, "image"),
  (8472, "weierp"),
  (8476, "real"),
  (8482, "trade"),
  (8501, "alefsym"),
  (8592, "larr"),
  (8593, "uarr"),
  (8594, "rarr"),
  (8595, "darr"),
  (8596, "harr"),
  (8629, "crarr"),
  (8656, "lArr"),
  (8657, "uArr"),
  (8658, "rArr"),
  (8659, "dArr"),
  (8660, "hArr"),
  (8704, "forall"),
  (8706, "part"),
  (8707, "exist"),
  (8709, "empty"),
  (8711, "nabla"),
  (8712, "isin"),
  (8713, "notin"),
  (8715, "ni"),
  (8719, "prod"),
  (8721, "sum"),
  (8722, "minus"),
  (8727, "lowast"),
  (8730, "radic"),
  (8733, "prop"),
  (8734, "infin"),
  (8736, "ang"),
  (8743, "and"),
  (8744, "or"),
  (8745, "cap"),
  (8746, "cup"),
  (8747, "int"),
  (8756, "there4"),
  (8764, "sim"),
  (8773, "cong"),
  (8776, "asymp"),
  (8800, "ne"),
  (8801, "equiv"),
  (8804, "le"),
  (8805, "ge"),
  (8834, "sub"),
  (8835, "sup"),
  (8836, "nsub"),
  (8838, "sube"),
  (8839, "supe"),
  (8853, "oplus"),
  (8855, "otimes"),
  (8869, "perp"),
  (8901, "sdot"),
  (8968, "lceil"),
  (8969, "rceil"),
  (8970, "lfloor"),
  (8971, "rfloor"),
  (9001, "lang"),
  (9002, "rang"),
  (9674, "loz"),
  (9824, "spades"),
  (9827, "clubs"),
  (9829, "hearts"),
  (9830, "diams"),
];
