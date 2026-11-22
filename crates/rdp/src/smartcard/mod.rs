//! Smart card readers redirected by the client (RDPDR `SCARD` device, MS-RDPESC). The protocol part:
//! `RdpSmartcard` turns the blocking `RemoteSmartcard` calls of the desktop side into device control requests
//! (`ServerEvent::Rdpdr`, the same path as the drives) and waits for their completion; `ndr` is the wire format.
//!
//! The client announces one `SCARD` device for all its readers, often before the user is logged on (in broker
//! mode the desktop does not exist yet): `SmartcardRegistry` keeps the device until the host of the session
//! appears, then attaches it, and detaches it when either goes away.

pub(crate) mod ndr;
#[cfg(test)]
mod reference_tests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_rdpdr::pdu::efs::NtStatus;
use ironrdp_server::RdpdrServerMessage;
use oxydesk_core::error::SmartcardError;
use oxydesk_core::smartcard::{
    code, CardStatus, Connected, ReaderQuery, ReaderStatus, RemoteSmartcard, ScCard, ScContext,
    SmartcardHost,
};

use crate::drive::{Body, Link};

/// A call that the client does not answer for this long fails (`get_status_change` has its own limit).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A wait for a card that the application allows to last forever still ends if the client is silent this long.
const WAIT_LIMIT: Duration = Duration::from_secs(60 * 60 * 24);

/// The device is never opened: Windows servers send their IOCTLs to the device without a create, and the clients
/// ignore the file id.
const SCARD_FILE_ID: u32 = 0;

/// `SCARD_PROTOCOL_*` as pcsc-lite (and so the port) numbers them, against the numbers of MS-RDPESC: only `RAW`
/// differs.
const PORT_RAW: u32 = 0x0004;
const WIRE_RAW: u32 = 0x0001_0000;
const PROTOCOL_T0_T1: u32 = 0x0003;

/// The protocols of the port in the numbering of the wire.
pub(crate) fn protocols_to_wire(protocols: u32) -> u32 {
    let mut wire = protocols & PROTOCOL_T0_T1;
    if protocols & PORT_RAW != 0 {
        wire |= WIRE_RAW;
    }
    wire
}

/// The protocol the client reports, in the numbering of the port.
pub(crate) fn protocol_from_wire(protocol: u32) -> u32 {
    let mut port = protocol & PROTOCOL_T0_T1;
    if protocol & WIRE_RAW != 0 {
        port |= PORT_RAW;
    }
    port
}

/// What the client gave for a connection: both parts are needed in every later request.
#[derive(Clone)]
struct Card {
    context: Vec<u8>,
    handle: Vec<u8>,
}

#[derive(Default)]
struct State {
    contexts: HashMap<u64, Vec<u8>>,
    cards: HashMap<u64, Card>,
}

/// The smart card device of the client, as the desktop side sees it.
pub struct RdpSmartcard {
    link: Arc<Link>,
    device_id: u32,
    next_id: AtomicU64,
    state: Mutex<State>,
}

impl RdpSmartcard {
    pub(crate) fn new(link: Arc<Link>, device_id: u32) -> Arc<Self> {
        Arc::new(Self {
            link,
            device_id,
            next_id: AtomicU64::new(1),
            state: Mutex::default(),
        })
    }

    fn link_error(error: impl std::fmt::Display) -> SmartcardError {
        SmartcardError::Link(error.to_string())
    }

    /// Sends one IOCTL and returns the raw output buffer.
    fn ioctl(
        &self,
        control_code: u32,
        input: Vec<u8>,
        max_output: u32,
        limit: Duration,
    ) -> Result<Vec<u8>, SmartcardError> {
        let done = self
            .link
            .call_with(
                RdpdrServerMessage::DeviceControl {
                    device_id: self.device_id,
                    file_id: SCARD_FILE_ID,
                    io_control_code: control_code,
                    input_buffer: input,
                    output_buffer_length: max_output,
                },
                Some(limit),
            )
            .map_err(Self::link_error)?;
        if done.status == NtStatus::CANCELLED {
            return Err(SmartcardError::Code(code::E_CANCELLED));
        }
        if done.status != NtStatus::SUCCESS {
            return Err(Self::link_error(format!(
                "the client failed the request: {:?}",
                done.status
            )));
        }
        match done.body {
            Body::Control(bytes) => Ok(bytes),
            _ => Ok(Vec::new()),
        }
    }

    /// An IOCTL whose output is only a return code.
    fn simple(&self, control_code: u32, input: Vec<u8>) -> Result<(), SmartcardError> {
        let output = self.ioctl(control_code, input, 2048, CALL_TIMEOUT)?;
        check(&output)?;
        Ok(())
    }

    fn context_bytes(&self, context: ScContext) -> Result<Vec<u8>, SmartcardError> {
        self.state
            .lock()
            .expect("state")
            .contexts
            .get(&context.0)
            .cloned()
            .ok_or(SmartcardError::Code(code::E_INVALID_HANDLE))
    }

