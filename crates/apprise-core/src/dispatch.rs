//! Sends prepared notifications to the selected services (upstream
//! `apprise/dispatch.py` and `Apprise.notify()`).
//!
//! One call is handled in three tiers:
//!
//! - the call: tag selection, priority escalation across independent tag
//!   chains, the call-wide deadline and the overall status;
//! - a batch: the services of one priority group, sent concurrently (or one
//!   after another when the asset disables async mode);
//! - a service: its message pieces, retries and waits, its own deadline, and
//!   the HTTP options its `send()` runs with.
//!
//! A filter with an explicit priority (`2:tag`) sends every matched service
//! as one flat batch. Otherwise each OR'd tag becomes its own chain whose
//! priority groups are tried in ascending order until one succeeds.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use tokio::time::Instant;

use crate::asset::AppriseAsset;
use crate::error::NotifyError;
use crate::notify::options::{MAX_SERVICE_RETRY, MAX_SERVICE_WAIT};
use crate::notify::{HttpOptions, Notify, NotifyContext, with_http_options, with_throttle};
use crate::prepare::prepare_contexts;
use crate::result::{NotifyAttempt, ResultStatus, SendResult, ServiceResult, aggregate_status};
use crate::tag::{AppriseTag, TagFilter, parse_service_tags};
use crate::utils::cwe312::cwe312_text;

/// How long past its deadline a service may run before the caller stops
/// waiting for it (upstream `_ABANDON_GRACE_SECONDS`).
const ABANDON_GRACE: Duration = Duration::from_millis(100);

/// Settings for one call, taken from the asset and the caller.
#[derive(Debug, Clone)]
pub(crate) struct DispatchOptions {
  /// Limit for the whole call (upstream `notify(timeout=)`).
  pub call_timeout: Option<Duration>,
  /// Limit for each service, retries and waits included.
  pub service_timeout: Option<Duration>,
  /// Send services one at a time instead of concurrently.
  pub sequential: bool,
  /// Stop once any tag chain has exhausted its priority groups.
  pub abort_on_chain_failure: bool,
  /// Retries for services whose URL sets none.
  pub default_retry: u32,
  /// Seconds between retries for services whose URL sets none.
  pub default_wait: f64,
  /// Follow redirects for services whose URL sets no `?redirect=`.
  pub http_redirects: bool,
  /// Mask secrets in log output.
  pub secure_logging: bool,
  /// Persistent storage settings and the instance's open stores.
  pub storage_path: Option<std::path::PathBuf>,
  pub storage_mode: crate::types::StorageMode,
  pub storage_salt: Vec<u8>,
  pub storage_idlen: usize,
  pub stores: crate::storage::StoreRegistry,
  /// Attachments that were requested but could not be read (masked
  /// sources). Services that take attachments fail; others send without.
  pub attachment_errors: Vec<String>,
}

impl DispatchOptions {
  pub fn from_asset(asset: &AppriseAsset) -> Self {
    Self {
      call_timeout: None,
      service_timeout: asset.service_timeout(),
      sequential: !asset.async_mode,
      abort_on_chain_failure: asset.abort_on_chain_failure,
      default_retry: asset.default_service_retry,
      default_wait: asset.default_service_wait,
      http_redirects: asset.http_redirects,
      secure_logging: asset.secure_logging,
      storage_path: asset.storage_path.clone(),
      storage_mode: asset.storage_mode.clone(),
      storage_salt: asset.storage_salt.clone(),
      storage_idlen: asset.storage_idlen,
      stores: crate::storage::StoreRegistry::default(),
      attachment_errors: Vec::new(),
    }
  }
}

/// One matched service and what the filter says about it.
struct Call<'a> {
  svc: &'a dyn Notify,
  tags: Vec<AppriseTag>,
  /// A `tag:N` retry override from the filter.
  retry_override: Option<u64>,
}

/// Validate a call-wide time limit: finite and not negative. Zero means no
/// limit.
pub(crate) fn validate_timeout(seconds: f64) -> Result<Option<Duration>, String> {
  if !seconds.is_finite() || seconds < 0.0 {
    return Err("timeout must be >= 0 and finite.".to_string());
  }
  Ok((seconds > 0.0).then(|| crate::utils::time::secs(seconds)))
}

/// Send `ctx` to the services selected by `filter`.
pub(crate) async fn notify(services: &[std::sync::Arc<dyn Notify>], filter: &TagFilter, ctx: &NotifyContext, opts: &DispatchOptions) -> SendResult {
  let start = Instant::now();
  let call_deadline = opts.call_timeout.map(|t| start + t);
  let finish = |status, results| SendResult::new(status, results, start.elapsed());

  if services.is_empty() {
    tracing::warn!("There are no service(s) to notify");
    return finish(ResultStatus::NoMatch, Vec::new());
  }

  if ctx.title.is_empty() && ctx.body.is_empty() && ctx.attachments.is_empty() && opts.attachment_errors.is_empty() {
    tracing::error!("No message content specified to deliver");
    return finish(ResultStatus::Failure, Vec::new());
  }

  let calls: Vec<Call> = services
    .iter()
    .filter_map(|svc| {
      let tags = parse_service_tags(&svc.tags());
      filter.matches(&tags).then(|| Call { svc: svc.as_ref(), retry_override: filter.retry_for(&tags), tags })
    })
    .collect();

  if calls.is_empty() {
    tracing::warn!("There are no service(s) to notify");
    return finish(ResultStatus::NoMatch, Vec::new());
  }

  if filter.has_explicit_priority() {
    // An exact priority sends one flat batch without escalation.
    let batch: Vec<&Call> = calls.iter().collect();
    let (ok, results) = run_batch(&batch, ctx, opts, call_deadline).await;
    let status = aggregate_status(ok, &results);
    return finish(status, results);
  }

  // Each OR tag becomes its own chain, starting at its most urgent group. A
  // failed group advances only its own chain; active chains run together.
  let mut chains = build_chains(&calls, filter);
  let mut results = Vec::new();
  loop {
    if opts.abort_on_chain_failure && chains.iter().any(|c| !c.succeeded && c.exhausted()) {
      break;
    }
    let active: Vec<usize> = (0..chains.len()).filter(|&i| chains[i].pending()).collect();
    if active.is_empty() {
      break;
    }
    let batches = join_all(active.iter().map(|&i| run_batch(chains[i].batch(), ctx, opts, call_deadline)).collect()).await;
    for (&i, (ok, batch_results)) in active.iter().zip(batches) {
      results.extend(batch_results);
      chains[i].settle(ok);
    }
  }
  let ok = chains.iter().all(|c| c.succeeded);
  let status = aggregate_status(ok, &results);
  finish(status, results)
}

