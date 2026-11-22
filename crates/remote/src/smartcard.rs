//! The client's smart card readers, for a session served by an agent: the agent serves them to the session as
//! PC/SC readers (it runs as that user, the server cannot) and sends every call through the link. The contexts
//! and cards are the numbers the RDP adapter minted: they cross the link as they are.

use std::sync::{Arc, Mutex, Weak};

use oxydesk_agent_proto::{ScOp, ScReaderState, ScResult, ToAgent};
use oxydesk_core::error::SmartcardError;
use oxydesk_core::smartcard::{ReaderQuery, ScCard, ScContext};
use oxydesk_core::{RemoteSmartcard, SmartcardHost};

use crate::link::AgentLink;

/// The device the client announced, if any.
#[derive(Default)]
pub(crate) struct Smartcards {
    current: Mutex<Option<Arc<dyn RemoteSmartcard>>>,
}

impl Smartcards {
    fn get(&self) -> Option<Arc<dyn RemoteSmartcard>> {
        self.current.lock().expect("smartcard mutex").clone()
    }
}

fn failed(e: SmartcardError) -> ScResult {
    match e {
        SmartcardError::Code(c) => ScResult::Code(c),
        SmartcardError::Link(m) => ScResult::Link(m),
    }
}

fn done<T>(r: Result<T, SmartcardError>, ok: impl FnOnce(T) -> ScResult) -> ScResult {
    r.map_or_else(failed, ok)
}

/// Runs the operation asked by the agent on the client's device. Blocking (a status change can wait for the
/// card for as long as the application allows): not for the reader thread.
pub(crate) fn execute(link: &AgentLink, op: ScOp) -> ScResult {
    let Some(sc) = link.smartcards.get() else {
        return ScResult::Link("no smart card device".into());
    };
    let card = ScCard;
    let context = ScContext;
    match op {
        ScOp::EstablishContext => done(sc.establish_context(), |c| ScResult::Context(c.0)),
        ScOp::ReleaseContext(c) => done(sc.release_context(context(c)), |()| ScResult::Ok),
        ScOp::ListReaders(c) => done(sc.list_readers(context(c)), ScResult::Readers),
        ScOp::GetStatusChange {
            context: c,
            timeout_ms,
            readers,
        } => {
            let queries: Vec<ReaderQuery> = readers
                .into_iter()
                .map(|r| ReaderQuery {
                    name: r.name,
                    current_state: r.current_state,
                })
                .collect();
            done(
                sc.get_status_change(context(c), timeout_ms, &queries),
                |states| {
                    ScResult::States(
                        states
                            .into_iter()
                            .map(|s| ScReaderState {
                                name: s.name,
                                event_state: s.event_state,
                                atr: s.atr,
                            })
                            .collect(),
                    )
                },
            )
        }
        ScOp::Cancel(c) => done(sc.cancel(context(c)), |()| ScResult::Ok),
        ScOp::Connect {
            context: c,
            reader,
            share_mode,
            protocols,
        } => done(
            sc.connect(context(c), &reader, share_mode, protocols),
            |k| ScResult::Connected {
                card: k.card.0,
                protocol: k.protocol,
            },
        ),
        ScOp::Reconnect {
            card: k,
            share_mode,
            protocols,
            initialization,
        } => done(
            sc.reconnect(card(k), share_mode, protocols, initialization),
            ScResult::Protocol,
        ),
        ScOp::Disconnect {
            card: k,
            disposition,
        } => done(sc.disconnect(card(k), disposition), |()| ScResult::Ok),
        ScOp::BeginTransaction(k) => done(sc.begin_transaction(card(k)), |()| ScResult::Ok),
        ScOp::EndTransaction {
            card: k,
            disposition,
        } => done(sc.end_transaction(card(k), disposition), |()| ScResult::Ok),
        ScOp::Transmit {
            card: k,
            protocol,
            apdu,
            max_response,
        } => done(
            sc.transmit(card(k), protocol, &apdu, max_response),
            ScResult::Data,
        ),
        ScOp::Status(k) => done(sc.status(card(k)), |s| ScResult::Card {
            reader: s.reader,
            state: s.state,
            protocol: s.protocol,
            atr: s.atr,
        }),
        ScOp::Control {
            card: k,
            code,
            input,
            max_response,
        } => done(
            sc.control(card(k), code, &input, max_response),
            ScResult::Data,
        ),
        ScOp::GetAttrib {
            card: k,
            attribute,
            max_response,
        } => done(
            sc.get_attrib(card(k), attribute, max_response),
            ScResult::Data,
        ),
    }
}

/// `SmartcardHost` that hands the client's readers to the agent of the session.
pub struct AgentSmartcards(pub(crate) Weak<AgentLink>);

impl SmartcardHost for AgentSmartcards {
    fn attached(&self, smartcard: Arc<dyn RemoteSmartcard>) {
        let Some(link) = self.0.upgrade() else { return };
        *link.smartcards.current.lock().expect("smartcard mutex") = Some(smartcard);
        link.send(&ToAgent::SmartcardAttached);
    }

    fn detached(&self) {
        let Some(link) = self.0.upgrade() else { return };
        *link.smartcards.current.lock().expect("smartcard mutex") = None;
        link.send(&ToAgent::SmartcardDetached);
    }
}
