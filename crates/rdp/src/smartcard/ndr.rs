//! Server side of MS-RDPESC: the IOCTL inputs the server sends to the client's smart card device, and the outputs
//! the client answers (NDR inside an MS-RPCE type serialization, see `third_party/ironrdp-rdpdr/src/pdu/esc`
//! for the client side, which is the reference these functions are tested against).
//!
//! Pure functions: no I/O, no protocol state. All the strings are UTF-16 (the `...W` IOCTLs).

use oxydesk_core::smartcard::{ReaderQuery, ReaderStatus, MAX_ATR};

/// `SCARD_IOCTL_*` codes the server sends ([MS-RDPESC] 2.2.2, the `W` variants).
pub mod ioctl {
    pub const ESTABLISH_CONTEXT: u32 = 0x0009_0014;
    pub const RELEASE_CONTEXT: u32 = 0x0009_0018;
    pub const LIST_READERS_W: u32 = 0x0009_002C;
    pub const GET_STATUS_CHANGE_W: u32 = 0x0009_00A4;
    pub const CANCEL: u32 = 0x0009_00A8;
    pub const CONNECT_W: u32 = 0x0009_00B0;
    pub const RECONNECT: u32 = 0x0009_00B4;
    pub const DISCONNECT: u32 = 0x0009_00B8;
    pub const BEGIN_TRANSACTION: u32 = 0x0009_00BC;
    pub const END_TRANSACTION: u32 = 0x0009_00C0;
    pub const STATUS_W: u32 = 0x0009_00CC;
    pub const TRANSMIT: u32 = 0x0009_00D0;
    pub const CONTROL: u32 = 0x0009_00D4;
    pub const GET_ATTRIB: u32 = 0x0009_00D8;
}

/// `SCARD_AUTOALLOCATE`: the client sizes the answer.
const AUTOALLOCATE: u32 = 0xFFFF_FFFF;
/// `SCARD_SCOPE_SYSTEM`, what Windows servers ask for.
const SCOPE_SYSTEM: u32 = 2;
/// The largest answer buffer MS-RDPESC allows for `Control` and `GetAttrib` (66560 and 65536).
const MAX_CONTROL: u32 = 66_560;
const MAX_ATTRIB: u32 = 65_536;
/// Most readers or bytes a client answer may declare: protects against a hostile length.
const MAX_ENTRIES: usize = 4096;

/// A malformed client answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed smart card answer: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

/// What an IOCTL answered: the `SCARD_*` return code, and the rest of the output when it is `SUCCESS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output<'a> {
    pub code: u32,
    body: &'a [u8],
}

// ---------------------------------------------------------------- writing

/// An NDR body being built: the fixed part, then the deferred values, each 4-byte aligned.
#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
    /// Index of the next pointer referent (`0x00020000 + 4 * index`).
    next_ref: u32,
}

impl Writer {
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn pad4(&mut self) {
        while !self.buf.len().is_multiple_of(4) {
            self.buf.push(0);
        }
    }

    /// A non-null pointer referent.
    fn referent(&mut self) {
        let v = 0x0002_0000 + self.next_ref * 4;
        self.next_ref += 1;
        self.u32(v);
    }

    /// `REDIR_SCARDCONTEXT`, the part that stays in the structure: `cbContext`, `pbContext`.
    fn context_ptr(&mut self, context: &[u8]) {
        self.u32(context.len() as u32);
        if context.is_empty() {
            self.u32(0);
        } else {
            self.referent();
        }
    }

    /// `REDIR_SCARDCONTEXT`, the deferred part.
    fn context_value(&mut self, context: &[u8]) {
        if context.is_empty() {
            return;
        }
        self.u32(context.len() as u32);
        self.buf.extend_from_slice(context);
        self.pad4();
    }

    /// `REDIR_SCARDHANDLE`, the part that stays in the structure: its context, `cbHandle`, `pbHandle`.
    fn handle_ptr(&mut self, context: &[u8], handle: &[u8]) {
        self.context_ptr(context);
        self.u32(handle.len() as u32);
        if handle.is_empty() {
            self.u32(0);
        } else {
            self.referent();
        }
    }

    fn handle_value(&mut self, context: &[u8], handle: &[u8]) {
        self.context_value(context);
        if !handle.is_empty() {
            self.u32(handle.len() as u32);
            self.buf.extend_from_slice(handle);
            self.pad4();
        }
    }

    /// A conformant varying UTF-16 string with its NUL, as the reader names are sent.
    fn wide_string(&mut self, text: &str) {
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        self.u32(units.len() as u32); // maximum count
        self.u32(0); // offset
        self.u32(units.len() as u32); // actual count
        for u in units {
            self.buf.extend_from_slice(&u.to_le_bytes());
        }
        self.pad4();
    }

