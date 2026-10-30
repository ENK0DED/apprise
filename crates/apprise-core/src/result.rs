//! Structured outcomes of one notification call (upstream `apprise/result.py`).
//!
//! - [`ResultStatus`]: the outcome of a call, a service or one attempt. Its
//!   values double as the CLI exit codes.
//! - [`NotifyAttempt`]: one call to a service's `send()` (one retry).
//! - [`ServiceResult`]: every attempt made for one service.
//! - [`SendResult`]: every service dispatched by one call.

use std::time::Duration;

use serde_json::{Value, json};

/// The outcome of a notification call, a service, or one attempt.
///
/// The numeric values are the CLI exit codes (2 is reserved for invalid
/// command-line arguments).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResultStatus {
  /// Every required service accepted the notification.
  Success = 0,
  /// A required service failed, or the call could not start.
  Failure = 1,
  /// Nothing was attempted: no services loaded, or no tag matched.
  NoMatch = 3,
  /// At least one real delivery and at least one non-delivery.
  Partial = 4,
  /// Nothing failed outright, but a service ran out of time.
  Timeout = 5,
}

impl ResultStatus {
  /// The CLI exit code for this status.
  pub fn exit_code(self) -> i32 {
    self as i32
  }

  /// Upstream's name for this status (`SUCCESS`, `NOMATCH`, ...).
  pub fn name(self) -> &'static str {
    match self {
      Self::Success => "SUCCESS",
      Self::Failure => "FAILURE",
      Self::NoMatch => "NOMATCH",
      Self::Partial => "PARTIAL",
      Self::Timeout => "TIMEOUT",
    }
  }
}

impl std::fmt::Display for ResultStatus {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.name())
  }
}

/// The raw outcome of one call to a service (`Success`, `Failure` or
/// `Timeout`). A zero-length `Timeout` means the deadline had already passed
/// and the attempt was never started.
#[derive(Debug, Clone, PartialEq)]
pub struct NotifyAttempt {
  pub status: ResultStatus,
  pub elapsed: Duration,
}

impl NotifyAttempt {
  pub fn is_success(&self) -> bool {
    self.status == ResultStatus::Success
  }

  pub fn to_json(&self) -> Value {
    json!({ "status": self.status.name(), "elapsed": self.elapsed.as_secs_f64() })
  }
}

/// The outcome of one service dispatched by a call.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceResult {
  /// Human-readable service name.
  pub name: String,
  /// The service URL with secrets masked; safe to log or print.
  pub url: String,
  /// The service's tags, sorted.
  pub tags: Vec<String>,
  /// Whether a failure of this service is tolerated (`?optional=yes`).
  pub optional: bool,
  /// Attempts allowed for this service (retries plus one).
  pub max_attempts: u32,
  /// Every attempt made, in order.
  pub attempts: Vec<NotifyAttempt>,
  /// Reflective status: `Success` for an optional service whatever happened.
  pub status: ResultStatus,
  /// Time from the first attempt starting to the last one ending, retry
  /// waits included.
  pub elapsed: Duration,
}

impl ServiceResult {
  pub(crate) fn new(name: String, url: String, tags: Vec<String>, optional: bool, max_attempts: u32, attempts: Vec<NotifyAttempt>, elapsed: Duration) -> Self {
    let last = attempts.last().map(|a| a.status).unwrap_or(ResultStatus::Failure);
    let status = if last == ResultStatus::Success || optional {
      ResultStatus::Success
    } else if last == ResultStatus::Timeout && !attempts.iter().any(|a| a.status == ResultStatus::Failure) {
      // No confirmed failure happened before time ran out.
      ResultStatus::Timeout
    } else {
      ResultStatus::Failure
    };
    Self { name, url, tags, optional, max_attempts, attempts, status, elapsed }
  }

  pub fn is_success(&self) -> bool {
    self.status == ResultStatus::Success
  }

  pub fn to_json(&self) -> Value {
    json!({
      "name": self.name,
      "url": self.url,
      "tag": self.tags,
      "status": self.status.name(),
      "optional": self.optional,
      "max_attempts": self.max_attempts,
      "elapsed": self.elapsed.as_secs_f64(),
      "attempts": self.attempts.iter().map(NotifyAttempt::to_json).collect::<Vec<_>>(),
    })
  }
}