    fn card(&self, card: ScCard) -> Result<Card, SmartcardError> {
        self.state
            .lock()
            .expect("state")
            .cards
            .get(&card.0)
            .cloned()
            .ok_or(SmartcardError::Code(code::E_INVALID_HANDLE))
    }

    fn mint(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// The return code of an output, as an error when the client failed the call.
fn check(output: &[u8]) -> Result<ndr::Output<'_>, SmartcardError> {
    let out = ndr::parse(output).map_err(RdpSmartcard::link_error)?;
    if out.code == code::SUCCESS {
        Ok(out)
    } else {
        Err(SmartcardError::Code(out.code))
    }
}

fn malformed(error: ndr::DecodeError) -> SmartcardError {
    RdpSmartcard::link_error(error)
}

impl RemoteSmartcard for RdpSmartcard {
    fn establish_context(&self) -> Result<ScContext, SmartcardError> {
        let output = self.ioctl(
            ndr::ioctl::ESTABLISH_CONTEXT,
            ndr::establish_context(),
            2048,
            CALL_TIMEOUT,
        )?;
        let out = check(&output)?;
        let bytes = ndr::read_establish_context(&out).map_err(malformed)?;
        let id = self.mint();
        self.state.lock().expect("state").contexts.insert(id, bytes);
        Ok(ScContext(id))
    }

    fn release_context(&self, context: ScContext) -> Result<(), SmartcardError> {
        let bytes = self.context_bytes(context)?;
        // The cards of the context go with it.
        {
            let mut state = self.state.lock().expect("state");
            state.contexts.remove(&context.0);
            state.cards.retain(|_, c| c.context != bytes);
        }
        self.simple(ndr::ioctl::RELEASE_CONTEXT, ndr::context_call(&bytes))
    }

    fn list_readers(&self, context: ScContext) -> Result<Vec<String>, SmartcardError> {
        let bytes = self.context_bytes(context)?;
        let output = self.ioctl(
            ndr::ioctl::LIST_READERS_W,
            ndr::list_readers(&bytes),
            4096,
            CALL_TIMEOUT,
        )?;
        let out = check(&output)?;
        ndr::read_list_readers(&out).map_err(malformed)
    }

    fn get_status_change(
        &self,
        context: ScContext,
        timeout_ms: u32,
        readers: &[ReaderQuery],
    ) -> Result<Vec<ReaderStatus>, SmartcardError> {
        let bytes = self.context_bytes(context)?;
        // The client waits for `timeout_ms`; the server side only adds a margin for a client that never answers.
        let limit = if timeout_ms == u32::MAX {
            WAIT_LIMIT
        } else {
            Duration::from_millis(u64::from(timeout_ms)) + CALL_TIMEOUT
        };
        let output = self.ioctl(
            ndr::ioctl::GET_STATUS_CHANGE_W,
            ndr::get_status_change(&bytes, timeout_ms, readers),
            4096,
            limit,
        )?;
        let out = check(&output)?;
        ndr::read_get_status_change(&out, readers).map_err(malformed)
    }

    fn cancel(&self, context: ScContext) -> Result<(), SmartcardError> {
        let bytes = self.context_bytes(context)?;
        self.simple(ndr::ioctl::CANCEL, ndr::context_call(&bytes))
    }

    fn connect(
        &self,
        context: ScContext,
        reader: &str,
        share_mode: u32,
        preferred_protocols: u32,
    ) -> Result<Connected, SmartcardError> {
        let ctx = self.context_bytes(context)?;
        let output = self.ioctl(
            ndr::ioctl::CONNECT_W,
            ndr::connect(
                &ctx,
                reader,
                share_mode,
                protocols_to_wire(preferred_protocols),
            ),
            2048,
            CALL_TIMEOUT,
        )?;
        let out = check(&output)?;
        let (handle, protocol) = ndr::read_connect(&out).map_err(malformed)?;
        let id = self.mint();
        self.state.lock().expect("state").cards.insert(
            id,
            Card {
                context: ctx,
                handle,
            },
        );
        Ok(Connected {
            card: ScCard(id),
            protocol: protocol_from_wire(protocol),
        })
    }

    fn reconnect(
        &self,
        card: ScCard,
        share_mode: u32,
        preferred_protocols: u32,
        initialization: u32,
    ) -> Result<u32, SmartcardError> {
        let c = self.card(card)?;
        let output = self.ioctl(
            ndr::ioctl::RECONNECT,
            ndr::reconnect(
                &c.context,
                &c.handle,
                share_mode,
                protocols_to_wire(preferred_protocols),
                initialization,
            ),
            2048,
            CALL_TIMEOUT,
        )?;
        let out = check(&output)?;
        ndr::read_reconnect(&out)
            .map(protocol_from_wire)
            .map_err(malformed)
    }

    fn disconnect(&self, card: ScCard, disposition: u32) -> Result<(), SmartcardError> {
        let c = self.card(card)?;
        self.state.lock().expect("state").cards.remove(&card.0);
        self.simple(
            ndr::ioctl::DISCONNECT,
            ndr::card_and_disposition(&c.context, &c.handle, disposition),
        )
    }