    /// A conformant byte array: its length, then the bytes.
    fn bytes(&mut self, data: &[u8]) {
        self.u32(data.len() as u32);
        self.buf.extend_from_slice(data);
        self.pad4();
    }
}

/// Wraps an NDR body in the MS-RPCE headers (8 bytes of stream header, 8 of private header) and pads it to 8 bytes.
fn frame(mut body: Writer) -> Vec<u8> {
    while !body.buf.len().is_multiple_of(8) {
        body.buf.push(0);
    }
    let mut out = Vec::with_capacity(16 + body.buf.len());
    out.extend_from_slice(&[0x01, 0x10, 0x08, 0x00, 0xCC, 0xCC, 0xCC, 0xCC]);
    out.extend_from_slice(&(body.buf.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&body.buf);
    out
}

pub fn establish_context() -> Vec<u8> {
    let mut w = Writer::default();
    w.u32(SCOPE_SYSTEM);
    w.u32(0); // reserved
    frame(w)
}

/// `Context_Call`: `ReleaseContext` and `Cancel`.
pub fn context_call(context: &[u8]) -> Vec<u8> {
    let mut w = Writer::default();
    w.context_ptr(context);
    w.context_value(context);
    frame(w)
}

pub fn list_readers(context: &[u8]) -> Vec<u8> {
    let mut w = Writer::default();
    w.context_ptr(context);
    w.u32(0); // cBytes of the groups: none, every reader
    w.u32(0); // pointer to the groups: null
    w.u32(0); // the multistring is wanted
    w.u32(AUTOALLOCATE);
    w.context_value(context);
    frame(w)
}

pub fn get_status_change(context: &[u8], timeout_ms: u32, readers: &[ReaderQuery]) -> Vec<u8> {
    let mut w = Writer::default();
    w.context_ptr(context);
    w.u32(timeout_ms);
    w.u32(readers.len() as u32);
    w.referent();
    w.context_value(context);
    w.u32(readers.len() as u32); // maximum count of the array
    for reader in readers {
        w.referent(); // szReader
        w.u32(reader.current_state);
        w.u32(0); // dwEventState
        w.u32(0); // cbAtr
        w.buf.extend_from_slice(&[0u8; 36]); // rgbAtr
    }
    for reader in readers {
        w.wide_string(&reader.name);
    }
    frame(w)
}

pub fn connect(context: &[u8], reader: &str, share_mode: u32, preferred_protocols: u32) -> Vec<u8> {
    let mut w = Writer::default();
    w.referent(); // szReader
    w.context_ptr(context);
    w.u32(share_mode);
    w.u32(preferred_protocols);
    w.wide_string(reader);
    w.context_value(context);
    frame(w)
}

pub fn reconnect(
    context: &[u8],
    handle: &[u8],
    share_mode: u32,
    preferred_protocols: u32,
    initialization: u32,
) -> Vec<u8> {
    let mut w = Writer::default();
    w.handle_ptr(context, handle);
    w.u32(share_mode);
    w.u32(preferred_protocols);
    w.u32(initialization);
    w.handle_value(context, handle);
    frame(w)
}

/// `HCardAndDisposition_Call`: `Disconnect`, `BeginTransaction`, `EndTransaction`.
pub fn card_and_disposition(context: &[u8], handle: &[u8], disposition: u32) -> Vec<u8> {
    let mut w = Writer::default();
    w.handle_ptr(context, handle);
    w.u32(disposition);
    w.handle_value(context, handle);
    frame(w)
}

pub fn transmit(
    context: &[u8],
    handle: &[u8],
    protocol: u32,
    apdu: &[u8],
    max_response: u32,
) -> Vec<u8> {
    let mut w = Writer::default();
    w.handle_ptr(context, handle);
    w.u32(protocol); // pioSendPci.dwProtocol
    w.u32(0); // cbExtraBytes
    w.u32(0); // pbExtraBytes: null
    w.u32(apdu.len() as u32);
    w.referent(); // pbSendBuffer
    w.u32(0); // pioRecvPci: null
    w.u32(0); // the receive buffer is wanted
    w.u32(max_response);
    w.handle_value(context, handle);
    w.bytes(apdu);
    frame(w)
}

pub fn status(context: &[u8], handle: &[u8]) -> Vec<u8> {
    let mut w = Writer::default();
    w.handle_ptr(context, handle);
    w.u32(0); // the reader names are wanted
    w.u32(AUTOALLOCATE);
    w.u32(MAX_ATR as u32 - 1); // cbAtrLen: 32
    w.handle_value(context, handle);
    frame(w)
}

pub fn control(
    context: &[u8],
    handle: &[u8],
    control_code: u32,
    input: &[u8],
    max_response: u32,
) -> Vec<u8> {
    let mut w = Writer::default();
    w.handle_ptr(context, handle);
    w.u32(control_code);
    w.u32(input.len() as u32);
    if input.is_empty() {
        w.u32(0);
    } else {
        w.referent();
    }
    w.u32(0); // the output buffer is wanted
    w.u32(max_response.min(MAX_CONTROL));
    w.handle_value(context, handle);
    if !input.is_empty() {
        w.bytes(input);
    }
    frame(w)
}

pub fn get_attrib(context: &[u8], handle: &[u8], attribute: u32, max_response: u32) -> Vec<u8> {
    let mut w = Writer::default();
    w.handle_ptr(context, handle);
    w.u32(attribute);
    w.u32(0); // the attribute is wanted
    w.u32(max_response.min(MAX_ATTRIB));
    w.handle_value(context, handle);
    frame(w)
}

// ---------------------------------------------------------------- reading

/// A cursor on the NDR body of an answer.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u32(&mut self) -> Result<u32, DecodeError> {
        let end = self.pos.checked_add(4).ok_or(DecodeError("offset"))?;
        let bytes = self
            .data
            .get(self.pos..end)
            .ok_or(DecodeError("truncated"))?;
        self.pos = end;
        Ok(u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError("offset"))?;
        let bytes = self
            .data
            .get(self.pos..end)
            .ok_or(DecodeError("truncated"))?;
        self.pos = end;
        Ok(bytes)
    }

    fn pad4(&mut self) {
        self.pos = (self.pos + 3) & !3;
    }

    /// A conformant byte array: count, then the bytes.
    fn bytes(&mut self) -> Result<Vec<u8>, DecodeError> {
        let n = self.u32()? as usize;
        if n > 1 << 20 {
            return Err(DecodeError("array too long"));
        }
        let out = self.take(n)?.to_vec();
        self.pad4();
        Ok(out)
    }

    /// `REDIR_SCARDCONTEXT` (pointer part): the declared length, and whether a value follows.
    fn context_ptr(&mut self) -> Result<usize, DecodeError> {
        let len = self.u32()? as usize;
        let ptr = self.u32()?;
        if len > 16 || (len == 0) != (ptr == 0) {
            return Err(DecodeError("bad context"));
        }
        Ok(len)
    }

    fn context_value(&mut self, len: usize) -> Result<Vec<u8>, DecodeError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        if self.u32()? as usize != len {
            return Err(DecodeError("context length mismatch"));
        }
        let out = self.take(len)?.to_vec();
        self.pad4();
        Ok(out)
    }
}

