//! Markdown to HTML, matching upstream's output.
//!
//! Upstream renders Markdown with Python-Markdown (3.11) and the `nl2br`
//! and `tables` extensions, after allowing every ASCII punctuation
//! character to be backslash-escaped. Python-Markdown is not CommonMark:
//! lists need a blank line before them, nested blocks are indented four
//! spaces, there are no fenced code blocks, and its emphasis rules are its
//! own. Notification text depends on those details (where line breaks go,
//! what becomes a list), so this module ports Python-Markdown's pipeline
//! rather than using a CommonMark renderer:
//!
//! 1. preprocessors normalize whitespace and lift raw HTML blocks out
//!    ([`htmlblock`]),
//! 2. block processors build an element tree ([`blocks`]),
//! 3. the inline processor applies inline patterns ([`inline`],
//!    [`emphasis`]), then the tree is tidied and serialized as XHTML,
//! 4. postprocessors put the raw HTML back.

mod blocks;
mod emphasis;
mod htmlblock;
mod inline;

use std::collections::HashMap;

use super::pystr;

/// Python-Markdown extensions upstream enables for notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownExtension {
  /// `markdown.extensions.nl2br`: a single line break becomes `<br />`.
  Nl2br,
  /// `markdown.extensions.tables`: pipe tables.
  Tables,
}

/// The extensions [`markdown_to_html`] uses.
pub const DEFAULT_EXTENSIONS: &[MarkdownExtension] = &[MarkdownExtension::Nl2br, MarkdownExtension::Tables];

/// Convert Markdown to HTML with upstream's notification defaults
/// (Python-Markdown with `nl2br` and `tables`).
///
/// ```
/// use apprise_core::conversion::markdown_to_html;
/// assert_eq!(markdown_to_html("first\nline\n\nsecond"), "<p>first<br />\nline</p>\n<p>second</p>");
/// ```
pub fn markdown_to_html(content: &str) -> String {
  markdown_to_html_with(content, DEFAULT_EXTENSIONS)
}

/// Convert Markdown to HTML with an explicit extension list (upstream's
/// `extensions=` argument; Matrix passes none).
pub fn markdown_to_html_with(content: &str, extensions: &[MarkdownExtension]) -> String {
  if pystr::strip(content).is_empty() {
    return String::new();
  }
  let mut md = Markdown { tree: Tree::default(), stash: Vec::new(), references: HashMap::new() };
  let nl2br = extensions.contains(&MarkdownExtension::Nl2br);
  let tables = extensions.contains(&MarkdownExtension::Tables);

  let source = normalize_whitespace(content);
  let source = htmlblock::extract(&source, &mut md.stash);

  let root = md.tree.element("div");
  blocks::parse_document(&mut md, root, &source, tables);
  inline::run(&mut md, root, nl2br);
  prettify(&mut md.tree, root);
  unescape_tree(&mut md.tree, root);

  let output = serialize(&md.tree, root);
  let output = match (output.find("<div>"), output.rfind("</div>")) {
    (Some(start), Some(end)) if start + 5 <= end => pystr::strip(&output[start + 5..end]).to_string(),
    _ => String::new(),
  };
  let output = raw_html_postprocess(&output, &md.stash);
  let output = output.replace(AMP_SUBSTITUTE, "&");
  let output = unescape_placeholders_as_html(&output);
  pystr::strip(&output).to_string()
}

pub(super) const STX: char = '\u{2}';
pub(super) const ETX: char = '\u{3}';
pub(super) const INLINE_PLACEHOLDER_PREFIX: &str = "\u{2}klzzwxh:";
pub(super) const AMP_SUBSTITUTE: &str = "\u{2}amp\u{3}";
const HTML_PLACEHOLDER_PREFIX: &str = "\u{2}wzxhzdk:";

/// Python-Markdown's `BLOCK_LEVEL_ELEMENTS`.
const BLOCK_LEVEL_ELEMENTS: &[&str] = &[
  "address",
  "article",
  "aside",
  "blockquote",
  "details",
  "div",
  "dl",
  "fieldset",
  "figcaption",
  "figure",
  "footer",
  "form",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "header",
  "hgroup",
  "hr",
  "main",
  "menu",
  "nav",
  "ol",
  "p",
  "pre",
  "section",
  "table",
  "ul",
  "canvas",
  "colgroup",
  "dd",
  "body",
  "dt",
  "group",
  "html",
  "iframe",
  "li",
  "legend",
  "math",
  "map",
  "noscript",
  "output",
  "object",
  "option",
  "progress",
  "script",
  "style",
  "summary",
  "tbody",
  "td",
  "textarea",
  "tfoot",
  "th",
  "thead",
  "tr",
  "video",
  "center",
];

