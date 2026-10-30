//! Just enough XML for an XMPP client stream: split the incoming stream
//! into its header, top-level stanzas and footer, parse each stanza into a
//! small element tree, and escape outgoing text.

/// One element of a parsed stanza. Names keep their prefix as written
/// (`stream:features`); `local()` drops it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
  pub name: String,
  pub attrs: Vec<(String, String)>,
  pub children: Vec<Element>,
  pub text: String,
}

impl Element {
  /// The name without its namespace prefix.
  pub fn local(&self) -> &str {
    self.name.rsplit(':').next().unwrap_or(&self.name)
  }

  pub fn attr(&self, name: &str) -> Option<&str> {
    self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
  }

  /// The first child with this local name.
  pub fn child(&self, local: &str) -> Option<&Element> {
    self.children.iter().find(|c| c.local() == local)
  }

  /// The first child with this local name in namespace `ns`.
  pub fn child_ns(&self, local: &str, ns: &str) -> Option<&Element> {
    self.children.iter().find(|c| c.local() == local && c.attr("xmlns") == Some(ns))
  }

  pub fn is(&self, local: &str) -> bool {
    self.local() == local
  }
}

/// What the stream reader found next.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
  /// `<stream:stream ...>`, with its attributes.
  StreamOpen(Vec<(String, String)>),
  /// A complete top-level element.
  Stanza(Element),
  /// `</stream:stream>`.
  StreamClose,
}

/// Collects incoming bytes and returns complete events from them.
#[derive(Default)]
pub struct StreamReader {
  buf: Vec<u8>,
}

impl StreamReader {
  pub fn feed(&mut self, data: &[u8]) {
    self.buf.extend_from_slice(data);
  }

  /// The next complete event, `Ok(None)` when more data is needed.
  pub fn next_event(&mut self) -> Result<Option<Event>, String> {
    loop {
      let skip = self.buf.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(self.buf.len());
      self.buf.drain(..skip);
      if self.buf.is_empty() {
        return Ok(None);
      }
      // A multi-byte character may be cut at the end of what has arrived.
      let text = match std::str::from_utf8(&self.buf) {
        Ok(text) => text,
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&self.buf[..e.valid_up_to()]).unwrap_or_default(),
        Err(_) => return Err("invalid UTF-8 in the stream".into()),
      };
      let Some((used, event)) = scan(text)? else { return Ok(None) };
      self.buf.drain(..used);
      if event.is_some() {
        return Ok(event);
      }
    }
  }
}

/// Look at the start of `text`: the bytes the next item takes and the event
/// it is (`None` for a declaration or comment), or `None` if incomplete.
fn scan(text: &str) -> Result<Option<(usize, Option<Event>)>, String> {
  if !text.starts_with('<') {
    return Err("text outside of a stanza".into());
  }
  // XML declaration, processing instruction or comment: skip it.
  if let Some(rest) = text.strip_prefix("<?") {
    return Ok(rest.find("?>").map(|end| (2 + end + 2, None)));
  }
  if let Some(rest) = text.strip_prefix("<!--") {
    return Ok(rest.find("-->").map(|end| (4 + end + 3, None)));
  }
  if let Some(rest) = text.strip_prefix("</") {
    let Some(end) = rest.find('>') else { return Ok(None) };
    let name = rest[..end].trim();
    if name.rsplit(':').next() == Some("stream") {
      return Ok(Some((2 + end + 1, Some(Event::StreamClose))));
    }
    return Err(format!("unexpected closing tag </{}>", name));
  }
  let Some(tag_end) = find_tag_end(text, 0) else { return Ok(None) };
  let (name, attrs, self_closing) = parse_start_tag(&text[..=tag_end])?;
  if name.rsplit(':').next() == Some("stream") && !self_closing {
    return Ok(Some((tag_end + 1, Some(Event::StreamOpen(attrs)))));
  }
  let Some(end) = find_element_end(text)? else { return Ok(None) };
  let (element, _) = parse_element(&text[..end], 0)?;
  Ok(Some((end, Some(Event::Stanza(element)))))
}