/// The overall outcome of one notification call.
#[derive(Debug, Clone, PartialEq)]
pub struct SendResult {
  /// Number of services dispatched (services skipped by priority escalation
  /// are not counted).
  pub total: usize,
  /// Dispatched services whose status is `Success` (optional services that
  /// failed count here, as upstream's `success_count`).
  pub succeeded: usize,
  /// Dispatched services whose status is not `Success`.
  pub failed: usize,
  /// The overall outcome.
  pub status: ResultStatus,
  /// One entry per dispatched service, in dispatch order.
  pub results: Vec<ServiceResult>,
  /// Wall-clock time of the whole call.
  pub elapsed: Duration,
}

impl SendResult {
  pub(crate) fn new(status: ResultStatus, results: Vec<ServiceResult>, elapsed: Duration) -> Self {
    let succeeded = results.iter().filter(|r| r.is_success()).count();
    Self { total: results.len(), succeeded, failed: results.len() - succeeded, status, results, elapsed }
  }

  /// Whether the call as a whole succeeded (upstream `bool(result)`).
  pub fn is_success(&self) -> bool {
    self.status == ResultStatus::Success
  }

  /// Dispatched services that ran out of time.
  pub fn timed_out(&self) -> usize {
    self.results.iter().filter(|r| r.status == ResultStatus::Timeout).count()
  }

  pub fn to_json(&self) -> Value {
    json!({
      "status": self.status.name(),
      "success_count": self.succeeded,
      "failed_count": self.failed,
      "elapsed": self.elapsed.as_secs_f64(),
      "results": self.results.iter().map(ServiceResult::to_json).collect::<Vec<_>>(),
    })
  }
}

/// Combine service results into the call status: a real delivery makes a
/// mixed batch `Partial`; otherwise `Failure` beats `Timeout`.
pub(crate) fn aggregate_status(ok: bool, results: &[ServiceResult]) -> ResultStatus {
  if ok {
    return ResultStatus::Success;
  }
  if results.iter().flat_map(|r| &r.attempts).any(NotifyAttempt::is_success) {
    return ResultStatus::Partial;
  }
  if results.iter().any(|r| r.status == ResultStatus::Failure) {
    return ResultStatus::Failure;
  }
  if results.iter().any(|r| r.status == ResultStatus::Timeout) {
    return ResultStatus::Timeout;
  }
  ResultStatus::Failure
}

#[cfg(test)]
mod tests {
  use super::*;

  fn attempt(status: ResultStatus) -> NotifyAttempt {
    NotifyAttempt { status, elapsed: Duration::from_millis(1) }
  }

  fn service(optional: bool, attempts: &[ResultStatus]) -> ServiceResult {
    ServiceResult::new(
      "Test".into(),
      "json://localhost".into(),
      vec![],
      optional,
      attempts.len() as u32,
      attempts.iter().copied().map(attempt).collect(),
      Duration::ZERO,
    )
  }

  #[test]
  fn test_service_status() {
    use ResultStatus::*;
    assert_eq!(service(false, &[Failure, Success]).status, Success);
    assert_eq!(service(false, &[Failure]).status, Failure);
    assert_eq!(service(true, &[Failure]).status, Success);
    assert_eq!(service(false, &[Timeout]).status, Timeout);
    // A confirmed failure is more useful than a later timeout.
    assert_eq!(service(false, &[Failure, Timeout]).status, Failure);
    assert_eq!(service(false, &[]).status, Failure);
  }

  #[test]
  fn test_aggregate_status() {
    use ResultStatus::*;
    assert_eq!(aggregate_status(true, &[service(false, &[Success])]), Success);
    assert_eq!(aggregate_status(false, &[service(false, &[Success]), service(false, &[Failure])]), Partial);
    assert_eq!(aggregate_status(false, &[service(false, &[Success]), service(false, &[Timeout])]), Partial);
    assert_eq!(aggregate_status(false, &[service(false, &[Failure]), service(false, &[Timeout])]), Failure);
    assert_eq!(aggregate_status(false, &[service(false, &[Timeout])]), Timeout);
    // An optional failure that was absorbed is not a real delivery.
    assert_eq!(aggregate_status(false, &[service(true, &[Failure]), service(false, &[Failure])]), Failure);
    assert_eq!(aggregate_status(false, &[]), Failure);
  }

  #[test]
  fn test_send_result_counts() {
    use ResultStatus::*;
    let r = SendResult::new(Partial, vec![service(false, &[Success]), service(false, &[Failure]), service(false, &[Timeout])], Duration::ZERO);
    assert_eq!((r.total, r.succeeded, r.failed, r.timed_out()), (3, 1, 2, 1));
    assert_eq!(r.status.exit_code(), 4);
    assert_eq!(r.to_json()["results"][2]["status"], "TIMEOUT");
  }
}
