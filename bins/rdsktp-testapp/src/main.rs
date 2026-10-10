//! Wayland test application (M3). Applies the `rdp-test-client` contract inside a real KWin:
//! 32 px marker at (0,0) changed by keys 1..8, 32 px heartbeat at (32,0) alternating at ~10 Hz.
//! Keys arrive as evdev codes (KEY_1 = 2 … KEY_8 = 9), identical to the RDP scancodes of the contract.

use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdsktp_capture::{BLOCK, HEARTBEAT_POS, KEYS, MARKER_POS, PALETTE};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const HEARTBEAT_ON: [u8; 3] = [255, 255, 255];
const HEARTBEAT_OFF: [u8; 3] = [48, 48, 48];
/// Screen identity block (M5): color `PALETTE[k]` at (64, 0) on output k (left to right).
const IDENTITY_POS: (u32, u32) = (2 * BLOCK, 0);

struct Canvas {
    file: File,
    width: u32,
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    heartbeat_on: bool,
}

impl Canvas {
    fn fill_block(&self, pos: (u32, u32), rgb: [u8; 3]) {
        let row: Vec<u8> = (0..BLOCK)
            .flat_map(|_| [rgb[2], rgb[1], rgb[0], 255])
            .collect(); // XRGB little-endian = BGRx
        for y in 0..BLOCK {
            let off = (u64::from(pos.1 + y) * u64::from(self.width) + u64::from(pos.0)) * 4;
            let _ = self.file.write_all_at(&row, off);
        }
    }

    fn commit(&self) {
        self.surface.attach(Some(&self.buffer), 0, 0);
        self.surface
            .damage_buffer(0, 0, (BLOCK * 3) as i32, BLOCK as i32);
        self.surface.commit();
    }
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    /// `wl_output` outputs with their logical abscissa (left→right order after sorting).
    outputs: Vec<(wl_output::WlOutput, i32)>,
    /// One fullscreen window per output, indexed like `outputs`.
    windows: Vec<Window>,
    canvases: Vec<Arc<Mutex<Canvas>>>,
}

