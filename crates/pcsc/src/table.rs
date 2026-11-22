//! The table of readers that the applications read (`READER_STATE[16]`), kept from what the client's smart card
//! subsystem reports. Pure: the monitor feeds it, the connections only read its encoded form.

use oxydesk_core::smartcard::{card_present, event_count, state, ReaderQuery, ReaderStatus};

use crate::wire::{
    self, reader_state, sharing, ReaderRecord, MAX_ATR_SIZE, MAX_READERNAME, MAX_READERS,
};

/// A reader of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderEntry {
    /// The name the applications see (unique, shorter than `MAX_READERNAME`).
    pub name: String,
    /// The name the client's subsystem knows the reader by.
    pub backend_name: String,
    pub event_counter: u32,
    /// `SCARD_*` bits of the reader (`wire::reader_state`).
    pub state: u32,
    pub sharing: i32,
    pub atr: Vec<u8>,
    /// Protocol of the last connection (`SCARD_PROTOCOL_*`), 0 when the card was not connected since it came.
    pub protocol: u32,
    /// The last event state reported by the client, given back to it as the state that the monitor knows.
    last_event_state: u32,
    observed: bool,
}

impl ReaderEntry {
    fn new(name: String, backend_name: String) -> Self {
        Self {
            name,
            backend_name,
            event_counter: 0,
            state: reader_state::UNKNOWN,
            sharing: sharing::NONE,
            atr: Vec::new(),
            protocol: 0,
            last_event_state: state::UNAWARE,
            observed: false,
        }
    }

    fn record(&self) -> ReaderRecord {
        let mut reader_state = self.state;
        // Once an application chose a protocol the card is no longer waiting for the negotiation (PTS).
        if self.protocol != 0 && reader_state & reader_state::NEGOTIABLE != 0 {
            reader_state = (reader_state & !reader_state::NEGOTIABLE) | reader_state::SPECIFIC;
        }
        ReaderRecord {
            name: self.name.clone(),
            event_counter: self.event_counter,
            reader_state,
            sharing: self.sharing,
            atr: self.atr.clone(),
            protocol: self.protocol,
        }
    }
}

/// The state a card is in for pcsc-lite, from the `SCARD_STATE_*` flags of the client.
pub fn translate_state(event_state: u32) -> u32 {
    let flags = event_state & state::FLAGS_MASK;
    if flags & (state::UNKNOWN | state::UNAVAILABLE | state::IGNORE) != 0 {
        reader_state::UNKNOWN
    } else if flags & state::PRESENT != 0 {
        if flags & (state::MUTE | state::UNPOWERED) != 0 {
            reader_state::PRESENT | reader_state::SWALLOWED
        } else {
            reader_state::PRESENT | reader_state::POWERED | reader_state::NEGOTIABLE
        }
    } else {
        reader_state::ABSENT
    }
}

/// The sharing of a reader (`PCSCLITE_SHARING_*`) from the `SCARD_STATE_*` flags of the client.
pub fn translate_sharing(event_state: u32) -> i32 {
    if event_state & state::EXCLUSIVE != 0 {
        sharing::EXCLUSIVE
    } else if event_state & state::INUSE != 0 {
        sharing::IN_USE
    } else {
        sharing::NONE
    }
}

