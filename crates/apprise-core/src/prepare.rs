//! Turns one notification into the message pieces a service receives:
//! format conversion, escape and emoji interpretation, title and body limits,
//! and overflow handling.

use crate::notify::{Notify, NotifyContext, OverflowMode};
use crate::utils;
use crate::utils::{emoji::interpret_emojis, escape::interpret_escapes, format::smart_split};

/// Prepare one or more NotifyContexts for a service, handling format conversion,
/// emoji/escape interpretation, title/body truncation, and overflow splitting.
pub(crate) fn prepare_contexts(svc: &dyn Notify, ctx: &NotifyContext) -> Vec<NotifyContext> {
  let mut svc_ctx = ctx.clone();

  // Per-service format conversion
  let target_format = svc.notify_format();
  if target_format != svc_ctx.body_format {
    svc_ctx.body = utils::format::convert_format(&svc_ctx.body, &svc_ctx.body_format, &target_format);
    svc_ctx.body_format = target_format;
  }

  // Escape / emoji interpretation
  if svc_ctx.interpret_escapes {
    svc_ctx.body = interpret_escapes(&svc_ctx.body);
  }
  if svc_ctx.interpret_emojis {
    svc_ctx.body = interpret_emojis(&svc_ctx.body);
  }

  // Line-count truncation
  let max_lines = svc.body_max_line_count();
  if max_lines > 0 {
    let lines: Vec<&str> = svc_ctx.body.lines().collect();
    if lines.len() > max_lines {
      svc_ctx.body = lines[..max_lines].join("\n");
    }
  }

  // Title truncation
  let title_max = svc.title_maxlen();
  if title_max > 0 && svc_ctx.title.len() > title_max {
    svc_ctx.title.truncate(title_max);
  } else if title_max == 0 {
    svc_ctx.title.clear();
  }

  // Overflow handling
  let body_max = svc.body_maxlen();
  match svc.overflow_mode() {
    OverflowMode::Upstream => vec![svc_ctx],
    OverflowMode::Truncate => {
      if body_max > 0 && svc_ctx.body.len() > body_max {
        svc_ctx.body.truncate(body_max);
      }
      vec![svc_ctx]
    }
    OverflowMode::Split => {
      if body_max == 0 || svc_ctx.body.len() <= body_max {
        return vec![svc_ctx];
      }
      let chunks = smart_split(&svc_ctx.body, body_max, &svc_ctx.body_format);
      let original_title = svc_ctx.title.clone();
      chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
          let mut chunk_ctx = svc_ctx.clone();
          chunk_ctx.body = chunk;
          if i > 0 {
            chunk_ctx.title = String::new();
          } else {
            chunk_ctx.title = original_title.clone();
          }
          chunk_ctx
        })
        .collect()
    }
  }
}
