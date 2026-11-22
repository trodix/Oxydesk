//! The server side codec of `ndr` against the client side of `ironrdp-rdpdr` (what mstsc and FreeRDP are
//! modelled on): every call built here must decode there, every answer built there must decode here.

use ironrdp_core::{encode_vec, ReadCursor};
use ironrdp_pdu::utils::CharacterSet;
use ironrdp_rdpdr::pdu::esc::{
    CardProtocol, CardState, CardStateFlags, ConnectReturn, ControlReturn, EstablishContextReturn,
    GetAttribReturn, GetStatusChangeReturn, ListReadersReturn, ReaderStateCommonCall,
    ReconnectReturn, ReturnCode, ScardCall, ScardContext, ScardHandle, ScardIoCtlCode,
    StatusReturn, TransmitReturn,
};
use oxydesk_core::smartcard::ReaderQuery;

use super::ndr::{self, ioctl};

const CTX8: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
const HANDLE4: [u8; 4] = [9, 10, 11, 12];

fn decode(code: u32, input: &[u8]) -> ScardCall {
    let code = ScardIoCtlCode::try_from(code).expect("known IOCTL");
    let mut cursor = ReadCursor::new(input);
    let call = ScardCall::decode(code, &mut cursor).expect("the client decodes the call");
    // Only the padding that aligns the buffer on 8 bytes is left.
    let rest = cursor.read_remaining();
    assert!(
        rest.len() < 8 && rest.iter().all(|b| *b == 0),
        "unread: {rest:?}"
    );
    call
}

fn handle() -> ScardHandle {
    ScardHandle::from_opaque(ScardContext::from_opaque(&CTX8).unwrap(), &HANDLE4).unwrap()
}

fn answer(pdu: impl ironrdp_core::Encode) -> Vec<u8> {
    encode_vec(&pdu).expect("encodes")
}

#[test]
fn establish_context_call() {
    let ScardCall::EstablishContextCall(call) =
        decode(ioctl::ESTABLISH_CONTEXT, &ndr::establish_context())
    else {
        panic!("wrong call")
    };
    assert_eq!(call.scope as u32, 2);
}

#[test]
fn context_calls() {
    for code in [ioctl::RELEASE_CONTEXT, ioctl::CANCEL] {
        let ScardCall::ContextCall(call) = decode(code, &ndr::context_call(&CTX8)) else {
            panic!("wrong call")
        };
        assert_eq!(call.context.as_bytes(), CTX8);
    }
    // A 4-byte context (32-bit client) too.
    let ScardCall::ContextCall(call) = decode(ioctl::CANCEL, &ndr::context_call(&[7, 0, 0, 0]))
    else {
        panic!("wrong call")
    };
    assert_eq!(call.context.as_bytes(), [7, 0, 0, 0]);
}

#[test]
fn list_readers_call() {
    let ScardCall::ListReadersCall(call) = decode(ioctl::LIST_READERS_W, &ndr::list_readers(&CTX8))
    else {
        panic!("wrong call")
    };
    assert_eq!(call.context.as_bytes(), CTX8);
    assert!(call.groups.is_empty());
    assert!(!call.readers_is_null);
    assert_eq!(call.readers_size, 0xFFFF_FFFF);
}

#[test]
fn get_status_change_call() {
    let readers = [
        ReaderQuery {
            name: "Reader A 0".into(),
            current_state: 0,
        },
        ReaderQuery {
            name: "Lecteur é 1".into(),
            current_state: 0x0002_0022,
        },
        ReaderQuery {
            name: "\\\\?PnP?\\Notification".into(),
            current_state: 0,
        },
    ];
    let ScardCall::GetStatusChangeCall(call) = decode(
        ioctl::GET_STATUS_CHANGE_W,
        &ndr::get_status_change(&CTX8, 1234, &readers),
    ) else {
        panic!("wrong call")
    };
    assert_eq!(call.context.as_bytes(), CTX8);
    assert_eq!(call.timeout, 1234);
    assert_eq!(call.states.len(), 3);
    for (state, query) in call.states.iter().zip(&readers) {
        assert_eq!(state.reader, query.name);
        assert_eq!(state.common.current_state.bits(), query.current_state);
    }
}

#[test]
fn connect_call() {
    let ScardCall::ConnectCall(call) = decode(
        ioctl::CONNECT_W,
        &ndr::connect(&CTX8, "Fake Reader 0", 2, 3),
    ) else {
        panic!("wrong call")
    };
    assert_eq!(call.reader, "Fake Reader 0");
    assert_eq!(call.common.context.as_bytes(), CTX8);
    assert_eq!(call.common.share_mode, 2);
    assert_eq!(call.common.preferred_protocols.bits(), 3);
}

