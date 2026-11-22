//! One client connection: reads the requests, answers them from the table of readers or through the
//! client's smart card subsystem.
//!
//! As in pcsc-lite one connection is one application context. The list of readers and the state changes
//! are not asked to the backend: the application reads the table of readers (`CMD_GET_READERS_STATE`) and
//! `libpcsclite` works out the changes itself. Only the card calls go to the backend.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use oxydesk_core::error::SmartcardError;
use oxydesk_core::smartcard::{code, RemoteSmartcard, ScCard, ScContext};

use crate::service::{ConnShared, Service};
use crate::wire::{
    self, read_request, Request, WireError, MAX_BUFFER_SIZE, MAX_BUFFER_SIZE_EXTENDED,
};

/// Most cards one context may hold open (pcsc-lite: 200).
const MAX_CARDS: usize = 64;
/// `SCARD_LEAVE_CARD`.
const LEAVE_CARD: u32 = 0;
/// `SCARD_SCOPE_*` accepted by `SCardEstablishContext`.
const MAX_SCOPE: u32 = 2;

/// The connection must be closed.
#[derive(Debug)]
struct Close(String);

type Outcome = Result<(), Close>;

struct Card {
    card: ScCard,
    /// The name of the reader as the application knows it.
    reader: String,
    epoch: u64,
}

/// A card handle of the current device, with the device that owns it.
type LiveCard = (Arc<dyn RemoteSmartcard>, ScCard);

struct Session {
    service: Arc<Service>,
    shared: Arc<ConnShared>,
    /// The `hContext` of this connection, once established.
    context: Option<u32>,
    /// The context of the backend (made at the first connection to a card) and the epoch it belongs to.
    backend_context: Option<(u64, ScContext)>,
    cards: HashMap<i32, Card>,
}

/// Serves one client until it disconnects. Everything it holds is released at the end.
pub(crate) fn serve(service: Arc<Service>, shared: Arc<ConnShared>, mut input: UnixStream) {
    let mut session = Session {
        service: service.clone(),
        shared: shared.clone(),
        context: None,
        backend_context: None,
        cards: HashMap::new(),
    };
    loop {
        let request = match read_request(&mut input) {
            Ok(r) => r,
            Err(WireError::Io(e)) => {
                tracing::debug!(conn = shared.id, "pcsc client gone: {e}");
                break;
            }
            Err(e) => {
                tracing::warn!(conn = shared.id, "pcsc client dropped: {e}");
                break;
            }
        };
        if let Err(Close(why)) = session.handle(request) {
            tracing::warn!(conn = shared.id, "pcsc client dropped: {why}");
            break;
        }
    }
    session.release_all();
    shared.shutdown();
    service.remove_conn(shared.id);
}

fn unavailable() -> SmartcardError {
    SmartcardError::Code(code::E_READER_UNAVAILABLE)
}

fn rv_of<T>(r: &Result<T, SmartcardError>) -> u32 {
    match r {
        Ok(_) => code::SUCCESS,
        Err(e) => e.scard_code(),
    }
}

impl Session {
    fn send(&self, parts: &[&[u8]]) -> Outcome {
        self.shared
            .send(parts)
            .map_err(|e| Close(format!("write failed: {e}")))
    }

    fn reply(&self, message: Vec<u8>) -> Outcome {
        self.send(&[&message])
    }

