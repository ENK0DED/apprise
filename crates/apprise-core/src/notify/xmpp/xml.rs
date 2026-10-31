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

/// Deepest element nesting accepted in a stanza, counting the stanza
/// itself. Stanzas nest a handful of levels; the parser recurses once per
/// level, so the peer must not choose how deep.
const MAX_DEPTH: usize = 128;
const TOO_DEEP: &str = "XML nested too deeply";

/// Largest stanza accepted, markup included. Stanzas are a few kilobytes;
/// without a limit, a peer that never finishes one would have us buffer
/// whatever it sends.
pub const MAX_STANZA_LEN: usize = 1024 * 1024;
const TOO_LARGE: &str = "XML stanza too large";

/// Collects incoming bytes and returns complete events from them. Each call
/// carries on scanning where the last one stopped, so an item that arrives
/// over many reads is looked at once.
#[derive(Default)]
pub struct StreamReader {
  buf: Vec<u8>,
  /// Bytes at the front of `buf` known to be valid UTF-8.
  valid: usize,
  /// How far the unfinished item at the front of `buf` has been scanned.
  scan: Scan,
}

impl StreamReader {
  pub fn feed(&mut self, data: &[u8]) {
    self.buf.extend_from_slice(data);
  }

  /// The next complete event, `Ok(None)` when more data is needed.
  pub fn next_event(&mut self) -> Result<Option<Event>, String> {
    // Only what arrived since the last call needs checking. A multi-byte
    // character may be cut at the end of it.
    match std::str::from_utf8(&self.buf[self.valid..]) {
      Ok(_) => self.valid = self.buf.len(),
      Err(e) if e.error_len().is_none() => self.valid += e.valid_up_to(),
      Err(_) => return Err("invalid UTF-8 in the stream".into()),
    }
    loop {
      if self.scan.pos == 0 {
        let skip = self.buf[..self.valid].iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(self.valid);
        self.consume(skip);
      }
      if self.valid == 0 {
        return Ok(None);
      }
      let Some(item) = self.scan.advance(&self.buf[..self.valid])? else {
        if self.buf.len() > MAX_STANZA_LEN {
          return Err(TOO_LARGE.into());
        }
        return Ok(None);
      };
      let (end, event) = match item {
        Item::Skip(end) => (end, None),
        Item::Event(event, end) => (end, Some(event)),
        Item::Stanza(end) => {
          if end > MAX_STANZA_LEN {
            return Err(TOO_LARGE.into());
          }
          // It ends with '>' inside the checked bytes, so it is whole
          let text = std::str::from_utf8(&self.buf[..end]).map_err(|_| "invalid UTF-8 in the stream")?;
          (end, Some(Event::Stanza(parse_element(text, 0, 0)?.0)))
        }
      };
      self.scan = Scan::default();
      self.consume(end);
      if event.is_some() {
        return Ok(event);
      }
    }
  }

  fn consume(&mut self, n: usize) {
    self.buf.drain(..n);
    self.valid -= n;
  }
}

/// A complete item at the front of the buffer and the bytes it takes.
enum Item {
  /// A declaration, processing instruction or comment between stanzas.
  Skip(usize),
  /// The stream header or its end.
  Event(Event, usize),
  Stanza(usize),
}

