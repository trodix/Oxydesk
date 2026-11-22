//! The pcsc-lite client/server wire protocol (`src/winscard_msg.h`, protocol 4.5), as pure functions.
//!
//! Every message is a header (`size`, `command`, both native-endian `u32`) followed by a packed C struct whose
//! size is `size`; `SCARD_TRANSMIT` and `SCARD_CONTROL` are followed by `cbSendLength` more bytes that the header does
//! not count. The structs are those of the C ABI of x86_64 and aarch64 (all fields are 32 bits wide, so there is
//! no padding except inside `READER_STATE`). Nothing here touches a socket: [`read_request`] only needs a `Read`.

use std::fmt;
use std::io::{self, Read};

/// `MAX_READERNAME` (pcsclite.h): size of the buffer of a reader name, NUL included.
pub const MAX_READERNAME: usize = 128;
/// `MAX_ATR_SIZE`.
pub const MAX_ATR_SIZE: usize = 33;
/// `MAX_BUFFER_SIZE`: the buffer of the attribute messages.
pub const MAX_BUFFER_SIZE: usize = 264;
/// `MAX_BUFFER_SIZE_EXTENDED`: largest APDU or answer.
pub const MAX_BUFFER_SIZE_EXTENDED: usize = 4 + 3 + (1 << 16) + 3 + 2;
/// `PCSCLITE_MAX_READERS_CONTEXTS`: entries of the table of readers.
pub const MAX_READERS: usize = 16;
/// `sizeof(READER_STATE)`: 128 + 4 + 4 + 4 + 33 (+3 padding) + 4 + 4.
pub const READER_STATE_SIZE: usize = 184;
/// Size of the whole table sent by `CMD_GET_READERS_STATE`.
pub const TABLE_SIZE: usize = MAX_READERS * READER_STATE_SIZE;

pub const PROTOCOL_VERSION_MAJOR: i32 = 4;
pub const PROTOCOL_VERSION_MINOR: i32 = 5;
/// Oldest minor version a client may speak (`PROTOCOL_VERSION_MINOR_SERVER_BACKWARD`).
pub const PROTOCOL_VERSION_MINOR_BACKWARD: i32 = 4;

/// `enum pcsc_msg_commands`.
pub mod cmd {
    pub const ESTABLISH_CONTEXT: u32 = 0x01;
    pub const RELEASE_CONTEXT: u32 = 0x02;
    pub const LIST_READERS: u32 = 0x03;
    pub const CONNECT: u32 = 0x04;
    pub const RECONNECT: u32 = 0x05;
    pub const DISCONNECT: u32 = 0x06;
    pub const BEGIN_TRANSACTION: u32 = 0x07;
    pub const END_TRANSACTION: u32 = 0x08;
    pub const TRANSMIT: u32 = 0x09;
    pub const CONTROL: u32 = 0x0A;
    pub const STATUS: u32 = 0x0B;
    pub const CANCEL: u32 = 0x0D;
    pub const GET_ATTRIB: u32 = 0x0F;
    pub const SET_ATTRIB: u32 = 0x10;
    pub const VERSION: u32 = 0x11;
    pub const GET_READERS_STATE: u32 = 0x12;
    pub const WAIT_READER_STATE_CHANGE: u32 = 0x13;
    pub const STOP_WAITING_READER_STATE_CHANGE: u32 = 0x14;
    pub const GET_READER_EVENTS: u32 = 0x15;
}

/// `readerState` bits of `READER_STATE` (`SCARD_*`, not the `SCARD_STATE_*` of the applications).
pub mod reader_state {
    pub const UNKNOWN: u32 = 0x01;
    pub const ABSENT: u32 = 0x02;
    pub const PRESENT: u32 = 0x04;
    pub const SWALLOWED: u32 = 0x08;
    pub const POWERED: u32 = 0x10;
    pub const NEGOTIABLE: u32 = 0x20;
    pub const SPECIFIC: u32 = 0x40;
}

/// `readerSharing` values.
pub mod sharing {
    pub const EXCLUSIVE: i32 = -1;
    pub const NONE: i32 = 0;
    pub const IN_USE: i32 = 1;
}

/// Why a message could not be understood.
#[derive(Debug)]
pub enum WireError {
    /// The command is not one a client sends (or none at all).
    UnknownCommand(u32),
    /// `header.size` is not the size of the struct of the command.
    BadLength {
        command: u32,
        expected: usize,
        got: usize,
    },
    /// A length field announces more data than the protocol allows.
    TooLarge { command: u32, len: usize },
    /// The body is shorter than its struct.
    Truncated,
    /// The connection failed or ended.
    Io(io::Error),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCommand(c) => write!(f, "unknown command {c:#x}"),
            Self::BadLength {
                command,
                expected,
                got,
            } => write!(f, "command {command:#x}: size {got}, expected {expected}"),
            Self::TooLarge { command, len } => {
                write!(f, "command {command:#x}: length {len} too large")
            }
            Self::Truncated => f.write_str("truncated message"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// `struct rxHeader`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub size: u32,
    pub command: u32,
}

impl Header {
    pub const SIZE: usize = 8;

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        b[..4].copy_from_slice(&self.size.to_ne_bytes());
        b[4..].copy_from_slice(&self.command.to_ne_bytes());
        b
    }