/// Index of the `>` that ends the tag starting at `start`, skipping quoted
/// attribute values.
fn find_tag_end(text: &str, start: usize) -> Option<usize> {
  let mut quote: Option<char> = None;
  for (i, c) in text[start..].char_indices() {
    match (quote, c) {
      (Some(q), c) if c == q => quote = None,
      (Some(_), _) => {}
      (None, '"' | '\'') => quote = Some(c),
      (None, '>') => return Some(start + i),
      _ => {}
    }
  }
  None
}

/// Byte offset just past the element that starts at 0, or `None` when it
/// is not complete yet.
fn find_element_end(text: &str) -> Result<Option<usize>, String> {
  let mut depth = 0usize;
  let mut pos = 0;
  while let Some(offset) = text[pos..].find('<') {
    let i = pos + offset;
    let rest = &text[i..];
    if rest.starts_with("<!--") {
      let Some(end) = rest.find("-->") else { return Ok(None) };
      pos = i + end + 3;
      continue;
    }
    if rest.starts_with("<![CDATA[") {
      let Some(end) = rest.find("]]>") else { return Ok(None) };
      pos = i + end + 3;
      continue;
    }
    let Some(end) = find_tag_end(text, i) else { return Ok(None) };
    let tag = &text[i..=end];
    if tag.starts_with("</") {
      depth = depth.checked_sub(1).ok_or("unbalanced closing tag")?;
    } else if !tag.starts_with("<?") && !tag.ends_with("/>") {
      depth += 1;
    }
    pos = end + 1;
    if depth == 0 {
      return Ok(Some(pos));
    }
  }
  Ok(None)
}

/// A start tag: its name, attributes and whether it closes itself.
type StartTag = (String, Vec<(String, String)>, bool);

/// Parse `<name attr='v' ...>` (or `.../>`).
fn parse_start_tag(tag: &str) -> Result<StartTag, String> {
  let self_closing = tag.ends_with("/>");
  let inner = tag.trim_start_matches('<').trim_end_matches('>').trim_end_matches('/');
  let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
  let name = inner[..name_end].to_string();
  if name.is_empty() {
    return Err("element without a name".into());
  }
  let mut attrs = Vec::new();
  let mut rest = inner[name_end..].trim_start();
  while !rest.is_empty() {
    let eq = rest.find('=').ok_or("attribute without a value")?;
    let key = rest[..eq].trim().to_string();
    let after = rest[eq + 1..].trim_start();
    let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'').ok_or("unquoted attribute value")?;
    let close = after[1..].find(quote).ok_or("unterminated attribute value")?;
    attrs.push((key, unescape(&after[1..1 + close])));
    rest = after[close + 2..].trim_start();
  }
  Ok((name, attrs, self_closing))
}

/// Parse the element starting at `pos`; returns it and the offset after it.
fn parse_element(text: &str, pos: usize) -> Result<(Element, usize), String> {
  let tag_end = find_tag_end(text, pos).ok_or("unterminated tag")?;
  let (name, attrs, self_closing) = parse_start_tag(&text[pos..=tag_end])?;
  let mut element = Element { name, attrs, ..Default::default() };
  let mut pos = tag_end + 1;
  if self_closing {
    return Ok((element, pos));
  }
  loop {
    let offset = text[pos..].find('<').ok_or("unterminated element")?;
    element.text.push_str(&unescape(&text[pos..pos + offset]));
    pos += offset;
    let rest = &text[pos..];
    if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
      let end = cdata.find("]]>").ok_or("unterminated CDATA")?;
      element.text.push_str(&cdata[..end]);
      pos += 9 + end + 3;
    } else if rest.starts_with("<!--") {
      pos += rest.find("-->").ok_or("unterminated comment")? + 3;
    } else if rest.starts_with("</") {
      let end = rest.find('>').ok_or("unterminated closing tag")?;
      return Ok((element, pos + end + 1));
    } else {
      let (child, next) = parse_element(text, pos)?;
      element.children.push(child);
      pos = next;
    }
  }
}

