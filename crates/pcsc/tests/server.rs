//! The PC/SC server over a real Unix socket, with a scripted smart card subsystem behind it and a client that
//! speaks the wire protocol by hand (offsets of the C structs written out on purpose: they are what `libpcsclite`
//! uses, `wire.rs` must agree with them).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxydesk_core::smartcard::{code, RemoteSmartcard, SmartcardHost};
use oxydesk_pcsc::fake::{FakeSmartcard, FAKE_ATR, FAKE_READER};
use oxydesk_pcsc::wire::{decode_table, ReaderRecord, TABLE_SIZE};
use oxydesk_pcsc::{Options, PcscServer};

const VERSION: u32 = 0x11;
const GET_READERS_STATE: u32 = 0x12;
const WAIT_CHANGE: u32 = 0x13;
const STOP_WAITING: u32 = 0x14;
const GET_READER_EVENTS: u32 = 0x15;
const ESTABLISH: u32 = 0x01;
const RELEASE: u32 = 0x02;
const CONNECT: u32 = 0x04;
const RECONNECT: u32 = 0x05;
const DISCONNECT: u32 = 0x06;
const BEGIN: u32 = 0x07;
const END: u32 = 0x08;
const TRANSMIT: u32 = 0x09;
const CONTROL: u32 = 0x0A;
const STATUS: u32 = 0x0B;
const CANCEL: u32 = 0x0D;
const GET_ATTRIB: u32 = 0x0F;
const SET_ATTRIB: u32 = 0x10;

const T1: u32 = 2;
const SHARED: u32 = 2;

/// Fields of a message, native endian.
#[derive(Default)]
struct Body(Vec<u8>);

impl Body {
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    fn bytes(mut self, b: &[u8], size: usize) -> Self {
        let mut v = b.to_vec();
        v.resize(size, 0);
        self.0.extend(v);
        self
    }
}

fn word(b: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(b[offset..offset + 4].try_into().unwrap())
}

struct Client {
    stream: UnixStream,
}

