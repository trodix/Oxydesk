//! A scripted in-memory smart card subsystem (what the RDP client would be), for the tests and for the
//! `fake_reader` example that real PC/SC clients can be tried against.
//!
//! One reader (`Fake Reader 0` unless changed) with a card (ATR `3b:75:13:00:00:45:09:ea:90:00`) that answers a few
//! APDUs: `SELECT` (INS A4) with `90 00`, `READ BINARY` (INS B0) with `Le` bytes of `0, 1, 2...` and `90 00`,
//! `GET DATA` (INS CA) with `de ad be ef 90 00`, anything else with `6d 00`.

use std::collections::{HashMap, HashSet};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use oxydesk_core::error::SmartcardError;
use oxydesk_core::smartcard::{
    code, state, CardStatus, Connected, ReaderQuery, ReaderStatus, RemoteSmartcard, ScCard,
    ScContext,
};

/// The ATR of the card.
pub const FAKE_ATR: [u8; 10] = [0x3b, 0x75, 0x13, 0x00, 0x00, 0x45, 0x09, 0xea, 0x90, 0x00];
/// The default reader.
pub const FAKE_READER: &str = "Fake Reader 0";
/// `SCARD_ATTR_ATR_STRING`.
pub const ATTR_ATR_STRING: u32 = 0x0009_0303;

struct State {
    readers: Vec<String>,
    present: bool,
    /// Card events, in the upper 16 bits of the event state (as Windows does).
    events: u32,
    next_id: u64,
    contexts: HashSet<u64>,
    /// card -> (context, protocol, card events when it was connected)
    cards: HashMap<u64, (u64, u32, u32)>,
    cancelled: HashSet<u64>,
    link_down: bool,
    calls: Vec<String>,
}

pub struct FakeSmartcard {
    state: Mutex<State>,
    changed: Condvar,
}

impl Default for FakeSmartcard {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn code_err<T>(c: u32) -> Result<T, SmartcardError> {
    Err(SmartcardError::Code(c))
}

impl FakeSmartcard {
    /// A reader with its card inserted.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                readers: vec![FAKE_READER.to_string()],
                present: true,
                events: 0,
                next_id: 0,
                contexts: HashSet::new(),
                cards: HashMap::new(),
                cancelled: HashSet::new(),
                link_down: false,
                calls: Vec::new(),
            }),
            changed: Condvar::new(),
        }
    }

    pub fn insert_card(&self) {
        let mut s = lock(&self.state);
        if !s.present {
            s.present = true;
            s.events += 1;
            self.changed.notify_all();
        }
    }

    pub fn remove_card(&self) {
        let mut s = lock(&self.state);
        if s.present {
            s.present = false;
            s.events += 1;
            self.changed.notify_all();
        }
    }

    /// The readers of the client (the card is in the first).
    pub fn set_readers(&self, readers: &[&str]) {
        let mut s = lock(&self.state);
        s.readers = readers.iter().map(|r| r.to_string()).collect();
        self.changed.notify_all();
    }

    /// The link to the client is down: every call fails with [`SmartcardError::Link`], including those waiting.
    pub fn fail_link(&self) {
        lock(&self.state).link_down = true;
        self.changed.notify_all();
    }

    /// The calls received so far, one line each (`name args`).
    pub fn calls(&self) -> Vec<String> {
        lock(&self.state).calls.clone()
    }

    /// Number of calls whose line starts with `prefix`.
    pub fn count(&self, prefix: &str) -> usize {
        lock(&self.state)
            .calls
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }

    /// Waits until `count(prefix) >= n`, at most `timeout`.
    pub fn wait_for(&self, prefix: &str, n: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.count(prefix) >= n {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Cards currently connected (a handle stays open after the card was removed, until it is disconnected).
    pub fn open_cards(&self) -> usize {
        lock(&self.state).cards.len()
    }

    /// Contexts currently established.
    pub fn open_contexts(&self) -> usize {
        lock(&self.state).contexts.len()
    }

    fn enter(&self, line: String) -> Result<MutexGuard<'_, State>, SmartcardError> {
        let mut s = lock(&self.state);
        s.calls.push(line);
        if s.link_down {
            return Err(SmartcardError::Link("fake link down".into()));
        }
        Ok(s)
    }

    /// The protocol of the card `card`, or the error of a card that is gone.
    fn card_protocol(s: &State, card: ScCard) -> Result<u32, SmartcardError> {
        match s.cards.get(&card.0) {
            None => code_err(code::E_INVALID_HANDLE),
            Some((_, protocol, events)) if s.present && *events == s.events => Ok(*protocol),
            Some(_) => code_err(code::W_REMOVED_CARD),
        }
    }

    fn event_state(s: &State, reader: &str) -> Option<ReaderStatus> {
        let index = s.readers.iter().position(|r| r == reader)?;
        let mut st = ReaderStatus {
            name: reader.to_string(),
            ..Default::default()
        };
        if index == 0 && s.present {
            st.event_state = state::PRESENT | (s.events << 16);
            st.atr = FAKE_ATR.to_vec();
            if s.cards.values().any(|(_, _, e)| *e == s.events) {
                st.event_state |= state::INUSE;
            }
        } else {
            st.event_state = state::EMPTY | (if index == 0 { s.events << 16 } else { 0 });
        }
        Some(st)
    }
}

