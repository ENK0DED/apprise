//! What a test logs. One subscriber serves the whole test binary and keeps
//! a buffer per thread: a subscriber set per test with `set_default` lets
//! tracing cache a callsite as disabled when a test on another thread
//! registers it at the same moment, which dropped lines under parallel
//! tests. A filter evaluated on every event leaves nothing to cache.

use std::cell::RefCell;
use std::sync::Once;

use tracing::Level;

thread_local! {
  /// The capture running on this thread: its level and what it logged.
  static CAPTURE: RefCell<Option<(Level, Vec<u8>)>> = const { RefCell::new(None) };
}

struct ThreadBuffer;

impl std::io::Write for ThreadBuffer {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    CAPTURE.with(|c| {
      if let Some((_, logs)) = c.borrow_mut().as_mut() {
        logs.extend_from_slice(buf);
      }
    });
    Ok(buf.len())
  }
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

fn install() {
  static INSTALL: Once = Once::new();
  INSTALL.call_once(|| {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    // A dynamic filter: a plain filter_fn is asked once per callsite and
    // its answer cached, which would disable whatever first logs outside
    // a capture
    let capturing =
      tracing_subscriber::filter::dynamic_filter_fn(|meta, _| CAPTURE.with(|c| c.borrow().as_ref().is_some_and(|(level, _)| meta.level() <= level)));
    let layer = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(|| ThreadBuffer).with_filter(capturing);
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer)).expect("only test_logs sets the global subscriber");
  });
}

/// What `fut` logs at `level` and above on this thread, with its output.
/// Run it on a current-thread runtime (`#[tokio::test]`'s default).
pub(crate) async fn capture<F: std::future::Future>(level: Level, fut: F) -> (F::Output, String) {
  install();
  CAPTURE.with(|c| *c.borrow_mut() = Some((level, Vec::new())));
  let out = fut.await;
  let logs = CAPTURE.with(|c| c.borrow_mut().take()).map(|(_, logs)| logs).unwrap_or_default();
  (out, String::from_utf8_lossy(&logs).into_owned())
}