/// One OR tag's services grouped by priority, tried in ascending order until
/// a group succeeds.
struct TagChain<'c, 'a> {
  key: String,
  groups: Vec<(u64, Vec<&'c Call<'a>>)>,
  index: usize,
  succeeded: bool,
}

impl<'c, 'a> TagChain<'c, 'a> {
  fn exhausted(&self) -> bool {
    self.index >= self.groups.len()
  }

  fn pending(&self) -> bool {
    !self.succeeded && !self.exhausted()
  }

  fn batch(&self) -> &[&'c Call<'a>] {
    &self.groups[self.index].1
  }

  fn settle(&mut self, ok: bool) {
    let priority = self.groups[self.index].0;
    if ok {
      tracing::trace!("Chain '{}' priority group {} succeeded.", self.key, priority);
      self.succeeded = true;
    } else {
      tracing::trace!("Chain '{}' priority group {} failed; escalating.", self.key, priority);
      self.index += 1;
    }
  }
}

/// Group matched services into escalation chains (upstream
/// `_build_tag_chains`). A service joins the first OR tag it carries, at
/// that tag's priority; anything else (AND groups, `always`, match-all)
/// shares the `""` chain at its most urgent matching priority.
fn build_chains<'c, 'a>(calls: &'c [Call<'a>], filter: &TagFilter) -> Vec<TagChain<'c, 'a>> {
  let or_names = filter.chain_names();
  let filter_names: HashSet<String> = filter.tokens().iter().map(|t| t.name().to_string()).collect();
  let mut order: Vec<String> = Vec::new();
  let mut chains: HashMap<String, BTreeMap<u64, Vec<&'c Call<'a>>>> = HashMap::new();
  for call in calls {
    let named = or_names.iter().filter(|n| !n.is_empty()).find_map(|name| call.tags.iter().find(|t| t.name() == name).map(|t| (name.clone(), t.priority)));
    let (key, priority) = named.unwrap_or_else(|| {
      let p = call.tags.iter().filter(|t| filter_names.contains(t.name())).map(|t| t.priority).min().unwrap_or(0);
      (String::new(), p)
    });
    if !chains.contains_key(&key) {
      order.push(key.clone());
    }
    chains.entry(key).or_default().entry(priority).or_default().push(call);
  }
  order
    .into_iter()
    .map(|key| {
      let groups = chains.remove(&key).unwrap_or_default().into_iter().collect();
      TagChain { key, groups, index: 0, succeeded: false }
    })
    .collect()
}