#[test]
fn reconnect_call() {
    let ScardCall::ReconnectCall(call) =
        decode(ioctl::RECONNECT, &ndr::reconnect(&CTX8, &HANDLE4, 2, 1, 1))
    else {
        panic!("wrong call")
    };
    assert_eq!(call.handle, handle());
    assert_eq!(
        (
            call.share_mode,
            call.preferred_protocols.bits(),
            call.initialization
        ),
        (2, 1, 1)
    );
}

#[test]
fn card_and_disposition_calls() {
    for code in [
        ioctl::DISCONNECT,
        ioctl::BEGIN_TRANSACTION,
        ioctl::END_TRANSACTION,
    ] {
        let ScardCall::HCardAndDispositionCall(call) =
            decode(code, &ndr::card_and_disposition(&CTX8, &HANDLE4, 2))
        else {
            panic!("wrong call")
        };
        assert_eq!(call.handle, handle());
        assert_eq!(call.disposition, 2);
    }
}

#[test]
fn transmit_call() {
    let apdu = [0x00, 0xA4, 0x04, 0x00, 0x02, 0xAA, 0xBB];
    let ScardCall::TransmitCall(call) = decode(
        ioctl::TRANSMIT,
        &ndr::transmit(&CTX8, &HANDLE4, 2, &apdu, 300),
    ) else {
        panic!("wrong call")
    };
    assert_eq!(call.handle, handle());
    assert_eq!(call.send_pci.protocol.bits(), 2);
    assert_eq!(call.send_buffer, apdu);
    assert!(call.recv_pci.is_none());
    assert_eq!(call.recv_length, 300);
    assert!(!call.recv_buffer_is_null);
}

#[test]
fn status_control_and_attrib_calls() {
    let ScardCall::StatusCall(call) = decode(ioctl::STATUS_W, &ndr::status(&CTX8, &HANDLE4)) else {
        panic!("wrong call")
    };
    assert_eq!(call.handle, handle());
    assert!(!call.reader_names_is_null);

    let ScardCall::ControlCall(call) = decode(
        ioctl::CONTROL,
        &ndr::control(&CTX8, &HANDLE4, 0x42000C00, &[1, 2, 3], 64),
    ) else {
        panic!("wrong call")
    };
    assert_eq!(call.control_code, 0x42000C00);
    assert_eq!(call.in_buffer, [1, 2, 3]);
    assert_eq!(call.out_buffer_size, 64);

    let ScardCall::ControlCall(call) = decode(
        ioctl::CONTROL,
        &ndr::control(&CTX8, &HANDLE4, 0x42000C00, &[], 64),
    ) else {
        panic!("wrong call")
    };
    assert!(call.in_buffer.is_empty());

    let ScardCall::GetAttribCall(call) = decode(
        ioctl::GET_ATTRIB,
        &ndr::get_attrib(&CTX8, &HANDLE4, 0x0001_0100, 256),
    ) else {
        panic!("wrong call")
    };
    assert_eq!(call.attr_id, 0x0001_0100);
    assert_eq!(call.attr_length, 256);
}

#[test]
fn establish_context_answer() {
    let bytes = answer(EstablishContextReturn::new(
        ReturnCode::Success,
        ScardContext::from_opaque(&CTX8).unwrap(),
    ));
    let out = ndr::parse(&bytes).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(ndr::read_establish_context(&out).unwrap(), CTX8);
}

#[test]
fn error_answers_carry_the_code() {
    let bytes = answer(ReconnectReturn::new(
        ReturnCode::NoSmartcard,
        CardProtocol::empty(),
    ));
    assert_eq!(ndr::parse(&bytes).unwrap().code, 0x8010_000C);
}

#[test]
fn list_readers_answer() {
    let names = vec!["Reader A 0".to_string(), "Lecteur é 1".to_string()];
    let bytes = answer(ListReadersReturn::new(
        ReturnCode::Success,
        names.clone(),
        CharacterSet::Unicode,
    ));
    let out = ndr::parse(&bytes).unwrap();
    assert_eq!(ndr::read_list_readers(&out).unwrap(), names);

    let none = answer(ListReadersReturn::new(
        ReturnCode::Success,
        Vec::new(),
        CharacterSet::Unicode,
    ));
    let out = ndr::parse(&none).unwrap();
    assert!(ndr::read_list_readers(&out).unwrap().is_empty());
}

