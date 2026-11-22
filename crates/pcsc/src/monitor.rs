//! The reader monitor: keeps the table of readers up to date from the client's smart card subsystem.
//!
//! One thread per attached device. It owns a private context of the backend and loops on `list_readers` and
//! `get_status_change` (each call blocks until the client has something to report, or `Options::reader_poll`
//! elapsed so that a reader that came or went is noticed); `detach` ends it with `cancel`.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use oxydesk_core::error::SmartcardError;
use oxydesk_core::smartcard::{code, RemoteSmartcard, ScContext};

use crate::service::{lock, Service};

/// Longest delay between two looks for a reader when there is none.
const NO_READER_POLL: Duration = Duration::from_secs(2);
/// Delay before asking again after a failure.
const RETRY: Duration = Duration::from_secs(1);
/// A `get_status_change` that returns this fast without news is not waiting: do not spin on it.
const TOO_FAST: Duration = Duration::from_millis(50);

#[derive(Default)]
struct State {
    stop: bool,
    context: Option<ScContext>,
    done: bool,
}

/// Control of one monitor thread.
#[derive(Default)]
pub(crate) struct Monitor {
    state: Mutex<State>,
    changed: Condvar,
}

impl Monitor {
    pub fn is_stopped(&self) -> bool {
        lock(&self.state).stop
    }

    /// Sleeps `d`, or until stopped. `true` when stopped.
    fn sleep(&self, d: Duration) -> bool {
        let g = lock(&self.state);
        let (g, _) = self
            .changed
            .wait_timeout_while(g, d, |s| !s.stop)
            .unwrap_or_else(|e| e.into_inner());
        g.stop
    }

    fn set_context(&self, context: Option<ScContext>) {
        lock(&self.state).context = context;
    }

    fn finish(&self) {
        lock(&self.state).done = true;
        self.changed.notify_all();
    }

    /// Stops the thread and waits (at most `max`) until it ended. The `cancel` is repeated because it may come
    /// before the thread blocks in `get_status_change`.
    pub fn stop_and_wait(&self, backend: &dyn RemoteSmartcard, max: Duration) {
        lock(&self.state).stop = true;
        self.changed.notify_all();
        let deadline = Instant::now() + max;
        loop {
            let context = lock(&self.state).context;
            if let Some(c) = context {
                let _ = backend.cancel(c);
            }
            let g = lock(&self.state);
            let (g, _) = self
                .changed
                .wait_timeout_while(g, Duration::from_millis(50), |s| !s.done)
                .unwrap_or_else(|e| e.into_inner());
            if g.done || Instant::now() >= deadline {
                return;
            }
        }
    }
}

/// The body of the monitor thread.
pub(crate) fn run(monitor: Arc<Monitor>, backend: Arc<dyn RemoteSmartcard>, service: Arc<Service>) {
    watch(&monitor, &*backend, &service);
    monitor.finish();
}

fn watch(monitor: &Monitor, backend: &dyn RemoteSmartcard, service: &Service) {
    let context = loop {
        if monitor.is_stopped() {
            return;
        }
        match backend.establish_context() {
            Ok(c) => break c,
            Err(e) => {
                tracing::warn!("smart card monitor: no context: {e}");
                if monitor.sleep(RETRY) {
                    return;
                }
            }
        }
    };
    monitor.set_context(Some(context));
    let wait_ms = u32::try_from(service.options.reader_poll.as_millis())
        .unwrap_or(u32::MAX - 1)
        .max(1);
    let no_reader_poll = service.options.reader_poll.min(NO_READER_POLL);
    loop {
        if monitor.is_stopped() {
            break;
        }
        let readers = match backend.list_readers(context) {
            Ok(r) => r,
            Err(SmartcardError::Code(code::E_NO_READERS_AVAILABLE)) => Vec::new(),
            Err(e) => {
                tracing::warn!("smart card monitor: listing the readers failed: {e}");
                service.update_table(monitor, |t| t.mark_unknown());
                if monitor.sleep(RETRY) {
                    break;
                }
                continue;
            }
        };
        service.update_table(monitor, |t| t.sync_readers(&readers));
        if readers.is_empty() {
            if monitor.sleep(no_reader_poll) {
                break;
            }
            continue;
        }
        let queries = service.table_queries();
        let started = Instant::now();
        match backend.get_status_change(context, wait_ms, &queries) {
            Ok(statuses) => {
                let changed = service.update_table(monitor, |t| t.apply_status(&statuses));
                if !changed && started.elapsed() < TOO_FAST && monitor.sleep(TOO_FAST * 4) {
                    break;
                }
            }
            Err(SmartcardError::Code(code::E_TIMEOUT | code::E_UNKNOWN_READER)) => {}
            Err(SmartcardError::Code(code::E_CANCELLED)) => {
                if monitor.sleep(TOO_FAST * 2) {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!("smart card monitor: status change failed: {e}");
                service.update_table(monitor, |t| t.mark_unknown());
                if monitor.sleep(RETRY) {
                    break;
                }
            }
        }
    }
    monitor.set_context(None);
    let _ = backend.release_context(context);
}