impl Client {
    fn connect(path: &PathBuf) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        Self { stream }
    }

    fn send(&mut self, command: u32, body: &[u8]) {
        self.send_raw(command, body.len() as u32, body);
    }

    fn send_raw(&mut self, command: u32, size: u32, body: &[u8]) {
        let mut m = Vec::new();
        m.extend_from_slice(&size.to_ne_bytes());
        m.extend_from_slice(&command.to_ne_bytes());
        m.extend_from_slice(body);
        self.stream.write_all(&m).unwrap();
    }

    fn recv(&mut self, n: usize) -> Vec<u8> {
        let mut b = vec![0; n];
        self.stream.read_exact(&mut b).unwrap();
        b
    }

    /// `true` when the server closed the connection (without sending anything more).
    fn closed(&mut self) -> bool {
        let mut b = [0u8; 1];
        matches!(self.stream.read(&mut b), Ok(0) | Err(_))
    }

    fn version(&mut self, major: u32, minor: u32) -> (u32, u32, u32) {
        self.send(VERSION, &Body::default().u32(major).u32(minor).u32(0).0);
        let r = self.recv(12);
        (word(&r, 0), word(&r, 4), word(&r, 8))
    }

    fn establish(&mut self) -> u32 {
        self.send(ESTABLISH, &Body::default().u32(2).u32(0).u32(0).0);
        let r = self.recv(12);
        assert_eq!(word(&r, 8), 0, "establish");
        word(&r, 4)
    }

    fn table(&mut self) -> Vec<ReaderRecord> {
        self.send(GET_READERS_STATE, &[]);
        decode_table(&self.recv(TABLE_SIZE)).unwrap()
    }

    /// `CMD_WAIT_READER_STATE_CHANGE`: the table comes at once, the notification later.
    fn register(&mut self) -> Vec<ReaderRecord> {
        self.send(WAIT_CHANGE, &[]);
        decode_table(&self.recv(TABLE_SIZE)).unwrap()
    }

    fn notification(&mut self) -> u32 {
        let r = self.recv(8);
        assert_eq!(word(&r, 0), 0);
        word(&r, 4)
    }

    fn connect_card(&mut self, ctx: u32, reader: &str, protocols: u32) -> (u32, i32, u32) {
        let b = Body::default()
            .u32(ctx)
            .bytes(reader.as_bytes(), 128)
            .u32(SHARED)
            .u32(protocols)
            .u32(0)
            .u32(0)
            .u32(0);
        self.send(CONNECT, &b.0);
        let r = self.recv(152);
        assert_eq!(
            &r[4..4 + reader.len().min(127)],
            &reader.as_bytes()[..reader.len().min(127)]
        );
        (word(&r, 148), word(&r, 140) as i32, word(&r, 144))
    }

    fn transmit(&mut self, card: i32, apdu: &[u8], buffer: u32) -> (u32, Vec<u8>, u32) {
        let b = Body::default()
            .u32(card as u32)
            .u32(T1)
            .u32(8)
            .u32(apdu.len() as u32)
            .u32(T1)
            .u32(8)
            .u32(buffer)
            .u32(0);
        let mut m = b.0;
        m.extend_from_slice(apdu);
        self.send(TRANSMIT, &m[..32]);
        self.stream.write_all(&m[32..]).unwrap();
        let r = self.recv(32);
        let (rv, len) = (word(&r, 28), word(&r, 24));
        let data = if rv == 0 {
            self.recv(len as usize)
        } else {
            vec![]
        };
        (rv, data, len)
    }

    fn simple(&mut self, command: u32, fields: &[u32], reply_len: usize) -> Vec<u8> {
        let mut b = Body::default();
        for f in fields {
            b = b.u32(*f);
        }
        self.send(command, &b.0);
        self.recv(reply_len)
    }

    fn disconnect_card(&mut self, card: i32) -> u32 {
        word(&self.simple(DISCONNECT, &[card as u32, 0, 0], 12), 8)
    }

    fn release(&mut self, ctx: u32) -> u32 {
        word(&self.simple(RELEASE, &[ctx, 0], 8), 4)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    server: Arc<PcscServer>,
    card: Arc<FakeSmartcard>,
}