#[test]
fn get_status_change_answer() {
    let mut atr = [0u8; 36];
    let card_atr = [0x3B, 0x75, 0x13, 0x00, 0x00, 0x45, 0x09, 0xEA, 0x90, 0x00];
    atr[..card_atr.len()].copy_from_slice(&card_atr);
    let states = vec![
        ReaderStateCommonCall {
            current_state: CardStateFlags::empty(),
            event_state: CardStateFlags::from_bits_retain(0x0001_0022),
            atr_length: card_atr.len() as u32,
            atr,
        },
        ReaderStateCommonCall {
            current_state: CardStateFlags::empty(),
            event_state: CardStateFlags::from_bits_retain(0x0000_0012),
            atr_length: 0,
            atr: [0; 36],
        },
    ];
    let asked = [
        ReaderQuery {
            name: "A".into(),
            current_state: 0,
        },
        ReaderQuery {
            name: "B".into(),
            current_state: 0,
        },
    ];
    let bytes = answer(GetStatusChangeReturn::new(ReturnCode::Success, states));
    let out = ndr::parse(&bytes).unwrap();
    let got = ndr::read_get_status_change(&out, &asked).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].name, "A");
    assert_eq!(got[0].event_state, 0x0001_0022);
    assert_eq!(got[0].atr, card_atr);
    assert_eq!(got[1].event_state, 0x12);
    assert!(got[1].atr.is_empty());
    // An answer for another number of readers is refused.
    assert!(ndr::read_get_status_change(&out, &asked[..1]).is_err());
}

#[test]
fn connect_answer() {
    let bytes = answer(ConnectReturn::new(
        ReturnCode::Success,
        handle(),
        CardProtocol::SCARD_PROTOCOL_T1,
    ));
    let out = ndr::parse(&bytes).unwrap();
    assert_eq!(ndr::read_connect(&out).unwrap(), (HANDLE4.to_vec(), 2));
}

#[test]
fn reconnect_answer() {
    let bytes = answer(ReconnectReturn::new(
        ReturnCode::Success,
        CardProtocol::SCARD_PROTOCOL_T0,
    ));
    let out = ndr::parse(&bytes).unwrap();
    assert_eq!(ndr::read_reconnect(&out).unwrap(), 1);
}

#[test]
fn transmit_answer() {
    let response = vec![0x61, 0x10, 0x90, 0x00, 0xAB];
    let bytes = answer(TransmitReturn::new(
        ReturnCode::Success,
        None,
        response.clone(),
    ));
    let out = ndr::parse(&bytes).unwrap();
    assert_eq!(ndr::read_transmit(&out).unwrap(), response);
}

#[test]
fn status_answer() {
    let mut atr = [0u8; 32];
    atr[..4].copy_from_slice(&[0x3B, 0x00, 0x01, 0x02]);
    let bytes = answer(StatusReturn::new(
        ReturnCode::Success,
        vec!["Fake Reader 0".into()],
        CardState::SpecificMode,
        CardProtocol::SCARD_PROTOCOL_T1,
        atr,
        4,
        CharacterSet::Unicode,
    ));
    let out = ndr::parse(&bytes).unwrap();
    assert_eq!(
        ndr::read_status(&out).unwrap(),
        (
            "Fake Reader 0".to_string(),
            CardState::SpecificMode.into(),
            2,
            vec![0x3B, 0x00, 0x01, 0x02]
        )
    );
}

#[test]
fn control_and_attrib_answers() {
    let data = vec![1, 2, 3, 4, 5, 6, 7];
    for bytes in [
        answer(ControlReturn::new(ReturnCode::Success, data.clone())),
        answer(GetAttribReturn::new(ReturnCode::Success, data.clone())),
    ] {
        let out = ndr::parse(&bytes).unwrap();
        assert_eq!(ndr::read_buffer(&out).unwrap(), data);
    }
}

#[test]
fn truncated_answers_are_errors_not_panics() {
    let bytes = answer(ListReadersReturn::new(
        ReturnCode::Success,
        vec!["Reader A 0".into()],
        CharacterSet::Unicode,
    ));
    for cut in 0..bytes.len() {
        if let Ok(out) = ndr::parse(&bytes[..cut]) {
            let _ = ndr::read_list_readers(&out);
        }
    }
    let bytes = answer(GetStatusChangeReturn::new(
        ReturnCode::Success,
        vec![ReaderStateCommonCall {
            current_state: CardStateFlags::empty(),
            event_state: CardStateFlags::empty(),
            atr_length: 0,
            atr: [0; 36],
        }],
    ));
    let asked = [ReaderQuery {
        name: "A".into(),
        current_state: 0,
    }];
    for cut in 0..bytes.len() {
        if let Ok(out) = ndr::parse(&bytes[..cut]) {
            let _ = ndr::read_get_status_change(&out, &asked);
        }
    }
}