    pub fn decode(b: &[u8; Self::SIZE]) -> Self {
        Self {
            size: u32::from_ne_bytes([b[0], b[1], b[2], b[3]]),
            command: u32::from_ne_bytes([b[4], b[5], b[6], b[7]]),
        }
    }
}

/// A request of a client. `hContext` and `hCard` are the handles the server minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Version {
        major: i32,
        minor: i32,
    },
    GetReadersState,
    WaitReaderStateChange,
    StopWaitingReaderStateChange,
    GetReaderEvents,
    EstablishContext {
        scope: u32,
    },
    ReleaseContext {
        context: u32,
    },
    Connect {
        context: u32,
        reader: String,
        share_mode: u32,
        preferred_protocols: u32,
    },
    Reconnect {
        card: i32,
        share_mode: u32,
        preferred_protocols: u32,
        initialization: u32,
    },
    Disconnect {
        card: i32,
        disposition: u32,
    },
    BeginTransaction {
        card: i32,
    },
    EndTransaction {
        card: i32,
        disposition: u32,
    },
    Cancel {
        context: u32,
    },
    Status {
        card: i32,
    },
    Transmit {
        card: i32,
        send_protocol: u32,
        send_pci_len: u32,
        recv_protocol: u32,
        recv_pci_len: u32,
        /// Size of the buffer of the application.
        recv_len: u32,
        data: Vec<u8>,
    },
    Control {
        card: i32,
        code: u32,
        recv_len: u32,
        data: Vec<u8>,
    },
    GetAttrib {
        card: i32,
        attribute: u32,
    },
    SetAttrib {
        card: i32,
        attribute: u32,
        data: Vec<u8>,
    },
}

/// Size of the struct that follows the header, or `None` for a command that no client sends.
pub fn body_size(command: u32) -> Option<usize> {
    Some(match command {
        cmd::VERSION => 12,
        cmd::GET_READERS_STATE
        | cmd::WAIT_READER_STATE_CHANGE
        | cmd::STOP_WAITING_READER_STATE_CHANGE
        | cmd::GET_READER_EVENTS => 0,
        cmd::ESTABLISH_CONTEXT => 12,
        cmd::RELEASE_CONTEXT => 8,
        cmd::CONNECT => 152,
        cmd::RECONNECT => 24,
        cmd::DISCONNECT => 12,
        cmd::BEGIN_TRANSACTION => 8,
        cmd::END_TRANSACTION => 12,
        cmd::CANCEL => 8,
        cmd::STATUS => 8,
        cmd::TRANSMIT => 32,
        cmd::CONTROL => 24,
        cmd::GET_ATTRIB | cmd::SET_ATTRIB => 280,
        _ => return None,
    })
}