/// Send one priority group. Results keep the batch order.
async fn run_batch(batch: &[&Call<'_>], ctx: &NotifyContext, opts: &DispatchOptions, call_deadline: Option<Instant>) -> (bool, Vec<ServiceResult>) {
  let results = if opts.sequential {
    let mut results = Vec::with_capacity(batch.len());
    for call in batch {
      // Each service's budget starts when it does.
      let deadline = compute_deadline(opts, call_deadline);
      results.push(run_bounded(call, ctx, opts, deadline).await);
    }
    results
  } else {
    if batch.len() > 1 {
      tracing::debug!("Notifying {} service(s) concurrently.", batch.len());
    }
    let deadline = compute_deadline(opts, call_deadline);
    join_all(batch.iter().map(|call| run_bounded(call, ctx, opts, deadline)).collect()).await
  };
  (results.iter().all(ServiceResult::is_success), results)
}

/// The earlier of the service limit and the call deadline.
fn compute_deadline(opts: &DispatchOptions, call_deadline: Option<Instant>) -> Option<Instant> {
  let service = opts.service_timeout.map(|t| Instant::now() + t);
  match (service, call_deadline) {
    (Some(a), Some(b)) => Some(a.min(b)),
    (a, b) => a.or(b),
  }
}

/// Run one service, but stop waiting for it shortly after its deadline.
async fn run_bounded(call: &Call<'_>, ctx: &NotifyContext, opts: &DispatchOptions, deadline: Option<Instant>) -> ServiceResult {
  let Some(deadline) = deadline else {
    return run_service(call, ctx, opts, None).await;
  };
  let started = Instant::now();
  match tokio::time::timeout_at(deadline + ABANDON_GRACE, run_service(call, ctx, opts, Some(deadline))).await {
    Ok(result) => result,
    Err(_) => {
      // The send is dropped, which cancels any request still in flight.
      let elapsed = started.elapsed();
      timeout_log(call.svc.service_name(), elapsed);
      service_result(call, resolve_retry(call, opts) + 1, vec![NotifyAttempt { status: ResultStatus::Timeout, elapsed }], elapsed)
    }
  }
}

fn timeout_log(name: &str, elapsed: Duration) {
  tracing::error!("Service '{}' did not finish within {:.3}s.", name, elapsed.as_secs_f64());
}

/// Retries for a service: the filter's `tag:N`, else its `?retry=`, else
/// the asset default, never more than [`MAX_SERVICE_RETRY`].
fn resolve_retry(call: &Call<'_>, opts: &DispatchOptions) -> u32 {
  let configured = call.svc.service_options().and_then(|o| o.retry).unwrap_or(opts.default_retry) as u64;
  call.retry_override.unwrap_or(configured).min(MAX_SERVICE_RETRY as u64) as u32
}

fn resolve_wait(call: &Call<'_>, opts: &DispatchOptions) -> Duration {
  let wait = call.svc.service_options().and_then(|o| o.wait).unwrap_or(opts.default_wait);
  if wait.is_finite() && wait > 0.0 { Duration::from_secs_f64(wait.min(MAX_SERVICE_WAIT)) } else { Duration::ZERO }
}

fn http_options(call: &Call<'_>, opts: &DispatchOptions) -> HttpOptions {
  let svc_opts = call.svc.service_options();
  HttpOptions {
    connect_timeout: svc_opts.and_then(|o| o.cto),
    read_timeout: svc_opts.and_then(|o| o.rto),
    redirect: Some(svc_opts.and_then(|o| o.redirect).unwrap_or(opts.http_redirects)),
    verify: svc_opts.and_then(|o| o.verify),
  }
}

fn service_result(call: &Call<'_>, max_attempts: u32, attempts: Vec<NotifyAttempt>, elapsed: Duration) -> ServiceResult {
  let optional = call.svc.service_options().and_then(|o| o.optional).unwrap_or(false);
  let url =
    call.svc.source_url().map(crate::utils::cwe312::privacy_url).unwrap_or_else(|| format!("{}://", call.svc.schemas().first().copied().unwrap_or("unknown")));
  let mut tags: Vec<String> = call.tags.iter().map(|t| t.name().to_string()).collect();
  tags.sort();
  ServiceResult::new(call.svc.service_name().to_string(), url, tags, optional, max_attempts, attempts, elapsed)
}

/// Send every piece of the message to one service, retrying failed pieces.
async fn run_service(call: &Call<'_>, ctx: &NotifyContext, opts: &DispatchOptions, deadline: Option<Instant>) -> ServiceResult {
  let name = call.svc.service_name();
  let retry = resolve_retry(call, opts);
  let wait = resolve_wait(call, opts);
  let total = retry + 1;
  let http = http_options(call, opts);
  // A message that cannot be prepared (nothing left to send) fails the
  // service without an attempt, as upstream.
  let pieces = match prepare_contexts(call.svc, ctx) {
    Ok(pieces) => pieces,
    Err(e) => {
      log_send_error(name, &e, opts.secure_logging);
      return service_result(call, total, vec![NotifyAttempt { status: ResultStatus::Failure, elapsed: Duration::ZERO }], Duration::ZERO);
    }
  };
  let interval = call.svc.request_rate_per_sec();
  let clock = call.svc.throttle_clock().unwrap_or_default();

  let blocked = attachment_problem(call.svc, ctx, opts);

  let run = async {
    let started = Instant::now();
    let mut attempts = Vec::with_capacity(total as usize);
    // Pieces already accepted are not sent again by a retry.
    let mut accepted = vec![false; pieces.len()];
    for attempt in 0..total {
      if deadline.is_some_and(|d| Instant::now() >= d) {
        tracing::trace!("Deadline already passed for '{}'; skipping attempt {}/{}.", name, attempt + 1, total);
        timeout_log(name, Duration::ZERO);
        attempts.push(NotifyAttempt { status: ResultStatus::Timeout, elapsed: Duration::ZERO });
        break;
      }
      tracing::trace!("Starting attempt {}/{} for '{}'.", attempt + 1, total, name);
      let attempt_start = Instant::now();
      let mut ok = true;
      for (index, piece) in pieces.iter().enumerate() {
        if accepted[index] {
          tracing::debug!("{} already accepted this message piece; skipping.", name);
          continue;
        }
        if let Some(problem) = &blocked {
          tracing::error!("{}", problem);
          ok = false;
          break;
        }
        set_piece(index);
        match with_throttle(interval, clock.clone(), with_http_options(http.clone(), call.svc.send(piece))).await {
          Ok(true) => accepted[index] = true,
          Ok(false) => {
            tracing::warn!("Failed to send {} notification.", name);
            ok = false;
          }
          Err(e) => {
            log_send_error(name, &e, opts.secure_logging);
            ok = false;
          }
        }
      }
      let elapsed = attempt_start.elapsed();
      tracing::trace!(
        "Attempt {}/{} for '{}' finished in {:.3}s: {}.",
        attempt + 1,
        total,
        name,
        elapsed.as_secs_f64(),
        if ok { "success" } else { "failure" }
      );
      attempts.push(NotifyAttempt { status: if ok { ResultStatus::Success } else { ResultStatus::Failure }, elapsed });
      if ok {
        tracing::info!("Sent {} notification.", name);
        break;
      }
      if attempt + 1 < total {
        tracing::warn!("Attempt {}/{} for '{}' failed; trying again.", attempt + 1, total, name);
        // Never wait past the deadline.
        let delay = deadline.map_or(wait, |d| wait.min(d.saturating_duration_since(Instant::now())));
        if !delay.is_zero() {
          tokio::time::sleep(delay).await;
        }
      }
    }
    let succeeded = attempts.iter().any(NotifyAttempt::is_success);
    if !succeeded && call.svc.service_options().and_then(|o| o.optional).unwrap_or(false) {
      tracing::info!("Optional service '{}' did not send successfully; continuing.", name);
    }
    service_result(call, total, attempts, started.elapsed())
  };

  // Plugins can only track targets across attempts when retries are on.
  let run = async move { if retry > 0 { DELIVERY.scope(DeliveryState::default(), run).await } else { run.await } };
  crate::storage::with_store_slot(store_slot(call, opts), run).await
}

/// Where this service's persistent store lives: its URL ID's namespace, in
/// memory only with `?store=no`.
fn store_slot(call: &Call<'_>, opts: &DispatchOptions) -> crate::storage::StoreSlot {
  let disabled = call.svc.service_options().and_then(|o| o.store) == Some(false);
  crate::storage::StoreSlot {
    registry: opts.stores.clone(),
    namespace: crate::notify::options::url_id(call.svc, &opts.storage_salt, opts.storage_idlen),
    path: opts.storage_path.clone(),
    mode: if disabled { crate::types::StorageMode::Memory } else { opts.storage_mode.clone() },
  }
}

/// Why this service cannot be sent the attachments it was given, if so.
fn attachment_problem(svc: &dyn Notify, ctx: &NotifyContext, opts: &DispatchOptions) -> Option<String> {
  let name = svc.service_name();
  let has_attachments = !ctx.attachments.is_empty() || !opts.attachment_errors.is_empty();
  if !svc.attachment_support() {
    // Without a body there is nothing left to send.
    return (has_attachments && ctx.body.is_empty()).then(|| format!("{name} does not support attachments; service skipped"));
  }
  // Sizes were checked as the attachments were loaded (Apprise::dispatch_with)
  opts.attachment_errors.first().map(|source| format!("Could not access {name} attachment {source}."))
}

/// Log a failed `send()` without leaking secrets from URLs in the error.
fn log_send_error(name: &str, e: &NotifyError, secure: bool) {
  let mask = |s: String| if secure { cwe312_text(&s) } else { s };
  match e {
    NotifyError::ServiceError { status, body } => {
      tracing::warn!("Failed to send {} notification: error={}.", name, status);
      if !body.is_empty() {
        let body: String = body.chars().take(512).collect();
        tracing::debug!("Response details: {}", mask(body));
      }
    }
    NotifyError::Http(err) => {
      tracing::warn!("A connection error occurred sending {} notification.", name);
      tracing::debug!("Socket exception: {}", mask(crate::error::loggable_http_error(err)));
    }
    other => tracing::warn!("Failed to send {} notification: {}", name, mask(other.to_string())),
  }
}

/// Poll every future concurrently on the current task and return their
/// outputs in order.
async fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
  let mut futures: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
  let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
  std::future::poll_fn(|cx| {
    let mut pending = false;
    for (fut, out) in futures.iter_mut().zip(outputs.iter_mut()) {
      if out.is_none() {
        match fut.as_mut().poll(cx) {
          Poll::Ready(v) => *out = Some(v),
          Poll::Pending => pending = true,
        }
      }
    }
    if pending { Poll::Pending } else { Poll::Ready(()) }
  })
  .await;
  outputs.into_iter().map(|o| o.expect("every future completed")).collect()
}

// ── Delivery tracking for plugins ────────────────────────────────────────────

/// What a service has delivered so far during the current notification
/// (upstream `_delivery_tracker` / `_delivery_memo`). Only present while
/// retries are enabled.
#[derive(Default)]
struct DeliveryState {
  piece: Cell<usize>,
  delivered: RefCell<HashSet<(Option<usize>, String)>>,
  memo: RefCell<HashMap<(Option<usize>, String), serde_json::Value>>,
}