fn options() -> Options {
    Options {
        reader_poll: Duration::from_millis(150),
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pcscd.comm");
        let server = PcscServer::start_with(&path, options()).unwrap();
        let card = Arc::new(FakeSmartcard::new());
        let fixture = Self {
            _dir: dir,
            path,
            server,
            card,
        };
        fixture.attach();
        fixture
    }

    fn attach(&self) {
        let backend: Arc<dyn RemoteSmartcard> = self.card.clone();
        self.server.attached(backend);
        self.wait_readers(1);
    }

    fn client(&self) -> Client {
        Client::connect(&self.path)
    }

    fn wait_readers(&self, n: usize) -> Vec<ReaderRecord> {
        let mut c = self.client();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let t = c.table();
            if t.len() == n && t.iter().all(|r| r.reader_state != 1) {
                return t;
            }
            assert!(Instant::now() < deadline, "readers never became {n}: {t:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A client with an established context.
    fn session(&self) -> (Client, u32) {
        let mut c = self.client();
        let ctx = c.establish();
        (c, ctx)
    }
}

type Attack = Box<dyn Fn(&mut Client)>;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn the_handshake_accepts_the_protocols_pcsc_lite_accepts() {
    let f = Fixture::new();
    let mut c = f.client();
    assert_eq!(c.version(4, 5), (4, 5, 0));
    assert_eq!(
        c.version(4, 4),
        (4, 5, 0),
        "the server answers with its own version"
    );
    assert_eq!(c.version(4, 3).2, code::E_SERVICE_STOPPED);
    assert_eq!(c.version(3, 5).2, code::E_SERVICE_STOPPED);
    // the connection stays usable: a client may retry
    assert_eq!(c.version(4, 5), (4, 5, 0));
}

#[test]
fn a_context_is_minted_once_per_connection() {
    let f = Fixture::new();
    let mut c = f.client();
    let ctx = c.establish();
    assert_ne!(ctx, 0);
    let again = c.simple(ESTABLISH, &[2, 0, 0], 12);
    assert_eq!(word(&again, 8), code::E_INVALID_VALUE);
    let bad_scope = f.client().simple(ESTABLISH, &[9, 0, 0], 12);
    assert_eq!(word(&bad_scope, 8), code::E_INVALID_VALUE);
    let mut other = f.client();
    assert_ne!(other.establish(), ctx, "contexts are distinct");
    assert_eq!(c.release(ctx), 0);
    assert_eq!(c.release(ctx), code::E_INVALID_HANDLE, "already released");
    let mut third = f.client();
    let t = third.establish();
    assert_eq!(third.release(t.wrapping_add(1)), code::E_INVALID_VALUE);
}

#[test]
fn the_table_lists_the_reader_with_its_card() {
    let f = Fixture::new();
    let t = f.client().table();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].name, FAKE_READER);
    assert_eq!(t[0].atr, FAKE_ATR);
    // PRESENT | POWERED | NEGOTIABLE, no protocol yet
    assert_eq!(t[0].reader_state, 0x4 | 0x10 | 0x20);
    assert_eq!((t[0].event_counter, t[0].protocol, t[0].sharing), (0, 0, 0));
    // the raw table has the size of 16 READER_STATE
    let mut c = f.client();
    c.send(GET_READERS_STATE, &[]);
    let raw = c.recv(TABLE_SIZE);
    assert_eq!(&raw[..13], b"Fake Reader 0");
    assert!(raw[184..].iter().all(|&b| b == 0));
}

#[test]
fn a_whole_session_goes_through_the_card() {
    let f = Fixture::new();
    let (mut c, ctx) = f.session();
    let (rv, card, protocol) = c.connect_card(ctx, FAKE_READER, 3);
    assert_eq!((rv, protocol), (0, T1));
    assert!(card > 0);
    // what the applications read afterwards: the protocol of the connection
    let t = c.table();
    assert_eq!(t[0].protocol, T1);
    assert_eq!(t[0].reader_state, 0x4 | 0x10 | 0x40);

    let (rv, data, _) = c.transmit(card, &[0x00, 0xA4, 0x04, 0x00], 258);
    assert_eq!((rv, hex(&data).as_str()), (0, "9000"));
    let (rv, data, len) = c.transmit(card, &[0x00, 0xB0, 0, 0, 0], 258);
    assert_eq!((rv, data.len(), len), (0, 258, 258));
    assert_eq!(f.card.count("transmit"), 2);

    assert_eq!(word(&c.simple(STATUS, &[card as u32, 0], 8), 4), 0);
    assert_eq!(word(&c.simple(BEGIN, &[card as u32, 0], 8), 4), 0);
    assert_eq!(word(&c.simple(END, &[card as u32, 0, 0], 12), 8), 0);
    // reconnect answers with the protocol (dwActiveProtocol is the fifth field)
    let r = c.simple(RECONNECT, &[card as u32, SHARED, 3, 1, 0, 0], 24);
    assert_eq!((word(&r, 16), word(&r, 20)), (T1, 0));

    // control: the input comes back
    let mut m = Body::default()
        .u32(card as u32)
        .u32(0x42000C00)
        .u32(3)
        .u32(16)
        .u32(0)
        .u32(0)
        .0;
    c.send(CONTROL, &m);
    c.stream.write_all(&[1, 2, 3]).unwrap();
    m = c.recv(24);
    assert_eq!((word(&m, 20), word(&m, 16)), (0, 3));
    assert_eq!(c.recv(3), vec![1, 2, 3]);

    // attributes: the ATR, then one the card does not have
    let attr = |c: &mut Client, id: u32| {
        let b = Body::default()
            .u32(card as u32)
            .u32(id)
            .bytes(&[], 264)
            .u32(264)
            .u32(0);
        c.send(GET_ATTRIB, &b.0);
        c.recv(280)
    };
    let r = attr(&mut c, 0x0009_0303);
    assert_eq!((word(&r, 276), word(&r, 272)), (0, 10));
    assert_eq!(&r[8..18], &FAKE_ATR);
    assert_eq!(word(&attr(&mut c, 7), 276), code::E_UNSUPPORTED_FEATURE);
    // SCardSetAttrib is not supported by the port
    let b = Body::default()
        .u32(card as u32)
        .u32(1)
        .bytes(&[1], 264)
        .u32(1)
        .u32(0);
    c.send(SET_ATTRIB, &b.0);
    assert_eq!(word(&c.recv(280), 276), code::E_UNSUPPORTED_FEATURE);

    assert_eq!(c.disconnect_card(card), 0);
    assert_eq!(c.release(ctx), 0);
    assert_eq!(f.card.count("disconnect"), 1);
    // the context of the monitor is the only one left
    assert!(f
        .card
        .wait_for("release_context", 1, Duration::from_secs(2)));
    assert_eq!(f.card.open_contexts(), 1);
}