    fn handle(&mut self, request: Request) -> Outcome {
        match request {
            Request::Version { major, minor } => {
                self.reply(wire::version_reply(wire::version_result(major, minor)))
            }
            Request::GetReadersState => {
                let blob = self.service.table_blob();
                self.send(&[&blob])
            }
            Request::WaitReaderStateChange => {
                let service = self.service.clone();
                self.shared
                    .register_wait(move || service.table_blob())
                    .map_err(|e| Close(format!("write failed: {e}")))
            }
            Request::StopWaitingReaderStateChange => self
                .shared
                .stop_wait()
                .map_err(|e| Close(format!("write failed: {e}"))),
            Request::GetReaderEvents => {
                self.reply(wire::reader_events_reply(self.service.reader_events()))
            }
            Request::EstablishContext { scope } => self.establish(scope),
            Request::ReleaseContext { context } => self.release(context),
            Request::Cancel { context } => {
                let rv = if self.service.cancel_context(context) {
                    code::SUCCESS
                } else {
                    code::E_INVALID_HANDLE
                };
                self.reply(wire::cancel_reply(context, rv))
            }
            Request::Connect {
                context,
                reader,
                share_mode,
                preferred_protocols,
            } => self.connect(context, &reader, share_mode, preferred_protocols),
            Request::Reconnect {
                card,
                share_mode,
                preferred_protocols,
                initialization,
            } => {
                let result = match self.live(card)? {
                    Some((sc, c)) => {
                        sc.reconnect(c, share_mode, preferred_protocols, initialization)
                    }
                    None => Err(unavailable()),
                };
                if let (Ok(p), Some(c)) = (&result, self.cards.get(&card)) {
                    self.service.note_protocol(&c.reader, *p);
                }
                self.reply(wire::reconnect_reply(
                    card,
                    share_mode,
                    preferred_protocols,
                    initialization,
                    *result.as_ref().unwrap_or(&0),
                    rv_of(&result),
                ))
            }
            Request::Disconnect { card, disposition } => self.disconnect(card, disposition),
            Request::BeginTransaction { card } => {
                let rv = match self.live(card)? {
                    Some((sc, c)) => rv_of(&sc.begin_transaction(c)),
                    None => code::E_READER_UNAVAILABLE,
                };
                self.reply(wire::begin_reply(card, rv))
            }
            Request::EndTransaction { card, disposition } => {
                let rv = match self.live(card)? {
                    Some((sc, c)) => rv_of(&sc.end_transaction(c, disposition)),
                    None => code::E_READER_UNAVAILABLE,
                };
                self.reply(wire::end_reply(card, disposition, rv))
            }
            Request::Status { card } => {
                let rv = match self.live(card)? {
                    Some((sc, c)) => rv_of(&sc.status(c)),
                    None => code::E_READER_UNAVAILABLE,
                };
                self.reply(wire::status_reply(card, rv))
            }
            Request::Transmit {
                card,
                send_protocol,
                send_pci_len,
                recv_protocol,
                recv_pci_len,
                recv_len,
                data,
            } => self.transmit(
                card,
                (send_protocol, send_pci_len, recv_protocol, recv_pci_len),
                recv_len,
                &data,
            ),
            Request::Control {
                card,
                code: control_code,
                recv_len,
                data,
            } => self.control(card, control_code, recv_len, &data),
            Request::GetAttrib { card, attribute } => {
                let result = match self.live(card)? {
                    Some((sc, c)) => sc.get_attrib(c, attribute, MAX_BUFFER_SIZE as u32),
                    None => Err(unavailable()),
                };
                match result {
                    Ok(v) if v.len() <= MAX_BUFFER_SIZE => self.reply(wire::attrib_reply(
                        card,
                        attribute,
                        &v,
                        v.len() as u32,
                        code::SUCCESS,
                    )),
                    Ok(v) => self.reply(wire::attrib_reply(
                        card,
                        attribute,
                        &[],
                        v.len() as u32,
                        code::E_INSUFFICIENT_BUFFER,
                    )),
                    Err(e) => {
                        self.reply(wire::attrib_reply(card, attribute, &[], 0, e.scard_code()))
                    }
                }
            }
            Request::SetAttrib {
                card, attribute, ..
            } => {
                // The port has no `SCardSetAttrib`: the client's subsystem is not asked.
                let rv = match self.live(card)? {
                    Some(_) => code::E_UNSUPPORTED_FEATURE,
                    None => code::E_READER_UNAVAILABLE,
                };
                self.reply(wire::attrib_reply(card, attribute, &[], 0, rv))
            }
        }
    }

    fn establish(&mut self, scope: u32) -> Outcome {
        let (context, rv) = if scope > MAX_SCOPE {
            (0, code::E_INVALID_VALUE)
        } else if self.context.is_some() {
            // A connection is one context.
            (0, code::E_INVALID_VALUE)
        } else {
            let id = self.service.register_context(self.shared.id);
            self.context = Some(id);
            (id, code::SUCCESS)
        };
        self.reply(wire::establish_reply(scope, context, rv))
    }

    fn release(&mut self, context: u32) -> Outcome {
        let rv = match self.context {
            None => code::E_INVALID_HANDLE,
            Some(c) if c != context => code::E_INVALID_VALUE,
            Some(c) => {
                self.release_all();
                self.service.unregister_context(c);
                self.context = None;
                code::SUCCESS
            }
        };
        self.reply(wire::release_reply(context, rv))
    }

    /// The backend and the context of the backend of this connection (made when first needed).
    fn backend_context(&mut self) -> Result<(Arc<dyn RemoteSmartcard>, ScContext, u64), u32> {
        let (epoch, smartcard) = self.service.current_backend().ok_or(code::E_NO_SERVICE)?;
        if let Some((e, context)) = self.backend_context {
            if e == epoch {
                return Ok((smartcard, context, epoch));
            }
        }
        // The device is new: whatever was opened on the old one is dead.
        self.backend_context = None;
        let context = smartcard.establish_context().map_err(|e| e.scard_code())?;
        self.backend_context = Some((epoch, context));
        Ok((smartcard, context, epoch))
    }

    fn connect(&mut self, context: u32, reader: &str, share_mode: u32, protocols: u32) -> Outcome {
        let fail = |rv: u32| wire::connect_reply(context, reader, share_mode, protocols, 0, 0, rv);
        match self.context {
            Some(c) if c == context => {}
            _ => return self.reply(fail(code::E_INVALID_HANDLE)),
        }
        if self.cards.len() >= MAX_CARDS {
            return self.reply(fail(code::E_NO_MEMORY));
        }
        let Some(backend_name) = self.service.backend_reader_name(reader) else {
            return self.reply(fail(code::E_UNKNOWN_READER));
        };
        let (smartcard, backend_context, epoch) = match self.backend_context() {
            Ok(x) => x,
            Err(rv) => return self.reply(fail(rv)),
        };
        match smartcard.connect(backend_context, &backend_name, share_mode, protocols) {
            Ok(connected) => {
                let cards = &self.cards;
                let handle = self.service.mint_card(|h| cards.contains_key(&h));
                self.cards.insert(
                    handle,
                    Card {
                        card: connected.card,
                        reader: reader.to_string(),
                        epoch,
                    },
                );
                self.service.note_protocol(reader, connected.protocol);
                self.reply(wire::connect_reply(
                    context,
                    reader,
                    share_mode,
                    protocols,
                    handle,
                    connected.protocol,
                    code::SUCCESS,
                ))
            }
            Err(e) => self.reply(fail(e.scard_code())),
        }
    }