tokio::task_local! {
  static DELIVERY: DeliveryState;
}

fn set_piece(index: usize) {
  let _ = DELIVERY.try_with(|d| d.piece.set(index));
}

fn slot(state: &DeliveryState, key: &str, per_message: bool) -> (Option<usize>, String) {
  ((!per_message).then(|| state.piece.get()), key.to_string())
}

/// Whether `key` (a stable target identifier such as a phone number, room
/// or address) already accepted the current message piece during an
/// earlier attempt. Always `false` when the service has no retries.
///
/// ```ignore
/// for target in &self.targets {
///   if crate::dispatch::is_delivered(target, false) { continue; }
///   if post(target).await? { crate::dispatch::mark_delivered(target, false); }
/// }
/// ```
///
/// A long message is sent in pieces and each piece is tracked on its own;
/// set `per_message` for something sent once per notification (an icon
/// upload, say).
pub fn is_delivered(key: &str, per_message: bool) -> bool {
  DELIVERY.try_with(|d| d.delivered.borrow().contains(&slot(d, key, per_message))).unwrap_or(false)
}

/// Record that `key` accepted the current message piece, so a retry can
/// skip it. See [`is_delivered`].
pub fn mark_delivered(key: &str, per_message: bool) {
  let _ = DELIVERY.try_with(|d| {
    let s = slot(d, key, per_message);
    d.delivered.borrow_mut().insert(s);
  });
}

/// Keep `value` for later attempts of this notification (an uploaded
/// attachment ID, say). Nothing is kept when the service has no retries.
pub fn remember(key: &str, value: serde_json::Value, per_message: bool) {
  let _ = DELIVERY.try_with(|d| {
    let s = slot(d, key, per_message);
    d.memo.borrow_mut().insert(s, value);
  });
}