fn answer(apdu: &[u8]) -> Vec<u8> {
    let ins = apdu.get(1).copied().unwrap_or(0);
    match ins {
        0xA4 => vec![0x90, 0x00],
        0xB0 => {
            let le = match apdu.get(4).copied() {
                None | Some(0) => 256,
                Some(n) => usize::from(n),
            };
            let mut v: Vec<u8> = (0..le).map(|i| i as u8).collect();
            v.extend_from_slice(&[0x90, 0x00]);
            v
        }
        0xCA => vec![0xde, 0xad, 0xbe, 0xef, 0x90, 0x00],
        _ => vec![0x6d, 0x00],
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl RemoteSmartcard for FakeSmartcard {
    fn establish_context(&self) -> Result<ScContext, SmartcardError> {
        let mut s = self.enter("establish_context".into())?;
        s.next_id += 1;
        let id = s.next_id;
        s.contexts.insert(id);
        Ok(ScContext(id))
    }

    fn release_context(&self, context: ScContext) -> Result<(), SmartcardError> {
        let mut s = self.enter(format!("release_context {}", context.0))?;
        if s.contexts.remove(&context.0) {
            s.cards.retain(|_, (c, _, _)| *c != context.0);
            Ok(())
        } else {
            code_err(code::E_INVALID_HANDLE)
        }
    }

    fn list_readers(&self, context: ScContext) -> Result<Vec<String>, SmartcardError> {
        let s = self.enter(format!("list_readers {}", context.0))?;
        if !s.contexts.contains(&context.0) {
            return code_err(code::E_INVALID_HANDLE);
        }
        if s.readers.is_empty() {
            return code_err(code::E_NO_READERS_AVAILABLE);
        }
        Ok(s.readers.clone())
    }

    fn get_status_change(
        &self,
        context: ScContext,
        timeout_ms: u32,
        readers: &[ReaderQuery],
    ) -> Result<Vec<ReaderStatus>, SmartcardError> {
        let mut s = self.enter(format!("get_status_change {}", context.0))?;
        let deadline = (timeout_ms != u32::MAX)
            .then(|| Instant::now() + Duration::from_millis(u64::from(timeout_ms)));
        loop {
            if s.link_down {
                return Err(SmartcardError::Link("fake link down".into()));
            }
            if s.cancelled.remove(&context.0) {
                return code_err(code::E_CANCELLED);
            }
            let mut out = Vec::new();
            let mut differs = false;
            for q in readers {
                let Some(mut st) = Self::event_state(&s, &q.name) else {
                    return code_err(code::E_UNKNOWN_READER);
                };
                if q.current_state & !state::CHANGED != st.event_state {
                    st.event_state |= state::CHANGED;
                    differs = true;
                }
                out.push(st);
            }
            if differs {
                return Ok(out);
            }
            s = match deadline {
                None => self.changed.wait(s).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return code_err(code::E_TIMEOUT);
                    }
                    self.changed
                        .wait_timeout(s, d - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
    }

    fn cancel(&self, context: ScContext) -> Result<(), SmartcardError> {
        let mut s = self.enter(format!("cancel {}", context.0))?;
        s.cancelled.insert(context.0);
        self.changed.notify_all();
        Ok(())
    }

    fn connect(
        &self,
        context: ScContext,
        reader: &str,
        share_mode: u32,
        preferred_protocols: u32,
    ) -> Result<Connected, SmartcardError> {
        let mut s = self.enter(format!(
            "connect {} {reader} {share_mode} {preferred_protocols}",
            context.0
        ))?;
        if !s.contexts.contains(&context.0) {
            return code_err(code::E_INVALID_HANDLE);
        }
        if s.readers.first().map(String::as_str) != Some(reader) {
            return code_err(code::E_UNKNOWN_READER);
        }
        if !s.present {
            return code_err(code::E_NO_SMARTCARD);
        }
        let protocol = if preferred_protocols & 2 != 0 {
            2
        } else if preferred_protocols & 1 != 0 {
            1
        } else {
            return code_err(code::E_PROTO_MISMATCH);
        };
        s.next_id += 1;
        let id = s.next_id;
        let events = s.events;
        s.cards.insert(id, (context.0, protocol, events));
        self.changed.notify_all();
        Ok(Connected {
            card: ScCard(id),
            protocol,
        })
    }

    fn reconnect(
        &self,
        card: ScCard,
        _share_mode: u32,
        preferred_protocols: u32,
        initialization: u32,
    ) -> Result<u32, SmartcardError> {
        let s = self.enter(format!(
            "reconnect {} {preferred_protocols} {initialization}",
            card.0
        ))?;
        Self::card_protocol(&s, card)
    }

    fn disconnect(&self, card: ScCard, disposition: u32) -> Result<(), SmartcardError> {
        let mut s = self.enter(format!("disconnect {} {disposition}", card.0))?;
        let had = s.cards.remove(&card.0).is_some();
        self.changed.notify_all();
        if had {
            Ok(())
        } else {
            code_err(code::E_INVALID_HANDLE)
        }
    }

    fn begin_transaction(&self, card: ScCard) -> Result<(), SmartcardError> {
        let s = self.enter(format!("begin_transaction {}", card.0))?;
        Self::card_protocol(&s, card).map(|_| ())
    }

    fn end_transaction(&self, card: ScCard, disposition: u32) -> Result<(), SmartcardError> {
        let s = self.enter(format!("end_transaction {} {disposition}", card.0))?;
        Self::card_protocol(&s, card).map(|_| ())
    }

    fn transmit(
        &self,
        card: ScCard,
        protocol: u32,
        apdu: &[u8],
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError> {
        let s = self.enter(format!("transmit {} {protocol} {}", card.0, hex(apdu)))?;
        Self::card_protocol(&s, card)?;
        let a = answer(apdu);
        if a.len() > max_response as usize {
            return code_err(code::E_INSUFFICIENT_BUFFER);
        }
        Ok(a)
    }

    fn status(&self, card: ScCard) -> Result<CardStatus, SmartcardError> {
        let s = self.enter(format!("status {}", card.0))?;
        let protocol = Self::card_protocol(&s, card)?;
        Ok(CardStatus {
            reader: s.readers[0].clone(),
            state: 6,
            protocol,
            atr: FAKE_ATR.to_vec(),
        })
    }

    fn control(
        &self,
        card: ScCard,
        control_code: u32,
        input: &[u8],
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError> {
        let s = self.enter(format!(
            "control {} {control_code:#x} {}",
            card.0,
            hex(input)
        ))?;
        Self::card_protocol(&s, card)?;
        if input.len() > max_response as usize {
            return code_err(code::E_INSUFFICIENT_BUFFER);
        }
        Ok(input.to_vec())
    }

    fn get_attrib(
        &self,
        card: ScCard,
        attribute: u32,
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError> {
        let s = self.enter(format!("get_attrib {} {attribute:#x}", card.0))?;
        Self::card_protocol(&s, card)?;
        let value = match attribute {
            ATTR_ATR_STRING => FAKE_ATR.to_vec(),
            _ => return code_err(code::E_UNSUPPORTED_FEATURE),
        };
        if value.len() > max_response as usize {
            return code_err(code::E_INSUFFICIENT_BUFFER);
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(state: u32) -> Vec<ReaderQuery> {
        vec![ReaderQuery {
            name: FAKE_READER.into(),
            current_state: state,
        }]
    }

    #[test]
    fn the_fake_answers_like_a_card() {
        let f = FakeSmartcard::new();
        let ctx = f.establish_context().unwrap();
        assert_eq!(f.list_readers(ctx).unwrap(), vec![FAKE_READER.to_string()]);
        let c = f.connect(ctx, FAKE_READER, 2, 3).unwrap();
        assert_eq!(c.protocol, 2);
        assert_eq!(
            f.transmit(c.card, 2, &[0, 0xA4, 4, 0], 258).unwrap(),
            vec![0x90, 0]
        );
        assert_eq!(
            f.transmit(c.card, 2, &[0, 0xB0, 0, 0, 4], 258).unwrap(),
            vec![0, 1, 2, 3, 0x90, 0]
        );
        assert_eq!(
            f.transmit(c.card, 2, &[0, 0xB0, 0, 0, 0], 258)
                .unwrap()
                .len(),
            258
        );
        assert_eq!(
            f.transmit(c.card, 2, &[0, 0xB0, 0, 0, 0], 100)
                .unwrap_err()
                .scard_code(),
            code::E_INSUFFICIENT_BUFFER
        );
        assert_eq!(f.transmit(c.card, 2, &[0, 0xCA], 10).unwrap().len(), 6);
        assert_eq!(
            f.transmit(c.card, 2, &[0, 0x01], 10).unwrap(),
            vec![0x6d, 0]
        );
        assert_eq!(f.status(c.card).unwrap().atr, FAKE_ATR);
        assert_eq!(f.control(c.card, 1, &[1, 2], 10).unwrap(), vec![1, 2]);
        assert_eq!(
            f.get_attrib(c.card, ATTR_ATR_STRING, 264).unwrap(),
            FAKE_ATR
        );
        assert!(f.get_attrib(c.card, 1, 264).is_err());
        f.begin_transaction(c.card).unwrap();
        f.end_transaction(c.card, 0).unwrap();
        assert_eq!(f.reconnect(c.card, 2, 3, 1).unwrap(), 2);
        f.disconnect(c.card, 0).unwrap();
        assert!(f.disconnect(c.card, 0).is_err());
        f.release_context(ctx).unwrap();
        assert_eq!(f.open_contexts(), 0);
        assert!(f.count("transmit") == 6 && f.calls().len() > 10);
    }

    #[test]
    fn connect_checks_the_reader_the_card_and_the_protocol() {
        let f = FakeSmartcard::new();
        let ctx = f.establish_context().unwrap();
        assert_eq!(
            f.connect(ctx, "Other", 2, 3).unwrap_err().scard_code(),
            code::E_UNKNOWN_READER
        );
        assert_eq!(
            f.connect(ctx, FAKE_READER, 2, 8).unwrap_err().scard_code(),
            code::E_PROTO_MISMATCH
        );
        assert_eq!(
            f.connect(ScContext(99), FAKE_READER, 2, 3)
                .unwrap_err()
                .scard_code(),
            code::E_INVALID_HANDLE
        );
        assert_eq!(f.connect(ctx, FAKE_READER, 2, 1).unwrap().protocol, 1);
        let c = f.connect(ctx, FAKE_READER, 2, 3).unwrap();
        f.remove_card();
        assert_eq!(
            f.open_cards(),
            2,
            "handles outlive the card until disconnect"
        );
        assert_eq!(
            f.transmit(c.card, 2, &[0, 0xA4], 10)
                .unwrap_err()
                .scard_code(),
            code::W_REMOVED_CARD
        );
        assert_eq!(
            f.status(c.card).unwrap_err().scard_code(),
            code::W_REMOVED_CARD
        );
        assert_eq!(
            f.connect(ctx, FAKE_READER, 2, 3).unwrap_err().scard_code(),
            code::E_NO_SMARTCARD
        );
        f.insert_card();
        assert_eq!(
            f.status(c.card).unwrap_err().scard_code(),
            code::W_REMOVED_CARD,
            "a new card is not the old one"
        );
        f.disconnect(c.card, 0).unwrap();
        assert!(f.connect(ctx, FAKE_READER, 2, 3).is_ok());
    }

    #[test]
    fn status_change_waits_for_a_difference() {
        let f = std::sync::Arc::new(FakeSmartcard::new());
        let ctx = f.establish_context().unwrap();
        let st = f.get_status_change(ctx, 0, &query(0)).unwrap();
        assert_eq!(st[0].event_state & 0xFFFF, state::PRESENT | state::CHANGED);
        assert_eq!(st[0].atr, FAKE_ATR);
        let known = st[0].event_state & !state::CHANGED;
        assert_eq!(
            f.get_status_change(ctx, 20, &query(known))
                .unwrap_err()
                .scard_code(),
            code::E_TIMEOUT
        );
        let f2 = f.clone();
        let t = std::thread::spawn(move || f2.get_status_change(ctx, u32::MAX, &query(known)));
        std::thread::sleep(Duration::from_millis(50));
        f.remove_card();
        let st = t.join().unwrap().unwrap();
        assert_ne!(st[0].event_state & state::EMPTY, 0);
        assert_eq!(st[0].event_state >> 16, 1);
        // unknown readers
        let q = vec![ReaderQuery {
            name: "zz".into(),
            current_state: 0,
        }];
        assert_eq!(
            f.get_status_change(ctx, 0, &q).unwrap_err().scard_code(),
            code::E_UNKNOWN_READER
        );
    }

    #[test]
    fn cancel_and_a_dead_link_end_a_wait() {
        let f = std::sync::Arc::new(FakeSmartcard::new());
        let ctx = f.establish_context().unwrap();
        let known = state::PRESENT;
        let f2 = f.clone();
        let t = std::thread::spawn(move || f2.get_status_change(ctx, u32::MAX, &query(known)));
        assert!(f.wait_for("get_status_change", 1, Duration::from_secs(2)));
        std::thread::sleep(Duration::from_millis(30));
        f.cancel(ctx).unwrap();
        assert_eq!(
            t.join().unwrap().unwrap_err().scard_code(),
            code::E_CANCELLED
        );
        let f2 = f.clone();
        let t = std::thread::spawn(move || f2.get_status_change(ctx, u32::MAX, &query(known)));
        std::thread::sleep(Duration::from_millis(30));
        f.fail_link();
        assert!(matches!(t.join().unwrap(), Err(SmartcardError::Link(_))));
        assert!(f.establish_context().is_err());
    }

    #[test]
    fn readers_can_be_changed() {
        let f = FakeSmartcard::new();
        let ctx = f.establish_context().unwrap();
        f.set_readers(&["A", "B"]);
        assert_eq!(f.list_readers(ctx).unwrap().len(), 2);
        let q = vec![ReaderQuery {
            name: "B".into(),
            current_state: 0,
        }];
        assert_eq!(
            f.get_status_change(ctx, 0, &q).unwrap()[0].event_state & 0xFFFF,
            state::EMPTY | state::CHANGED
        );
        f.set_readers(&[]);
        assert_eq!(
            f.list_readers(ctx).unwrap_err().scard_code(),
            code::E_NO_READERS_AVAILABLE
        );
        assert_eq!(
            f.list_readers(ScContext(77)).unwrap_err().scard_code(),
            code::E_INVALID_HANDLE
        );
    }
}