/// A cursor over a body, failing with [`WireError::Truncated`].
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or(WireError::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(WireError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        let b = self.bytes(4)?;
        Ok(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Result<i32, WireError> {
        Ok(self.u32()? as i32)
    }
}

/// The text of a fixed-size NUL-terminated field (what is after the first NUL is ignored).
pub fn c_string(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Decodes the struct of `command`. `Transmit` and `Control` come back with `data` zero-filled to the announced
/// length ([`trailing_len`]): [`read_request`] reads the bytes that follow the struct into it.
pub fn decode_body(command: u32, body: &[u8]) -> Result<Request, WireError> {
    let expected = body_size(command).ok_or(WireError::UnknownCommand(command))?;
    if body.len() != expected {
        return Err(WireError::BadLength {
            command,
            expected,
            got: body.len(),
        });
    }
    let mut c = Cursor::new(body);
    Ok(match command {
        cmd::VERSION => Request::Version {
            major: c.i32()?,
            minor: c.i32()?,
        },
        cmd::GET_READERS_STATE => Request::GetReadersState,
        cmd::WAIT_READER_STATE_CHANGE => Request::WaitReaderStateChange,
        cmd::STOP_WAITING_READER_STATE_CHANGE => Request::StopWaitingReaderStateChange,
        cmd::GET_READER_EVENTS => Request::GetReaderEvents,
        cmd::ESTABLISH_CONTEXT => Request::EstablishContext { scope: c.u32()? },
        cmd::RELEASE_CONTEXT => Request::ReleaseContext { context: c.u32()? },
        cmd::CONNECT => {
            let context = c.u32()?;
            let reader = c_string(c.bytes(MAX_READERNAME)?);
            Request::Connect {
                context,
                reader,
                share_mode: c.u32()?,
                preferred_protocols: c.u32()?,
            }
        }
        cmd::RECONNECT => Request::Reconnect {
            card: c.i32()?,
            share_mode: c.u32()?,
            preferred_protocols: c.u32()?,
            initialization: c.u32()?,
        },
        cmd::DISCONNECT => Request::Disconnect {
            card: c.i32()?,
            disposition: c.u32()?,
        },
        cmd::BEGIN_TRANSACTION => Request::BeginTransaction { card: c.i32()? },
        cmd::END_TRANSACTION => Request::EndTransaction {
            card: c.i32()?,
            disposition: c.u32()?,
        },
        cmd::CANCEL => Request::Cancel { context: c.u32()? },
        cmd::STATUS => Request::Status { card: c.i32()? },
        cmd::TRANSMIT => {
            let card = c.i32()?;
            let send_protocol = c.u32()?;
            let send_pci_len = c.u32()?;
            let send_len = c.u32()? as usize;
            if send_len > MAX_BUFFER_SIZE_EXTENDED {
                return Err(WireError::TooLarge {
                    command,
                    len: send_len,
                });
            }
            let recv_protocol = c.u32()?;
            let recv_pci_len = c.u32()?;
            let recv_len = c.u32()?;
            Request::Transmit {
                card,
                send_protocol,
                send_pci_len,
                recv_protocol,
                recv_pci_len,
                recv_len,
                data: Vec::new(),
            }
            .with_announced(send_len)
        }
        cmd::CONTROL => {
            let card = c.i32()?;
            let code = c.u32()?;
            let send_len = c.u32()? as usize;
            if send_len > MAX_BUFFER_SIZE_EXTENDED {
                return Err(WireError::TooLarge {
                    command,
                    len: send_len,
                });
            }
            let recv_len = c.u32()?;
            Request::Control {
                card,
                code,
                recv_len,
                data: Vec::new(),
            }
            .with_announced(send_len)
        }
        cmd::GET_ATTRIB | cmd::SET_ATTRIB => {
            let card = c.i32()?;
            let attribute = c.u32()?;
            let attr = c.bytes(MAX_BUFFER_SIZE)?;
            let len = c.u32()? as usize;
            if len > MAX_BUFFER_SIZE {
                return Err(WireError::TooLarge { command, len });
            }
            if command == cmd::GET_ATTRIB {
                Request::GetAttrib { card, attribute }
            } else {
                Request::SetAttrib {
                    card,
                    attribute,
                    data: attr[..len].to_vec(),
                }
            }
        }
        _ => return Err(WireError::UnknownCommand(command)),
    })
}

impl Request {
    /// Helper of [`decode_body`]: a request that still waits for `n` trailing bytes, which `data` is sized for.
    fn with_announced(mut self, n: usize) -> Self {
        match &mut self {
            Self::Transmit { data, .. } | Self::Control { data, .. } => *data = vec![0; n],
            _ => {}
        }
        self
    }
}

/// How many bytes follow the struct (not counted by the header): the APDU of a transmit, the input of a control.
pub fn trailing_len(request: &Request) -> usize {
    match request {
        Request::Transmit { data, .. } | Request::Control { data, .. } => data.len(),
        _ => 0,
    }
}

/// Reads one request: header, struct, then the bytes that follow it. Nothing is allocated before the length
/// fields were checked, and no field makes it read more than [`MAX_BUFFER_SIZE_EXTENDED`] bytes.
pub fn read_request(input: &mut impl Read) -> Result<Request, WireError> {
    let mut h = [0u8; Header::SIZE];
    input.read_exact(&mut h)?;
    let header = Header::decode(&h);
    let expected = body_size(header.command).ok_or(WireError::UnknownCommand(header.command))?;
    if header.size as usize != expected {
        return Err(WireError::BadLength {
            command: header.command,
            expected,
            got: header.size as usize,
        });
    }
    let mut body = vec![0u8; expected];
    input.read_exact(&mut body)?;
    let mut request = decode_body(header.command, &body)?;
    match &mut request {
        Request::Transmit { data, .. } | Request::Control { data, .. } => input.read_exact(data)?,
        _ => {}
    }
    Ok(request)
}

/// Writes native-endian fields into a message.
#[derive(Default)]
struct Put(Vec<u8>);

impl Put {
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }

    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }

    /// `v` in a field of `n` bytes, zero-filled (and cut if too long).
    fn field(&mut self, v: &[u8], n: usize) -> &mut Self {
        let k = v.len().min(n);
        self.0.extend_from_slice(&v[..k]);
        self.0.resize(self.0.len() + (n - k), 0);
        self
    }
}

/// The reply of `CMD_VERSION`: always the version the server speaks, with `rv`.
pub fn version_reply(rv: u32) -> Vec<u8> {
    let mut p = Put::default();
    p.i32(PROTOCOL_VERSION_MAJOR)
        .i32(PROTOCOL_VERSION_MINOR)
        .u32(rv);
    p.0
}

/// The answer to a version proposed by a client: `SCARD_E_SERVICE_STOPPED` for a protocol the server does not
/// speak (what pcsc-lite does), success for 4.4 and 4.5.
pub fn version_result(major: i32, minor: i32) -> u32 {
    use oxydesk_core::smartcard::code;
    if major == PROTOCOL_VERSION_MAJOR && minor >= PROTOCOL_VERSION_MINOR_BACKWARD {
        code::SUCCESS
    } else {
        code::E_SERVICE_STOPPED
    }
}

/// `struct wait_reader_state_change`: the notification sent to a client that waits for a change (`rv` is
/// `SCARD_S_SUCCESS` for an event, `SCARD_E_CANCELLED` after `SCardCancel`) and the answer to
/// `CMD_STOP_WAITING_READER_STATE_CHANGE`.
pub fn wait_reply(rv: u32) -> Vec<u8> {
    let mut p = Put::default();
    p.u32(0).u32(rv);
    p.0
}

pub fn reader_events_reply(events: u32) -> Vec<u8> {
    let mut p = Put::default();
    p.u32(events).u32(0);
    p.0
}

pub fn establish_reply(scope: u32, context: u32, rv: u32) -> Vec<u8> {
    let mut p = Put::default();
    p.u32(scope).u32(context).u32(rv);
    p.0
}

pub fn release_reply(context: u32, rv: u32) -> Vec<u8> {
    let mut p = Put::default();
    p.u32(context).u32(rv);
    p.0
}

pub fn connect_reply(
    context: u32,
    reader: &str,
    share_mode: u32,
    preferred_protocols: u32,
    card: i32,
    active_protocol: u32,
    rv: u32,
) -> Vec<u8> {
    let mut p = Put::default();
    p.u32(context)
        .field(
            &reader.as_bytes()[..reader.len().min(MAX_READERNAME - 1)],
            MAX_READERNAME,
        )
        .u32(share_mode)
        .u32(preferred_protocols)
        .i32(card)
        .u32(active_protocol)
        .u32(rv);
    p.0
}

pub fn reconnect_reply(
    card: i32,
    share_mode: u32,
    preferred_protocols: u32,
    initialization: u32,
    active_protocol: u32,
    rv: u32,
) -> Vec<u8> {
    let mut p = Put::default();
    p.i32(card)
        .u32(share_mode)
        .u32(preferred_protocols)
        .u32(initialization)
        .u32(active_protocol)
        .u32(rv);
    p.0
}

pub fn disconnect_reply(card: i32, disposition: u32, rv: u32) -> Vec<u8> {
    let mut p = Put::default();
    p.i32(card).u32(disposition).u32(rv);
    p.0
}

pub fn begin_reply(card: i32, rv: u32) -> Vec<u8> {
    let mut p = Put::default();
    p.i32(card).u32(rv);
    p.0
}

pub fn end_reply(card: i32, disposition: u32, rv: u32) -> Vec<u8> {
    let mut p = Put::default();
    p.i32(card).u32(disposition).u32(rv);
    p.0
}

pub fn cancel_reply(context: u32, rv: u32) -> Vec<u8> {
    release_reply(context, rv)
}

pub fn status_reply(card: i32, rv: u32) -> Vec<u8> {
    begin_reply(card, rv)
}

/// The struct that answers a transmit; the answer bytes follow it when `rv` is success.
#[allow(clippy::too_many_arguments)]
pub fn transmit_reply(
    card: i32,
    send_protocol: u32,
    send_pci_len: u32,
    send_len: u32,
    recv_protocol: u32,
    recv_pci_len: u32,
    recv_len: u32,
    rv: u32,
) -> Vec<u8> {
    let mut p = Put::default();
    p.i32(card)
        .u32(send_protocol)
        .u32(send_pci_len)
        .u32(send_len)
        .u32(recv_protocol)
        .u32(recv_pci_len)
        .u32(recv_len)
        .u32(rv);
    p.0
}

/// The struct that answers a control; the output bytes follow it when `rv` is success.
pub fn control_reply(
    card: i32,
    code: u32,
    send_len: u32,
    recv_len: u32,
    bytes_returned: u32,
    rv: u32,
) -> Vec<u8> {
    let mut p = Put::default();
    p.i32(card)
        .u32(code)
        .u32(send_len)
        .u32(recv_len)
        .u32(bytes_returned)
        .u32(rv);
    p.0
}

/// The (fixed size) answer to `SCARD_GET_ATTRIB` / `SCARD_SET_ATTRIB`.
pub fn attrib_reply(card: i32, attribute: u32, value: &[u8], len: u32, rv: u32) -> Vec<u8> {
    let mut p = Put::default();
    p.i32(card)
        .u32(attribute)
        .field(value, MAX_BUFFER_SIZE)
        .u32(len)
        .u32(rv);
    p.0
}

/// One entry of the table of readers, as the applications see it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReaderRecord {
    pub name: String,
    pub event_counter: u32,
    pub reader_state: u32,
    pub sharing: i32,
    pub atr: Vec<u8>,
    pub protocol: u32,
}

