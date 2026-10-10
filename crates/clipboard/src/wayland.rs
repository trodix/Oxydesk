//! Presse-papiers texte via `zwlr_data_control_manager_v1`.
//!
//! Un thread dédié dispatche les événements Wayland ; `poll_text` / `set_text` ne bloquent pas.
//! Notre propre sélection est reconnue grâce à un type MIME privé propre à l'instance, jamais
//! par comparaison de texte : une autre application qui recopie le même texte reste un vrai changement.

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};

use rdsktp_core::error::ClipboardError;
use rdsktp_core::ports::ClipboardBackend;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};

mod proto {
    #![allow(clippy::all, non_camel_case_types, non_upper_case_globals, unused)]
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/wlr-data-control-unstable-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/wlr-data-control-unstable-v1.xml");
}
use proto::zwlr_data_control_device_v1::{self as device_ev, ZwlrDataControlDeviceV1};
use proto::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use proto::zwlr_data_control_offer_v1::{self as offer_ev, ZwlrDataControlOfferV1};
use proto::zwlr_data_control_source_v1::{self as source_ev, ZwlrDataControlSourceV1};

/// Types MIME texte, du plus au moins préféré (réception) ; on les offre tous (émission).
const TEXT_MIMES: &[&str] = &["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT", "STRING"];
/// Plafond de lecture d'une sélection distante.
const MAX_TEXT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Default)]
struct Shared {
    incoming: Mutex<Option<String>>,
}

/// MIME proposés par une offre (rempli par les événements `offer`).
#[derive(Default)]
struct OfferMimes(Mutex<Vec<String>>);

struct State {
    shared: Arc<Shared>,
    instance_mime: String,
    manager: Option<ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    conn: Connection,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "zwlr_data_control_manager_v1" => state.manager = Some(registry.bind(name, version.min(1), qh, ())),
                "wl_seat" if state.seat.is_none() => state.seat = Some(registry.bind(name, version.min(1), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZwlrDataControlManagerV1, _: proto::zwlr_data_control_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(state: &mut Self, _: &ZwlrDataControlDeviceV1, event: device_ev::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let device_ev::Event::Selection { id: Some(offer) } = event {
            state.on_selection(offer);
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        device_ev::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, OfferMimes::default()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, OfferMimes> for State {
    fn event(_: &mut Self, _: &ZwlrDataControlOfferV1, event: offer_ev::Event, data: &OfferMimes, _: &Connection, _: &QueueHandle<Self>) {
        let offer_ev::Event::Offer { mime_type } = event;
        data.0.lock().expect("mimes").push(mime_type);
    }
}

/// Texte servi par une source ; une source = un `set_text`.
struct SourceText(String);

impl Dispatch<ZwlrDataControlSourceV1, SourceText> for State {
    fn event(_: &mut Self, source: &ZwlrDataControlSourceV1, event: source_ev::Event, data: &SourceText, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            source_ev::Event::Send { fd, .. } => {
                let text = data.0.clone();
                // Thread dédié : un lecteur lent ne doit pas bloquer la boucle Wayland.
                std::thread::spawn(move || {
                    let mut file = std::fs::File::from(fd);
                    let _ = file.write_all(text.as_bytes());
                });
            }
            source_ev::Event::Cancelled => source.destroy(),
        }
    }
}

impl State {
    fn on_selection(&mut self, offer: ZwlrDataControlOfferV1) {
        let mimes = offer.data::<OfferMimes>().map(|m| m.0.lock().expect("mimes").clone()).unwrap_or_default();
        if mimes.contains(&self.instance_mime) {
            offer.destroy(); // notre propre sélection
            return;
        }
        let Some(mime) = TEXT_MIMES.iter().find(|w| mimes.iter().any(|m| m == **w)) else {
            offer.destroy();
            return;
        };
        let Ok((reader, writer)) = std::io::pipe() else {
            offer.destroy();
            return;
        };
        offer.receive((*mime).to_string(), OwnedFd::from(writer).as_fd());
        offer.destroy();
        // Le descripteur d'écriture est fermé (drop) après l'envoi : le lecteur verra la fin du flux.
        let _ = self.conn.flush();
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if reader.take(MAX_TEXT_BYTES).read_to_end(&mut bytes).is_ok() && !bytes.is_empty() {
                *shared.incoming.lock().expect("incoming") = Some(String::from_utf8_lossy(&bytes).into_owned());
            }
        });
    }
}

pub struct WaylandClipboard {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: ZwlrDataControlManagerV1,
    device: ZwlrDataControlDeviceV1,
    shared: Arc<Shared>,
    instance_mime: String,
}

impl WaylandClipboard {
    /// Se connecte au socket Wayland `socket` (nom relatif à `XDG_RUNTIME_DIR`) ou à l'environnement.
    pub fn connect(socket: Option<&str>) -> Result<Self, ClipboardError> {
        let err = |m: String| ClipboardError::Backend(m);
        let conn = match socket {
            Some(name) => {
                let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| err("XDG_RUNTIME_DIR absent".into()))?;
                let stream = std::os::unix::net::UnixStream::connect(std::path::Path::new(&dir).join(name))
                    .map_err(|e| err(format!("socket Wayland {name} : {e}")))?;
                Connection::from_socket(stream)
            }
            None => Connection::connect_to_env(),
        }
        .map_err(|e| err(format!("connexion Wayland : {e}")))?;

        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let instance_mime = format!("application/x-rdsktp-instance-{}-{nonce:x}", std::process::id());
        let shared = Arc::new(Shared::default());
        let mut queue = conn.new_event_queue::<State>();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State { shared: shared.clone(), instance_mime: instance_mime.clone(), manager: None, seat: None, conn: conn.clone() };
        queue.roundtrip(&mut state).map_err(|e| err(format!("registre Wayland : {e}")))?;
        let manager = state.manager.clone().ok_or_else(|| err("zwlr_data_control_manager_v1 non annoncé par le compositeur".into()))?;
        let seat = state.seat.clone().ok_or_else(|| err("wl_seat absent".into()))?;
        let device = manager.get_data_device(&seat, &qh, ());
        queue.roundtrip(&mut state).map_err(|e| err(format!("data device : {e}")))?;

        std::thread::Builder::new()
            .name("clipboard-wayland".into())
            .spawn(move || {
                // Le thread vit autant que le processus : la connexion se ferme avec lui.
                while queue.blocking_dispatch(&mut state).is_ok() {}
            })
            .map_err(|e| err(e.to_string()))?;
        Ok(Self { conn, qh, manager, device, shared, instance_mime })
    }
}

impl ClipboardBackend for WaylandClipboard {
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        let source = self.manager.create_data_source(&self.qh, SourceText(text.to_string()));
        for mime in TEXT_MIMES {
            source.offer((*mime).to_string());
        }
        source.offer(self.instance_mime.clone());
        self.device.set_selection(Some(&source));
        self.conn.flush().map_err(|e| ClipboardError::Backend(format!("flush Wayland : {e}")))
    }

    fn poll_text(&mut self) -> Result<Option<String>, ClipboardError> {
        Ok(self.shared.incoming.lock().expect("incoming").take())
    }
}