    fn disconnect(&mut self, handle: i32, disposition: u32) -> Outcome {
        let Some(card) = self.cards.get(&handle) else {
            return Err(Close(format!("unknown card handle {handle:#x}")));
        };
        let rv = match self.service.current_backend() {
            Some((epoch, smartcard)) if epoch == card.epoch => {
                let r = smartcard.disconnect(card.card, disposition);
                match &r {
                    // The backend still has the card: keep the handle so that the end of the context frees it.
                    Err(SmartcardError::Code(_)) => {}
                    _ => {
                        self.cards.remove(&handle);
                    }
                }
                rv_of(&r)
            }
            // The device went away: the card went with it.
            _ => {
                self.cards.remove(&handle);
                code::SUCCESS
            }
        };
        self.reply(wire::disconnect_reply(handle, disposition, rv))
    }

    /// The backend and the card behind `handle`. A handle that this connection does not own ends the connection
    /// (as in pcsc-lite); `None` is a card of a device that is gone (`SCARD_E_READER_UNAVAILABLE`).
    fn live(&self, handle: i32) -> Result<Option<LiveCard>, Close> {
        let Some(card) = self.cards.get(&handle) else {
            return Err(Close(format!("unknown card handle {handle:#x}")));
        };
        Ok(match self.service.current_backend() {
            Some((epoch, smartcard)) if epoch == card.epoch => Some((smartcard, card.card)),
            _ => None,
        })
    }

    fn transmit(
        &mut self,
        handle: i32,
        pci: (u32, u32, u32, u32),
        recv_len: u32,
        apdu: &[u8],
    ) -> Outcome {
        let (send_protocol, send_pci_len, recv_protocol, recv_pci_len) = pci;
        // As pcsc-lite, ask for the largest answer: the size is then known to tell an application whose buffer is
        // too small how much it needs.
        let max_response = recv_len as usize;
        let result = match self.live(handle)? {
            Some((sc, c)) => sc.transmit(c, send_protocol, apdu, MAX_BUFFER_SIZE_EXTENDED as u32),
            None => Err(unavailable()),
        };
        let reply = |len: u32, rv: u32| {
            wire::transmit_reply(
                handle,
                send_protocol,
                send_pci_len,
                apdu.len() as u32,
                // The answer carries the protocol the application sent with.
                if recv_protocol == 0 {
                    send_protocol
                } else {
                    recv_protocol
                },
                recv_pci_len,
                len,
                rv,
            )
        };
        match result {
            Ok(answer) if answer.len() > max_response => {
                self.reply(reply(answer.len() as u32, code::E_INSUFFICIENT_BUFFER))
            }
            Ok(answer) => self.send(&[&reply(answer.len() as u32, code::SUCCESS), &answer]),
            Err(e) => self.reply(reply(0, e.scard_code())),
        }
    }

    fn control(&mut self, handle: i32, control_code: u32, recv_len: u32, input: &[u8]) -> Outcome {
        let max_response = recv_len as usize;
        let result = match self.live(handle)? {
            Some((sc, c)) => sc.control(c, control_code, input, MAX_BUFFER_SIZE_EXTENDED as u32),
            None => Err(unavailable()),
        };
        let reply = |len: u32, rv: u32| {
            wire::control_reply(handle, control_code, input.len() as u32, recv_len, len, rv)
        };
        match result {
            Ok(out) if out.len() > max_response => {
                self.reply(reply(out.len() as u32, code::E_INSUFFICIENT_BUFFER))
            }
            Ok(out) => self.send(&[&reply(out.len() as u32, code::SUCCESS), &out]),
            Err(e) => self.reply(reply(0, e.scard_code())),
        }
    }

    /// Disconnects the cards and releases the context of the backend (the client left, or released its context).
    fn release_all(&mut self) {
        let cards = std::mem::take(&mut self.cards);
        let backend_context = self.backend_context.take();
        let Some((epoch, smartcard)) = self.service.current_backend() else {
            return; // nothing is attached: nothing to give back
        };
        for (handle, card) in cards {
            if card.epoch == epoch {
                if let Err(e) = smartcard.disconnect(card.card, LEAVE_CARD) {
                    tracing::debug!(handle, "disconnect at the end of the context: {e}");
                }
            }
        }
        if let Some((e, context)) = backend_context {
            if e == epoch {
                if let Err(e) = smartcard.release_context(context) {
                    tracing::debug!("release of the context: {e}");
                }
            }
        }
    }
}