#[derive(Default, Clone, Copy)]
struct Window {
    size: (u32, u32),
    configured: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        s: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = ev
        {
            match interface.as_str() {
                "wl_compositor" => s.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => s.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => s.wm_base = Some(reg.bind(name, version.min(2), qh, ())),
                "wl_output" => {
                    let idx = s.outputs.len();
                    s.outputs.push((reg.bind(name, version.min(2), qh, idx), 0));
                }
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
no_events!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface
);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        b: &xdg_wm_base::XdgWmBase,
        ev: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            b.pong(serial);
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for App {
    fn event(
        s: &mut Self,
        _: &wl_output::WlOutput,
        ev: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Geometry { x, .. } = ev {
            s.outputs[*idx].1 = x;
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, usize> for App {
    fn event(
        s: &mut Self,
        x: &xdg_surface::XdgSurface,
        ev: xdg_surface::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = ev {
            x.ack_configure(serial);
            s.windows[*idx].configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, usize> for App {
    fn event(
        s: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        ev: xdg_toplevel::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                s.windows[*idx].size = (width as u32, height as u32)
            }
            xdg_toplevel::Event::Close => std::process::exit(0),
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        ev: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(c),
        } = ev
        {
            if c.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = ev
        else {
            return;
        };
        let Some(i) = KEYS.iter().position(|&k| u32::from(k) == key) else {
            return;
        };
        if let Some(c) = s.canvases.first() {
            let c = c.lock().expect("canvas");
            c.fill_block(MARKER_POS, PALETTE[i]);
            c.commit();
        }
    }
}

fn play_tone() {
    use rdsktp_core::ports::{AudioParams, AudioProvider};
    use std::process::{Command, Stdio};
    let params = AudioParams {
        sample_rate: 48_000,
        channels: 2,
    };
    let mut source = rdsktp_audio::ToneProvider.open(params).expect("tone");
    let mut player = Command::new("paplay")
        .args([
            "--raw",
            "--rate=48000",
            "--channels=2",
            "--format=s16le",
            "--latency-msec=20",
            "-d",
            "rdsktp",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("paplay (pulseaudio-utils package)");
    let mut stdin = player.stdin.take().expect("paplay stdin");
    let mut buf = vec![0u8; params.bytes_for_ms(20)];
    while source.read_exact(&mut buf).is_ok() {
        if stdin.write_all(&buf).is_err() {
            break;
        }
    }
    drop(stdin);
    let _ = player.wait();
}

fn main() {
    let (mut width, mut height) = (1920u32, 1080u32); // indicative size before the first configure
    let echo = std::env::args().any(|a| a == "--clipboard-echo");
    let args: Vec<String> = std::env::args()
        .filter(|a| a != "--clipboard-echo" && a != "--tone")
        .collect();
    if args.len() == 3 {
        width = args[1].parse().expect("largeur");
        height = args[2].parse().expect("hauteur");
    }
    if std::env::args().any(|a| a == "--tone") {
        // Audio oracle: plays the test tone into the session's PipeWire sink (see ADR 007).
        std::thread::spawn(play_tone);
    }
    if echo {
        // Deterministic clipboard oracle: copies back any received text selection, as an
        // application would. MIME instance distinct from rdsktp's (no loop).
        std::thread::spawn(|| {
            use rdsktp_core::ports::ClipboardBackend;
            let mut cb =
                rdsktp_clipboard::WaylandClipboard::connect(None).expect("Wayland clipboard");
            loop {
                if let Ok(Some(text)) = cb.poll_text() {
                    let _ = cb.set_text(&text);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }
    let conn = Connection::connect_to_env().expect("Wayland connection (WAYLAND_DISPLAY)");
    let mut queue = conn.new_event_queue::<App>();
    let qh = queue.handle();
    let _reg = conn.display().get_registry(&qh, ());
    let mut app = App::default();
    queue.roundtrip(&mut app).expect("registre");
    queue.roundtrip(&mut app).expect("output geometry"); // Geometry events
                                                         // Logical indices: left to right.
    let mut order: Vec<usize> = (0..app.outputs.len()).collect();
    order.sort_by_key(|&i| (app.outputs[i].1, i));
    assert!(!order.is_empty(), "no wl_output output");

    let compositor = app.compositor.clone().expect("wl_compositor");
    let shm = app.shm.clone().expect("wl_shm");
    let wm_base = app.wm_base.clone().expect("xdg_wm_base");
    app.windows = vec![
        Window {
            size: (width, height),
            configured: false
        };
        order.len()
    ];
    let mut surfaces = Vec::new();
    for (k, &o) in order.iter().enumerate() {
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm_base.get_xdg_surface(&surface, &qh, k);
        let toplevel = xdg.get_toplevel(&qh, k);
        toplevel.set_title(format!("rdsktp-testapp-{k}"));
        toplevel.set_app_id("rdsktp-testapp".into());
        toplevel.set_fullscreen(Some(&app.outputs[o].0));
        surface.commit();
        surfaces.push((surface, xdg, toplevel));
    }
    while !app.windows.iter().all(|w| w.configured) {
        queue.blocking_dispatch(&mut app).expect("configure");
    }

    let dir = std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
    for (k, (surface, _, _)) in surfaces.iter().enumerate() {
        let (w, h) = app.windows[k].size;
        let path =
            std::path::Path::new(&dir).join(format!("rdsktp-testapp-{}-{k}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("shm file");
        std::fs::remove_file(&path).ok();
        file.set_len(u64::from(w) * u64::from(h) * 4)
            .expect("shm size");
        // Non-black background (contract: first image non-black), light gradient.
        let mut bg = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                bg.extend_from_slice(&[
                    (60 + (x * 40 / w)) as u8,
                    (80 + (y * 40 / h)) as u8,
                    110,
                    255,
                ]);
            }
        }
        (&file).write_all(&bg).expect("fond");
        let pool = shm.create_pool(file.as_fd(), (w * h * 4) as i32, &qh, ());
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            (w * 4) as i32,
            wl_shm::Format::Xrgb8888,
            &qh,
            (),
        );
        let canvas = Canvas {
            file,
            width: w,
            surface: surface.clone(),
            buffer,
            heartbeat_on: false,
        };
        canvas.fill_block(MARKER_POS, [0, 0, 0]);
        canvas.fill_block(HEARTBEAT_POS, HEARTBEAT_OFF);
        canvas.fill_block(IDENTITY_POS, PALETTE[k % PALETTE.len()]);
        canvas.surface.attach(Some(&canvas.buffer), 0, 0);
        canvas.surface.damage_buffer(0, 0, w as i32, h as i32);
        canvas.surface.commit();
        app.canvases.push(Arc::new(Mutex::new(canvas)));
    }

    // Heartbeat: alternates ~10 Hz from a thread (Wayland requests are thread-safe).
    let canvases = app.canvases.clone();
    let hb_conn = conn.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(50));
        for canvas in &canvases {
            let mut c = canvas.lock().expect("canvas");
            c.heartbeat_on = !c.heartbeat_on;
            c.fill_block(
                HEARTBEAT_POS,
                if c.heartbeat_on {
                    HEARTBEAT_ON
                } else {
                    HEARTBEAT_OFF
                },
            );
            c.commit();
        }
        let _ = hb_conn.flush();
    });

    loop {
        queue.blocking_dispatch(&mut app).expect("dispatch");
        let _ = conn.flush();
    }
}
