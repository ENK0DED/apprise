//! Log output in upstream's format (`2026-01-01 12:00:00,000 - INFO - ...`,
//! on stdout), limited to Apprise's own messages, with secrets masked.

use std::fmt;

use apprise_core::utils::cwe312::cwe312_text;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, filter};

/// Upstream's level for a `-v` count: errors only, then warnings, info,
/// debug and trace.
pub fn level_for(verbosity: u32) -> Level {
  match verbosity {
    0 => Level::ERROR,
    1 => Level::WARN,
    2 => Level::INFO,
    3 => Level::DEBUG,
    _ => Level::TRACE,
  }
}

/// Install the logger. With `secure`, URLs in messages are masked
/// (CWE-312) whatever logged them.
pub fn init(verbosity: u32, secure: bool) {
  let max = level_for(verbosity);
  let only_apprise = filter::filter_fn(move |meta| meta.target().starts_with("apprise") && *meta.level() <= max);
  let layer = tracing_subscriber::fmt::layer().event_format(UpstreamFormat { secure }).with_writer(std::io::stdout).with_filter(only_apprise);
  let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer));
}

struct UpstreamFormat {
  secure: bool,
}

impl<S, N> FormatEvent<S, N> for UpstreamFormat
where
  S: Subscriber + for<'a> LookupSpan<'a>,
  N: for<'a> FormatFields<'a> + 'static,
{
  fn format_event(&self, _ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
    let mut message = String::new();
    event.record(&mut MessageVisitor(&mut message));
    let message = if self.secure { cwe312_text(&message) } else { message };
    writeln!(writer, "{} - {} - {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S,%3f"), level_name(*event.metadata().level()), message)
  }
}

fn level_name(level: Level) -> &'static str {
  match level {
    Level::ERROR => "ERROR",
    Level::WARN => "WARNING",
    Level::INFO => "INFO",
    Level::DEBUG => "DEBUG",
    Level::TRACE => "TRACE",
  }
}

struct MessageVisitor<'a>(&'a mut String);

impl Visit for MessageVisitor<'_> {
  fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
    use std::fmt::Write;
    if field.name() == "message" {
      let _ = write!(self.0, "{value:?}");
    } else {
      let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_levels() {
    assert_eq!(level_for(0), Level::ERROR);
    assert_eq!(level_for(1), Level::WARN);
    assert_eq!(level_for(2), Level::INFO);
    assert_eq!(level_for(3), Level::DEBUG);
    assert_eq!(level_for(9), Level::TRACE);
    assert_eq!(level_name(Level::WARN), "WARNING");
  }
}
