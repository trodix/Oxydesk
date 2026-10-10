//! Injection d'entrées via `org_kde_kwin_fake_input`.
//!
//! Le processus doit être autorisé par KWin (voir ADR 004) : sans cela l'interface n'est pas annoncée.

use std::sync::Mutex;

use rdsktp_core::error::InputError;
use rdsktp_core::ports::{InputSink, PointerButton};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

use crate::scancode::rdp_to_evdev;

mod proto {
    #![allow(clippy::all, non_camel_case_types, non_upper_case_globals, unused)]
    use wayland_client;
    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("protocols/fake-input.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/fake-input.xml");
}
use proto::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
/// Pixels de défilement Wayland pour un cran de molette RDP (120).
const PIXELS_PER_NOTCH: f64 = 15.0;

struct Globals {
    fake: Option<OrgKdeKwinFakeInput>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == "org_kde_kwin_fake_input" {
                state.fake = Some(registry.bind(name, version.min(5), qh, ()));
            }
        }
    }
}

impl Dispatch<OrgKdeKwinFakeInput, ()> for Globals {
    fn event(_: &mut Self, _: &OrgKdeKwinFakeInput, _: proto::org_kde_kwin_fake_input::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

pub struct KwinFakeInput {
    conn: Connection,
    fake: OrgKdeKwinFakeInput,
    // Les requêtes sont thread-safe ; le verrou garde l'ordre des événements d'une même rafale.
    order: Mutex<()>,
}

impl KwinFakeInput {
    /// Se connecte au socket Wayland `socket` (nom relatif à `XDG_RUNTIME_DIR`) ou à l'environnement.
    pub fn connect(socket: Option<&str>) -> Result<Self, InputError> {
        let conn = match socket {
            Some(name) => {
                let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| InputError::Backend("XDG_RUNTIME_DIR absent".into()))?;
                let stream = std::os::unix::net::UnixStream::connect(std::path::Path::new(&dir).join(name))
                    .map_err(|e| InputError::Backend(format!("socket Wayland {name} : {e}")))?;
                Connection::from_socket(stream)
            }
            None => Connection::connect_to_env(),
        }
        .map_err(|e| InputError::Backend(format!("connexion Wayland : {e}")))?;
        let mut queue = conn.new_event_queue::<Globals>();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut globals = Globals { fake: None };
        queue.roundtrip(&mut globals).map_err(|e| InputError::Backend(format!("registre Wayland : {e}")))?;
        let fake = globals.fake.ok_or_else(|| {
            InputError::Backend("org_kde_kwin_fake_input non annoncé : exécutable non autorisé par KWin (ADR 004)".into())
        })?;
        fake.authenticate("rdsktp".into(), "serveur RDP : injection des entrées distantes".into());
        conn.flush().map_err(|e| InputError::Backend(e.to_string()))?;
        Ok(Self { conn, fake, order: Mutex::new(()) })
    }

    fn flush(&self) -> Result<(), InputError> {
        self.conn.flush().map_err(|e| InputError::Backend(format!("flush Wayland : {e}")))
    }
}

impl InputSink for KwinFakeInput {
    fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError> {
        let Some(code) = rdp_to_evdev(scancode) else { return Ok(()) };
        let _g = self.order.lock().expect("ordre");
        self.fake.keyboard_key(u32::from(code), u32::from(pressed));
        self.flush()
    }

    fn pointer_move(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        let _g = self.order.lock().expect("ordre");
        self.fake.pointer_motion_absolute(f64::from(x), f64::from(y));
        self.flush()
    }

    fn pointer_button(&mut self, button: PointerButton, pressed: bool) -> Result<(), InputError> {
        let code = match button {
            PointerButton::Left => BTN_LEFT,
            PointerButton::Right => BTN_RIGHT,
            PointerButton::Middle => BTN_MIDDLE,
            PointerButton::Back => BTN_SIDE,
            PointerButton::Forward => BTN_EXTRA,
        };
        let _g = self.order.lock().expect("ordre");
        self.fake.button(code, u32::from(pressed));
        self.flush()
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        let _g = self.order.lock().expect("ordre");
        // RDP : dy > 0 = vers le haut ; Wayland : valeur positive = vers le bas.
        if dy != 0 {
            self.fake.axis(0, -f64::from(dy) / 120.0 * PIXELS_PER_NOTCH);
        }
        if dx != 0 {
            self.fake.axis(1, f64::from(dx) / 120.0 * PIXELS_PER_NOTCH);
        }
        self.flush()
    }
}