impl ReaderRecord {
    /// `READER_STATE`: name[128], eventCounter, readerState, readerSharing, cardAtr[33] + 3 bytes of padding,
    /// cardAtrLength, cardProtocol.
    pub fn encode(&self) -> [u8; READER_STATE_SIZE] {
        let mut p = Put::default();
        p.field(self.name.as_bytes(), MAX_READERNAME - 1);
        // The name field is 128 bytes: the last one is always the NUL.
        p.0.resize(MAX_READERNAME, 0);
        let atr = &self.atr[..self.atr.len().min(MAX_ATR_SIZE)];
        p.u32(self.event_counter)
            .u32(self.reader_state)
            .i32(self.sharing)
            .field(atr, MAX_ATR_SIZE)
            .field(&[], 3)
            .u32(atr.len() as u32)
            .u32(self.protocol);
        let mut out = [0u8; READER_STATE_SIZE];
        out.copy_from_slice(&p.0);
        out
    }

    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() != READER_STATE_SIZE {
            return Err(WireError::Truncated);
        }
        let mut c = Cursor::new(b);
        let name = c_string(c.bytes(MAX_READERNAME)?);
        let event_counter = c.u32()?;
        let reader_state = c.u32()?;
        let sharing = c.i32()?;
        let atr_bytes = c.bytes(MAX_ATR_SIZE)?;
        c.bytes(3)?;
        let atr_len = (c.u32()? as usize).min(MAX_ATR_SIZE);
        let protocol = c.u32()?;
        Ok(Self {
            name,
            event_counter,
            reader_state,
            sharing,
            atr: atr_bytes[..atr_len].to_vec(),
            protocol,
        })
    }
}