#[test]
fn errors_of_the_card_reach_the_application() {
    let f = Fixture::new();
    let (mut c, ctx) = f.session();
    assert_eq!(
        c.connect_card(ctx, "No such reader", 3).0,
        code::E_UNKNOWN_READER
    );
    assert_eq!(
        c.connect_card(ctx, FAKE_READER, 8).0,
        code::E_PROTO_MISMATCH
    );
    assert_eq!(
        c.connect_card(ctx.wrapping_add(1), FAKE_READER, 3).0,
        code::E_INVALID_HANDLE
    );
    let mut no_context = f.client();
    assert_eq!(
        no_context.connect_card(7, FAKE_READER, 3).0,
        code::E_INVALID_HANDLE
    );
    let (_, card, _) = c.connect_card(ctx, FAKE_READER, 3);
    f.card.remove_card();
    assert_eq!(c.connect_card(ctx, FAKE_READER, 3).0, code::E_NO_SMARTCARD);
    let (rv, data, _) = c.transmit(card, &[0, 0xA4, 4, 0], 258);
    assert_eq!((rv, data.len()), (code::W_REMOVED_CARD, 0));
    assert_eq!(
        word(&c.simple(STATUS, &[card as u32, 0], 8), 4),
        code::W_REMOVED_CARD
    );
    assert_eq!(
        word(&c.simple(BEGIN, &[card as u32, 0], 8), 4),
        code::W_REMOVED_CARD
    );
    // the handle can still be given back
    assert_eq!(c.disconnect_card(card), 0);
}

#[test]
fn an_answer_larger_than_the_buffer_tells_the_size() {
    let f = Fixture::new();
    let (mut c, ctx) = f.session();
    let (_, card, _) = c.connect_card(ctx, FAKE_READER, 3);
    let (rv, data, len) = c.transmit(card, &[0, 0xB0, 0, 0, 0], 20);
    assert_eq!((rv, data.len(), len), (code::E_INSUFFICIENT_BUFFER, 0, 258));
    // the connection is still in sync
    let (rv, data, _) = c.transmit(card, &[0, 0xA4, 4, 0], 20);
    assert_eq!((rv, hex(&data).as_str()), (0, "9000"));
}

#[test]
fn a_big_apdu_goes_through_in_one_piece() {
    let f = Fixture::new();
    let (mut c, ctx) = f.session();
    let (_, card, _) = c.connect_card(ctx, FAKE_READER, 3);
    let mut apdu = vec![0x00, 0xA4, 0x04, 0x00];
    apdu.resize(60_000, 0x55);
    let (rv, data, _) = c.transmit(card, &apdu, 258);
    assert_eq!((rv, hex(&data).as_str()), (0, "9000"));
    assert!(
        f.card.calls().iter().any(|l| l.len() > 100_000),
        "the backend got every byte"
    );
}