/// Splits an output buffer: checks the MS-RPCE headers and reads the `SCARD_*` return code.
pub fn parse(output: &[u8]) -> Result<Output<'_>, DecodeError> {
    // Stream header: version 1, little endian, header length 8. Private header: the length of the object.
    if output.len() < 20 {
        return Err(DecodeError("shorter than the headers"));
    }
    if output[0] != 0x01 || output[1] != 0x10 || output[2..4] != [0x08, 0x00] {
        return Err(DecodeError("bad stream header"));
    }
    let body = &output[16..];
    let code = u32::from_le_bytes(body[..4].try_into().expect("4 bytes"));
    Ok(Output { code, body })
}

/// `EstablishContext_Return`: the context of the client.
pub fn read_establish_context(out: &Output<'_>) -> Result<Vec<u8>, DecodeError> {
    let mut r = Reader {
        data: out.body,
        pos: 4,
    };
    let len = r.context_ptr()?;
    r.context_value(len)
}

/// `ListReaders_Return`: the names of the readers (a UTF-16 multistring).
pub fn read_list_readers(out: &Output<'_>) -> Result<Vec<String>, DecodeError> {
    let mut r = Reader {
        data: out.body,
        pos: 4,
    };
    let _c_bytes = r.u32()?;
    if r.u32()? == 0 {
        return Ok(Vec::new());
    }
    let data = r.bytes()?;
    Ok(decode_multistring(&data))
}