/// The whole table: the records, then empty entries up to [`MAX_READERS`] (at most that many records are kept).
pub fn encode_table(records: &[ReaderRecord]) -> Vec<u8> {
    let mut out = Vec::with_capacity(TABLE_SIZE);
    for r in records.iter().take(MAX_READERS) {
        out.extend_from_slice(&r.encode());
    }
    out.resize(TABLE_SIZE, 0);
    out
}

/// The readers of a table (the entries with a name), for the clients and the tests.
pub fn decode_table(b: &[u8]) -> Result<Vec<ReaderRecord>, WireError> {
    if b.len() != TABLE_SIZE {
        return Err(WireError::Truncated);
    }
    let mut v = Vec::new();
    for chunk in b.chunks(READER_STATE_SIZE) {
        let r = ReaderRecord::decode(chunk)?;
        if !r.name.is_empty() {
            v.push(r);
        }
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as IoCursor;

    fn frame(command: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Header {
            size: body.len() as u32,
            command,
        }
        .encode()
        .to_vec();
        v.extend_from_slice(body);
        v
    }

    fn u32s(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_ne_bytes()).collect()
    }

    #[test]
    fn struct_sizes_are_those_of_the_c_abi() {
        assert_eq!(READER_STATE_SIZE, 184);
        assert_eq!(TABLE_SIZE, 2944);
        assert_eq!(body_size(cmd::CONNECT), Some(4 + 128 + 5 * 4));
        assert_eq!(body_size(cmd::TRANSMIT), Some(8 * 4));
        assert_eq!(body_size(cmd::CONTROL), Some(6 * 4));
        assert_eq!(body_size(cmd::GET_ATTRIB), Some(4 + 4 + 264 + 4 + 4));
        assert_eq!(body_size(0x0C), None, "SCARD_GET_STATUS_CHANGE is not sent");
        assert_eq!(body_size(0x0E), None);
        assert_eq!(body_size(0), None);
        assert_eq!(body_size(0x16), None);
    }

    #[test]
    fn header_round_trips() {
        let h = Header {
            size: 12,
            command: cmd::VERSION,
        };
        assert_eq!(Header::decode(&h.encode()), h);
    }

    #[test]
    fn version_is_decoded_and_answered() {
        let f = frame(cmd::VERSION, &u32s(&[4, 5, 0]));
        let r = read_request(&mut IoCursor::new(f)).unwrap();
        assert_eq!(r, Request::Version { major: 4, minor: 5 });
        assert_eq!(version_result(4, 5), 0);
        assert_eq!(version_result(4, 4), 0);
        assert_eq!(
            version_result(4, 3),
            oxydesk_core::smartcard::code::E_SERVICE_STOPPED
        );
        assert_eq!(
            version_result(3, 5),
            oxydesk_core::smartcard::code::E_SERVICE_STOPPED
        );
        assert_eq!(
            version_result(5, 5),
            oxydesk_core::smartcard::code::E_SERVICE_STOPPED
        );
        assert_eq!(version_reply(0), u32s(&[4, 5, 0]));
    }

    #[test]
    fn connect_is_decoded_with_its_reader_name() {
        let mut body = u32s(&[0x1234]);
        let mut name = b"Fake Reader 0".to_vec();
        name.resize(128, 0);
        body.extend(name);
        body.extend(u32s(&[2, 3, 0, 0, 0]));
        let r = decode_body(cmd::CONNECT, &body).unwrap();
        assert_eq!(
            r,
            Request::Connect {
                context: 0x1234,
                reader: "Fake Reader 0".into(),
                share_mode: 2,
                preferred_protocols: 3
            }
        );
    }

    #[test]
    fn a_reader_name_without_nul_is_cut_at_the_field() {
        let mut body = u32s(&[1]);
        body.extend(vec![b'A'; 128]);
        body.extend(u32s(&[2, 3, 0, 0, 0]));
        match decode_body(cmd::CONNECT, &body).unwrap() {
            Request::Connect { reader, .. } => assert_eq!(reader.len(), 128),
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn transmit_reads_the_apdu_after_the_struct() {
        let apdu = [0x00, 0xA4, 0x04, 0x00];
        let body = u32s(&[7, 2, 8, 4, 2, 8, 258, 0]);
        let mut f = frame(cmd::TRANSMIT, &body);
        f.extend_from_slice(&apdu);
        f.push(0xEE); // the next message starts here: must stay unread
        let mut input = IoCursor::new(f);
        let r = read_request(&mut input).unwrap();
        assert_eq!(
            r,
            Request::Transmit {
                card: 7,
                send_protocol: 2,
                send_pci_len: 8,
                recv_protocol: 2,
                recv_pci_len: 8,
                recv_len: 258,
                data: apdu.to_vec()
            }
        );
        assert_eq!(input.position() as usize, Header::SIZE + 32 + 4);
        assert_eq!(trailing_len(&r), 4);
    }

    #[test]
    fn control_reads_its_input() {
        let body = u32s(&[7, 0x42000C00, 3, 16, 0, 0]);
        let mut f = frame(cmd::CONTROL, &body);
        f.extend_from_slice(&[1, 2, 3]);
        let r = read_request(&mut IoCursor::new(f)).unwrap();
        assert_eq!(
            r,
            Request::Control {
                card: 7,
                code: 0x42000C00,
                recv_len: 16,
                data: vec![1, 2, 3]
            }
        );
    }

    #[test]
    fn oversized_lengths_are_refused_before_any_allocation() {
        let big = (MAX_BUFFER_SIZE_EXTENDED + 1) as u32;
        let body = u32s(&[7, 2, 8, big, 2, 8, 258, 0]);
        let err = read_request(&mut IoCursor::new(frame(cmd::TRANSMIT, &body))).unwrap_err();
        assert!(
            matches!(err, WireError::TooLarge { len, .. } if len == big as usize),
            "{err}"
        );
        let body = u32s(&[7, 1, u32::MAX, 0, 0, 0]);
        assert!(matches!(
            read_request(&mut IoCursor::new(frame(cmd::CONTROL, &body))),
            Err(WireError::TooLarge { .. })
        ));
        // The largest legal APDU is accepted.
        let ok = (MAX_BUFFER_SIZE_EXTENDED) as u32;
        let body = u32s(&[7, 2, 8, ok, 2, 8, 258, 0]);
        let mut f = frame(cmd::TRANSMIT, &body);
        f.extend(vec![0u8; MAX_BUFFER_SIZE_EXTENDED]);
        assert!(read_request(&mut IoCursor::new(f)).is_ok());
    }

    #[test]
    fn attribute_length_is_bounded_by_the_buffer() {
        let mut body = u32s(&[7, 0x90303]);
        body.extend(vec![0u8; 264]);
        body.extend(u32s(&[265, 0]));
        assert!(matches!(
            decode_body(cmd::GET_ATTRIB, &body),
            Err(WireError::TooLarge { .. })
        ));
        let mut body = u32s(&[7, 5]);
        let mut attr = vec![0u8; 264];
        attr[..3].copy_from_slice(&[9, 8, 7]);
        body.extend(attr);
        body.extend(u32s(&[3, 0]));
        assert_eq!(
            decode_body(cmd::SET_ATTRIB, &body).unwrap(),
            Request::SetAttrib {
                card: 7,
                attribute: 5,
                data: vec![9, 8, 7]
            }
        );
    }

    #[test]
    fn wrong_sizes_and_unknown_commands_are_errors() {
        // header says 4 bytes for a 12-byte struct
        let f = frame(cmd::VERSION, &[0; 4]);
        assert!(matches!(
            read_request(&mut IoCursor::new(f)),
            Err(WireError::BadLength {
                expected: 12,
                got: 4,
                ..
            })
        ));
        // commands without a body must have size 0
        let f = frame(cmd::GET_READERS_STATE, &[0; 4]);
        assert!(matches!(
            read_request(&mut IoCursor::new(f)),
            Err(WireError::BadLength { .. })
        ));
        for c in [0, 0x0C, 0x0E, 0x16, 0xFFFF_FFFF] {
            let f = frame(c, &[]);
            assert!(
                matches!(read_request(&mut IoCursor::new(f)), Err(WireError::UnknownCommand(x)) if x == c),
                "{c:#x}"
            );
        }
        assert!(matches!(
            decode_body(cmd::VERSION, &[0; 5]),
            Err(WireError::BadLength { .. })
        ));
    }

    #[test]
    fn truncated_input_is_an_io_error() {
        let full = frame(cmd::VERSION, &u32s(&[4, 5, 0]));
        for cut in 0..full.len() {
            let r = read_request(&mut IoCursor::new(full[..cut].to_vec()));
            assert!(matches!(r, Err(WireError::Io(_))), "cut {cut}");
        }
        // A transmit whose APDU is cut.
        let mut f = frame(cmd::TRANSMIT, &u32s(&[7, 2, 8, 4, 2, 8, 258, 0]));
        f.extend_from_slice(&[1, 2]);
        assert!(matches!(
            read_request(&mut IoCursor::new(f)),
            Err(WireError::Io(_))
        ));
        assert!(matches!(
            decode_body(cmd::TRANSMIT, &[0; 8]),
            Err(WireError::BadLength { .. })
        ));
    }

    #[test]
    fn all_fixed_requests_decode() {
        let cases: Vec<(u32, Vec<u8>, Request)> = vec![
            (cmd::GET_READERS_STATE, vec![], Request::GetReadersState),
            (
                cmd::WAIT_READER_STATE_CHANGE,
                vec![],
                Request::WaitReaderStateChange,
            ),
            (
                cmd::STOP_WAITING_READER_STATE_CHANGE,
                vec![],
                Request::StopWaitingReaderStateChange,
            ),
            (cmd::GET_READER_EVENTS, vec![], Request::GetReaderEvents),
            (
                cmd::ESTABLISH_CONTEXT,
                u32s(&[2, 0, 0]),
                Request::EstablishContext { scope: 2 },
            ),
            (
                cmd::RELEASE_CONTEXT,
                u32s(&[9, 0]),
                Request::ReleaseContext { context: 9 },
            ),
            (
                cmd::RECONNECT,
                u32s(&[5, 1, 3, 1, 0, 0]),
                Request::Reconnect {
                    card: 5,
                    share_mode: 1,
                    preferred_protocols: 3,
                    initialization: 1,
                },
            ),
            (
                cmd::DISCONNECT,
                u32s(&[5, 2, 0]),
                Request::Disconnect {
                    card: 5,
                    disposition: 2,
                },
            ),
            (
                cmd::BEGIN_TRANSACTION,
                u32s(&[5, 0]),
                Request::BeginTransaction { card: 5 },
            ),
            (
                cmd::END_TRANSACTION,
                u32s(&[5, 0, 0]),
                Request::EndTransaction {
                    card: 5,
                    disposition: 0,
                },
            ),
            (cmd::CANCEL, u32s(&[9, 0]), Request::Cancel { context: 9 }),
            (cmd::STATUS, u32s(&[5, 0]), Request::Status { card: 5 }),
        ];
        for (c, body, expected) in cases {
            assert_eq!(decode_body(c, &body).unwrap(), expected, "{c:#x}");
        }
        assert_eq!(
            decode_body(cmd::DISCONNECT, &u32s(&[0xFFFF_FFFF, 0, 0])).unwrap(),
            Request::Disconnect {
                card: -1,
                disposition: 0
            }
        );
    }

    #[test]
    fn replies_have_the_size_of_their_struct() {
        assert_eq!(wait_reply(0).len(), 8);
        assert_eq!(reader_events_reply(3).len(), 8);
        assert_eq!(establish_reply(2, 5, 0).len(), 12);
        assert_eq!(release_reply(5, 0).len(), 8);
        assert_eq!(connect_reply(1, "R", 2, 3, 7, 1, 0).len(), 152);
        assert_eq!(reconnect_reply(1, 2, 3, 0, 1, 0).len(), 24);
        assert_eq!(disconnect_reply(1, 0, 0).len(), 12);
        assert_eq!(begin_reply(1, 0).len(), 8);
        assert_eq!(end_reply(1, 0, 0).len(), 12);
        assert_eq!(cancel_reply(1, 0).len(), 8);
        assert_eq!(status_reply(1, 0).len(), 8);
        assert_eq!(transmit_reply(1, 2, 8, 4, 2, 8, 2, 0).len(), 32);
        assert_eq!(control_reply(1, 2, 3, 4, 5, 0).len(), 24);
        assert_eq!(attrib_reply(1, 2, &[1, 2], 2, 0).len(), 280);
        assert_eq!(attrib_reply(1, 2, &[0; 400], 2, 0).len(), 280);
    }

    #[test]
    fn connect_reply_puts_the_fields_where_the_client_reads_them() {
        let b = connect_reply(0x11, "Reader", 2, 3, 0x55, 2, 0);
        let word = |o: usize| u32::from_ne_bytes(b[o..o + 4].try_into().unwrap());
        assert_eq!(word(0), 0x11);
        assert_eq!(&b[4..10], b"Reader");
        assert_eq!(word(132), 2); // dwShareMode
        assert_eq!(word(136), 3); // dwPreferredProtocols
        assert_eq!(word(140), 0x55); // hCard
        assert_eq!(word(144), 2); // dwActiveProtocol
        assert_eq!(word(148), 0); // rv
    }

    #[test]
    fn transmit_and_attrib_replies_put_rv_last() {
        let b = transmit_reply(1, 2, 8, 4, 2, 8, 258, 0x8010_0008);
        assert_eq!(u32::from_ne_bytes(b[24..28].try_into().unwrap()), 258);
        assert_eq!(
            u32::from_ne_bytes(b[28..32].try_into().unwrap()),
            0x8010_0008
        );
        let b = attrib_reply(1, 0x90303, &[0xAA; 3], 3, 0);
        assert_eq!(&b[8..11], &[0xAA; 3]);
        assert_eq!(u32::from_ne_bytes(b[272..276].try_into().unwrap()), 3);
    }

    #[test]
    fn reader_record_round_trips_with_the_c_layout() {
        let r = ReaderRecord {
            name: "Fake Reader 0".into(),
            event_counter: 3,
            reader_state: reader_state::PRESENT | reader_state::POWERED,
            sharing: sharing::IN_USE,
            atr: vec![0x3b, 0x75, 0x13],
            protocol: 2,
        };
        let b = r.encode();
        assert_eq!(b.len(), 184);
        assert_eq!(&b[..13], b"Fake Reader 0");
        assert_eq!(b[13], 0);
        let word = |o: usize| u32::from_ne_bytes(b[o..o + 4].try_into().unwrap());
        assert_eq!(word(128), 3);
        assert_eq!(word(132), 0x14);
        assert_eq!(word(136), 1);
        assert_eq!(&b[140..143], &[0x3b, 0x75, 0x13]);
        assert_eq!(word(176), 3);
        assert_eq!(word(180), 2);
        assert_eq!(ReaderRecord::decode(&b).unwrap(), r);
        assert!(ReaderRecord::decode(&b[..100]).is_err());
    }

    #[test]
    fn a_long_name_and_atr_are_cut_to_their_fields() {
        let r = ReaderRecord {
            name: "N".repeat(300),
            atr: vec![1; 50],
            ..Default::default()
        };
        let b = r.encode();
        assert_eq!(b[127], 0, "the name keeps its NUL");
        let back = ReaderRecord::decode(&b).unwrap();
        assert_eq!(back.name.len(), 127);
        assert_eq!(back.atr.len(), 33);
    }

    #[test]
    fn the_table_has_sixteen_entries_and_keeps_the_empty_ones_blank() {
        let recs: Vec<ReaderRecord> = (0..20)
            .map(|i| ReaderRecord {
                name: format!("R{i}"),
                ..Default::default()
            })
            .collect();
        let t = encode_table(&recs[..2]);
        assert_eq!(t.len(), TABLE_SIZE);
        assert!(t[2 * 184..].iter().all(|&b| b == 0));
        assert_eq!(decode_table(&t).unwrap().len(), 2);
        assert_eq!(decode_table(&encode_table(&recs)).unwrap().len(), 16);
        assert!(decode_table(&t[..100]).is_err());
        assert_eq!(decode_table(&encode_table(&[])).unwrap(), vec![]);
    }

    #[test]
    fn error_messages_are_readable() {
        assert!(WireError::UnknownCommand(0x30).to_string().contains("0x30"));
        assert!(WireError::Truncated.to_string().contains("truncated"));
        assert!(WireError::TooLarge { command: 9, len: 5 }
            .to_string()
            .contains("too large"));
        assert!(WireError::BadLength {
            command: 9,
            expected: 1,
            got: 2
        }
        .to_string()
        .contains("expected 1"));
        let io: WireError = io::Error::other("x").into();
        assert_eq!(io.to_string(), "x");
    }
}