#[test]
fn an_abrupt_disconnect_gives_everything_back() {
    let f = Fixture::new();
    let (mut c, ctx) = f.session();
    let (rv, _card, _) = c.connect_card(ctx, FAKE_READER, 3);
    assert_eq!(rv, 0);
    let (_, card2, _) = c.connect_card(ctx, FAKE_READER, 3);
    assert!(card2 > 0);
    assert_eq!(f.card.open_cards(), 2);
    drop(c); // no release, no disconnect
    assert!(
        f.card.wait_for("disconnect", 2, Duration::from_secs(3)),
        "{:?}",
        f.card.calls()
    );
    assert!(f
        .card
        .wait_for("release_context", 1, Duration::from_secs(3)));
    assert_eq!(f.card.open_cards(), 0);
    assert_eq!(f.card.open_contexts(), 1, "only the monitor's");
    // SCARD_LEAVE_CARD
    assert!(f
        .card
        .calls()
        .iter()
        .any(|l| l.ends_with(" 0") && l.starts_with("disconnect")));
}

#[test]
fn a_card_removal_wakes_the_waiting_clients() {
    let f = Fixture::new();
    let (mut a, _) = f.session();
    let (mut b, _) = f.session();
    assert_eq!(a.register()[0].event_counter, 0);
    b.register();
    f.card.remove_card();
    assert_eq!(a.notification(), 0);
    assert_eq!(b.notification(), 0);
    let t = a.table();
    assert_eq!(t[0].reader_state, 0x2, "SCARD_ABSENT");
    assert_eq!((t[0].event_counter, t[0].atr.len()), (1, 0));
    // wait again: the insertion
    a.register();
    f.card.insert_card();
    assert_eq!(a.notification(), 0);
    let t = a.table();
    assert_eq!((t[0].reader_state & 0x4, t[0].event_counter), (0x4, 2));
    assert_eq!(t[0].atr, FAKE_ATR);
}

#[test]
fn a_client_that_does_not_wait_is_not_notified() {
    let f = Fixture::new();
    let mut a = f.client();
    f.card.remove_card();
    f.wait_readers(1);
    std::thread::sleep(Duration::from_millis(100));
    // The next thing it reads is the answer to its own request.
    a.send(GET_READER_EVENTS, &[]);
    let r = a.recv(8);
    assert_eq!(word(&r, 4), 0);
}

#[test]
fn stopping_the_wait_is_answered_only_while_waiting() {
    let f = Fixture::new();
    let mut c = f.client();
    c.register();
    c.send(STOP_WAITING, &[]);
    assert_eq!(c.notification(), 0, "answer of the stop");
    // not waiting any more: no answer; the next thing read is the reply to the following request
    c.send(STOP_WAITING, &[]);
    c.send(GET_READER_EVENTS, &[]);
    let r = c.recv(8);
    assert_eq!(
        word(&r, 0),
        1,
        "the first message is the reply of GET_READER_EVENTS (one reader event)"
    );
    // a notification already sent is the answer: the stop gets nothing more
    c.register();
    f.card.remove_card();
    f.card
        .wait_for("get_status_change", 2, Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(100));
    c.send(STOP_WAITING, &[]);
    c.send(GET_READER_EVENTS, &[]);
    assert_eq!(c.notification(), 0);
    let r = c.recv(8);
    assert_eq!(word(&r, 0), 1);
}

