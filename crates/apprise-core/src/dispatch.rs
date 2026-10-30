//! Sends prepared notifications to the selected services.

use crate::SendResult;
use crate::notify::{Notify, NotifyContext};
use crate::prepare::prepare_contexts;

/// Send `ctx` to each service in `indices`.
pub(crate) async fn notify(services: &[Box<dyn Notify>], indices: &[usize], ctx: &NotifyContext) -> SendResult {
  let total = indices.len();
  let mut succeeded = 0usize;
  let mut failed = 0usize;
  for &i in indices {
    let svc = &services[i];
    let mut ok = true;
    let prepared = prepare_contexts(svc.as_ref(), ctx).unwrap_or_else(|_| {
      ok = false;
      Vec::new()
    });
    for svc_ctx in &prepared {
      match svc.send(svc_ctx).await {
        Ok(true) => {}
        Ok(false) | Err(_) => ok = false,
      }
    }
    if ok {
      succeeded += 1;
    } else {
      failed += 1;
    }
  }
  SendResult { total, succeeded, failed }
}

/// Send `ctx` to each service in `indices`, one at a time, sleeping between
/// sends to respect each service's request rate.
pub(crate) async fn notify_sequential(services: &[Box<dyn Notify>], indices: &[usize], ctx: &NotifyContext) -> SendResult {
  let total = indices.len();
  let mut succeeded = 0usize;
  let mut failed = 0usize;
  let mut last_send = std::time::Instant::now();

  for &i in indices {
    let svc = &services[i];
    let rate = svc.request_rate_per_sec();
    if rate > 0.0 {
      let min_interval = std::time::Duration::from_secs_f64(1.0 / rate);
      let elapsed = last_send.elapsed();
      if elapsed < min_interval {
        tokio::time::sleep(min_interval - elapsed).await;
      }
    }
    let mut svc_ok = true;
    let prepared = prepare_contexts(svc.as_ref(), ctx).unwrap_or_else(|_| {
      svc_ok = false;
      Vec::new()
    });
    for svc_ctx in &prepared {
      last_send = std::time::Instant::now();
      match svc.send(svc_ctx).await {
        Ok(true) => {}
        Ok(false) | Err(_) => svc_ok = false,
      }
    }
    if svc_ok {
      succeeded += 1;
    } else {
      failed += 1;
    }
  }
  SendResult { total, succeeded, failed }
}