/// Where the scan of the item at the front of the buffer stands.
#[derive(Default)]
struct Scan {
  /// The next byte to look at.
  pos: usize,
  /// Elements open in the stanza; 0 until its start tag is complete.
  depth: usize,
  /// What `pos` is inside of, and where that began.
  within: Within,
  start: usize,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Within {
  #[default]
  Text,
  /// A tag, and the quote of the attribute value it is in.
  Tag(Option<u8>),
  Comment,
  CData,
  /// A declaration or processing instruction between stanzas.
  Declaration,
}

impl Scan {
  /// Scan on through `text`, the front of the buffer, up to the end of the
  /// item, or `Ok(None)` when it is not complete yet. The delimiters are
  /// ASCII, so bytes can be scanned without decoding characters.
  fn advance(&mut self, text: &[u8]) -> Result<Option<Item>, String> {
    loop {
      match self.within {
        Within::Text => {
          if self.depth == 0 && self.pos == 0 && text[0] != b'<' {
            return Err("text outside of a stanza".into());
          }
          let Some(offset) = text[self.pos..].iter().position(|&b| b == b'<') else {
            self.pos = text.len();
            return Ok(None);
          };
          let i = self.pos + offset;
          let rest = &text[i..];
          let cut = |opener: &[u8]| rest.len() < opener.len() && opener.starts_with(rest);
          if rest.len() < 2 || cut(b"<!--") || cut(b"<![CDATA[") {
            self.pos = i;
            return Ok(None);
          }
          self.start = i;
          (self.within, self.pos) = if rest.starts_with(b"<!--") {
            (Within::Comment, i + 4)
          } else if rest.starts_with(b"<![CDATA[") {
            if self.depth == 0 {
              return Err("text outside of a stanza".into());
            }
            (Within::CData, i + 9)
          } else if rest.starts_with(b"<?") && self.depth == 0 {
            (Within::Declaration, i + 2)
          } else {
            (Within::Tag(None), i + 1)
          };
        }
        Within::Comment | Within::CData | Within::Declaration => {
          let close: &[u8] = match self.within {
            Within::Comment => b"-->",
            Within::CData => b"]]>",
            _ => b"?>",
          };
          let Some(offset) = text[self.pos..].windows(close.len()).position(|w| w == close) else {
            // The closer may be cut at the end; look at its start again
            self.pos = text.len().saturating_sub(close.len() - 1).max(self.pos);
            return Ok(None);
          };
          self.pos += offset + close.len();
          self.within = Within::Text;
          if self.depth == 0 {
            return Ok(Some(Item::Skip(self.pos)));
          }
        }
        Within::Tag(mut quote) => {
          let mut tag_end = None;
          for (offset, &b) in text[self.pos..].iter().enumerate() {
            match quote {
              Some(q) if b == q => quote = None,
              Some(_) => {}
              None if b == b'"' || b == b'\'' => quote = Some(b),
              None if b == b'>' => {
                tag_end = Some(self.pos + offset);
                break;
              }
              None => {}
            }
          }
          let Some(end) = tag_end else {
            self.within = Within::Tag(quote);
            self.pos = text.len();
            return Ok(None);
          };
          self.within = Within::Text;
          self.pos = end + 1;
          let tag = std::str::from_utf8(&text[self.start..=end]).map_err(|_| "invalid UTF-8 in the stream")?;
          if let Some(close) = tag.strip_prefix("</") {
            if self.depth == 0 {
              let name = close.trim_end_matches('>').trim();
              if name.rsplit(':').next() == Some("stream") {
                return Ok(Some(Item::Event(Event::StreamClose, self.pos)));
              }
              return Err(format!("unexpected closing tag </{}>", name));
            }
            self.depth -= 1;
            if self.depth == 0 {
              return Ok(Some(Item::Stanza(self.pos)));
            }
          } else if tag.starts_with("<?") {
            // A processing instruction inside a stanza does not nest
          } else if self.depth == 0 {
            let (name, attrs, self_closing) = parse_start_tag(tag)?;
            if name.rsplit(':').next() == Some("stream") && !self_closing {
              return Ok(Some(Item::Event(Event::StreamOpen(attrs), self.pos)));
            }
            if self_closing {
              return Ok(Some(Item::Stanza(self.pos)));
            }
            self.depth = 1;
          } else if !tag.ends_with("/>") {
            self.depth += 1;
            // Refuse it now rather than buffer the rest of it
            if self.depth > MAX_DEPTH {
              return Err(TOO_DEEP.into());
            }
          }
        }
      }
    }
  }
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

/// Parse the element starting at `pos`, `depth` levels below the stanza;
/// returns it and the offset after it.
fn parse_element(text: &str, pos: usize, depth: usize) -> Result<(Element, usize), String> {
  if depth >= MAX_DEPTH {
    return Err(TOO_DEEP.into());
  }
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
      let (child, next) = parse_element(text, pos, depth + 1)?;
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

  /// Terminators and openers cut between reads are still found.
  #[test]
  fn test_split_delimiters() {
    let got = events(&[
      "<?xml version='1.0'?",
      "><stream:stream><",
      "!-",
      "- a comment -",
      "-><!-- another -",
      "-",
      "><message><body><!",
      "[CDATA[x]",
      "]>",
      "y<",
      "/body></message>",
    ]);
    assert_eq!(got.len(), 2, "{:?}", got);
    let Event::Stanza(message) = &got[1] else { panic!("{:?}", got[1]) };
    assert_eq!(message.child("body").unwrap().text, "xy");

    // One byte at a time, with a multi-byte character split too
    let doc = "<stream:stream><message to='\u{263A}>'><body>caf\u{e9} <![CDATA[<b>]]></body><x/></message></stream:stream>";
    let mut reader = StreamReader::default();
    let mut got = Vec::new();
    for b in doc.as_bytes() {
      reader.feed(&[*b]);
      while let Some(event) = reader.next_event().unwrap() {
        got.push(event);
      }
    }
    assert_eq!(got.len(), 3, "{:?}", got);
    let Event::Stanza(message) = &got[1] else { panic!("{:?}", got[1]) };
    assert_eq!(message.attr("to"), Some("\u{263A}>"));
    assert_eq!(message.child("body").unwrap().text, "caf\u{e9} <b>");
    assert_eq!(got[2], Event::StreamClose);
  }

  /// A stanza that never ends is refused once it passes the limit, instead
  /// of being buffered for as long as the server sends it. Upstream
  /// (slixmpp over expat) keeps all of it.
  #[test]
  fn test_stanza_size_limit() {
    let mut reader = StreamReader::default();
    reader.feed(b"<stream:stream><stream:features>");
    assert!(matches!(reader.next_event(), Ok(Some(Event::StreamOpen(_)))));
    let chunk = "x".repeat(8192);
    let mut fed = "<stream:features>".len();
    let result = loop {
      reader.feed(chunk.as_bytes());
      fed += chunk.len();
      match reader.next_event() {
        Ok(None) => assert!(fed <= MAX_STANZA_LEN, "still buffering after {} bytes", fed),
        other => break other,
      }
    };
    assert_eq!(result, Err(TOO_LARGE.to_string()));
    assert!(fed <= MAX_STANZA_LEN + chunk.len());

    // The same goes for one long start tag, and for one that arrives whole
    let mut reader = StreamReader::default();
    reader.feed(format!("<message a='{}", "x".repeat(MAX_STANZA_LEN)).as_bytes());
    assert_eq!(reader.next_event(), Err(TOO_LARGE.to_string()));
    let mut reader = StreamReader::default();
    reader.feed(format!("<message>{}</message>", "x".repeat(MAX_STANZA_LEN)).as_bytes());
    assert_eq!(reader.next_event(), Err(TOO_LARGE.to_string()));

    // Just under the limit is fine, and whitespace between stanzas does
    // not count towards it
    let mut reader = StreamReader::default();
    let body = "x".repeat(MAX_STANZA_LEN - "<message></message>".len());
    for _ in 0..3 {
      reader.feed(" ".repeat(MAX_STANZA_LEN / 2).as_bytes());
      assert_eq!(reader.next_event(), Ok(None));
    }
    reader.feed(format!("<message>{}</message>", body).as_bytes());
    let Ok(Some(Event::Stanza(message))) = reader.next_event() else { panic!() };
    assert_eq!(message.text.len(), body.len());
  }

  /// A stanza that arrives in many small reads is scanned once, not again
  /// from its start on every read (which took seconds for a few MiB).
  #[test]
  fn test_scan_resumes() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
      let mut reader = StreamReader::default();
      reader.feed(b"<stream:stream><stream:features>");
      let _ = reader.next_event();
      // Many small elements, then a long text, in 64-byte reads
      let mut doc = "<mechanism a='1'>PLAIN</mechanism>".repeat(16 * 1024);
      doc.push_str(&"y".repeat(MAX_STANZA_LEN - doc.len() - 1024));
      doc.push_str("</stream:features>");
      let start = std::time::Instant::now();
      let mut got = None;
      for chunk in doc.as_bytes().chunks(64) {
        reader.feed(chunk);
        if let Some(event) = reader.next_event().unwrap() {
          got = Some(event);
        }
      }
      let _ = tx.send((got, start.elapsed()));
    });
    let (got, elapsed) = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("the reader rescans what it has already seen");
    let Some(Event::Stanza(features)) = got else { panic!("{:?}", got) };
    assert_eq!(features.children.len(), 16 * 1024);
    assert!(elapsed < std::time::Duration::from_secs(5), "{:?}", elapsed);
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