/// `Markdown.is_block_level`.
pub(super) fn is_block_level(tag: &str) -> bool {
  let t = tag.to_lowercase();
  BLOCK_LEVEL_ELEMENTS.contains(&t.trim_end_matches('/'))
}

/// One conversion's state: the element tree, the raw HTML stash and the
/// link references.
pub(super) struct Markdown {
  pub tree: Tree,
  pub stash: Vec<String>,
  pub references: HashMap<String, (String, Option<String>)>,
}

/// An HTML placeholder for raw HTML stored in the stash.
pub(super) fn store_html(stash: &mut Vec<String>, html: String) -> String {
  stash.push(html);
  format!("{}{}{}", HTML_PLACEHOLDER_PREFIX, stash.len() - 1, ETX)
}

// ─── Element tree ───────────────────────────────────────────────────────────

pub(super) type Id = usize;

/// Element text. `atomic` marks text inline patterns must not touch
/// (Python-Markdown's `AtomicString`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Text {
  pub s: String,
  pub atomic: bool,
}

impl Text {
  pub fn plain(s: impl Into<String>) -> Self {
    Text { s: s.into(), atomic: false }
  }
  pub fn atomic(s: impl Into<String>) -> Self {
    Text { s: s.into(), atomic: true }
  }
}

#[derive(Debug, Clone, Default)]
pub(super) struct Node {
  pub tag: String,
  pub attrs: Vec<(String, String)>,
  pub text: Option<Text>,
  pub tail: Option<Text>,
  pub children: Vec<Id>,
}

/// An arena of ElementTree-style elements.
#[derive(Debug, Default)]
pub(super) struct Tree {
  pub nodes: Vec<Node>,
}

impl Tree {
  pub fn element(&mut self, tag: &str) -> Id {
    self.nodes.push(Node { tag: tag.to_string(), ..Default::default() });
    self.nodes.len() - 1
  }

  pub fn sub(&mut self, parent: Id, tag: &str) -> Id {
    let id = self.element(tag);
    self.nodes[parent].children.push(id);
    id
  }

  pub fn insert(&mut self, parent: Id, pos: usize, child: Id) {
    let children = &mut self.nodes[parent].children;
    let pos = pos.min(children.len());
    children.insert(pos, child);
  }

  pub fn last_child(&self, id: Id) -> Option<Id> {
    self.nodes[id].children.last().copied()
  }

  pub fn len(&self, id: Id) -> usize {
    self.nodes[id].children.len()
  }

  pub fn tag(&self, id: Id) -> &str {
    &self.nodes[id].tag
  }

  /// `elem.text` is set and non-empty.
  pub fn has_text(&self, id: Id) -> bool {
    self.nodes[id].text.as_ref().is_some_and(|t| !t.s.is_empty())
  }

  pub fn text_str(&self, id: Id) -> &str {
    self.nodes[id].text.as_ref().map(|t| t.s.as_str()).unwrap_or("")
  }

  pub fn tail_str(&self, id: Id) -> &str {
    self.nodes[id].tail.as_ref().map(|t| t.s.as_str()).unwrap_or("")
  }

  pub fn set_attr(&mut self, id: Id, key: &str, value: String) {
    let attrs = &mut self.nodes[id].attrs;
    match attrs.iter_mut().find(|(k, _)| k == key) {
      Some(slot) => slot.1 = value,
      None => attrs.push((key.to_string(), value)),
    }
  }

  /// Depth-first, pre-order, like `Element.iter()`.
  pub fn iter(&self, root: Id) -> Vec<Id> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
      out.push(id);
      for &c in self.nodes[id].children.iter().rev() {
        stack.push(c);
      }
    }
    out
  }
}

// ─── Preprocessing ──────────────────────────────────────────────────────────

/// `NormalizeWhitespace`: drop placeholder markers, unify line endings,
/// expand tabs and empty out lines that hold only spaces.
fn normalize_whitespace(source: &str) -> String {
  let mut s: String = source.chars().filter(|&c| c != STX && c != ETX).collect();
  s = s.replace("\r\n", "\n").replace('\r', "\n");
  s.push_str("\n\n");
  let s = pystr::expandtabs(&s, 4);
  // re.sub(r"(?<=\n) +\n", "\n", source)
  let mut out = String::with_capacity(s.len());
  let mut rest = s.as_str();
  let mut at_line_start = false;
  while !rest.is_empty() {
    if at_line_start {
      let spaces = rest.bytes().take_while(|&b| b == b' ').count();
      if spaces > 0 && rest.as_bytes().get(spaces) == Some(&b'\n') {
        rest = &rest[spaces..];
      }
    }
    let ch = rest.chars().next().unwrap();
    out.push(ch);
    at_line_start = ch == '\n';
    rest = &rest[ch.len_utf8()..];
  }
  out
}

// ─── Tree processors ────────────────────────────────────────────────────────

