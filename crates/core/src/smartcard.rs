//! Smart card readers redirected by the client (RDPDR, MS-RDPESC): the readers of the client's machine,
//! seen by the desktop as PC/SC readers.
//!
//! `RemoteSmartcard` is the port the desktop side uses (a PC/SC socket for the applications of the session):
//! blocking and not tied to any protocol. Its calls mirror the winscard API (`SCardEstablishContext`,
//! `SCardTransmit`, ...), because that is exactly what the client's smart card subsystem executes. `SmartcardHost`
//! is what the RDP side calls when the client announces or removes its smart card device.
//!
//! The numeric values (error codes, state flags, protocols) are the winscard ones: MS-RDPESC and pcsc-lite share
//! them, so they cross the port unchanged.

use std::sync::Arc;

use crate::error::SmartcardError;

/// An opaque handle of a context of the client's smart card subsystem. Minted by the adapter of the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScContext(pub u64);

/// An opaque handle of a connection to a card (`SCARDHANDLE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScCard(pub u64);

/// `SCARD_*` result codes (winscard.h) that the desktop side reacts to.
pub mod code {
    pub const SUCCESS: u32 = 0;
    pub const F_INTERNAL_ERROR: u32 = 0x8010_0001;
    pub const E_CANCELLED: u32 = 0x8010_0002;
    pub const E_INVALID_HANDLE: u32 = 0x8010_0003;
    pub const E_INVALID_PARAMETER: u32 = 0x8010_0004;
    pub const E_NO_MEMORY: u32 = 0x8010_0006;
    pub const E_INSUFFICIENT_BUFFER: u32 = 0x8010_0008;
    pub const E_UNKNOWN_READER: u32 = 0x8010_0009;
    pub const E_TIMEOUT: u32 = 0x8010_000A;
    pub const E_SHARING_VIOLATION: u32 = 0x8010_000B;
    pub const E_NO_SMARTCARD: u32 = 0x8010_000C;
    pub const E_PROTO_MISMATCH: u32 = 0x8010_000F;
    pub const E_NOT_READY: u32 = 0x8010_0010;
    pub const E_INVALID_VALUE: u32 = 0x8010_0011;
    pub const F_COMM_ERROR: u32 = 0x8010_0013;
    pub const E_READER_UNAVAILABLE: u32 = 0x8010_0017;
    pub const E_NO_SERVICE: u32 = 0x8010_001D;
    pub const E_SERVICE_STOPPED: u32 = 0x8010_001E;
    pub const E_UNSUPPORTED_FEATURE: u32 = 0x8010_001F;
    pub const E_NO_READERS_AVAILABLE: u32 = 0x8010_002E;
    pub const W_UNSUPPORTED_CARD: u32 = 0x8010_0065;
    pub const W_UNRESPONSIVE_CARD: u32 = 0x8010_0066;
    pub const W_UNPOWERED_CARD: u32 = 0x8010_0067;
    pub const W_RESET_CARD: u32 = 0x8010_0068;
    pub const W_REMOVED_CARD: u32 = 0x8010_0069;
}

/// `SCARD_STATE_*` flags of a reader (winscard.h). The upper 16 bits of an event state count the card events.
pub mod state {
    pub const UNAWARE: u32 = 0x0000;
    pub const IGNORE: u32 = 0x0001;
    pub const CHANGED: u32 = 0x0002;
    pub const UNKNOWN: u32 = 0x0004;
    pub const UNAVAILABLE: u32 = 0x0008;
    pub const EMPTY: u32 = 0x0010;
    pub const PRESENT: u32 = 0x0020;
    pub const ATRMATCH: u32 = 0x0040;
    pub const EXCLUSIVE: u32 = 0x0080;
    pub const INUSE: u32 = 0x0100;
    pub const MUTE: u32 = 0x0200;
    pub const UNPOWERED: u32 = 0x0400;
    /// The bits that describe the reader; the rest is the event counter.
    pub const FLAGS_MASK: u32 = 0xFFFF;
}

/// Longest ATR a card can answer (ISO 7816-3).
pub const MAX_ATR: usize = 33;

/// What an application asks about one reader in a `SCardGetStatusChange`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderQuery {
    pub name: String,
    /// The state the application already knows (`state::UNAWARE` for the first call).
    pub current_state: u32,
}

/// What the client answers about one reader.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReaderStatus {
    pub name: String,
    /// `state::*` flags, and the event counter in the upper 16 bits.
    pub event_state: u32,
    pub atr: Vec<u8>,
}