#[test]
fn another_connection_can_cancel_a_wait() {
    let f = Fixture::new();
    let (mut waiting, ctx) = f.session();
    waiting.register();
    let mut canceller = f.client();
    let r = canceller.simple(CANCEL, &[ctx, 0], 8);
    assert_eq!(word(&r, 4), 0);
    assert_eq!(waiting.notification(), code::E_CANCELLED);
    // cancelling again, when nothing waits, is fine; an unknown context is not
    let r = canceller.simple(CANCEL, &[ctx, 0], 8);
    assert_eq!(word(&r, 4), 0);
    let r = canceller.simple(CANCEL, &[ctx.wrapping_add(1), 0], 8);
    assert_eq!(word(&r, 4), code::E_INVALID_HANDLE);
    // and the cancelled client is not notified of the next event
    f.card.remove_card();
    f.wait_readers(1);
    waiting.send(GET_READER_EVENTS, &[]);
    assert_eq!(word(&waiting.recv(8), 0), 1);
}

#[test]
fn detaching_the_device_empties_the_table_and_wakes_the_clients() {
    let f = Fixture::new();
    let (mut c, ctx) = f.session();
    let (_, card, _) = c.connect_card(ctx, FAKE_READER, 3);
    let mut waiting = f.client();
    waiting.register();
    f.server.detached();
    assert_eq!(waiting.notification(), 0);
    assert!(waiting.table().is_empty());
    // a client that was connected sees its card gone
    let (rv, _, _) = c.transmit(card, &[0, 0xA4, 4, 0], 258);
    assert_eq!(rv, code::E_READER_UNAVAILABLE);
    assert_eq!(
        word(&c.simple(STATUS, &[card as u32, 0], 8), 4),
        code::E_READER_UNAVAILABLE
    );
    // new connections find no reader
    assert_eq!(
        c.connect_card(ctx, FAKE_READER, 3).0,
        code::E_UNKNOWN_READER
    );
    // the monitor gave its context back
    assert!(f
        .card
        .wait_for("release_context", 1, Duration::from_secs(2)));
    // the handle can be given back, and nothing is asked of the device that is gone
    let disconnects = f.card.count("disconnect");
    assert_eq!(c.disconnect_card(card), 0);
    assert_eq!(f.card.count("disconnect"), disconnects);
    // the device comes back: the readers too, and the old handle stays dead
    f.attach();
    assert_eq!(f.client().table().len(), 1);
    let (_, card2, _) = c.connect_card(ctx, FAKE_READER, 3);
    assert!(card2 > 0);
    assert_eq!(c.transmit(card2, &[0, 0xA4, 4, 0], 258).0, 0);
    assert_eq!(c.release(ctx), 0);
}

#[test]
fn a_stale_card_survives_a_new_device_without_reaching_it() {
    let f = Fixture::new();
    let (mut c, ctx) = f.session();
    let (_, card, _) = c.connect_card(ctx, FAKE_READER, 3);
    f.attach(); // attached again without detach: a new device
    f.wait_readers(1);
    assert_eq!(
        c.transmit(card, &[0, 0xA4, 4, 0], 258).0,
        code::E_READER_UNAVAILABLE
    );
    assert_eq!(f.card.count("transmit"), 0);
}