fn blank(s: Option<&Text>) -> bool {
  s.is_none_or(|t| pystr::strip(&t.s).is_empty())
}

/// `PrettifyTreeprocessor`.
fn prettify(tree: &mut Tree, root: Id) {
  fn walk(tree: &mut Tree, elem: Id) {
    let tag = tree.tag(elem).to_string();
    if is_block_level(&tag) && tag != "code" && tag != "pre" {
      if blank(tree.nodes[elem].text.as_ref()) && tree.len(elem) > 0 && is_block_level(tree.tag(tree.nodes[elem].children[0])) {
        tree.nodes[elem].text = Some(Text::plain("\n"));
      }
      for e in tree.nodes[elem].children.clone() {
        if is_block_level(tree.tag(e)) {
          walk(tree, e);
        }
      }
    }
    if blank(tree.nodes[elem].tail.as_ref()) {
      tree.nodes[elem].tail = Some(Text::plain("\n"));
    }
  }
  walk(tree, root);
  for id in tree.iter(root) {
    if tree.tag(id) == "br" {
      let tail = &mut tree.nodes[id].tail;
      *tail = Some(match tail.take() {
        Some(t) if !pystr::strip(&t.s).is_empty() => Text::plain(format!("\n{}", t.s)),
        _ => Text::plain("\n"),
      });
    }
  }
  for id in tree.iter(root) {
    if tree.tag(id) == "pre" && tree.len(id) > 0 {
      let code = tree.nodes[id].children[0];
      if tree.tag(code) == "code" && tree.len(code) == 0 && tree.nodes[code].text.is_some() {
        let t = pystr::rstrip(tree.text_str(code)).to_string();
        tree.nodes[code].text = Some(Text::atomic(format!("{t}\n")));
      }
    }
  }
}

/// Replace `STX digits ETX` escape placeholders with their character.
pub(super) fn unescape_placeholders(text: &str) -> String {
  replace_escape_placeholders(text, |c| c.to_string())
}

fn replace_escape_placeholders(text: &str, f: impl Fn(char) -> String) -> String {
  if !text.contains(STX) {
    return text.to_string();
  }
  let mut out = String::with_capacity(text.len());
  let mut rest = text;
  while let Some(p) = rest.find(STX) {
    out.push_str(&rest[..p]);
    let after = &rest[p + 1..];
    let digits = after.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && after[digits..].starts_with(ETX) {
      let n: u32 = after[..digits].parse().unwrap_or(0xFFFD);
      out.push_str(&f(char::from_u32(n).unwrap_or('\u{fffd}')));
      rest = &after[digits + 1..];
    } else {
      out.push(STX);
      rest = after;
    }
  }
  out.push_str(rest);
  out
}

/// `UnescapeTreeprocessor`: restore escaped characters outside code.
fn unescape_tree(tree: &mut Tree, root: Id) {
  for id in tree.iter(root) {
    let node = &mut tree.nodes[id];
    if node.tag != "code"
      && let Some(t) = node.text.as_mut()
      && !t.s.is_empty()
    {
      t.s = unescape_placeholders(&t.s);
    }
    if let Some(t) = node.tail.as_mut()
      && !t.s.is_empty()
    {
      t.s = unescape_placeholders(&t.s);
    }
    for (_, v) in node.attrs.iter_mut() {
      *v = unescape_placeholders(v);
    }
  }
}

// ─── Serialization ──────────────────────────────────────────────────────────

const HTML_EMPTY: &[&str] =
  &["area", "base", "basefont", "br", "col", "embed", "frame", "hr", "img", "input", "isindex", "link", "meta", "param", "source", "track", "wbr"];

// `&(?!(?:\#[0-9]+|\#x[0-9a-f]+|[0-9a-z]+);)` (case-insensitive)
fn escape_amp(text: &str, out: &mut String) {
  let b = text.as_bytes();
  let mut last = 0;
  for (i, _) in text.match_indices('&') {
    out.push_str(&text[last..i]);
    let rest = &b[i + 1..];
    let entity = if rest.first() == Some(&b'#') {
      let num = rest[1..].iter().take_while(|c| c.is_ascii_digit()).count();
      let hex = if matches!(rest.get(1), Some(b'x' | b'X')) { rest[2..].iter().take_while(|c| c.is_ascii_hexdigit()).count() } else { 0 };
      (num > 0 && rest.get(1 + num) == Some(&b';')) || (hex > 0 && rest.get(2 + hex) == Some(&b';'))
    } else {
      let n = rest.iter().take_while(|c| c.is_ascii_alphanumeric()).count();
      n > 0 && rest.get(n) == Some(&b';')
    };
    out.push_str(if entity { "&" } else { "&amp;" });
    last = i + 1;
  }
  out.push_str(&text[last..]);
}

