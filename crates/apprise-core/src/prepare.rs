//! Turns one notification into the message pieces a service receives, as
//! upstream does before calling a plugin's `send()` (`Apprise._create_notify_gen`
//! and `NotifyBase._build_send_calls`, `_apply_overflow` and
//! `_apply_dialect_overflow`):
//!
//! 1. Pick the format this send renders as (`?format=`, the plugin's
//!    declared formats and the input format).
//! 2. Interpret backslash escapes and `:emoji:` shortcodes (`?emojis=`).
//! 3. Convert the body (and a title blended into the body) to that format.
//! 4. Fold the title into the body when the service has no title field,
//!    apply `body_max_line_count`, then the title and body limits with the
//!    `upstream`, `truncate` or `split` overflow mode (`?overflow=`).
//! 5. Repair CommonMark cut at a piece boundary, then convert each piece to
//!    the plugin's dialect, re-splitting so the converted text still fits.
//!
//! Every length is counted in characters, as upstream counts them.

use crate::conversion::pystr;
use crate::conversion::{RepairState, commonmark_repair_chunk, convert_between, split_dialect_chunk, truncate_dialect_chunk};
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, OverflowMode};
use crate::types::NotifyFormat;
use crate::utils::{emoji::interpret_emojis, escape::interpret_escapes, format::smart_split};

// Upstream NotifyBase class defaults no plugin overrides.
const OVERFLOW_MAX_DISPLAY_COUNT_WIDTH: i64 = 12;
const OVERFLOW_BUFFER: i64 = 0;
const OVERFLOW_DISPLAY_COUNT_THRESHOLD: i64 = 130;
const DEFAULT_HTML_TAG_ID: &str = "b";

/// One prepared title/body pair.
#[derive(Debug, Clone, PartialEq)]
struct Piece {
  title: String,
  body: String,
  // Repeat the title on pieces a dialect conversion splits off this one.
  repeat_title: bool,
}

fn char_len(s: &str) -> i64 {
  s.chars().count() as i64
}

/// Python `s[:n]` for a possibly negative `n`.
fn head(s: &str, n: i64) -> String {
  let len = char_len(s);
  let end = if n < 0 { (len + n).max(0) } else { n.min(len) };
  s.chars().take(end as usize).collect()
}

/// Python `s[n:]` for a possibly negative `n`.
fn tail(s: &str, n: i64) -> String {
  let len = char_len(s);
  let start = if n < 0 { (len + n).max(0) } else { n.min(len) };
  s.chars().skip(start as usize).collect()
}

/// Python `s[a:b]` for non-negative bounds.
fn slice(s: &str, a: i64, b: i64) -> String {
  let a = a.max(0) as usize;
  let b = b.max(0) as usize;
  if b <= a {
    return String::new();
  }
  s.chars().skip(a).take(b - a).collect()
}

/// `chunk.lstrip("\r\n\x0b\x0c").rstrip()`
fn tidy_chunk(chunk: &str) -> String {
  pystr::rstrip(pystr::lstrip_chars(chunk, "\r\n\x0b\x0c")).to_string()
}

/// Upstream `resolve_format`: the format this send renders as.
fn resolve_format(svc: &dyn Notify, input: &NotifyFormat) -> NotifyFormat {
  let requested = if svc.notify_format_overridable() { svc.service_options().and_then(|o| o.format.clone()) } else { None };
  let mut formats = svc.notify_formats();
  if formats.is_empty() {
    formats.push(svc.notify_format());
  }
  if formats.len() == 1 {
    // A single-format plugin has `?format=` replace its one format.
    return requested.unwrap_or_else(|| formats.remove(0));
  }
  if let Some(r) = requested {
    if formats.contains(&r) {
      return r;
    }
    tracing::debug!("{} does not support format {}; ignoring override.", svc.service_name(), r);
  }
  if formats.contains(input) {
    return input.clone();
  }
  formats.remove(0)
}