/// A name the applications can use: no control characters, shorter than the name field, not in `taken`.
pub fn expose_name(raw: &str, taken: &[&str]) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    let base = if cleaned.is_empty() {
        "Smart card reader"
    } else {
        cleaned
    };
    let fit = |s: &str, max: usize| -> String {
        let mut end = s.len().min(max);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].trim_end().to_string()
    };
    let first = fit(base, MAX_READERNAME - 1);
    if !taken.contains(&first.as_str()) {
        return first;
    }
    for n in 1u32.. {
        let suffix = format!(" {n:02}");
        let candidate = format!("{}{suffix}", fit(base, MAX_READERNAME - 1 - suffix.len()));
        if !taken.contains(&candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("the suffixes are not exhausted")
}

/// The readers of the client and their state.
#[derive(Debug, Clone, Default)]
pub struct ReaderTable {
    entries: Vec<ReaderEntry>,
    reader_events: u32,
}

impl ReaderTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &[ReaderEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of readers that came or went (`CMD_GET_READER_EVENTS`), modulo 2^16.
    pub fn reader_events(&self) -> u32 {
        self.reader_events
    }

    /// The entry an application names.
    pub fn lookup(&self, name: &str) -> Option<&ReaderEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Takes the list of readers of the client: keeps the known ones, adds the new ones (at most
    /// [`MAX_READERS`]), drops the missing ones. `true` when the list changed.
    pub fn sync_readers(&mut self, backend_names: &[String]) -> bool {
        let wanted = &backend_names[..backend_names.len().min(MAX_READERS)];
        if self.entries.len() == wanted.len()
            && self
                .entries
                .iter()
                .zip(wanted)
                .all(|(e, n)| &e.backend_name == n)
        {
            return false;
        }
        let mut next: Vec<ReaderEntry> = Vec::with_capacity(wanted.len());
        for name in wanted {
            if next.iter().any(|e| &e.backend_name == name) {
                continue; // the client listed the same reader twice
            }
            let entry = match self.entries.iter().find(|e| &e.backend_name == name) {
                Some(known) => known.clone(),
                None => {
                    let taken: Vec<&str> = next
                        .iter()
                        .map(|e| e.name.as_str())
                        .chain(
                            self.entries
                                .iter()
                                .filter(|e| wanted.contains(&e.backend_name))
                                .map(|e| e.name.as_str()),
                        )
                        .collect();
                    ReaderEntry::new(expose_name(name, &taken), name.clone())
                }
            };
            next.push(entry);
        }
        let changed =
            next.len() != self.entries.len() || next.iter().zip(&self.entries).any(|(a, b)| a != b);
        self.entries = next;
        if changed {
            self.reader_events = (self.reader_events + 1) & 0xFFFF;
        }
        changed
    }

    /// What to ask the client: each reader with the state already known.
    pub fn queries(&self) -> Vec<ReaderQuery> {
        self.entries
            .iter()
            .map(|e| ReaderQuery {
                name: e.backend_name.clone(),
                current_state: e.last_event_state,
            })
            .collect()
    }

    /// Takes the answer of the client. `true` when something an application can see changed.
    pub fn apply_status(&mut self, statuses: &[ReaderStatus]) -> bool {
        let mut changed = false;
        for (i, status) in statuses.iter().enumerate() {
            let slot = self
                .entries
                .iter()
                .position(|e| e.backend_name == status.name)
                .or_else(|| (status.name.is_empty() && i < self.entries.len()).then_some(i));
            if let Some(slot) = slot {
                changed |= self.entries[slot].apply(status);
            }
        }
        changed
    }

    /// The client cannot be asked: every reader is `SCARD_UNKNOWN` until it answers again.
    pub fn mark_unknown(&mut self) -> bool {
        let mut changed = false;
        for e in &mut self.entries {
            if e.state != reader_state::UNKNOWN {
                e.state = reader_state::UNKNOWN;
                e.atr.clear();
                e.protocol = 0;
                e.sharing = sharing::NONE;
                changed = true;
            }
            e.last_event_state = state::UNAWARE;
        }
        changed
    }

    /// An application connected to the card of `name` with this protocol (0 forgets it).
    pub fn note_protocol(&mut self, name: &str, protocol: u32) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.name == name) {
            e.protocol = protocol;
        }
    }

    pub fn records(&self) -> Vec<ReaderRecord> {
        self.entries.iter().map(ReaderEntry::record).collect()
    }

    /// The bytes of a `CMD_GET_READERS_STATE` answer.
    pub fn encode(&self) -> Vec<u8> {
        wire::encode_table(&self.records())
    }
}