  /// A stanza nested deeper than any XMPP server sends: parsing it used to
  /// recurse once per level and overflow the stack, aborting the process.
  #[test]
  fn test_nesting_limit() {
    let nested = |depth: usize| format!("<stream:stream><stream:features>{}{}</stream:features>", "<a>".repeat(depth), "</a>".repeat(depth));

    // On half the stack a Tokio worker thread gets
    let run = |doc: String| {
      std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || {
          let mut reader = StreamReader::default();
          reader.feed(doc.as_bytes());
          assert!(matches!(reader.next_event(), Ok(Some(Event::StreamOpen(_)))));
          reader.next_event().map(|e| e.is_some())
        })
        .unwrap()
        .join()
        .unwrap()
    };

    // The features element is one level, so this is as deep as it goes
    assert_eq!(run(nested(MAX_DEPTH - 1)), Ok(true));
    assert_eq!(run(nested(MAX_DEPTH)), Err("XML nested too deeply".to_string()));
    // Refused while it is still arriving, before it is complete
    assert_eq!(run(format!("<stream:stream><stream:features>{}", "<a>".repeat(MAX_DEPTH))), Err("XML nested too deeply".to_string()));
    assert_eq!(run(nested(100_000)), Err("XML nested too deeply".to_string()));
  }
}