/// Prepare the contexts to hand `svc.send()` for one notification.
///
/// Returns an error, without any piece to send, when nothing is left to
/// deliver: an empty body with no attachment, or attachments only for a
/// service that cannot send them. Upstream fails the service in both cases.
pub(crate) fn prepare_contexts(svc: &dyn Notify, ctx: &NotifyContext) -> Result<Vec<NotifyContext>, NotifyError> {
  let opts = svc.service_options();
  let target = resolve_format(svc, &ctx.body_format);
  let emojis = opts.and_then(|o| o.emojis).unwrap_or(ctx.interpret_emojis);

  let mut title = ctx.title.clone();
  let mut body = ctx.body.clone();

  // Escapes and emojis first, while their source markers are recognizable.
  if ctx.interpret_escapes {
    body = interpret_escapes(&body);
    title = interpret_escapes(&title);
  }
  if emojis {
    body = interpret_emojis(&body);
    title = interpret_emojis(&title);
  }

  // A title is only converted when it is blended into the body.
  if !title.is_empty() && svc.title_maxlen() == 0 {
    title = convert_between(&ctx.body_format, &target, &title);
  }
  body = convert_between(&ctx.body_format, &target, &body);

  if svc.bypass_overflow() {
    // The plugin takes the converted message whole (upstream plugins that
    // override `_build_send_calls`).
    let mut c = ctx.clone();
    c.title = title;
    c.body = body;
    c.body_format = target;
    return Ok(vec![c]);
  }

  if body.is_empty() && ctx.attachments.is_empty() {
    tracing::warn!("No message body or attachment was specified.");
    return Err(NotifyError::MissingParam("body".into()));
  }
  if body.is_empty() && !svc.attachment_support() {
    tracing::warn!("{} does not support attachments; service skipped", svc.service_name());
    return Err(NotifyError::NotSupported);
  }

  let overflow = opts.and_then(|o| o.overflow).filter(|_| svc.overflow_mode_overridable()).unwrap_or_else(|| svc.overflow_mode());

  let pieces: Vec<Piece> =
    apply_overflow(svc, &body, &title, overflow, &target).into_iter().flat_map(|p| apply_dialect_overflow(svc, p, overflow, &target)).collect();

  // Truncate mode keeps only the first attachment, and only one piece
  // carries attachments so they are never repeated across a split.
  let attachments = if overflow == OverflowMode::Truncate && ctx.attachments.len() > 1 { ctx.attachments[..1].to_vec() } else { ctx.attachments.clone() };
  let attach_index = svc.attachment_send_index(pieces.len());

  Ok(
    pieces
      .into_iter()
      .enumerate()
      .map(|(i, piece)| {
        let mut c = ctx.clone();
        c.title = piece.title;
        c.body = piece.body;
        c.body_format = target.clone();
        c.attachments = if i == attach_index { attachments.clone() } else { Vec::new() };
        c
      })
      .collect(),
  )
}