impl ReaderEntry {
    fn apply(&mut self, status: &ReaderStatus) -> bool {
        let before = (
            self.event_counter,
            self.state,
            self.sharing,
            self.atr.clone(),
            self.protocol,
        );
        let new_state = translate_state(status.event_state);
        let was_present = self.state & reader_state::PRESENT != 0;
        let now_present = new_state & reader_state::PRESENT != 0;
        if self.observed {
            let card_event = was_present != now_present
                || (now_present
                    && event_count(status.event_state) != event_count(self.last_event_state));
            if card_event {
                self.event_counter = (self.event_counter + 1) & 0xFFFF;
                self.protocol = 0;
            }
        }
        self.observed = true;
        self.state = new_state;
        self.sharing = translate_sharing(status.event_state);
        if card_present(status.event_state) && new_state & reader_state::PRESENT != 0 {
            self.atr = status.atr[..status.atr.len().min(MAX_ATR_SIZE)].to_vec();
        } else {
            self.atr.clear();
            self.protocol = 0;
        }
        // The client reports `CHANGED` in what it answers; it is not part of the state it compares against.
        self.last_event_state = status.event_state & !state::CHANGED;
        before
            != (
                self.event_counter,
                self.state,
                self.sharing,
                self.atr.clone(),
                self.protocol,
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn st(name: &str, event_state: u32, atr: &[u8]) -> ReaderStatus {
        ReaderStatus {
            name: name.into(),
            event_state,
            atr: atr.to_vec(),
        }
    }

    const PRESENT: u32 = state::PRESENT | state::CHANGED;
    const EMPTY: u32 = state::EMPTY | state::CHANGED;

    #[test]
    fn states_are_translated_to_the_pcsc_lite_bits() {
        assert_eq!(translate_state(state::EMPTY), reader_state::ABSENT);
        assert_eq!(translate_state(0), reader_state::ABSENT);
        assert_eq!(
            translate_state(state::PRESENT),
            reader_state::PRESENT | reader_state::POWERED | reader_state::NEGOTIABLE
        );
        assert_eq!(
            translate_state(state::PRESENT | state::MUTE),
            reader_state::PRESENT | reader_state::SWALLOWED
        );
        assert_eq!(
            translate_state(state::PRESENT | state::UNPOWERED),
            reader_state::PRESENT | reader_state::SWALLOWED
        );
        assert_eq!(translate_state(state::UNKNOWN), reader_state::UNKNOWN);
        assert_eq!(translate_state(state::UNAVAILABLE), reader_state::UNKNOWN);
        assert_eq!(translate_state(state::IGNORE), reader_state::UNKNOWN);
        // The counter of the upper half is not part of the state.
        assert_eq!(translate_state(0x0005_0010), reader_state::ABSENT);
    }

    #[test]
    fn sharing_is_translated() {
        assert_eq!(translate_sharing(state::PRESENT), sharing::NONE);
        assert_eq!(
            translate_sharing(state::PRESENT | state::INUSE),
            sharing::IN_USE
        );
        assert_eq!(
            translate_sharing(state::PRESENT | state::INUSE | state::EXCLUSIVE),
            sharing::EXCLUSIVE
        );
    }

    #[test]
    fn names_are_cleaned_cut_and_made_unique() {
        assert_eq!(expose_name("Reader", &[]), "Reader");
        assert_eq!(expose_name("  A\u{0}B\nC ", &[]), "A B C");
        assert_eq!(expose_name("", &[]), "Smart card reader");
        assert_eq!(expose_name("Reader", &["Reader"]), "Reader 01");
        assert_eq!(expose_name("Reader", &["Reader", "Reader 01"]), "Reader 02");
        let long = "é".repeat(100);
        let n = expose_name(&long, &[]);
        assert!(n.len() <= 127 && n.chars().all(|c| c == 'é'), "{}", n.len());
        let n2 = expose_name(&long, &[n.as_str()]);
        assert!(n2.len() <= 127 && n2.ends_with(" 01") && n2 != n);
        let ascii = "x".repeat(200);
        assert_eq!(expose_name(&ascii, &[]).len(), 127);
    }

    #[test]
    fn readers_come_and_go_and_events_are_counted() {
        let mut t = ReaderTable::new();
        assert!(t.is_empty());
        assert!(!t.sync_readers(&[]), "no readers is not a change");
        assert!(t.sync_readers(&names(&["A", "B"])));
        assert_eq!(t.reader_events(), 1);
        assert!(!t.sync_readers(&names(&["A", "B"])));
        assert_eq!(t.reader_events(), 1);
        assert!(t.sync_readers(&names(&["B"])));
        assert_eq!(t.entries().len(), 1);
        assert_eq!(t.reader_events(), 2);
        assert!(t.sync_readers(&[]));
        assert!(t.is_empty());
        assert_eq!(t.reader_events(), 3);
    }

    #[test]
    fn a_known_reader_keeps_its_state_when_the_list_changes() {
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["A"]));
        t.apply_status(&[st("A", PRESENT, &[0x3b, 0])]);
        t.sync_readers(&names(&["A", "B"]));
        assert_eq!(t.lookup("A").unwrap().atr, vec![0x3b, 0]);
        assert!(t.lookup("B").is_some());
    }

