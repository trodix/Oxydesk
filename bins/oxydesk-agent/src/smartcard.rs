//! The client's smart card readers, seen from the agent: a `RemoteSmartcard` whose calls are sent to the server
//! through the link, and which the PC/SC service exposes to the user's session.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxydesk_agent_proto::{FromAgent, ScOp, ScReaderQuery, ScResult};
use oxydesk_core::error::SmartcardError;
use oxydesk_core::smartcard::{
    CardStatus, Connected, ReaderQuery, ReaderStatus, ScCard, ScContext,
};
use oxydesk_core::{RemoteSmartcard, SmartcardHost};

use crate::drives::LinkSend;

/// Longest wait for an answer of a call that is not a wait for a card: the server itself gives up on the
/// client after 60 s.
const REPLY_TIMEOUT: Duration = Duration::from_secs(90);

/// The calls waiting for their answer.
#[derive(Default)]
pub struct Pending {
    next: Mutex<Waiting>,
}

#[derive(Default)]
struct Waiting {
    counter: u32,
    waiters: HashMap<u32, Sender<ScResult>>,
    /// The device is gone (or the link ended): a call would wait for an answer that nobody sends.
    closed: bool,
}

impl Pending {
    pub fn complete(&self, req_id: u32, result: ScResult) {
        let waiter = self
            .next
            .lock()
            .expect("pending mutex")
            .waiters
            .remove(&req_id);
        if let Some(w) = waiter {
            let _ = w.send(result);
        }
    }

    /// The link ended or the device is gone: the waiters are woken with an error, and so is every later call
    /// until `reopen`. (Without that, the `cancel` that ends the reader monitor at detach would wait for an
    /// answer of a device that no longer exists, and the readers would stay listed.)
    pub fn fail_all(&self) {
        let mut g = self.next.lock().expect("pending mutex");
        g.closed = true;
        g.waiters.clear();
    }

    /// A device is attached: calls are possible again.
    pub fn reopen(&self) {
        self.next.lock().expect("pending mutex").closed = false;
    }

    fn register(&self) -> Option<(u32, Receiver<ScResult>)> {
        let (tx, rx) = channel();
        let mut g = self.next.lock().expect("pending mutex");
        if g.closed {
            return None;
        }
        g.counter = g.counter.wrapping_add(1);
        let id = g.counter;
        g.waiters.insert(id, tx);
        Some((id, rx))
    }

    fn forget(&self, req_id: u32) {
        self.next
            .lock()
            .expect("pending mutex")
            .waiters
            .remove(&req_id);
    }
}

/// The smart card subsystem of the client, reached through the server.
#[derive(Clone)]
struct ProxySmartcard {
    send: LinkSend,
    pending: Arc<Pending>,
}

impl ProxySmartcard {
    fn call(&self, op: ScOp) -> Result<ScResult, SmartcardError> {
        // A wait for a card lasts as long as the application wants: only the end of the link ends it.
        let waits = matches!(op, ScOp::GetStatusChange { .. });
        let Some((req_id, rx)) = self.pending.register() else {
            return Err(SmartcardError::Link("the client's reader is gone".into()));
        };
        if !(self.send)(&FromAgent::SmartcardCall { req_id, op }) {
            self.pending.forget(req_id);
            return Err(SmartcardError::Link("link to the server closed".into()));
        }
        let answer = if waits {
            rx.recv().map_err(|_| ())
        } else {
            rx.recv_timeout(REPLY_TIMEOUT).map_err(|_| ())
        };
        match answer {
            Ok(ScResult::Code(c)) => Err(SmartcardError::Code(c)),
            Ok(ScResult::Link(m)) => Err(SmartcardError::Link(m)),
            Ok(r) => Ok(r),
            Err(()) => {
                self.pending.forget(req_id);
                Err(SmartcardError::Link("no answer from the server".into()))
            }
        }
    }