fn escape_cdata(text: &str) -> String {
  let mut amp = String::with_capacity(text.len());
  escape_amp(text, &mut amp);
  amp.replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_attrib(text: &str) -> String {
  escape_cdata(text).replace('"', "&quot;")
}

/// Serialize as Python-Markdown's XHTML writer does.
pub(super) fn serialize(tree: &Tree, root: Id) -> String {
  let mut out = String::new();
  write_element(tree, root, &mut out);
  out
}

fn write_element(tree: &Tree, id: Id, out: &mut String) {
  let node = &tree.nodes[id];
  out.push('<');
  out.push_str(&node.tag);
  let mut attrs = node.attrs.clone();
  attrs.sort();
  for (k, v) in attrs {
    out.push_str(&format!(" {}=\"{}\"", k, escape_attrib(&v)));
  }
  let lower = node.tag.to_lowercase();
  if HTML_EMPTY.contains(&lower.as_str()) {
    out.push_str(" />");
  } else {
    out.push('>');
    if let Some(t) = &node.text
      && !t.s.is_empty()
    {
      if lower == "script" || lower == "style" {
        out.push_str(&t.s);
      } else {
        out.push_str(&escape_cdata(&t.s));
      }
    }
    for &c in &node.children {
      write_element(tree, c, out);
    }
    out.push_str(&format!("</{}>", node.tag));
  }
  if let Some(t) = &node.tail
    && !t.s.is_empty()
  {
    out.push_str(&escape_cdata(&t.s));
  }
}

// ─── Postprocessing ─────────────────────────────────────────────────────────

/// `RawHtmlPostprocessor`: put stashed raw HTML back.
fn raw_html_postprocess(text: &str, stash: &[String]) -> String {
  if stash.is_empty() {
    return text.to_string();
  }
  substitute_html(text, stash)
}

fn parse_html_placeholder(s: &str) -> Option<(usize, usize)> {
  let rest = s.strip_prefix(HTML_PLACEHOLDER_PREFIX)?;
  let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
  if digits == 0 || !rest[digits..].starts_with(ETX) {
    return None;
  }
  let key = rest[..digits].parse().unwrap_or(usize::MAX);
  Some((key, HTML_PLACEHOLDER_PREFIX.len() + digits + 1))
}

fn substitute_html(text: &str, stash: &[String]) -> String {
  let mut out = String::with_capacity(text.len());
  let mut i = 0;
  while i < text.len() {
    let rest = &text[i..];
    // <p>PLACEHOLDER</p>
    if let Some(inner) = rest.strip_prefix("<p>")
      && let Some((key, len)) = parse_html_placeholder(inner)
      && inner[len..].starts_with("</p>")
    {
      let whole = 3 + len + 4;
      match stash.get(key) {
        None => out.push_str(&rest[..whole]),
        Some(html) if html_is_block_level(html) => out.push_str(&substitute_html(html, stash)),
        Some(html) => out.push_str(&substitute_html(&format!("<p>{}</p>", html), stash)),
      }
      i += whole;
      continue;
    }
    if let Some((key, len)) = parse_html_placeholder(rest) {
      match stash.get(key) {
        None => out.push_str(&rest[..len]),
        Some(html) => out.push_str(&substitute_html(html, stash)),
      }
      i += len;
      continue;
    }
    let ch = rest.chars().next().unwrap();
    out.push(ch);
    i += ch.len_utf8();
  }
  out
}

// `^\<\/?([^ >]+)`
fn html_is_block_level(html: &str) -> bool {
  let Some(rest) = html.strip_prefix('<') else {
    return false;
  };
  let rest = rest.strip_prefix('/').unwrap_or(rest);
  let name: String = rest.chars().take_while(|&c| c != ' ' && c != '>').collect();
  if name.is_empty() {
    return false;
  }
  if matches!(name.chars().next(), Some('!' | '?' | '@' | '%')) {
    return true;
  }
  is_block_level(&name)
}

/// Upstream's compatibility postprocessor: escape placeholders left in code
/// become their character, HTML-escaped.
fn unescape_placeholders_as_html(text: &str) -> String {
  replace_escape_placeholders(text, |c| pystr::escape_html(&c.to_string()))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_examples() {
    // Expected values come from upstream's markdown_to_html().
    assert_eq!(markdown_to_html("first\nline\n\nsecond"), "<p>first<br />\nline</p>\n<p>second</p>");
    assert_eq!(markdown_to_html("Q:\tQuestion\nA:\tAnswer"), "<p>Q:  Question<br />\nA:  Answer</p>");
    assert_eq!(markdown_to_html("`inline`"), "<p><code>inline</code></p>");
    assert_eq!(markdown_to_html("a\nb"), "<p>a<br />\nb</p>");
    assert_eq!(markdown_to_html_with("a\nb", &[]), "<p>a\nb</p>");
    assert_eq!(markdown_to_html("   \n "), "");
  }
}