/// `GetStatusChange_Return`: one status per reader asked, in order. `asked` supplies the names back.
pub fn read_get_status_change(
    out: &Output<'_>,
    asked: &[ReaderQuery],
) -> Result<Vec<ReaderStatus>, DecodeError> {
    let mut r = Reader {
        data: out.body,
        pos: 4,
    };
    let count = r.u32()? as usize;
    if count > MAX_ENTRIES || count != asked.len() {
        return Err(DecodeError("wrong number of readers"));
    }
    if r.u32()? == 0 && count != 0 {
        return Err(DecodeError("no reader states"));
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let max = r.u32()? as usize;
    if max != count {
        return Err(DecodeError("reader count mismatch"));
    }
    let mut statuses = Vec::with_capacity(count);
    for query in asked {
        let _current = r.u32()?;
        let event_state = r.u32()?;
        let atr_len = (r.u32()? as usize).min(36);
        let atr = r.take(36)?[..atr_len.min(MAX_ATR)].to_vec();
        statuses.push(ReaderStatus {
            name: query.name.clone(),
            event_state,
            atr,
        });
    }
    Ok(statuses)
}

/// `Connect_Return`: the handle of the card (its own bytes) and the protocol in use. The context comes
/// back too (the client repeats it).
pub fn read_connect(out: &Output<'_>) -> Result<(Vec<u8>, u32), DecodeError> {
    let mut r = Reader {
        data: out.body,
        pos: 4,
    };
    let ctx_len = r.context_ptr()?;
    let handle_len = r.u32()? as usize;
    if handle_len > 16 {
        return Err(DecodeError("bad handle"));
    }
    let handle_ptr = r.u32()?;
    if (handle_len == 0) != (handle_ptr == 0) {
        return Err(DecodeError("bad handle"));
    }
    let protocol = r.u32()?;
    let _context = r.context_value(ctx_len)?;
    let handle = if handle_len == 0 {
        Vec::new()
    } else {
        if r.u32()? as usize != handle_len {
            return Err(DecodeError("handle length mismatch"));
        }
        let h = r.take(handle_len)?.to_vec();
        r.pad4();
        h
    };
    Ok((handle, protocol))
}

/// `Reconnect_Return`: the protocol in use.
pub fn read_reconnect(out: &Output<'_>) -> Result<u32, DecodeError> {
    let mut r = Reader {
        data: out.body,
        pos: 4,
    };
    r.u32()
}

/// `Transmit_Return`: the answer of the card.
pub fn read_transmit(out: &Output<'_>) -> Result<Vec<u8>, DecodeError> {
    let mut r = Reader {
        data: out.body,
        pos: 4,
    };
    if r.u32()? != 0 {
        // pioRecvPci: protocol, cbExtraBytes, pbExtraBytes, and the extra bytes themselves.
        let _protocol = r.u32()?;
        let extra = r.u32()? as usize;
        let extra_ptr = r.u32()?;
        let _ = (extra, extra_ptr);
    }
    let _cb_recv = r.u32()?;
    if r.u32()? == 0 {
        return Ok(Vec::new());
    }
    r.bytes()
}

/// `Status_Return`: the names of the reader (first one), the state, protocol and ATR.
pub fn read_status(out: &Output<'_>) -> Result<(String, u32, u32, Vec<u8>), DecodeError> {
    let mut r = Reader {
        data: out.body,
        pos: 4,
    };
    let _c_bytes = r.u32()?;
    let names_ptr = r.u32()?;
    let state = r.u32()?;
    let protocol = r.u32()?;
    let atr_raw = r.take(32)?.to_vec();
    let atr_len = (r.u32()? as usize).min(32);
    let reader = if names_ptr == 0 {
        String::new()
    } else {
        let data = r.bytes()?;
        decode_multistring(&data)
            .into_iter()
            .next()
            .unwrap_or_default()
    };
    Ok((reader, state, protocol, atr_raw[..atr_len].to_vec()))
}

/// `Control_Return` and `GetAttrib_Return`: a byte buffer.
pub fn read_buffer(out: &Output<'_>) -> Result<Vec<u8>, DecodeError> {
    let mut r = Reader {
        data: out.body,
        pos: 4,
    };
    let _len = r.u32()?;
    if r.u32()? == 0 {
        return Ok(Vec::new());
    }
    r.bytes()
}

/// A UTF-16LE multistring (strings each ended by NUL, then one more NUL) as the names it holds.
pub fn decode_multistring(data: &[u8]) -> Vec<String> {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    units
        .split(|u| *u == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_aligned_and_headed() {
        for body in [establish_context(), context_call(&[1, 2, 3, 4])] {
            assert_eq!(&body[..8], &[1, 0x10, 8, 0, 0xCC, 0xCC, 0xCC, 0xCC]);
            assert_eq!(body.len() % 8, 0);
            let declared = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
            assert_eq!(declared, body.len() - 16);
        }
    }

    #[test]
    fn multistrings_split_on_nul() {
        let mut data = Vec::new();
        for s in ["Reader A", "Reader B"] {
            for u in s.encode_utf16().chain(std::iter::once(0)) {
                data.extend_from_slice(&u.to_le_bytes());
            }
        }
        data.extend_from_slice(&[0, 0]);
        assert_eq!(decode_multistring(&data), ["Reader A", "Reader B"]);
        assert!(decode_multistring(&[0, 0]).is_empty());
        assert!(decode_multistring(&[]).is_empty());
    }

    #[test]
    fn short_outputs_are_refused() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[0; 19]).is_err());
        let mut bad = vec![0u8; 24];
        bad[0] = 2;
        assert!(parse(&bad).is_err());
    }
}