/// A connection to a card that was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Connected {
    pub card: ScCard,
    /// The protocol in use (`SCARD_PROTOCOL_T0` = 1, `T1` = 2, `RAW` = 4).
    pub protocol: u32,
}

/// `SCardStatus`: the reader a card is in, its state, protocol and ATR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardStatus {
    pub reader: String,
    /// `SCARD_ABSENT` (1) .. `SCARD_SPECIFIC` (6), the Windows values.
    pub state: u32,
    pub protocol: u32,
    pub atr: Vec<u8>,
}

/// The smart card subsystem of the client. Every call blocks until the client answered, except that a call
/// stuck on a missing client answer ends with [`SmartcardError::Link`] once the client is gone.
/// `get_status_change` may block for as long as the application allows; `cancel` ends it.
pub trait RemoteSmartcard: Send + Sync {
    fn establish_context(&self) -> Result<ScContext, SmartcardError>;
    fn release_context(&self, context: ScContext) -> Result<(), SmartcardError>;
    fn list_readers(&self, context: ScContext) -> Result<Vec<String>, SmartcardError>;
    /// Waits until the state of one reader differs from `current_state`, or `timeout_ms` elapsed
    /// (`u32::MAX`: no limit, which is `INFINITE`). Returns one status per query, in the same order.
    fn get_status_change(
        &self,
        context: ScContext,
        timeout_ms: u32,
        readers: &[ReaderQuery],
    ) -> Result<Vec<ReaderStatus>, SmartcardError>;
    /// Ends the `get_status_change` that is blocked on `context` (it fails with `E_CANCELLED`).
    fn cancel(&self, context: ScContext) -> Result<(), SmartcardError>;
    fn connect(
        &self,
        context: ScContext,
        reader: &str,
        share_mode: u32,
        preferred_protocols: u32,
    ) -> Result<Connected, SmartcardError>;
    /// Returns the protocol in use.
    fn reconnect(
        &self,
        card: ScCard,
        share_mode: u32,
        preferred_protocols: u32,
        initialization: u32,
    ) -> Result<u32, SmartcardError>;
    fn disconnect(&self, card: ScCard, disposition: u32) -> Result<(), SmartcardError>;
    fn begin_transaction(&self, card: ScCard) -> Result<(), SmartcardError>;
    fn end_transaction(&self, card: ScCard, disposition: u32) -> Result<(), SmartcardError>;
    /// Sends an APDU with the given protocol (T=0 or T=1) and returns the answer. `max_response` is the size of
    /// the buffer of the application.
    fn transmit(
        &self,
        card: ScCard,
        protocol: u32,
        apdu: &[u8],
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError>;
    fn status(&self, card: ScCard) -> Result<CardStatus, SmartcardError>;
    /// `SCardControl`: a reader-specific command (PIN pad, firmware...).
    fn control(
        &self,
        card: ScCard,
        control_code: u32,
        input: &[u8],
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError>;
    fn get_attrib(
        &self,
        card: ScCard,
        attribute: u32,
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError>;
}

/// Where the smart card device announced by the client is made available to the desktop.
pub trait SmartcardHost: Send + Sync {
    /// The client announced its smart card device (all its readers). Called outside the connection thread.
    fn attached(&self, smartcard: Arc<dyn RemoteSmartcard>);
    /// The device is gone (client removed it or disconnected); the calls in progress must fail.
    fn detached(&self);
}

/// `true` when `event_state` says that a card is in the reader.
pub fn card_present(event_state: u32) -> bool {
    event_state & state::PRESENT != 0
}

/// The event counter carried by the upper 16 bits of an event state.
pub fn event_count(event_state: u32) -> u32 {
    event_state >> 16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_is_read_from_the_flags() {
        assert!(card_present(state::PRESENT | state::CHANGED));
        assert!(card_present(0x0002_0020));
        assert!(!card_present(state::EMPTY));
        assert!(!card_present(0x0003_0010));
    }

    #[test]
    fn the_counter_is_the_upper_half() {
        assert_eq!(event_count(0x0003_0022), 3);
        assert_eq!(event_count(state::PRESENT), 0);
        assert_eq!(event_count(0xFFFF_FFFF), 0xFFFF);
    }

    #[test]
    fn codes_have_the_winscard_values() {
        assert_eq!(code::E_CANCELLED, 0x8010_0002);
        assert_eq!(code::E_NO_SERVICE, 0x8010_001D);
        assert_eq!(code::W_REMOVED_CARD, 0x8010_0069);
        assert_eq!(code::E_NO_READERS_AVAILABLE, 0x8010_002E);
    }
}
