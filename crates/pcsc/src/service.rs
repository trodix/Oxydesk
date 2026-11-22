//! What the connections share: the device that is attached, the table of readers, the connected clients.

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use oxydesk_core::smartcard::{code, ReaderQuery, RemoteSmartcard};

use crate::ids::IdMint;
use crate::monitor::{self, Monitor};
use crate::table::ReaderTable;
use crate::wire;

/// Most clients served at once (pcsc-lite allows 200 contexts).
pub(crate) const MAX_CONNECTIONS: usize = 256;
/// A client that does not read for this long is dropped (it would block the notifications to the others).
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A poisoned lock only means that another thread panicked: the data is still usable, and one client must
/// not be able to take the others down.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Backend {
    /// Changes each time the device is attached or detached: handles of an older epoch are dead.
    epoch: u64,
    smartcard: Option<Arc<dyn RemoteSmartcard>>,
    monitor: Option<Arc<Monitor>>,
}

struct TableState {
    table: ReaderTable,
    /// `table` as the clients read it, shared by all the answers.
    blob: Arc<Vec<u8>>,
}

#[derive(Default)]
struct Registry {
    next_conn: u64,
    conns: HashMap<u64, Arc<ConnShared>>,
    /// `hContext` -> connection that owns it.
    contexts: HashMap<u32, u64>,
    ids: Option<IdMint>,
}

/// Tuning of the service.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// How long the monitor waits for a card event before it looks again for readers that came or went
    /// (the client's subsystem does not tell about new readers).
    pub reader_poll: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            reader_poll: Duration::from_secs(5),
        }
    }
}

pub(crate) struct Service {
    pub options: Options,
    backend: Mutex<Backend>,
    table: Mutex<TableState>,
    registry: Mutex<Registry>,
    registry_changed: Condvar,
    stopping: AtomicBool,
}

struct Out {
    stream: UnixStream,
    /// The client sent `CMD_WAIT_READER_STATE_CHANGE` and was not notified since.
    waiting: bool,
}

/// What of a connection other threads need: to write to it, whatever the connection thread is doing.
pub(crate) struct ConnShared {
    pub id: u64,
    out: Mutex<Out>,
}

impl ConnShared {
    /// Sends one message (its parts are written one after the other, without another message in between).
    pub fn send(&self, parts: &[&[u8]]) -> io::Result<()> {
        let mut out = lock(&self.out);
        Self::write(&mut out, parts)
    }

    fn write(out: &mut Out, parts: &[&[u8]]) -> io::Result<()> {
        let result = parts.iter().try_for_each(|p| out.stream.write_all(p));
        if result.is_err() {
            // Wakes the thread of the connection, which cleans up.
            let _ = out.stream.shutdown(std::net::Shutdown::Both);
        }
        result
    }

    /// `CMD_WAIT_READER_STATE_CHANGE`: sends the table, and remembers that the client waits for the next change.
    /// The table is read with the connection locked so that a change cannot slip between the two.
    pub fn register_wait(&self, table: impl FnOnce() -> Arc<Vec<u8>>) -> io::Result<()> {
        let mut out = lock(&self.out);
        let blob = table();
        Self::write(&mut out, &[&blob])?;
        out.waiting = true;
        Ok(())
    }

    /// `CMD_STOP_WAITING_READER_STATE_CHANGE`: answers only if the client still waited (otherwise the
    /// notification it is going to read is the answer).
    pub fn stop_wait(&self) -> io::Result<()> {
        let mut out = lock(&self.out);
        if out.waiting {
            out.waiting = false;
            Self::write(&mut out, &[&wire::wait_reply(code::SUCCESS)])?;
        }
        Ok(())
    }

    /// Notifies the client if it waits. `true` when it did.
    pub fn signal(&self, rv: u32) -> bool {
        let mut out = lock(&self.out);
        if !out.waiting {
            return false;
        }
        out.waiting = false;
        let _ = Self::write(&mut out, &[&wire::wait_reply(rv)]);
        true
    }

    pub fn shutdown(&self) {
        let _ = lock(&self.out).stream.shutdown(std::net::Shutdown::Both);
    }
}

