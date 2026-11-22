//! Client side of the smart card redirection (MS-RDPESC) over a fake reader with one fake card: what the
//! server's loopback test talks to. It answers like the winscard subsystem of a Windows client would, and
//! records the calls it received.

use std::sync::{Arc, Mutex};

use ironrdp_core::impl_as_any;
use ironrdp_pdu::utils::CharacterSet;
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::pdu::efs::{
    DeviceControlRequest, DeviceControlResponse, NtStatus, PrinterIoRequest,
    ServerDeviceAnnounceResponse, ServerDriveIoRequest,
};
use ironrdp_rdpdr::pdu::esc::{
    CardProtocol, CardState, CardStateFlags, ConnectReturn, ControlReturn, EstablishContextReturn,
    GetStatusChangeReturn, ListReadersReturn, LongReturn, ReaderStateCommonCall, ReconnectReturn,
    ReturnCode, ScardCall, ScardContext, ScardHandle, ScardIoCtlCode, StatusReturn, TransmitReturn,
};
use ironrdp_rdpdr::pdu::RdpdrPdu;
use ironrdp_rdpdr::Rdpdr;
use ironrdp_svc::SvcMessage;

/// The name of the fake reader (the PC/SC naming scheme: vendor, model, slot).
pub const READER: &str = "Fake Vendor Reader 00 00";
/// The ATR of the fake card (a carte vitale).
pub const ATR: [u8; 10] = [0x3B, 0x75, 0x13, 0x00, 0x00, 0x45, 0x09, 0xEA, 0x90, 0x00];
/// The bytes the fake card answers to any APDU, after the two first bytes of the command.
pub const RESPONSE: [u8; 4] = [0xCA, 0xFE, 0x90, 0x00];

/// What the fake client received, in order (`"EstablishContext"`, `"Transmit"`, ...).
pub type Calls = Arc<Mutex<Vec<String>>>;

#[derive(Debug)]
pub struct FakeSmartcard {
    calls: Calls,
    /// The reader holds a card.
    card: bool,
    next_context: u32,
}

impl_as_any!(FakeSmartcard);

impl FakeSmartcard {
    pub fn new(calls: Calls, card: bool) -> Self {
        Self {
            calls,
            card,
            next_context: 0x1000,
        }
    }

    /// The RDPDR channel of the fake client, announcing its smart card device.
    pub fn channel(calls: Calls, card: bool, device_id: u32, computer: &str) -> Rdpdr {
        Rdpdr::new(Box::new(Self::new(calls, card)), computer.to_owned()).with_smartcard(device_id)
    }

    fn log(&self, name: &str) {
        self.calls.lock().unwrap().push(name.to_owned());
    }

    fn event_state(&self) -> u32 {
        if self.card {
            // PRESENT | CHANGED, one event.
            0x0001_0022
        } else {
            0x0001_0012
        }
    }

    fn answer(
        req: DeviceControlRequest<ScardIoCtlCode>,
        pdu: impl ironrdp_rdpdr::pdu::esc::rpce::Encode + 'static,
    ) -> PduResult<Vec<SvcMessage>> {
        let response = DeviceControlResponse::new(req, NtStatus::SUCCESS, Some(Box::new(pdu)));
        Ok(vec![SvcMessage::from(RdpdrPdu::DeviceControlResponse(
            response,
        ))])
    }
}

impl ironrdp_rdpdr::RdpdrBackend for FakeSmartcard {
    fn handle_server_device_announce_response(
        &mut self,
        _: ServerDeviceAnnounceResponse,
    ) -> PduResult<()> {
        Ok(())
    }