/// Upstream `_apply_overflow`: title blending, line count, limits and overflow.
fn apply_overflow(svc: &dyn Notify, body: &str, title: &str, overflow: OverflowMode, body_format: &NotifyFormat) -> Vec<Piece> {
  let mut title = pystr::strip(title).to_string();
  let mut body = pystr::rstrip(body).to_string();
  let svc_title_maxlen = svc.title_maxlen() as i64;
  let svc_body_maxlen = svc.body_maxlen() as i64;
  let amalgamate = svc.overflow_amalgamate_title();

  // A service without a title field gets the title at the top of the body.
  if svc_title_maxlen <= 0 && !title.is_empty() {
    match body_format {
      NotifyFormat::Html => {
        body = format!("<{tag}>{title}</{tag}><br />\r\n{body}", tag = DEFAULT_HTML_TAG_ID);
      }
      NotifyFormat::Markdown => {
        let heading = pystr::lstrip_chars(&title, "\r\n \t\x0b\x0c#-");
        if !heading.is_empty() {
          body = format!("# {heading}\n{body}");
        }
      }
      NotifyFormat::Text => body = format!("{title}\r\n{body}"),
    }
    title.clear();
  }

  let line_count = svc.body_max_line_count();
  if line_count > 0 {
    // re.split(r"\r*\n", body)
    let mut lines: Vec<&str> = Vec::new();
    let mut rest = body.as_str();
    while let Some(p) = rest.find('\n') {
      lines.push(rest[..p].trim_end_matches('\r'));
      rest = &rest[p + 1..];
    }
    lines.push(rest);
    body = lines.into_iter().take(line_count).collect::<Vec<_>>().join("\r\n");
  }

  if overflow == OverflowMode::Upstream {
    return vec![Piece { title, body, repeat_title: false }];
  }

  // "2" allows for the \r\n applied when amalgamating.
  let overflow_buffer = if svc_title_maxlen == 0 && !title.is_empty() { OVERFLOW_BUFFER.max(2) } else { OVERFLOW_BUFFER };

  let title_maxlen =
    if amalgamate { (char_len(&title) + OVERFLOW_MAX_DISPLAY_COUNT_WIDTH).min(svc_title_maxlen).min(svc_body_maxlen) } else { svc_title_maxlen };
  if char_len(&title) > title_maxlen {
    title = pystr::rstrip(&head(&title, title_maxlen)).to_string();
  }

  let body_maxlen = if amalgamate && (svc_body_maxlen - overflow_buffer) >= title_maxlen {
    (if title.is_empty() { svc_body_maxlen } else { svc_body_maxlen - title_maxlen }) - overflow_buffer
  } else if amalgamate {
    svc_body_maxlen - overflow_buffer
  } else {
    svc_body_maxlen
  };

  if body_maxlen > 0 && char_len(&body) <= body_maxlen {
    return vec![Piece { title, body, repeat_title: false }];
  }

  let markdown = *body_format == NotifyFormat::Markdown;

  if overflow == OverflowMode::Truncate {
    let mut truncated = pystr::rstrip(pystr::lstrip_chars(&head(&body, body_maxlen), "\r\n\x0b\x0c")).to_string();
    if markdown {
      // An odd run of trailing backslashes lost the character it escaped.
      let backslashes = truncated.chars().rev().take_while(|&c| c == '\\').count();
      if backslashes % 2 == 1 {
        truncated.pop();
        truncated = pystr::rstrip(&truncated).to_string();
      }
      // Repair truncated constructs; bounded discarded text can identify a closer.
      let lookahead_end = body_maxlen + body_maxlen * 8;
      let lookahead = slice(&body, body_maxlen, lookahead_end);
      let boundary = body.chars().nth(lookahead_end.max(0) as usize).filter(|_| lookahead_end >= 0);
      let next = if lookahead.is_empty() { None } else { Some(lookahead.as_str()) };
      truncated = commonmark_repair_chunk(&truncated, &RepairState::default(), next, boundary).0;
    }
    return vec![Piece { title, body: truncated, repeat_title: false }];
  }

  // SPLIT mode.
  let display_title_once = amalgamate && body_maxlen < OVERFLOW_DISPLAY_COUNT_THRESHOLD;
  let mut response: Vec<Piece> = Vec::new();

  if !display_title_once && !(amalgamate && body_maxlen <= 0) {
    // Repeat the title on every piece, with a [X/Y] counter when it fits.
    let mut show_counter = !title.is_empty()
      && char_len(&body) > body_maxlen
      && ((amalgamate && body_maxlen >= OVERFLOW_DISPLAY_COUNT_THRESHOLD) || (!amalgamate && title_maxlen > OVERFLOW_DISPLAY_COUNT_THRESHOLD))
      && (title_maxlen > OVERFLOW_MAX_DISPLAY_COUNT_WIDTH + overflow_buffer && svc_title_maxlen >= OVERFLOW_DISPLAY_COUNT_THRESHOLD);

    let mut effective_body_maxlen = body_maxlen;
    if show_counter {
      effective_body_maxlen -= overflow_buffer;
    }
    let chunks = smart_split(&body, effective_body_maxlen.max(0) as usize, body_format);
    let count = chunks.len();
    let mut digits = 0;
    if show_counter {
      digits = count.to_string().len();
      let width = 4 + (digits as i64 * 2);
      if width <= OVERFLOW_MAX_DISPLAY_COUNT_WIDTH {
        let t_max = title_maxlen - width;
        if char_len(&title) > t_max {
          title = head(&title, t_max);
        }
      } else {
        show_counter = false;
      }
    }
    for (idx, chunk) in chunks.iter().enumerate() {
      let suffix = if show_counter { format!(" [{:0w$}/{:0w$}]", idx + 1, count, w = digits) } else { String::new() };
      response.push(Piece { title: format!("{title}{suffix}"), body: tidy_chunk(chunk), repeat_title: true });
    }
  } else {
    // Show the title once, on the first piece.
    let mut remainder = body.clone();
    if body_maxlen > 0 && !body.is_empty() {
      let first_chunks = smart_split(&body, body_maxlen as usize, body_format);
      let first_body = first_chunks.into_iter().next().unwrap_or_default();
      remainder = tail(&body, char_len(&first_body));
      response.push(Piece { title: title.clone(), body: tidy_chunk(&first_body), repeat_title: false });
    } else {
      response.push(Piece { title: title.clone(), body: String::new(), repeat_title: false });
    }
    if !remainder.is_empty() {
      for chunk in smart_split(&remainder, svc_body_maxlen.max(0) as usize, body_format) {
        response.push(Piece { title: String::new(), body: tidy_chunk(&chunk), repeat_title: false });
      }
    }
  }

  if markdown {
    // Repair each piece; the next piece's raw text classifies a trailing delimiter.
    let mut pending = RepairState::default();
    for idx in 0..response.len() {
      let next = response.get(idx + 1).map(|p| p.body.clone());
      let (fixed, state) = commonmark_repair_chunk(&response[idx].body, &pending, next.as_deref(), None);
      response[idx].body = fixed;
      pending = state;
    }
  }
  response
}

/// Upstream `_apply_dialect_overflow`: convert one piece to the plugin's
/// dialect and re-split or re-truncate it so the converted text fits.
fn apply_dialect_overflow(svc: &dyn Notify, piece: Piece, overflow: OverflowMode, body_format: &NotifyFormat) -> Vec<Piece> {
  let converted = svc.dialect_convert(&piece.body, body_format);
  if !svc.has_dialect() && converted == piece.body {
    // No dialect: the piece is sent exactly as prepared.
    return vec![piece];
  }
  let convert = |body: &str| svc.dialect_convert(body, body_format);
  let limit = svc.body_maxlen();
  let bodies = match overflow {
    OverflowMode::Split => split_dialect_chunk(&piece.body, limit, &convert, Some(body_format)),
    OverflowMode::Truncate => vec![truncate_dialect_chunk(&piece.body, limit, &convert, Some(body_format))],
    OverflowMode::Upstream => vec![converted],
  };
  bodies
    .into_iter()
    .enumerate()
    .map(|(idx, body)| Piece { title: if idx == 0 || piece.repeat_title { piece.title.clone() } else { String::new() }, body, repeat_title: false })
    .collect()
}

#[cfg(test)]
mod tests;
