//! The smart card redirection server (`DriveBackend`, `RdpSmartcard`) against a fake client reader, without
//! network: what the desktop does with a `RemoteSmartcard` is what a winscard subsystem executes.
#![cfg(feature = "ironrdp")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ironrdp_rdpdr::server::RdpdrServer;
use ironrdp_server::ServerEvent;
use ironrdp_svc::{SvcMessage, SvcProcessor};
use oxydesk_core::smartcard::{code, state, ReaderQuery};
use oxydesk_core::{RemoteSmartcard, SmartcardError, SmartcardHost};
use oxydesk_rdp::drive::{DriveBackend, EventSender};
use oxydesk_rdp::smartcard::SmartcardRegistry;
use rdp_test_client::smartcard_client::{Calls, FakeSmartcard, ATR, READER, RESPONSE};

#[derive(Default)]
struct Host {
    attached: Mutex<Vec<Arc<dyn RemoteSmartcard>>>,
    detached: Mutex<usize>,
    changed: Condvar,
}

impl SmartcardHost for Host {
    fn attached(&self, smartcard: Arc<dyn RemoteSmartcard>) {
        self.attached.lock().unwrap().push(smartcard);
        self.changed.notify_all();
    }

    fn detached(&self) {
        *self.detached.lock().unwrap() += 1;
    }
}

struct Wire {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Wire {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn encode(messages: Vec<SvcMessage>, out: &mut VecDeque<Vec<u8>>) {
    for m in messages {
        out.push_back(m.encode_unframed_pdu().expect("encode"));
    }
}

/// The server and a fake client wired back to back. `None` when the device is not announced within 3 s.
fn connect(card: bool) -> (Option<Arc<dyn RemoteSmartcard>>, Calls, Wire, Arc<Host>) {
    let host = Arc::new(Host::default());
    let registry = SmartcardRegistry::new();
    registry.set_host(Some(host.clone()));
    let (tx, rx): (_, Receiver<ServerEvent>) = mpsc::channel();
    let tx = Mutex::new(tx);
    let send: EventSender = Arc::new(move |event| tx.lock().unwrap().send(event).is_ok());
    let mut server = RdpdrServer::new(Box::new(
        DriveBackend::new(send, None, None).with_smartcards(registry),
    ));
    let calls = Calls::default();
    let mut client = FakeSmartcard::channel(calls.clone(), card, 7, "TESTPC");
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        let (mut to_client, mut to_server) = (VecDeque::new(), VecDeque::new());
        encode(server.start().expect("server start"), &mut to_client);
        encode(client.start().expect("client start"), &mut to_server);
        while !flag.load(Ordering::Relaxed) {
            if let Ok(ServerEvent::Rdpdr(tag, message)) = rx.recv_timeout(Duration::from_millis(2))
            {
                server.set_next_tag(tag);
                if let Ok(m) = server.send_message(message) {
                    encode(m, &mut to_client);
                }
            }
            while let Some(p) = to_client.pop_front() {
                encode(client.process(&p).expect("client process"), &mut to_server);
            }
            while let Some(p) = to_server.pop_front() {
                encode(server.process(&p).expect("server process"), &mut to_client);
            }
        }
    });
    let wire = Wire {
        stop,
        thread: Some(thread),
    };
    let mut attached = host.attached.lock().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while attached.is_empty() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            drop(attached);
            return (None, calls, wire, host);
        }
        let (guard, _) = host.changed.wait_timeout(attached, left).unwrap();
        attached = guard;
    }
    let smartcard = attached[0].clone();
    drop(attached);
    (Some(smartcard), calls, wire, host)
}

fn names(calls: &Calls) -> Vec<String> {
    calls.lock().unwrap().clone()
}

#[test]
fn a_whole_pcsc_session_goes_through_the_channel() {
    let (sc, calls, _wire, _host) = connect(true);
    let sc = sc.expect("the device is announced");

    let ctx = sc.establish_context().expect("establish");
    assert_eq!(sc.list_readers(ctx).expect("list"), vec![READER.to_owned()]);

    let query = [ReaderQuery {
        name: READER.to_owned(),
        current_state: state::UNAWARE,
    }];
    let status = sc
        .get_status_change(ctx, 1000, &query)
        .expect("status change");
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].name, READER);
    assert!(oxydesk_core::smartcard::card_present(status[0].event_state));
    assert_eq!(status[0].atr, ATR);

    let connected = sc.connect(ctx, READER, 2, 3).expect("connect to the card");
    assert_eq!(connected.protocol, 2);

    let answer = sc
        .transmit(connected.card, 2, &[0x00, 0xA4, 0x04, 0x00], 258)
        .expect("transmit");
    let mut expected = vec![0x00, 0xA4];
    expected.extend_from_slice(&RESPONSE);
    assert_eq!(answer, expected);

    let card = sc.status(connected.card).expect("status");
    assert_eq!(card.reader, READER);
    assert_eq!(card.atr, ATR);
    assert_eq!(card.protocol, 2);

    sc.begin_transaction(connected.card).expect("begin");
    sc.end_transaction(connected.card, 0).expect("end");
    assert_eq!(sc.reconnect(connected.card, 2, 3, 1).expect("reconnect"), 2);
    assert_eq!(
        sc.control(connected.card, 0x42, &[9], 16).expect("control"),
        vec![1, 2, 3]
    );
    sc.disconnect(connected.card, 0).expect("disconnect");
    sc.release_context(ctx).expect("release");

    assert_eq!(
        names(&calls),
        [
            "EstablishContext",
            "ListReaders",
            "GetStatusChange",
            "Connect",
            "Transmit",
            "Status",
            "BeginTransaction",
            "EndTransaction",
            "Reconnect",
            "Control",
            "Disconnect",
            "ReleaseContext",
        ]
    );
}

#[test]
fn an_empty_reader_has_no_card_to_connect_to() {
    let (sc, _calls, _wire, _host) = connect(false);
    let sc = sc.expect("the device is announced");
    let ctx = sc.establish_context().expect("establish");
    let query = [ReaderQuery {
        name: READER.to_owned(),
        current_state: state::UNAWARE,
    }];
    let status = sc.get_status_change(ctx, 0, &query).expect("status change");
    assert!(!oxydesk_core::smartcard::card_present(
        status[0].event_state
    ));
    assert!(status[0].atr.is_empty());
    match sc.connect(ctx, READER, 2, 3) {
        Err(SmartcardError::Code(c)) => assert_eq!(c, code::E_NO_SMARTCARD),
        other => panic!("{other:?}"),
    }
    match sc.connect(ctx, "Unknown 99", 2, 3) {
        Err(SmartcardError::Code(c)) => assert_eq!(c, code::E_UNKNOWN_READER),
        other => panic!("{other:?}"),
    }
}

#[test]
fn contexts_are_distinct_between_establishments() {
    let (sc, _calls, _wire, _host) = connect(true);
    let sc = sc.expect("the device is announced");
    let a = sc.establish_context().expect("a");
    let b = sc.establish_context().expect("b");
    assert_ne!(a, b);
    sc.release_context(a).expect("release a");
    sc.release_context(b).expect("release b");
}