    fn handle_scard_call(
        &mut self,
        req: DeviceControlRequest<ScardIoCtlCode>,
        call: ScardCall,
    ) -> PduResult<Vec<SvcMessage>> {
        match call {
            ScardCall::EstablishContextCall(_) => {
                self.log("EstablishContext");
                self.next_context += 1;
                let context = ScardContext::new(self.next_context);
                Self::answer(
                    req,
                    EstablishContextReturn::new(ReturnCode::Success, context),
                )
            }
            ScardCall::ContextCall(_) => {
                // Release and Cancel carry the same body; the IOCTL tells them apart.
                let name = match req.io_control_code {
                    ScardIoCtlCode::ReleaseContext => "ReleaseContext",
                    ScardIoCtlCode::Cancel => "Cancel",
                    _ => "Context",
                };
                self.log(name);
                Self::answer(req, LongReturn::new(ReturnCode::Success))
            }
            ScardCall::ListReadersCall(_) => {
                self.log("ListReaders");
                Self::answer(
                    req,
                    ListReadersReturn::new(
                        ReturnCode::Success,
                        vec![READER.to_owned()],
                        CharacterSet::Unicode,
                    ),
                )
            }
            ScardCall::GetStatusChangeCall(c) => {
                self.log("GetStatusChange");
                let states = c
                    .states
                    .iter()
                    .map(|s| {
                        let mut atr = [0u8; 36];
                        let known = s.reader == READER;
                        let present = known && self.card;
                        if present {
                            atr[..ATR.len()].copy_from_slice(&ATR);
                        }
                        ReaderStateCommonCall {
                            current_state: s.common.current_state,
                            event_state: CardStateFlags::from_bits_retain(if known {
                                self.event_state()
                            } else {
                                0x0000_0004 | 0x0002
                            }),
                            atr_length: if present { ATR.len() as u32 } else { 0 },
                            atr,
                        }
                    })
                    .collect();
                Self::answer(req, GetStatusChangeReturn::new(ReturnCode::Success, states))
            }
            ScardCall::ConnectCall(c) => {
                self.log("Connect");
                if c.reader != READER {
                    return Self::answer(
                        req,
                        ConnectReturn::new(
                            ReturnCode::UnknownReader,
                            ScardHandle::new(c.common.context, 0),
                            CardProtocol::empty(),
                        ),
                    );
                }
                if !self.card {
                    return Self::answer(
                        req,
                        ConnectReturn::new(
                            ReturnCode::NoSmartcard,
                            ScardHandle::new(c.common.context, 0),
                            CardProtocol::empty(),
                        ),
                    );
                }
                let handle = ScardHandle::new(c.common.context, 0x2000);
                Self::answer(
                    req,
                    ConnectReturn::new(
                        ReturnCode::Success,
                        handle,
                        CardProtocol::SCARD_PROTOCOL_T1,
                    ),
                )
            }
            ScardCall::ReconnectCall(_) => {
                self.log("Reconnect");
                Self::answer(
                    req,
                    ReconnectReturn::new(ReturnCode::Success, CardProtocol::SCARD_PROTOCOL_T1),
                )
            }
            ScardCall::HCardAndDispositionCall(_) => {
                let name = match req.io_control_code {
                    ScardIoCtlCode::Disconnect => "Disconnect",
                    ScardIoCtlCode::BeginTransaction => "BeginTransaction",
                    ScardIoCtlCode::EndTransaction => "EndTransaction",
                    _ => "HCardAndDisposition",
                };
                self.log(name);
                Self::answer(req, LongReturn::new(ReturnCode::Success))
            }
            ScardCall::TransmitCall(c) => {
                self.log("Transmit");
                // The card echoes the two first bytes of the command, then answers `RESPONSE`.
                let mut response: Vec<u8> = c.send_buffer.iter().take(2).copied().collect();
                response.extend_from_slice(&RESPONSE);
                Self::answer(
                    req,
                    TransmitReturn::new(ReturnCode::Success, None, response),
                )
            }
            ScardCall::StatusCall(_) => {
                self.log("Status");
                let mut atr = [0u8; 32];
                atr[..ATR.len()].copy_from_slice(&ATR);
                Self::answer(
                    req,
                    StatusReturn::new(
                        ReturnCode::Success,
                        vec![READER.to_owned()],
                        CardState::SpecificMode,
                        CardProtocol::SCARD_PROTOCOL_T1,
                        atr,
                        ATR.len() as u32,
                        CharacterSet::Unicode,
                    ),
                )
            }
            ScardCall::ControlCall(_) => {
                self.log("Control");
                Self::answer(req, ControlReturn::new(ReturnCode::Success, vec![1, 2, 3]))
            }
            _ => {
                self.log("Unsupported");
                Self::answer(req, LongReturn::new(ReturnCode::UnsupportedFeature))
            }
        }
    }

    fn handle_printer_io_request(&mut self, _: PrinterIoRequest) -> PduResult<Vec<SvcMessage>> {
        Ok(Vec::new())
    }

    fn handle_drive_io_request(&mut self, _: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        Ok(Vec::new())
    }
}