    #[test]
    fn the_list_is_limited_and_deduplicated() {
        let mut t = ReaderTable::new();
        let many: Vec<String> = (0..20).map(|i| format!("R{i}")).collect();
        t.sync_readers(&many);
        assert_eq!(t.entries().len(), MAX_READERS);
        t.sync_readers(&names(&["A", "A"]));
        assert_eq!(t.entries().len(), 1);
        t.sync_readers(&names(&["X", "Y"]));
        // two readers called alike on the client keep distinct names here
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["Twin", "Twin\n"]));
        let n: Vec<&str> = t.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(n, vec!["Twin", "Twin 01"]);
    }

    #[test]
    fn insertion_and_removal_bump_the_counter() {
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["A"]));
        // first observation: no event counted, whatever the state
        assert!(t.apply_status(&[st("A", PRESENT, &[0x3b])]));
        assert_eq!(t.lookup("A").unwrap().event_counter, 0);
        assert_eq!(
            t.lookup("A").unwrap().state,
            reader_state::PRESENT | reader_state::POWERED | reader_state::NEGOTIABLE
        );
        // the same state again changes nothing
        assert!(!t.apply_status(&[st("A", PRESENT, &[0x3b])]));
        // removal
        assert!(t.apply_status(&[st("A", EMPTY, &[])]));
        let e = t.lookup("A").unwrap();
        assert_eq!((e.event_counter, e.state), (1, reader_state::ABSENT));
        assert!(e.atr.is_empty());
        // insertion
        assert!(t.apply_status(&[st("A", PRESENT, &[0x3b, 1])]));
        let e = t.lookup("A").unwrap();
        assert_eq!(e.event_counter, 2);
        assert_eq!(e.atr, vec![0x3b, 1]);
    }

    #[test]
    fn a_swap_between_two_looks_is_an_event_too() {
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["A"]));
        t.apply_status(&[st("A", PRESENT | (1 << 16), &[0x3b])]);
        // still present but the client counted two more events
        assert!(t.apply_status(&[st("A", PRESENT | (3 << 16), &[0x3b, 9])]));
        assert_eq!(t.lookup("A").unwrap().event_counter, 1);
        assert_eq!(t.lookup("A").unwrap().atr, vec![0x3b, 9]);
    }

    #[test]
    fn the_counter_wraps_at_16_bits() {
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["A"]));
        t.apply_status(&[st("A", EMPTY, &[])]);
        for _ in 0..0x10000 {
            t.apply_status(&[st("A", PRESENT, &[1])]);
            t.apply_status(&[st("A", EMPTY, &[])]);
            // two events per turn
        }
        assert!(t.lookup("A").unwrap().event_counter <= 0xFFFF);
        assert_eq!(t.lookup("A").unwrap().event_counter, 0);
    }

    #[test]
    fn the_client_is_asked_with_the_state_already_known() {
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["A"]));
        assert_eq!(t.queries()[0].current_state, state::UNAWARE);
        t.apply_status(&[st("A", PRESENT | (2 << 16), &[1])]);
        let q = t.queries();
        assert_eq!(q[0].name, "A");
        assert_eq!(
            q[0].current_state,
            state::PRESENT | (2 << 16),
            "CHANGED is dropped"
        );
    }

    #[test]
    fn answers_for_unknown_readers_are_ignored_and_an_empty_name_means_the_slot() {
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["A"]));
        assert!(!t.apply_status(&[st("Z", PRESENT, &[1])]));
        assert!(t.apply_status(&[st("", PRESENT, &[1])]));
        assert!(!t.apply_status(&[st("", PRESENT, &[1]), st("", PRESENT, &[1])]));
    }

    #[test]
    fn unknown_marks_every_reader_once() {
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["A"]));
        t.apply_status(&[st("A", PRESENT | state::INUSE, &[1])]);
        assert!(t.mark_unknown());
        let e = t.lookup("A").unwrap();
        assert_eq!(e.state, reader_state::UNKNOWN);
        assert!(e.atr.is_empty());
        assert_eq!(e.sharing, sharing::NONE);
        assert!(!t.mark_unknown());
        assert_eq!(t.queries()[0].current_state, state::UNAWARE);
    }

    #[test]
    fn the_protocol_is_remembered_until_the_card_changes() {
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["A"]));
        t.apply_status(&[st("A", PRESENT, &[1])]);
        t.note_protocol("A", 2);
        t.note_protocol("nobody", 2);
        let r = &t.records()[0];
        assert_eq!(r.protocol, 2);
        assert_eq!(
            r.reader_state,
            reader_state::PRESENT | reader_state::POWERED | reader_state::SPECIFIC
        );
        // sharing changes do not forget it
        t.apply_status(&[st("A", PRESENT | state::INUSE, &[1])]);
        assert_eq!(t.records()[0].protocol, 2);
        // a removal does
        t.apply_status(&[st("A", EMPTY, &[])]);
        assert_eq!(t.records()[0].protocol, 0);
    }

    #[test]
    fn a_long_atr_is_cut() {
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["A"]));
        t.apply_status(&[st("A", PRESENT, &[7; 60])]);
        assert_eq!(t.lookup("A").unwrap().atr.len(), 33);
    }

    #[test]
    fn the_encoded_table_lists_the_readers() {
        let mut t = ReaderTable::new();
        t.sync_readers(&names(&["A", "B"]));
        t.apply_status(&[st("A", PRESENT, &[1, 2]), st("B", EMPTY, &[])]);
        let rec = wire::decode_table(&t.encode()).unwrap();
        assert_eq!(rec.len(), 2);
        assert_eq!(rec[0].name, "A");
        assert_eq!(rec[0].atr, vec![1, 2]);
        assert_eq!(rec[1].reader_state, reader_state::ABSENT);
    }
}
