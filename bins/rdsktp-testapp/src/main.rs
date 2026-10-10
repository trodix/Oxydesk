//! Application Wayland de test (M3). Applique le contrat de `rdp-test-client` dans un vrai KWin :
//! marqueur 32 px en (0,0) changé par les touches 1..8, heartbeat 32 px en (32,0) qui alterne ~10 Hz.
//! Les touches arrivent en codes evdev (KEY_1 = 2 … KEY_8 = 9), identiques aux scancodes RDP du contrat.

use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdsktp_capture::{BLOCK, HEARTBEAT_POS, KEYS, MARKER_POS, PALETTE};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const HEARTBEAT_ON: [u8; 3] = [255, 255, 255];
const HEARTBEAT_OFF: [u8; 3] = [48, 48, 48];

struct Canvas {
    file: File,
    width: u32,
    height: u32,
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    heartbeat_on: bool,
}

impl Canvas {
    fn fill_block(&self, pos: (u32, u32), rgb: [u8; 3]) {
        let row: Vec<u8> = (0..BLOCK).flat_map(|_| [rgb[2], rgb[1], rgb[0], 255]).collect(); // XRGB little-endian = BGRx
        for y in 0..BLOCK {
            let off = (u64::from(pos.1 + y) * u64::from(self.width) + u64::from(pos.0)) * 4;
            let _ = self.file.write_all_at(&row, off);
        }
    }

    fn commit(&self) {
        self.surface.attach(Some(&self.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, (BLOCK * 2) as i32, BLOCK as i32);
        self.surface.commit();
    }
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    size: (u32, u32),
    canvas: Option<Arc<Mutex<Canvas>>>,
    configured: bool,
    serial: u32,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => s.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => s.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => s.wm_base = Some(reg.bind(name, version.min(2), qh, ())),
                "wl_seat" => s.seat = Some(reg.bind(name, version.min(5), qh, ())),
                _ => {}
            }
        }
    }
}

macro_rules! no_events {
    ($($t:ty),*) => { $(impl Dispatch<$t, ()> for App {
        fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    })* };
}
no_events!(wl_compositor::WlCompositor, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_surface::WlSurface);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(_: &mut Self, b: &xdg_wm_base::XdgWmBase, ev: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            b.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(s: &mut Self, x: &xdg_surface::XdgSurface, ev: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            x.ack_configure(serial);
            s.serial = serial;
            s.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(s: &mut Self, _: &xdg_toplevel::XdgToplevel, ev: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => s.size = (width as u32, height as u32),
            xdg_toplevel::Event::Close => std::process::exit(0),
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(_: &mut Self, seat: &wl_seat::WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = ev {
            if c.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(s: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let wl_keyboard::Event::Key { key, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. } = ev else { return };
        let Some(i) = KEYS.iter().position(|&k| u32::from(k) == key) else { return };
        if let Some(c) = &s.canvas {
            let c = c.lock().expect("canvas");
            c.fill_block(MARKER_POS, PALETTE[i]);
            c.commit();
        }
    }
}

fn main() {
    let (mut width, mut height) = (1920u32, 1080u32);
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 {
        width = args[1].parse().expect("largeur");
        height = args[2].parse().expect("hauteur");
    }
    let conn = Connection::connect_to_env().expect("connexion Wayland (WAYLAND_DISPLAY)");
    let mut queue = conn.new_event_queue::<App>();
    let qh = queue.handle();
    let _reg = conn.display().get_registry(&qh, ());
    let mut app = App { size: (width, height), ..Default::default() };
    queue.roundtrip(&mut app).expect("registre");

    let compositor = app.compositor.clone().expect("wl_compositor");
    let shm = app.shm.clone().expect("wl_shm");
    let wm_base = app.wm_base.clone().expect("xdg_wm_base");
    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("rdsktp-testapp".into());
    toplevel.set_app_id("rdsktp-testapp".into());
    toplevel.set_fullscreen(None);
    surface.commit();
    while !app.configured {
        queue.blocking_dispatch(&mut app).expect("configure");
    }
    let (w, h) = app.size;

    let dir = std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
    let path = std::path::Path::new(&dir).join(format!("rdsktp-testapp-{}", std::process::id()));
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("fichier shm");
    std::fs::remove_file(&path).ok();
    file.set_len(u64::from(w) * u64::from(h) * 4).expect("taille shm");
    // Fond non noir (contrat : première image non noire), dégradé léger.
    let mut bg = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            bg.extend_from_slice(&[(60 + (x * 40 / w)) as u8, (80 + (y * 40 / h)) as u8, 110, 255]);
        }
    }
    (&file).write_all(&bg).expect("fond");
    let pool = shm.create_pool(file.as_fd(), (w * h * 4) as i32, &qh, ());
    let buffer = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Xrgb8888, &qh, ());
    let canvas = Arc::new(Mutex::new(Canvas { file, width: w, height: h, surface: surface.clone(), buffer, heartbeat_on: false }));
    {
        let c = canvas.lock().expect("canvas");
        c.fill_block(MARKER_POS, [0, 0, 0]);
        c.fill_block(HEARTBEAT_POS, HEARTBEAT_OFF);
        c.surface.attach(Some(&c.buffer), 0, 0);
        c.surface.damage_buffer(0, 0, w as i32, h as i32);
        c.surface.commit();
    }
    app.canvas = Some(canvas.clone());

    // Heartbeat : alterne ~10 Hz depuis un thread (les requêtes Wayland sont thread-safe).
    let hb = canvas.clone();
    let hb_conn = conn.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(50));
        let mut c = hb.lock().expect("canvas");
        c.heartbeat_on = !c.heartbeat_on;
        c.fill_block(HEARTBEAT_POS, if c.heartbeat_on { HEARTBEAT_ON } else { HEARTBEAT_OFF });
        c.commit();
        drop(c);
        let _ = hb_conn.flush();
    });

    loop {
        queue.blocking_dispatch(&mut app).expect("dispatch");
        let _ = conn.flush();
    }
}