impl Service {
    pub fn new(options: Options) -> Arc<Self> {
        let table = ReaderTable::new();
        let blob = Arc::new(table.encode());
        Arc::new(Self {
            options,
            backend: Mutex::new(Backend {
                epoch: 0,
                smartcard: None,
                monitor: None,
            }),
            table: Mutex::new(TableState { table, blob }),
            registry: Mutex::new(Registry::default()),
            registry_changed: Condvar::new(),
            stopping: AtomicBool::new(false),
        })
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    // --- the device ---------------------------------------------------------------------------------------

    /// The device and its epoch, when one is attached.
    pub fn current_backend(&self) -> Option<(u64, Arc<dyn RemoteSmartcard>)> {
        let b = lock(&self.backend);
        b.smartcard.clone().map(|s| (b.epoch, s))
    }

    pub fn attach(self: &Arc<Self>, smartcard: Arc<dyn RemoteSmartcard>) {
        if self.is_stopping() {
            return;
        }
        self.detach();
        let monitor = Arc::new(Monitor::default());
        {
            let mut b = lock(&self.backend);
            b.epoch += 1;
            b.smartcard = Some(smartcard.clone());
            b.monitor = Some(monitor.clone());
        }
        let service = self.clone();
        let spawned = std::thread::Builder::new()
            .name("pcsc-monitor".into())
            .spawn({
                let monitor = monitor.clone();
                move || monitor::run(monitor, smartcard, service)
            });
        if let Err(e) = spawned {
            tracing::error!("smart card monitor not started: {e}");
        }
    }

    pub fn detach(&self) {
        let (monitor, smartcard) = {
            let mut b = lock(&self.backend);
            b.epoch += 1;
            (b.monitor.take(), b.smartcard.take())
        };
        if let (Some(m), Some(s)) = (monitor, smartcard) {
            m.stop_and_wait(&*s, Duration::from_secs(2));
        }
        // The readers are gone: tell the clients that wait.
        let changed = {
            let mut t = lock(&self.table);
            let changed = t.table.sync_readers(&[]);
            t.blob = Arc::new(t.table.encode());
            changed
        };
        if changed {
            self.signal_all(code::SUCCESS);
        }
    }

    // --- the table ----------------------------------------------------------------------------------------

    pub fn table_blob(&self) -> Arc<Vec<u8>> {
        lock(&self.table).blob.clone()
    }

    pub fn reader_events(&self) -> u32 {
        lock(&self.table).table.reader_events()
    }

    pub fn table_queries(&self) -> Vec<ReaderQuery> {
        lock(&self.table).table.queries()
    }

    /// The name the client's subsystem knows the reader `name` (as the applications see it) by.
    pub fn backend_reader_name(&self, name: &str) -> Option<String> {
        lock(&self.table)
            .table
            .lookup(name)
            .map(|e| e.backend_name.clone())
    }

    /// Changes the table on behalf of the monitor; ignored once the monitor was stopped. Clients that wait are
    /// notified when `f` says that something changed. Returns that answer.
    pub fn update_table(
        &self,
        monitor: &Monitor,
        f: impl FnOnce(&mut ReaderTable) -> bool,
    ) -> bool {
        let changed = {
            let mut t = lock(&self.table);
            if monitor.is_stopped() {
                return false;
            }
            let changed = f(&mut t.table);
            if changed {
                t.blob = Arc::new(t.table.encode());
            }
            changed
        };
        if changed {
            self.signal_all(code::SUCCESS);
        }
        changed
    }

    /// An application connected to the card of the reader `name` (no notification: pcsc-lite sends none).
    pub fn note_protocol(&self, name: &str, protocol: u32) {
        let mut t = lock(&self.table);
        t.table.note_protocol(name, protocol);
        t.blob = Arc::new(t.table.encode());
    }

    // --- the clients --------------------------------------------------------------------------------------

    /// Notifies every client that waits for a change.
    pub fn signal_all(&self, rv: u32) {
        let conns: Vec<Arc<ConnShared>> = lock(&self.registry).conns.values().cloned().collect();
        for c in conns {
            c.signal(rv);
        }
    }

    /// Registers a connection; `None` when there are too many.
    pub fn add_conn(&self, stream: UnixStream) -> Option<Arc<ConnShared>> {
        let mut r = lock(&self.registry);
        if r.conns.len() >= MAX_CONNECTIONS || self.is_stopping() {
            return None;
        }
        r.next_conn += 1;
        let conn = Arc::new(ConnShared {
            id: r.next_conn,
            out: Mutex::new(Out {
                stream,
                waiting: false,
            }),
        });
        r.conns.insert(conn.id, conn.clone());
        Some(conn)
    }

    pub fn remove_conn(&self, id: u64) {
        let mut r = lock(&self.registry);
        r.conns.remove(&id);
        r.contexts.retain(|_, c| *c != id);
        drop(r);
        self.registry_changed.notify_all();
    }

    /// Mints the `hContext` of a connection.
    pub fn register_context(&self, conn: u64) -> u32 {
        let mut r = lock(&self.registry);
        let r = &mut *r;
        let mint = r.ids.get_or_insert_with(IdMint::new);
        let contexts = &r.contexts;
        let id = mint.context(|v| contexts.contains_key(&v));
        r.contexts.insert(id, conn);
        id
    }

    pub fn unregister_context(&self, context: u32) {
        lock(&self.registry).contexts.remove(&context);
    }

    /// `SCardCancel`: ends the wait of the client that owns `context`. `false` for an unknown context.
    pub fn cancel_context(&self, context: u32) -> bool {
        let conn = {
            let r = lock(&self.registry);
            r.contexts
                .get(&context)
                .and_then(|id| r.conns.get(id))
                .cloned()
        };
        match conn {
            Some(c) => {
                c.signal(code::E_CANCELLED);
                true
            }
            None => false,
        }
    }

    /// Mints the `hCard` of a card, which `taken` does not already have.
    pub fn mint_card(&self, taken: impl Fn(i32) -> bool) -> i32 {
        let mut r = lock(&self.registry);
        r.ids.get_or_insert_with(IdMint::new).card(taken)
    }

    /// Stops everything: the device, the clients (and waits for their threads, at most `max`).
    pub fn shutdown(&self, max: Duration) {
        self.stopping.store(true, Ordering::SeqCst);
        self.detach();
        let conns: Vec<Arc<ConnShared>> = lock(&self.registry).conns.values().cloned().collect();
        for c in conns {
            c.shutdown();
        }
        let r = lock(&self.registry);
        let _ = self
            .registry_changed
            .wait_timeout_while(r, max, |r| !r.conns.is_empty())
            .unwrap_or_else(|e| e.into_inner());
    }
}