/// What [`remember`] stored for `key` during an earlier attempt.
pub fn recall(key: &str, per_message: bool) -> Option<serde_json::Value> {
  DELIVERY.try_with(|d| d.memo.borrow().get(&slot(d, key, per_message)).cloned()).ok().flatten()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Apprise;
  use crate::notify::{ConfiguredService, OverflowMode, ServiceDetails, ServiceOptions};
  use crate::utils::parse::ParsedUrl;
  use async_trait::async_trait;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex};

  /// What a scripted send does.
  #[derive(Clone, Copy, Debug)]
  enum Step {
    Ok,
    Fail,
    Err,
    /// Fail with an error whose text holds a URL with a token.
    SecretErr,
    /// Fail with an HTTP status and a response body.
    Status,
    /// Sleep this many milliseconds, then succeed.
    Slow(u64),
  }

  /// A plugin whose sends follow a script, recording every call.
  struct Scripted {
    name: &'static str,
    tags: Vec<String>,
    script: Vec<Step>,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
    http: Arc<Mutex<Vec<HttpOptions>>>,
    body_max: usize,
    attach: bool,
    attachments: Arc<Mutex<Vec<String>>>,
  }

  #[async_trait]
  impl Notify for Scripted {
    fn attachment_support(&self) -> bool {
      self.attach
    }
    fn schemas(&self) -> &[&str] {
      &["test"]
    }
    fn service_name(&self) -> &str {
      self.name
    }
    fn details(&self) -> ServiceDetails {
      ServiceDetails { service_name: "Test", service_url: None, setup_url: None, protocols: vec!["test"], description: "", attachment_support: false }
    }
    fn tags(&self) -> Vec<String> {
      self.tags.clone()
    }
    fn body_maxlen(&self) -> usize {
      self.body_max
    }
    fn overflow_mode(&self) -> OverflowMode {
      OverflowMode::Split
    }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
      let n = self.calls.fetch_add(1, Ordering::SeqCst);
      self.bodies.lock().unwrap().push(ctx.body.clone());
      self.http.lock().unwrap().push(crate::notify::current_http_options());
      self.attachments.lock().unwrap().extend(ctx.attachments.iter().map(|a| a.name.clone()));
      match self.script.get(n).or(self.script.last()).copied().unwrap_or(Step::Ok) {
        Step::Ok => Ok(true),
        Step::Fail => Ok(false),
        Step::Err => Err(NotifyError::Other("boom".into())),
        Step::SecretErr => Err(NotifyError::Other("request to https://api.example.com/botSECRETTOKEN0123456789/send failed".into())),
        Step::Status => Err(NotifyError::ServiceError { status: 503, body: "upstream says no to https://h.example/k/SECRETTOKEN0123456789".into() }),
        Step::Slow(ms) => {
          tokio::time::sleep(Duration::from_millis(ms)).await;
          Ok(true)
        }
      }
    }
  }

  struct Handle {
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
    http: Arc<Mutex<Vec<HttpOptions>>>,
    attachments: Arc<Mutex<Vec<String>>>,
  }

  impl Handle {
    fn calls(&self) -> usize {
      self.calls.load(Ordering::SeqCst)
    }
  }

  /// A scripted service configured from `url`'s common options and tags.
  fn scripted(name: &'static str, url: &str, script: &[Step]) -> (Box<dyn Notify>, Handle) {
    let parsed = ParsedUrl::parse(url).unwrap();
    let options = ServiceOptions::from_url(&parsed).unwrap();
    let handle = Handle { calls: Arc::default(), bodies: Arc::default(), http: Arc::default(), attachments: Arc::default() };
    let plugin = Scripted {
      name,
      tags: parsed.tags(),
      script: script.to_vec(),
      calls: handle.calls.clone(),
      bodies: handle.bodies.clone(),
      http: handle.http.clone(),
      body_max: parsed.get("max").and_then(|m| m.parse().ok()).unwrap_or(32768),
      attach: parsed.get("attach").is_some(),
      attachments: handle.attachments.clone(),
    };
    (Box::new(ConfiguredService::new(Box::new(plugin), options).with_url(url)), handle)
  }

  fn apprise(services: Vec<Box<dyn Notify>>) -> Apprise {
    let mut a = Apprise::new();
    for svc in services {
      a.entries.push(crate::Entry::Service(svc.into()));
    }
    a
  }

  fn ctx() -> NotifyContext {
    NotifyContext { body: "body".into(), title: "title".into(), ..Default::default() }
  }

  #[tokio::test(start_paused = true)]
  async fn test_retry_until_success() {
    let (svc, h) = scripted("A", "test://host/?retry=2&wait=1", &[Step::Fail, Step::Err, Step::Ok]);
    let a = apprise(vec![svc]);
    let started = Instant::now();
    let r = a.notify(&ctx()).await;
    assert_eq!(r.status, ResultStatus::Success);
    assert_eq!(h.calls(), 3);
    let s = &r.results[0];
    assert_eq!(s.max_attempts, 3);
    assert_eq!(s.attempts.iter().map(|a| a.status).collect::<Vec<_>>(), vec![ResultStatus::Failure, ResultStatus::Failure, ResultStatus::Success]);
    // Two waits of one second each.
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(started.elapsed() < Duration::from_secs(3));
  }

  #[tokio::test(start_paused = true)]
  async fn test_no_retry_by_default_and_exhausted_retries_fail() {
    let (svc, h) = scripted("A", "test://host", &[Step::Fail]);
    let r = apprise(vec![svc]).notify(&ctx()).await;
    assert_eq!((r.status, h.calls(), r.results[0].max_attempts), (ResultStatus::Failure, 1, 1));

    let (svc, h) = scripted("A", "test://host/?retry=2&wait=0", &[Step::Fail]);
    let started = Instant::now();
    let r = apprise(vec![svc]).notify(&ctx()).await;
    assert_eq!((r.status, h.calls(), r.results[0].attempts.len()), (ResultStatus::Failure, 3, 3));
    // wait=0 never sleeps.
    assert_eq!(started.elapsed(), Duration::ZERO);
  }

  #[tokio::test(start_paused = true)]
  async fn test_asset_default_retry_and_wait() {
    let (svc, h) = scripted("A", "test://host", &[Step::Fail, Step::Ok]);
    let mut a = apprise(vec![svc]);
    a.asset.default_service_retry = 1;
    let started = Instant::now();
    assert!(a.notify(&ctx()).await.is_success());
    assert_eq!(h.calls(), 2);
    // The asset's default wait is half a second.
    assert_eq!(started.elapsed(), Duration::from_millis(500));
  }

  #[tokio::test(start_paused = true)]
  async fn test_retry_override_from_tag_suffix_is_bounded() {
    let (svc, h) = scripted("A", "test://host/?tag=alerts&wait=0", &[Step::Fail]);
    let mut a = apprise(vec![svc]);
    a.set_tag_strings(&["alerts:3".into()]);
    let r = a.notify(&ctx()).await;
    assert_eq!((h.calls(), r.results[0].max_attempts), (4, 4));

    let (svc, h) = scripted("A", "test://host/?tag=alerts&wait=0", &[Step::Fail]);
    let mut a = apprise(vec![svc]);
    a.set_tag_strings(&["alerts:99".into()]);
    a.notify(&ctx()).await;
    assert_eq!(h.calls(), MAX_SERVICE_RETRY as usize + 1);
  }

  #[tokio::test(start_paused = true)]
  async fn test_retry_skips_pieces_already_accepted() {
    // Three pieces; the second fails once.
    let (svc, h) = scripted("A", "test://host/?retry=1&wait=0&max=10", &[Step::Ok, Step::Fail, Step::Ok, Step::Ok]);
    let a = apprise(vec![svc]);
    let c = NotifyContext { body: "aaaaaaaaa bbbbbbbbb ccccccccc".into(), title: String::new(), ..Default::default() };
    let r = a.notify(&c).await;
    assert!(r.is_success());
    let bodies = h.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4, "{bodies:?}");
    // The retry re-sent only the piece that failed.
    assert_eq!(bodies[3], bodies[1]);
    assert_ne!(bodies[3], bodies[0]);
  }

  #[tokio::test(start_paused = true)]
  async fn test_services_run_concurrently() {
    let (a1, _) = scripted("A", "test://a", &[Step::Slow(1000)]);
    let (a2, _) = scripted("B", "test://b", &[Step::Slow(1000)]);
    let a = apprise(vec![a1, a2]);
    let started = Instant::now();
    assert!(a.notify(&ctx()).await.is_success());
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    let started = Instant::now();
    assert!(a.notify_sequential(&ctx()).await.is_success());
    assert_eq!(started.elapsed(), Duration::from_secs(2));
  }

  #[tokio::test(start_paused = true)]
  async fn test_exclusive_priority_dispatch() {
    let (p1, h1) = scripted("P1", "test://a/?tag=1:alerts", &[Step::Ok]);
    let (p2, h2) = scripted("P2", "test://b/?tag=2:alerts", &[Step::Ok]);
    let (p2b, h2b) = scripted("P2b", "test://c/?tag=2:alerts", &[Step::Fail]);
    let mut a = apprise(vec![p1, p2, p2b]);
    a.set_tag_strings(&["2:alerts".into()]);
    let r = a.notify(&ctx()).await;
    // Only the priority-2 entries, as one flat batch.
    assert_eq!((h1.calls(), h2.calls(), h2b.calls()), (0, 1, 1));
    assert_eq!(r.status, ResultStatus::Partial);
  }

  #[tokio::test(start_paused = true)]
  async fn test_escalation_stops_after_success_and_escalates_on_failure() {
    let (p0, h0) = scripted("P0", "test://a/?tag=alerts", &[Step::Ok]);
    let (p1, h1) = scripted("P1", "test://b/?tag=1:alerts", &[Step::Ok]);
    let mut a = apprise(vec![p1, p0]);
    a.set_tag_strings(&["alerts".into()]);
    let r = a.notify(&ctx()).await;
    assert_eq!((h0.calls(), h1.calls(), r.total), (1, 0, 1));
    assert!(r.is_success());

    let (p0, h0) = scripted("P0", "test://a/?tag=0:alerts", &[Step::Fail]);
    let (p1, h1) = scripted("P1", "test://b/?tag=1:alerts", &[Step::Ok]);
    let (p2, h2) = scripted("P2", "test://c/?tag=2:alerts", &[Step::Ok]);
    let mut a = apprise(vec![p2, p1, p0]);
    a.set_tag_strings(&["alerts".into()]);
    let r = a.notify(&ctx()).await;
    assert_eq!((h0.calls(), h1.calls(), h2.calls()), (1, 1, 0));
    // The chain succeeded at its second group, but a failure was recorded.
    assert_eq!(r.status, ResultStatus::Success);
    assert_eq!(r.results.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["P0", "P1"]);
  }

  #[tokio::test(start_paused = true)]
  async fn test_all_priority_groups_exhausted_fails() {
    let (p0, _) = scripted("P0", "test://a/?tag=0:alerts", &[Step::Fail]);
    let (p1, _) = scripted("P1", "test://b/?tag=1:alerts", &[Step::Fail]);
    let mut a = apprise(vec![p0, p1]);
    a.set_tag_strings(&["alerts".into()]);
    let r = a.notify(&ctx()).await;
    assert_eq!((r.status, r.total), (ResultStatus::Failure, 2));
  }

  #[tokio::test(start_paused = true)]
  async fn test_independent_chains() {
    // Chain "a" fails at priority 0 and escalates; chain "b" succeeds at once.
    let (a0, ha0) = scripted("A0", "test://a0/?tag=a", &[Step::Fail]);
    let (a1, ha1) = scripted("A1", "test://a1/?tag=1:a", &[Step::Ok]);
    let (b0, hb0) = scripted("B0", "test://b0/?tag=b", &[Step::Ok]);
    let (b1, hb1) = scripted("B1", "test://b1/?tag=1:b", &[Step::Ok]);
    let mut a = apprise(vec![a0, a1, b0, b1]);
    a.set_tag_strings(&["a".into(), "b".into()]);
    let r = a.notify(&ctx()).await;
    assert_eq!((ha0.calls(), ha1.calls(), hb0.calls(), hb1.calls()), (1, 1, 1, 0));
    assert!(r.is_success());

    // Every chain must succeed.
    let (a0, _) = scripted("A0", "test://a0/?tag=a", &[Step::Fail]);
    let (b0, _) = scripted("B0", "test://b0/?tag=b", &[Step::Ok]);
    let mut a = apprise(vec![a0, b0]);
    a.set_tag_strings(&["a".into(), "b".into()]);
    assert_eq!(a.notify(&ctx()).await.status, ResultStatus::Partial);
  }

  #[tokio::test(start_paused = true)]
  async fn test_and_group_shares_one_chain() {
    let (x0, h0) = scripted("X0", "test://x0/?tag=a,b", &[Step::Ok]);
    let (x1, h1) = scripted("X1", "test://x1/?tag=1:a,1:b", &[Step::Ok]);
    let mut a = apprise(vec![x0, x1]);
    a.set_tag_strings(&["a,b".into()]);
    a.notify(&ctx()).await;
    assert_eq!((h0.calls(), h1.calls()), (1, 0));
  }

  #[tokio::test(start_paused = true)]
  async fn test_abort_on_chain_failure() {
    let (a0, _) = scripted("A0", "test://a0/?tag=a", &[Step::Fail]);
    let (b0, _) = scripted("B0", "test://b0/?tag=b", &[Step::Fail]);
    let (b1, hb1) = scripted("B1", "test://b1/?tag=1:b", &[Step::Ok]);
    let mut a = apprise(vec![a0, b0, b1]);
    a.set_tag_strings(&["a".into(), "b".into()]);
    a.asset.abort_on_chain_failure = true;
    let r = a.notify(&ctx()).await;
    assert_eq!(hb1.calls(), 0);
    assert_eq!(r.status, ResultStatus::Failure);

    a.asset.abort_on_chain_failure = false;
    a.notify(&ctx()).await;
    assert_eq!(hb1.calls(), 1);
  }

  #[tokio::test(start_paused = true)]
  async fn test_optional_services() {
    let (opt, _) = scripted("Opt", "test://a/?optional=yes", &[Step::Fail]);
    let (req, _) = scripted("Req", "test://b", &[Step::Ok]);
    let r = apprise(vec![opt, req]).notify(&ctx()).await;
    assert_eq!(r.status, ResultStatus::Success);
    assert!(r.results[0].optional && r.results[0].is_success());

    // All optional and all failing still succeeds.
    let (opt, _) = scripted("Opt", "test://a/?optional=yes", &[Step::Err]);
    assert!(apprise(vec![opt]).notify(&ctx()).await.is_success());

    // A required failure still fails the call; an absorbed optional failure
    // is not a real delivery.
    let (opt, _) = scripted("Opt", "test://a/?optional=yes", &[Step::Fail]);
    let (req, _) = scripted("Req", "test://b", &[Step::Fail]);
    assert_eq!(apprise(vec![opt, req]).notify(&ctx()).await.status, ResultStatus::Failure);
  }

  #[tokio::test(start_paused = true)]
  async fn test_service_timeout() {
    let (slow, _) = scripted("Slow", "test://a", &[Step::Slow(5000)]);
    let (fast, _) = scripted("Fast", "test://b", &[Step::Ok]);
    let mut a = apprise(vec![slow, fast]);
    a.asset.service_timeout = 1.0;
    let started = Instant::now();
    let r = a.notify(&ctx()).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(r.results[0].status, ResultStatus::Timeout);
    assert_eq!(r.results[0].attempts[0].status, ResultStatus::Timeout);
    assert_eq!(r.status, ResultStatus::Partial);
    assert_eq!(r.timed_out(), 1);

    // Only a timeout: TIMEOUT. An optional timeout is absorbed.
    let (slow, _) = scripted("Slow", "test://a", &[Step::Slow(5000)]);
    let mut a = apprise(vec![slow]);
    a.asset.service_timeout = 1.0;
    assert_eq!(a.notify(&ctx()).await.status, ResultStatus::Timeout);
    let (slow, _) = scripted("Slow", "test://a/?optional=yes", &[Step::Slow(5000)]);
    let mut a = apprise(vec![slow]);
    a.asset.service_timeout = 1.0;
    assert_eq!(a.notify(&ctx()).await.status, ResultStatus::Success);
  }

  #[tokio::test(start_paused = true)]
  async fn test_call_timeout_wins_when_stricter() {
    let (slow, _) = scripted("Slow", "test://a", &[Step::Slow(5000)]);
    let mut a = apprise(vec![slow]);
    a.set_timeout(0.5).unwrap();
    let started = Instant::now();
    assert_eq!(a.notify(&ctx()).await.status, ResultStatus::Timeout);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(a.set_timeout(-1.0).is_err());
    assert!(a.set_timeout(f64::INFINITY).is_err());
    assert!(a.set_timeout(f64::NAN).is_err());
    assert!(a.set_timeout(0.0).is_ok());
  }

  #[tokio::test(start_paused = true)]
  async fn test_retry_wait_never_passes_deadline() {
    // The deadline arrives during the wait, so the next attempt is skipped.
    let (svc, h) = scripted("A", "test://a/?retry=3&wait=10", &[Step::Fail]);
    let mut a = apprise(vec![svc]);
    a.asset.service_timeout = 2.0;
    let started = Instant::now();
    let r = a.notify(&ctx()).await;
    assert_eq!(h.calls(), 1);
    assert!(started.elapsed() <= Duration::from_secs(2) + ABANDON_GRACE);
    let s = &r.results[0];
    assert_eq!(s.attempts.iter().map(|a| a.status).collect::<Vec<_>>(), vec![ResultStatus::Failure, ResultStatus::Timeout]);
    // A confirmed failure is reported rather than the later timeout.
    assert_eq!(s.status, ResultStatus::Failure);
  }

  #[tokio::test(start_paused = true)]
  async fn test_no_match_and_bad_content() {
    let r = Apprise::new().notify(&ctx()).await;
    assert_eq!(r.status, ResultStatus::NoMatch);

    let (svc, h) = scripted("A", "test://a/?tag=x", &[Step::Ok]);
    let mut a = apprise(vec![svc]);
    a.set_tag_strings(&["y".into()]);
    assert_eq!(a.notify(&ctx()).await.status, ResultStatus::NoMatch);
    a.set_tag_filter(TagFilter::Untagged);
    assert_eq!(a.notify(&ctx()).await.status, ResultStatus::NoMatch);
    assert_eq!(h.calls(), 0);

    a.set_tag_filter(TagFilter::All);
    let empty = NotifyContext::default();
    assert_eq!(a.notify(&empty).await.status, ResultStatus::Failure);
    assert_eq!(h.calls(), 0);
  }

  #[tokio::test(start_paused = true)]
  async fn test_http_options_reach_the_plugin() {
    let (svc, h) = scripted("A", "test://a/?cto=2&rto=7.5&redirect=no&verify=no", &[Step::Ok]);
    let (plain, hp) = scripted("B", "test://b", &[Step::Ok]);
    let mut a = apprise(vec![svc, plain]);
    a.notify(&ctx()).await;
    let seen = h.http.lock().unwrap()[0].clone();
    assert_eq!(seen, HttpOptions { connect_timeout: Some(2.0), read_timeout: Some(7.5), redirect: Some(false), verify: Some(false) });
    let seen = hp.http.lock().unwrap()[0].clone();
    assert_eq!(seen, HttpOptions { connect_timeout: None, read_timeout: None, redirect: Some(true), verify: None });
    // The asset decides redirects for URLs that do not.
    a.asset.http_redirects = false;
    a.notify(&ctx()).await;
    assert_eq!(hp.http.lock().unwrap()[1].redirect, Some(false));
  }

  #[tokio::test(start_paused = true)]
  async fn test_result_url_is_masked() {
    let (svc, _) = scripted("A", "test://user:supersecret@host/?tag=b", &[Step::Ok]);
    let r = apprise(vec![svc]).notify(&ctx()).await;
    assert!(!r.results[0].url.contains("supersecret"), "{}", r.results[0].url);
    assert_eq!(r.results[0].tags, vec!["b"]);
  }

  /// A plugin with several targets that uses the delivery tracker.
  struct MultiTarget {
    sent: Arc<Mutex<Vec<String>>>,
    attempt: AtomicUsize,
  }

  #[async_trait]
  impl Notify for MultiTarget {
    fn schemas(&self) -> &[&str] {
      &["multi"]
    }
    fn service_name(&self) -> &str {
      "Multi"
    }
    fn details(&self) -> ServiceDetails {
      ServiceDetails { service_name: "Multi", service_url: None, setup_url: None, protocols: vec!["multi"], description: "", attachment_support: false }
    }
    async fn send(&self, _ctx: &NotifyContext) -> Result<bool, NotifyError> {
      let attempt = self.attempt.fetch_add(1, Ordering::SeqCst);
      let mut ok = true;
      for target in ["t1", "t2"] {
        if crate::delivery::is_delivered(target, false) {
          continue;
        }
        // t2 fails on the first attempt only.
        if target == "t2" && attempt == 0 {
          ok = false;
          continue;
        }
        self.sent.lock().unwrap().push(target.to_string());
        crate::delivery::mark_delivered(target, false);
      }
      if attempt == 0 {
        crate::delivery::remember("upload", serde_json::json!("id-1"), true);
      } else {
        assert_eq!(crate::delivery::recall("upload", true), Some(serde_json::json!("id-1")));
      }
      Ok(ok)
    }
  }

  #[tokio::test(start_paused = true)]
  async fn test_delivery_tracker_skips_accepted_targets() {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let plugin = MultiTarget { sent: sent.clone(), attempt: AtomicUsize::new(0) };
    let options = ServiceOptions::from_url(&ParsedUrl::parse("multi://x/?retry=1&wait=0").unwrap()).unwrap();
    let a = apprise(vec![Box::new(ConfiguredService::new(Box::new(plugin), options))]);
    assert!(a.notify(&ctx()).await.is_success());
    assert_eq!(*sent.lock().unwrap(), vec!["t1", "t2"]);
    // Without retries nothing is tracked.
    assert!(!crate::delivery::is_delivered("t1", false));
    assert_eq!(crate::delivery::recall("upload", true), None);
  }

  /// A plugin that counts its sends in its persistent store.
  struct Counting;

  #[async_trait]
  impl Notify for Counting {
    fn schemas(&self) -> &[&str] {
      &["count"]
    }
    fn service_name(&self) -> &str {
      "Counting"
    }
    fn details(&self) -> ServiceDetails {
      ServiceDetails { service_name: "Counting", service_url: None, setup_url: None, protocols: vec!["count"], description: "", attachment_support: false }
    }
    async fn send(&self, _ctx: &NotifyContext) -> Result<bool, NotifyError> {
      let stored = crate::storage::with_service_store(|s| {
        let n = s.get("sent").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        s.set("sent", serde_json::json!(n), None, true);
        n
      });
      Ok(stored.is_some())
    }
  }

  fn counting(url: &str) -> Box<dyn Notify> {
    let options = ServiceOptions::from_url(&ParsedUrl::parse(url).unwrap()).unwrap();
    Box::new(ConfiguredService::new(Box::new(Counting), options).with_url(url))
  }

  #[tokio::test]
  async fn test_plugins_reach_their_persistent_store() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = apprise(vec![counting("count://host/a"), counting("count://host/b?store=no")]);
    a.asset.storage_path = Some(tmp.path().to_path_buf());
    assert!(a.notify(&ctx()).await.is_success());
    assert!(a.notify(&ctx()).await.is_success());
    a.flush_stores();
    let id = a.url_id(a.services().next().unwrap()).unwrap();
    assert_eq!(id.len(), 8);
    let mut store = crate::storage::PersistentStore::new(Some(tmp.path()), &id, crate::types::StorageMode::Auto).unwrap();
    assert_eq!(store.get("sent"), Some(serde_json::json!(2)));
    // store=no keeps the second service in memory only.
    let other = a.url_id(a.services().nth(1).unwrap()).unwrap();
    assert!(!tmp.path().join(other).exists());
  }

  /// A plugin making `requests` requests per send, throttled.
  struct Throttled {
    interval: f64,
    requests: usize,
    wait: Option<Duration>,
  }

  #[async_trait]
  impl Notify for Throttled {
    fn schemas(&self) -> &[&str] {
      &["throttled"]
    }
    fn service_name(&self) -> &str {
      "Throttled"
    }
    fn details(&self) -> ServiceDetails {
      ServiceDetails { service_name: "Throttled", service_url: None, setup_url: None, protocols: vec!["throttled"], description: "", attachment_support: false }
    }
    fn request_rate_per_sec(&self) -> f64 {
      self.interval
    }
    async fn send(&self, _ctx: &NotifyContext) -> Result<bool, NotifyError> {
      for _ in 0..self.requests {
        crate::notify::throttle_for(self.wait).await;
      }
      Ok(true)
    }
  }

  fn throttled(interval: f64, requests: usize, wait: Option<Duration>) -> Apprise {
    let options = ServiceOptions::from_url(&ParsedUrl::parse("throttled://x").unwrap()).unwrap();
    apprise(vec![Box::new(ConfiguredService::new(Box::new(Throttled { interval, requests, wait }), options))])
  }

  #[tokio::test(start_paused = true)]
  async fn test_throttle_spaces_requests_per_instance() {
    let a = throttled(2.0, 3, None);
    crate::notify::with_throttling_in_tests(async {
      // The first request never waits; the next two wait two seconds each.
      let started = Instant::now();
      assert!(a.notify(&ctx()).await.is_success());
      assert_eq!(started.elapsed(), Duration::from_secs(4));
      // The clock carries over to the next notification.
      let started = Instant::now();
      assert!(a.notify(&ctx()).await.is_success());
      assert_eq!(started.elapsed(), Duration::from_secs(6));
      // After a long pause there is nothing left to wait for.
      tokio::time::sleep(Duration::from_secs(10)).await;
      let started = Instant::now();
      a.notify(&ctx()).await;
      assert_eq!(started.elapsed(), Duration::from_secs(4));
    })
    .await;
  }

  #[tokio::test(start_paused = true)]
  async fn test_throttle_forced_wait_and_disabled_interval() {
    let a = throttled(0.0, 3, None);
    crate::notify::with_throttling_in_tests(async {
      let started = Instant::now();
      a.notify(&ctx()).await;
      assert_eq!(started.elapsed(), Duration::ZERO);
    })
    .await;
    // A forced wait applies even with throttling disabled, after the first
    // request.
    let a = throttled(0.0, 3, Some(Duration::from_secs(3)));
    crate::notify::with_throttling_in_tests(async {
      let started = Instant::now();
      a.notify(&ctx()).await;
      assert_eq!(started.elapsed(), Duration::from_secs(6));
    })
    .await;
  }

  #[tokio::test(start_paused = true)]
  async fn test_throttle_is_a_no_op_in_tests_and_outside_dispatch() {
    let a = throttled(5.5, 3, None);
    let started = Instant::now();
    a.notify(&ctx()).await;
    assert_eq!(started.elapsed(), Duration::ZERO);
    crate::notify::with_throttling_in_tests(async {
      let started = Instant::now();
      Throttled { interval: 5.5, requests: 3, wait: None }.send(&ctx()).await.unwrap();
      assert_eq!(started.elapsed(), Duration::ZERO);
    })
    .await;
  }

  #[tokio::test(start_paused = true)]
  async fn test_attachments_reach_plugins_that_take_them() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("report.txt");
    std::fs::write(&file, b"data").unwrap();
    let (with, hw) = scripted("With", "test://a/?attach=yes", &[Step::Ok]);
    let (without, hwo) = scripted("Without", "test://b", &[Step::Ok]);
    let a = apprise(vec![with, without]);
    let r = a.notify_with_attachments(&ctx(), &[file.to_string_lossy().into_owned()]).await;
    assert!(r.is_success());
    assert_eq!(*hw.attachments.lock().unwrap(), vec!["report.txt"]);
    assert_eq!(hwo.calls(), 1);
  }

  #[tokio::test(start_paused = true)]
  async fn test_unreadable_attachment_fails_only_attachment_services() {
    let (with, hw) = scripted("With", "test://a/?attach=yes", &[Step::Ok]);
    let (without, hwo) = scripted("Without", "test://b", &[Step::Ok]);
    let a = apprise(vec![with, without]);
    let r = a.notify_with_attachments(&ctx(), &["/nonexistent/file".into()]).await;
    assert_eq!(r.status, ResultStatus::Partial);
    assert_eq!((hw.calls(), hwo.calls()), (0, 1));
    assert_eq!(r.results[0].status, ResultStatus::Failure);
  }

  #[tokio::test(start_paused = true)]
  async fn test_unusable_attachment_source_fails_the_call() {
    let (svc, h) = scripted("With", "test://a/?attach=yes", &[Step::Ok]);
    let a = apprise(vec![svc]);
    let r = a.notify_with_attachments(&ctx(), &["foo://x".into()]).await;
    assert_eq!((r.status, r.total, h.calls()), (ResultStatus::Failure, 0, 0));
    // With nothing loaded at all, there is nothing to match.
    assert_eq!(Apprise::new().notify_with_attachments(&ctx(), &["foo://x".into()]).await.status, ResultStatus::NoMatch);
  }

  #[tokio::test(start_paused = true)]
  async fn test_attachment_only_message_skips_services_without_support() {
    let (with, hw) = scripted("With", "test://a/?attach=yes", &[Step::Ok]);
    let (without, hwo) = scripted("Without", "test://b", &[Step::Ok]);
    let a = apprise(vec![with, without]);
    let c = NotifyContext { attachments: vec![crate::attachment::memory::from_text("x")], ..Default::default() };
    let r = a.notify(&c).await;
    assert_eq!(r.status, ResultStatus::Partial);
    assert_eq!((hw.calls(), hwo.calls()), (1, 0));
  }

  /// Collects formatted log lines.
  async fn logs_of(a: &Apprise) -> String {
    crate::test_logs::capture(tracing::Level::DEBUG, a.notify(&ctx())).await.1
  }

  #[tokio::test]
  async fn test_send_errors_are_logged_without_secrets() {
    let (svc, _) = scripted("Test", "test://a", &[Step::SecretErr]);
    let logs = logs_of(&apprise(vec![svc])).await;
    assert!(logs.contains("WARN") && logs.contains("Failed to send Test notification: request to https://api.example.com/"), "{logs}");
    assert!(!logs.contains("SECRETTOKEN0123456789"), "{logs}");

    let (svc, _) = scripted("Test", "test://a", &[Step::Status]);
    let logs = logs_of(&apprise(vec![svc])).await;
    assert!(logs.contains("Failed to send Test notification: error=503."), "{logs}");
    // The response body is kept to debug output, masked.
    assert!(logs.contains("Response details: upstream says no to https://h.example/"), "{logs}");
    assert!(!logs.contains("SECRETTOKEN0123456789"), "{logs}");

    let (svc, _) = scripted("Test", "test://a", &[Step::Fail]);
    assert!(logs_of(&apprise(vec![svc])).await.contains("Failed to send Test notification."));

    // With secure logging off the text is left alone.
    let (svc, _) = scripted("Test", "test://a", &[Step::SecretErr]);
    let mut a = apprise(vec![svc]);
    a.asset.secure_logging = false;
    assert!(logs_of(&a).await.contains("SECRETTOKEN0123456789"));
  }
}