    fn begin_transaction(&self, card: ScCard) -> Result<(), SmartcardError> {
        let c = self.card(card)?;
        self.simple(
            ndr::ioctl::BEGIN_TRANSACTION,
            ndr::card_and_disposition(&c.context, &c.handle, 0),
        )
    }

    fn end_transaction(&self, card: ScCard, disposition: u32) -> Result<(), SmartcardError> {
        let c = self.card(card)?;
        self.simple(
            ndr::ioctl::END_TRANSACTION,
            ndr::card_and_disposition(&c.context, &c.handle, disposition),
        )
    }

    fn transmit(
        &self,
        card: ScCard,
        protocol: u32,
        apdu: &[u8],
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError> {
        let c = self.card(card)?;
        let output = self.ioctl(
            ndr::ioctl::TRANSMIT,
            ndr::transmit(
                &c.context,
                &c.handle,
                protocols_to_wire(protocol),
                apdu,
                max_response,
            ),
            max_response.saturating_add(256),
            CALL_TIMEOUT,
        )?;
        let out = check(&output)?;
        ndr::read_transmit(&out).map_err(malformed)
    }

    fn status(&self, card: ScCard) -> Result<CardStatus, SmartcardError> {
        let c = self.card(card)?;
        let output = self.ioctl(
            ndr::ioctl::STATUS_W,
            ndr::status(&c.context, &c.handle),
            2048,
            CALL_TIMEOUT,
        )?;
        let out = check(&output)?;
        let (reader, state, protocol, atr) = ndr::read_status(&out).map_err(malformed)?;
        Ok(CardStatus {
            reader,
            state,
            protocol: protocol_from_wire(protocol),
            atr,
        })
    }

    fn control(
        &self,
        card: ScCard,
        control_code: u32,
        input: &[u8],
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError> {
        let c = self.card(card)?;
        let output = self.ioctl(
            ndr::ioctl::CONTROL,
            ndr::control(&c.context, &c.handle, control_code, input, max_response),
            max_response.saturating_add(256),
            CALL_TIMEOUT,
        )?;
        let out = check(&output)?;
        ndr::read_buffer(&out).map_err(malformed)
    }

    fn get_attrib(
        &self,
        card: ScCard,
        attribute: u32,
        max_response: u32,
    ) -> Result<Vec<u8>, SmartcardError> {
        let c = self.card(card)?;
        let output = self.ioctl(
            ndr::ioctl::GET_ATTRIB,
            ndr::get_attrib(&c.context, &c.handle, attribute, max_response),
            max_response.saturating_add(256),
            CALL_TIMEOUT,
        )?;
        let out = check(&output)?;
        ndr::read_buffer(&out).map_err(malformed)
    }
}

#[derive(Default)]
struct Registered {
    host: Option<Arc<dyn SmartcardHost>>,
    device: Option<Arc<RdpSmartcard>>,
    /// The host the device is attached to.
    attached: Option<Arc<dyn SmartcardHost>>,
}

/// Brings the smart card device of the client and the host of the session together, whichever comes first.
#[derive(Default)]
pub struct SmartcardRegistry {
    inner: Mutex<Registered>,
}

impl SmartcardRegistry {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// The host of the session appeared (or went away): attaches the device that waits for it.
    pub fn set_host(&self, host: Option<Arc<dyn SmartcardHost>>) {
        let mut inner = self.inner.lock().expect("registry");
        inner.host = host;
        Self::sync(&mut inner);
    }

    /// The client announced its device.
    pub(crate) fn device_announced(&self, device: Arc<RdpSmartcard>) {
        let mut inner = self.inner.lock().expect("registry");
        inner.device = Some(device);
        Self::sync(&mut inner);
    }

    /// The client removed its device, or the connection ended.
    pub fn device_removed(&self) {
        let mut inner = self.inner.lock().expect("registry");
        inner.device = None;
        Self::sync(&mut inner);
    }

    /// Detaches from a host that is not the current one, attaches to the current one.
    fn sync(inner: &mut Registered) {
        let wanted = inner.host.clone().zip(inner.device.clone());
        let same = match (&inner.attached, &wanted) {
            (Some(a), Some((h, _))) => Arc::ptr_eq(a, h),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        if let Some(old) = inner.attached.take() {
            tracing::info!("smart card device detached from the session");
            old.detached();
        }
        if let Some((host, device)) = wanted {
            tracing::info!("smart card device attached to the session");
            host.attached(device);
            inner.attached = Some(host);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_is_renumbered_and_the_rest_kept() {
        assert_eq!(protocols_to_wire(0x3), 0x3);
        assert_eq!(protocols_to_wire(0x4), 0x1_0000);
        assert_eq!(protocols_to_wire(0x7), 0x1_0003);
        assert_eq!(protocols_to_wire(0), 0);
        assert_eq!(protocol_from_wire(0x2), 0x2);
        assert_eq!(protocol_from_wire(0x1_0000), 0x4);
        assert_eq!(protocol_from_wire(0x8000_0001), 0x1);
    }
}