    fn unexpected() -> SmartcardError {
        SmartcardError::Link("unexpected answer from the server".into())
    }

    fn unit(&self, op: ScOp) -> Result<(), SmartcardError> {
        match self.call(op)? {
            ScResult::Ok => Ok(()),
            _ => Err(Self::unexpected()),
        }
    }

    fn data(&self, op: ScOp) -> Result<Vec<u8>, SmartcardError> {
        match self.call(op)? {
            ScResult::Data(d) => Ok(d),
            _ => Err(Self::unexpected()),
        }
    }
}

impl RemoteSmartcard for ProxySmartcard {
    fn establish_context(&self) -> Result<ScContext, SmartcardError> {
        match self.call(ScOp::EstablishContext)? {
            ScResult::Context(c) => Ok(ScContext(c)),
            _ => Err(Self::unexpected()),
        }
    }

    fn release_context(&self, context: ScContext) -> Result<(), SmartcardError> {
        self.unit(ScOp::ReleaseContext(context.0))
    }

    fn list_readers(&self, context: ScContext) -> Result<Vec<String>, SmartcardError> {
        match self.call(ScOp::ListReaders(context.0))? {
            ScResult::Readers(r) => Ok(r),
            _ => Err(Self::unexpected()),
        }
    }

    fn get_status_change(
        &self,
        context: ScContext,
        timeout_ms: u32,
        readers: &[ReaderQuery],
    ) -> Result<Vec<ReaderStatus>, SmartcardError> {
        let readers = readers
            .iter()
            .map(|r| ScReaderQuery {
                name: r.name.clone(),
                current_state: r.current_state,
            })
            .collect();
        match self.call(ScOp::GetStatusChange {
            context: context.0,
            timeout_ms,
            readers,
        })? {
            ScResult::States(states) => Ok(states
                .into_iter()
                .map(|s| ReaderStatus {
                    name: s.name,
                    event_state: s.event_state,
                    atr: s.atr,
                })
                .collect()),
            _ => Err(Self::unexpected()),
        }
    }

    fn cancel(&self, context: ScContext) -> Result<(), SmartcardError> {
        self.unit(ScOp::Cancel(context.0))
    }

    fn connect(
        &self,
        context: ScContext,
        reader: &str,
        share_mode: u32,
        preferred_protocols: u32,
    ) -> Result<Connected, SmartcardError> {
        match self.call(ScOp::Connect {
            context: context.0,
            reader: reader.to_owned(),
            share_mode,
            protocols: preferred_protocols,
        })? {
            ScResult::Connected { card, protocol } => Ok(Connected {
                card: ScCard(card),
                protocol,
            }),
            _ => Err(Self::unexpected()),
        }
    }

    fn reconnect(
        &self,
        card: ScCard,
        share_mode: u32,
        preferred_protocols: u32,
        initialization: u32,
    ) -> Result<u32, SmartcardError> {
        match self.call(ScOp::Reconnect {
            card: card.0,
            share_mode,
            protocols: preferred_protocols,
            initialization,
        })? {
            ScResult::Protocol(p) => Ok(p),
            _ => Err(Self::unexpected()),
        }
    }

    fn disconnect(&self, card: ScCard, disposition: u32) -> Result<(), SmartcardError> {
        self.unit(ScOp::Disconnect {
            card: card.0,
            disposition,
        })
    }

    fn begin_transaction(&self, card: ScCard) -> Result<(), SmartcardError> {
        self.unit(ScOp::BeginTransaction(card.0))
    }

    fn end_transaction(&self, card: ScCard, disposition: u32) -> Result<(), SmartcardError> {
        self.unit(ScOp::EndTransaction {
            card: card.0,
            disposition,
        })
    }