#[test]
fn a_dead_link_fails_the_calls_and_the_readers_become_unknown() {
    let f = Fixture::new();
    let (mut c, ctx) = f.session();
    let (_, card, _) = c.connect_card(ctx, FAKE_READER, 3);
    f.card.fail_link();
    let (rv, _, _) = c.transmit(card, &[0, 0xA4, 4, 0], 258);
    assert_eq!(rv, code::E_NO_SERVICE);
    assert_eq!(c.connect_card(ctx, FAKE_READER, 3).0, code::E_NO_SERVICE);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let t = f.client().table();
        if t.first().is_some_and(|r| r.reader_state == 1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the reader never became unknown: {t:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // the table is read, not asked: the server still answers
    f.server.detached();
    assert!(f.client().table().is_empty());
}

#[test]
fn readers_that_come_and_go_are_noticed() {
    let f = Fixture::new();
    let mut waiting = f.client();
    waiting.register();
    f.card.set_readers(&[FAKE_READER, "Second reader"]);
    assert_eq!(waiting.notification(), 0);
    let t = f.wait_readers(2);
    assert_eq!(t[1].name, "Second reader");
    assert_eq!(t[1].reader_state, 0x2);
    let mut c = f.client();
    c.send(GET_READER_EVENTS, &[]);
    assert!(word(&c.recv(8), 0) >= 2);
    f.card.set_readers(&[]);
    assert!(f.wait_readers(0).is_empty());
    let (mut s, ctx) = f.session();
    assert_eq!(
        s.connect_card(ctx, FAKE_READER, 3).0,
        code::E_UNKNOWN_READER
    );
}

#[test]
fn malformed_clients_are_dropped_and_nobody_else_cares() {
    let f = Fixture::new();
    let (mut healthy, ctx) = f.session();
    let (_, card, _) = healthy.connect_card(ctx, FAKE_READER, 3);

    let big = (65548 + 1) as u32;
    let attacks: Vec<Attack> = vec![
        // unknown commands
        Box::new(|c| c.send(0, &[])),
        Box::new(|c| c.send(0x0C, &[])),
        Box::new(|c| c.send(0x16, &[])),
        Box::new(|c| c.send(0xFFFF_FFFF, &[0; 4])),
        // wrong sizes
        Box::new(|c| c.send_raw(VERSION, 4, &[0; 4])),
        Box::new(|c| c.send_raw(GET_READERS_STATE, 4, &[0; 4])),
        Box::new(|c| c.send_raw(ESTABLISH, u32::MAX, &[])),
        // lengths beyond the protocol
        Box::new(move |c| {
            c.send(
                TRANSMIT,
                &Body::default()
                    .u32(1)
                    .u32(2)
                    .u32(8)
                    .u32(big)
                    .u32(2)
                    .u32(8)
                    .u32(258)
                    .u32(0)
                    .0,
            )
        }),
        Box::new(move |c| {
            c.send(
                CONTROL,
                &Body::default()
                    .u32(1)
                    .u32(1)
                    .u32(big)
                    .u32(0)
                    .u32(0)
                    .u32(0)
                    .0,
            )
        }),
        Box::new(|c| {
            c.send(
                GET_ATTRIB,
                &Body::default()
                    .u32(1)
                    .u32(1)
                    .bytes(&[], 264)
                    .u32(265)
                    .u32(0)
                    .0,
            )
        }),
        // handles that are not theirs (the healthy client's card, a made-up one)
        Box::new(move |c| {
            c.send(STATUS, &Body::default().u32(card as u32).u32(0).0);
        }),
        Box::new(|c| {
            c.send(DISCONNECT, &Body::default().u32(0x1234).u32(0).u32(0).0);
        }),
        Box::new(|c| {
            c.send(BEGIN, &Body::default().u32(0).u32(0).0);
        }),
        // garbage
        Box::new(|c| c.stream.write_all(&[0xAB; 64]).unwrap()),
    ];
    for (i, attack) in attacks.iter().enumerate() {
        let mut bad = f.client();
        let _ctx = bad.establish();
        attack(&mut bad);
        assert!(bad.closed(), "attack {i} was not dropped");
        // and the healthy client does not notice
        assert_eq!(
            healthy.transmit(card, &[0, 0xA4, 4, 0], 258).0,
            0,
            "attack {i}"
        );
    }

    // a frame cut in the middle, and a client that vanishes while a transmit is half sent
    let mut cut = f.client();
    cut.stream.write_all(&[9, 0, 0]).unwrap();
    drop(cut);
    let mut half = f.client();
    half.establish();
    half.send(
        TRANSMIT,
        &Body::default()
            .u32(1)
            .u32(2)
            .u32(8)
            .u32(100)
            .u32(2)
            .u32(8)
            .u32(258)
            .u32(0)
            .0,
    );
    half.stream.write_all(&[0; 10]).unwrap();
    drop(half);
    assert_eq!(healthy.transmit(card, &[0, 0xA4, 4, 0], 258).0, 0);
    assert_eq!(healthy.release(ctx), 0);
}

#[test]
fn a_client_cannot_use_the_card_of_another() {
    let f = Fixture::new();
    let (mut a, ctx_a) = f.session();
    let (_, card, _) = a.connect_card(ctx_a, FAKE_READER, 3);
    let (mut b, _) = f.session();
    b.send(BEGIN, &Body::default().u32(card as u32).u32(0).0);
    assert!(b.closed());
    assert_eq!(a.transmit(card, &[0, 0xA4, 4, 0], 258).0, 0);
}

#[test]
fn too_many_clients_are_refused_without_harm() {
    let f = Fixture::new();
    let mut held: Vec<Client> = (0..256).map(|_| f.client()).collect();
    // Everything up to the limit works...
    let mut probe = held.pop().unwrap();
    assert_eq!(probe.version(4, 5).2, 0);
    // ... and the ones beyond are closed at once.
    let mut refused = 0;
    for _ in 0..20 {
        let mut c = f.client();
        c.stream.write_all(&[0; 0]).unwrap();
        if c.closed() {
            refused += 1;
        }
    }
    assert!(refused >= 10, "refused {refused}");
    drop(held);
    drop(probe);
    // room again
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut c = f.client();
        c.send(VERSION, &Body::default().u32(4).u32(5).u32(0).0);
        let mut b = [0u8; 12];
        if c.stream.read_exact(&mut b).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the server never took clients again"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn stopping_the_server_removes_the_socket_and_gives_everything_back() {
    let f = Fixture::new();
    let (mut c, ctx) = f.session();
    let (_, _card, _) = c.connect_card(ctx, FAKE_READER, 3);
    let mut waiting = f.client();
    waiting.register();
    let path = f.path.clone();
    f.server.shutdown();
    assert!(!path.exists(), "socket removed");
    let mut rest = Vec::new();
    let _ = waiting.stream.read_to_end(&mut rest); // a last notification, then the end
    assert!(rest.len() <= 8);
    assert!(c.closed());
    assert!(UnixStream::connect(&path).is_err());
    // Stopping is a detach: the handles belong to a device that is let go, nothing more is asked of it (the
    // monitor still gives its own context back).
    assert!(f
        .card
        .wait_for("release_context", 1, Duration::from_secs(2)));
    f.server.shutdown(); // idempotent

    // a device announced to a stopped server is ignored
    let backend: Arc<dyn RemoteSmartcard> = f.card.clone();
    f.server.attached(backend);
}

#[test]
fn dropping_the_server_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s/pcscd.comm");
    let card = Arc::new(FakeSmartcard::new());
    {
        let server = PcscServer::start(&path).unwrap();
        assert_eq!(server.socket_path(), path);
        server.attached(card.clone());
        assert!(path.exists());
        assert!(card.wait_for("get_status_change", 1, Duration::from_secs(3)));
    }
    assert!(!path.exists());
    assert!(card.wait_for("release_context", 1, Duration::from_secs(2)));
}

#[test]
fn the_server_has_no_readers_until_a_device_is_attached() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pcscd.comm");
    let server = PcscServer::start_with(&path, options()).unwrap();
    let mut c = Client::connect(&path);
    assert!(c.table().is_empty());
    let ctx = c.establish();
    assert_eq!(
        c.connect_card(ctx, FAKE_READER, 3).0,
        code::E_UNKNOWN_READER
    );
    server.detached(); // harmless
    assert!(c.table().is_empty());
    assert_eq!(c.release(ctx), 0);
}

#[test]
fn starting_refuses_a_symlink_and_replaces_a_stale_socket() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, b"x").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(PcscServer::start(&link).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"x");

    let path = dir.path().join("s");
    let first = PcscServer::start(&path).unwrap();
    // a second server over the first one's socket takes it over (it is stale as far as the file says)
    let second = PcscServer::start(&path).unwrap();
    assert!(UnixStream::connect(&path).is_ok());
    drop(first);
    drop(second);
    assert!(PcscServer::start(&path).is_ok());
}