/// Decode the predefined entities and character references.
pub fn unescape(text: &str) -> String {
  if !text.contains('&') {
    return text.to_string();
  }
  let mut out = String::with_capacity(text.len());
  let mut rest = text;
  while let Some(i) = rest.find('&') {
    out.push_str(&rest[..i]);
    rest = &rest[i..];
    let Some(end) = rest.find(';') else { break };
    let entity = &rest[1..end];
    let decoded = match entity {
      "lt" => Some('<'),
      "gt" => Some('>'),
      "amp" => Some('&'),
      "quot" => Some('"'),
      "apos" => Some('\''),
      _ => entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
        .and_then(char::from_u32),
    };
    match decoded {
      Some(c) => {
        out.push(c);
        rest = &rest[end + 1..];
      }
      None => {
        out.push('&');
        rest = &rest[1..];
      }
    }
  }
  out.push_str(rest);
  out
}

/// Escape character data, dropping characters XML 1.0 does not allow.
pub fn escape_text(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  for c in text.chars().filter(|c| is_xml_char(*c)) {
    match c {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      _ => out.push(c),
    }
  }
  out
}

/// Escape an attribute value written between single quotes.
pub fn escape_attr(text: &str) -> String {
  escape_text(text).replace('\'', "&apos;").replace('"', "&quot;")
}

fn is_xml_char(c: char) -> bool {
  matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

#[cfg(test)]
mod tests {
  use super::*;

  fn events(chunks: &[&str]) -> Vec<Event> {
    let mut reader = StreamReader::default();
    let mut out = Vec::new();
    for chunk in chunks {
      reader.feed(chunk.as_bytes());
      while let Some(event) = reader.next_event().unwrap() {
        out.push(event);
      }
    }
    out
  }

  #[test]
  fn test_stream_events() {
    let got = events(&[
      "<?xml version='1.0'?><stream:stream xmlns='jabber:client' id='abc' version='1.0'>",
      "<stream:features><starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'><required/></starttls>",
      "<mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>PLAIN</mechanism></mechanisms></stream:features>",
      "  <proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'/></stream:stream>",
    ]);
    assert_eq!(got.len(), 4);
    assert_eq!(got[0], Event::StreamOpen(vec![("xmlns".into(), "jabber:client".into()), ("id".into(), "abc".into()), ("version".into(), "1.0".into())]));
    let Event::Stanza(features) = &got[1] else { panic!("{:?}", got[1]) };
    assert_eq!(features.name, "stream:features");
    assert!(features.is("features"));
    assert!(features.child("starttls").unwrap().child("required").is_some());
    assert_eq!(features.child_ns("mechanisms", "urn:ietf:params:xml:ns:xmpp-sasl").unwrap().child("mechanism").unwrap().text, "PLAIN");
    assert_eq!(
      got[2],
      Event::Stanza(Element { name: "proceed".into(), attrs: vec![("xmlns".into(), "urn:ietf:params:xml:ns:xmpp-tls".into())], ..Default::default() })
    );
    assert_eq!(got[3], Event::StreamClose);
  }

  #[test]
  fn test_split_chunks_and_entities() {
    // An element split across reads, with entities, CDATA and a '>' inside
    // an attribute value
    let got = events(&["<stream:stream>", "<message from=\"a&amp;b\" note='x>y'><bo", "dy>1 &lt; 2 &#x263A; <![CDATA[<raw>]]></body></mess", "age>"]);
    let Event::Stanza(message) = &got[1] else { panic!() };
    assert_eq!(message.attr("from"), Some("a&b"));
    assert_eq!(message.attr("note"), Some("x>y"));
    assert_eq!(message.child("body").unwrap().text, "1 < 2 \u{263A} <raw>");
  }

  #[test]
  fn test_escape() {
    assert_eq!(escape_text("a < b & c > d\u{1}\r\n"), "a &lt; b &amp; c &gt; d\r\n");
    assert_eq!(escape_attr("it's \"x\""), "it&apos;s &quot;x&quot;");
    assert_eq!(unescape("&lt;&gt;&amp;&quot;&apos;&#65;&#x42;&bogus;"), "<>&\"'AB&bogus;");
  }

  #[test]
  fn test_errors() {
    let mut reader = StreamReader::default();
    reader.feed(b"junk");
    assert!(reader.next_event().is_err());
  }
}