    fn transmit(
        &self,
        card: ScCard,
        protocol: u32,
        apdu: &[u8],
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError> {
        self.data(ScOp::Transmit {
            card: card.0,
            protocol,
            apdu: apdu.to_vec(),
            max_response,
        })
    }

    fn status(&self, card: ScCard) -> Result<CardStatus, SmartcardError> {
        match self.call(ScOp::Status(card.0))? {
            ScResult::Card {
                reader,
                state,
                protocol,
                atr,
            } => Ok(CardStatus {
                reader,
                state,
                protocol,
                atr,
            }),
            _ => Err(Self::unexpected()),
        }
    }

    fn control(
        &self,
        card: ScCard,
        control_code: u32,
        input: &[u8],
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError> {
        self.data(ScOp::Control {
            card: card.0,
            code: control_code,
            input: input.to_vec(),
            max_response,
        })
    }

    fn get_attrib(
        &self,
        card: ScCard,
        attribute: u32,
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError> {
        self.data(ScOp::GetAttrib {
            card: card.0,
            attribute,
            max_response,
        })
    }
}

/// The readers of one connection to the server, exposed through `host`.
pub struct Smartcards {
    host: Arc<dyn SmartcardHost>,
    proxy: ProxySmartcard,
}

impl Smartcards {
    pub fn new(host: Arc<dyn SmartcardHost>, send: LinkSend) -> Self {
        Self {
            host,
            proxy: ProxySmartcard {
                send,
                pending: Arc::new(Pending::default()),
            },
        }
    }

    pub fn pending(&self) -> &Pending {
        &self.proxy.pending
    }

    pub fn attached(&self) {
        self.proxy.pending.reopen();
        // The service may take a moment to attach: not in the reader thread.
        let host = self.host.clone();
        let proxy = Arc::new(self.proxy.clone());
        std::thread::spawn(move || host.attached(proxy));
    }

    pub fn detached(&self) {
        self.proxy.pending.fail_all();
        self.host.detached();
    }

    /// The connection ended: nothing of the client remains in the session.
    pub fn close(&self) {
        self.detached();
    }
}

#[cfg(test)]
mod tests {
    //! The whole relay in one process: a call made on the proxy of the agent goes through the agent protocol
    //! and the server's link to the (fake) smart card subsystem of the client, and back.
    use super::*;
    use oxydesk_agent_proto::{read_msg, write_msg, ToAgent};
    use oxydesk_remote::AgentLink;
    use std::os::unix::net::UnixStream;
    use std::sync::Condvar;
    use std::time::Instant;

    /// The client's subsystem: one reader with a card that answers every APDU with its first byte and 9000.
    struct Client {
        waiting: Mutex<bool>,
        woken: Condvar,
    }

    impl RemoteSmartcard for Client {
        fn establish_context(&self) -> Result<ScContext, SmartcardError> {
            Ok(ScContext(5))
        }
        fn release_context(&self, _: ScContext) -> Result<(), SmartcardError> {
            Ok(())
        }
        fn list_readers(&self, _: ScContext) -> Result<Vec<String>, SmartcardError> {
            Ok(vec!["Reader 00 00".into()])
        }
        fn get_status_change(
            &self,
            _: ScContext,
            timeout_ms: u32,
            readers: &[ReaderQuery],
        ) -> Result<Vec<ReaderStatus>, SmartcardError> {
            if timeout_ms == u32::MAX {
                // Waits for a card that never comes, until cancelled.
                let mut waiting = self.waiting.lock().unwrap();
                *waiting = true;
                self.woken.notify_all();
                while *waiting {
                    waiting = self.woken.wait(waiting).unwrap();
                }
                return Err(SmartcardError::Code(0x8010_0002));
            }
            Ok(readers
                .iter()
                .map(|r| ReaderStatus {
                    name: r.name.clone(),
                    event_state: 0x0001_0022,
                    atr: vec![0x3B, 0x75],
                })
                .collect())
        }
        fn cancel(&self, _: ScContext) -> Result<(), SmartcardError> {
            *self.waiting.lock().unwrap() = false;
            self.woken.notify_all();
            Ok(())
        }
        fn connect(
            &self,
            _: ScContext,
            _: &str,
            _: u32,
            _: u32,
        ) -> Result<Connected, SmartcardError> {
            Ok(Connected {
                card: ScCard(9),
                protocol: 2,
            })
        }
        fn reconnect(&self, _: ScCard, _: u32, _: u32, _: u32) -> Result<u32, SmartcardError> {
            Ok(2)
        }
        fn disconnect(&self, _: ScCard, _: u32) -> Result<(), SmartcardError> {
            Ok(())
        }
        fn begin_transaction(&self, _: ScCard) -> Result<(), SmartcardError> {
            Ok(())
        }
        fn end_transaction(&self, _: ScCard, _: u32) -> Result<(), SmartcardError> {
            Ok(())
        }
        fn transmit(
            &self,
            _: ScCard,
            _: u32,
            apdu: &[u8],
            _: u32,
        ) -> Result<Vec<u8>, SmartcardError> {
            Ok(vec![apdu[0], 0x90, 0x00])
        }
        fn status(&self, _: ScCard) -> Result<CardStatus, SmartcardError> {
            Ok(CardStatus {
                reader: "Reader 00 00".into(),
                state: 6,
                protocol: 2,
                atr: vec![0x3B, 0x75],
            })
        }
        fn control(&self, _: ScCard, _: u32, i: &[u8], _: u32) -> Result<Vec<u8>, SmartcardError> {
            Ok(i.to_vec())
        }
        fn get_attrib(&self, _: ScCard, _: u32, _: u32) -> Result<Vec<u8>, SmartcardError> {
            Err(SmartcardError::Code(0x8010_001F))
        }
    }

    /// What the PC/SC service of the agent receives.
    #[derive(Default)]
    struct Service {
        card: Mutex<Option<Arc<dyn RemoteSmartcard>>>,
        changed: Condvar,
        detached: Mutex<u32>,
    }

    impl SmartcardHost for Service {
        fn attached(&self, smartcard: Arc<dyn RemoteSmartcard>) {
            *self.card.lock().unwrap() = Some(smartcard);
            self.changed.notify_all();
        }
        fn detached(&self) {
            *self.detached.lock().unwrap() += 1;
        }
    }

    #[test]
    fn calls_of_the_session_reach_the_reader_of_the_client() {
        let (server_end, mut agent_end) = UnixStream::pair().unwrap();
        write_msg(
            &mut agent_end,
            &FromAgent::Hello {
                outputs: 1,
                width: 64,
                height: 64,
            },
        )
        .unwrap();
        let link = AgentLink::connect(server_end, 1).unwrap();
        let service = Arc::new(Service::default());
        let writer = Arc::new(Mutex::new(agent_end.try_clone().unwrap()));
        let smartcards = Arc::new(Smartcards::new(
            service.clone(),
            Arc::new(move |m: &FromAgent| write_msg(&mut *writer.lock().unwrap(), m).is_ok()),
        ));
        // The agent's reader loop.
        let reader = smartcards.clone();
        std::thread::spawn(move || {
            while let Ok(Some(msg)) = read_msg::<_, ToAgent>(&mut agent_end) {
                match msg {
                    ToAgent::SmartcardAttached => reader.attached(),
                    ToAgent::SmartcardDetached => reader.detached(),
                    ToAgent::SmartcardReply { req_id, result } => {
                        reader.pending().complete(req_id, result)
                    }
                    _ => {}
                }
            }
        });

        let client = Arc::new(Client {
            waiting: Mutex::new(false),
            woken: Condvar::new(),
        });
        let host = AgentLink::smartcard_host(&link);
        host.attached(client.clone());
        let proxy = {
            let mut card = service.card.lock().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while card.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "the service never got the device");
                card = service.changed.wait_timeout(card, left).unwrap().0;
            }
            card.clone().unwrap()
        };

        let ctx = proxy.establish_context().unwrap();
        assert_eq!(ctx, ScContext(5));
        assert_eq!(proxy.list_readers(ctx).unwrap(), ["Reader 00 00"]);
        let query = [ReaderQuery {
            name: "Reader 00 00".into(),
            current_state: 0,
        }];
        let status = proxy.get_status_change(ctx, 100, &query).unwrap();
        assert_eq!(status[0].event_state, 0x0001_0022);
        assert_eq!(status[0].atr, [0x3B, 0x75]);
        let connected = proxy.connect(ctx, "Reader 00 00", 2, 3).unwrap();
        assert_eq!(connected.card, ScCard(9));
        assert_eq!(
            proxy
                .transmit(connected.card, 2, &[0xA4, 0, 0], 256)
                .unwrap(),
            [0xA4, 0x90, 0x00]
        );
        assert_eq!(proxy.status(connected.card).unwrap().state, 6);
        assert_eq!(
            proxy.control(connected.card, 1, &[7, 8], 8).unwrap(),
            [7, 8]
        );
        // An error code of the client reaches the application as it is.
        match proxy.get_attrib(connected.card, 1, 8) {
            Err(SmartcardError::Code(c)) => assert_eq!(c, 0x8010_001F),
            other => panic!("{other:?}"),
        }

        // A wait for a card, ended by a cancel that goes through the same link meanwhile.
        let waiter = {
            let proxy = proxy.clone();
            std::thread::spawn(move || proxy.get_status_change(ctx, u32::MAX, &query))
        };
        {
            let mut waiting = client.waiting.lock().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !*waiting {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "the wait never reached the client");
                waiting = client.woken.wait_timeout(waiting, left).unwrap().0;
            }
        }
        proxy.cancel(ctx).unwrap();
        match waiter.join().unwrap() {
            Err(SmartcardError::Code(c)) => assert_eq!(c, 0x8010_0002),
            other => panic!("{other:?}"),
        }

        // The client's device goes away: the service is told.
        host.detached();
        let deadline = Instant::now() + Duration::from_secs(5);
        while *service.detached.lock().unwrap() == 0 {
            assert!(Instant::now() < deadline, "detach not relayed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_wait_ends_with_a_link_error_when_the_connection_closes() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let log = sent.clone();
        let smartcards = Smartcards::new(
            Arc::new(Service::default()),
            Arc::new(move |m: &FromAgent| {
                log.lock().unwrap().push(m.clone());
                let _ = tx.send(());
                true
            }),
        );
        let proxy = smartcards.proxy.clone();
        let waiter =
            std::thread::spawn(move || proxy.get_status_change(ScContext(1), u32::MAX, &[]));
        // Once the call went out, the connection ends.
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        smartcards.close();
        match waiter.join().unwrap() {
            Err(SmartcardError::Link(_)) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    /// Regression: the reader monitor ends a detach with a `cancel`. Sent to a device that is already gone, it
    /// waited for ever for an answer, the detach never completed and the readers stayed listed.
    #[test]
    fn calls_after_the_device_went_fail_at_once_until_the_next_one() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let smartcards = Smartcards::new(
            Arc::new(Service::default()),
            Arc::new(move |_: &FromAgent| {
                let _ = tx.send(());
                true
            }),
        );
        smartcards.detached();
        let started = Instant::now();
        match smartcards.proxy.cancel(ScContext(1)) {
            Err(SmartcardError::Link(_)) => {}
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(rx.try_recv().is_err(), "nothing is sent for a gone device");
        // A new device: calls go out again.
        smartcards.attached();
        let proxy = smartcards.proxy.clone();
        let call = std::thread::spawn(move || proxy.cancel(ScContext(1)));
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        smartcards.proxy.pending.fail_all();
        assert!(call.join().unwrap().is_err());
    }
}
