//! Text and image (PNG) clipboard through the Wayland data-control protocols: `ext_data_control_manager_v1`
//! (KWin 6.4+, tried first) or `zwlr_data_control_manager_v1` (older KWin, wlroots).
//!
//! A dedicated thread dispatches Wayland events; `poll_*` / `set_*` do not block.
//! Our own selection is recognized thanks to a private MIME type specific to the instance, never
//! by text comparison: another application copying the same text remains a real change.

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};

use oxydesk_core::error::ClipboardError;
use oxydesk_core::files::FileList;

use crate::fuse::RemoteFiles;
use crate::local_files::{format_uri_list, list_from_paths, parse_uri_list};
use oxydesk_core::ports::ClipboardBackend;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};

/// Text MIME types, from most to least preferred (receiving); all are offered (sending).
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];
const IMAGE_MIME: &str = "image/png";
const URI_LIST_MIME: &str = "text/uri-list";
/// Marks a selection made by a file manager (KDE: cut flag; GNOME, Thunar, PCManFM: copied files).
const KDE_CUT_MIME: &str = "application/x-kde-cutselection";
const GNOME_FILES_MIME: &str = "x-special/gnome-copied-files";
/// What Klipper (the Plasma clipboard manager) keeps of a file manager's copy: it re-offers the selection
/// without the two markers above.
const PORTAL_FILES_MIME: &str = "application/vnd.portal.filetransfer";
const KDE4_URI_LIST_MIME: &str = "application/x-kde4-urilist";
/// Read cap for a remote selection.
const MAX_TEXT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Default)]
struct Shared {
    incoming: Mutex<Option<String>>,
    incoming_image: Mutex<Option<Vec<u8>>>,
    incoming_files: Mutex<Option<FileList>>,
}

/// What a received selection holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Image,
    Files,
}

/// Picks what to receive from the MIME types of a selection: files (a file manager's copy also offers their
/// paths as text), then text, then an image (a copy from a page offers both).
fn classify(mimes: &[String]) -> Option<(&'static str, Kind)> {
    let has = |w: &str| mimes.iter().any(|m| m == w);
    let has_text = TEXT_MIMES.iter().any(|w| has(w));
    // A link copied from a page is a `text/uri-list` too: only the markers of a copy of files count.
    // Klipper keeps the file URIs under their KDE 4 name when the application gave no text.
    let files = has(URI_LIST_MIME)
        && (has(KDE_CUT_MIME)
            || has(GNOME_FILES_MIME)
            || has(PORTAL_FILES_MIME)
            || (has(KDE4_URI_LIST_MIME) && !has_text));
    if files {
        Some((URI_LIST_MIME, Kind::Files))
    } else if let Some(m) = TEXT_MIMES.iter().find(|w| has(w)) {
        Some((*m, Kind::Text))
    } else if has(IMAGE_MIME) {
        Some((IMAGE_MIME, Kind::Image))
    } else {
        None
    }
}

/// MIME types offered by an offer (filled by the `offer` events).
#[derive(Default)]
struct OfferMimes(Mutex<Vec<String>>);

/// What a source serves, by MIME type; one source = one `set_selection`.
struct SourceText(Vec<(String, Vec<u8>)>);

/// Why a protocol could not be started.
enum StartError {
    NotAnnounced(&'static str),
    Failed(String),
}

/// Sends a text as the selection of the seat (implemented by each protocol).
trait Backend: Send {
    /// Offers `items` (MIME type, bytes) as the selection of the seat.
    fn set_selection(&mut self, items: Vec<(String, Vec<u8>)>) -> Result<(), ClipboardError>;
}

/// Reads the received selection in a thread and stores it in `shared`.
fn read_selection(reader: std::io::PipeReader, shared: Arc<Shared>, kind: Kind) {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if !reader.take(MAX_TEXT_BYTES).read_to_end(&mut bytes).is_ok() || bytes.is_empty() {
            return;
        }
        match kind {
            Kind::Image => *shared.incoming_image.lock().expect("incoming") = Some(bytes),
            Kind::Text => {
                *shared.incoming.lock().expect("incoming") =
                    Some(String::from_utf8_lossy(&bytes).into_owned())
            }
            Kind::Files => {
                let text = String::from_utf8_lossy(&bytes);
                if let Some(list) = parse_uri_list(&text).and_then(|p| list_from_paths(&p)) {
                    *shared.incoming_files.lock().expect("incoming") = Some(list);
                }
            }
        }
    });
}

/// Writes `data` into the descriptor of a reader in a dedicated thread (a slow reader must not block the loop).
fn serve_bytes(fd: OwnedFd, data: Vec<u8>) {
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(fd);
        let _ = file.write_all(&data);
    });
}

/// Instantiates the client of one data-control protocol; both have the same requests and events.
macro_rules! data_control {
    (
        $m:ident, xml $xml:literal, global $global:literal,
        $mm:ident :: $Manager:ident, $dm:ident :: $Device:ident,
        $om:ident :: $Offer:ident, $sm:ident :: $Source:ident
    ) => {
        mod $m {
            use super::*;

            mod proto {
                #![allow(clippy::all, non_camel_case_types, non_upper_case_globals, unused)]
                use wayland_client;
                use wayland_client::protocol::*;
                pub mod __interfaces {
                    use wayland_client::protocol::__interfaces::*;
                    wayland_scanner::generate_interfaces!($xml);
                }
                use self::__interfaces::*;
                wayland_scanner::generate_client_code!($xml);
            }
            use proto::$dm::{self as device_ev, $Device};
            use proto::$mm::$Manager;
            use proto::$om::{self as offer_ev, $Offer};
            use proto::$sm::{self as source_ev, $Source};

            struct State {
                shared: Arc<Shared>,
                instance_mime: String,
                manager: Option<$Manager>,
                seat: Option<wl_seat::WlSeat>,
                conn: Connection,
            }

            impl Dispatch<wl_registry::WlRegistry, ()> for State {
                fn event(
                    state: &mut Self,
                    registry: &wl_registry::WlRegistry,
                    event: wl_registry::Event,
                    _: &(),
                    _: &Connection,
                    qh: &QueueHandle<Self>,
                ) {
                    if let wl_registry::Event::Global {
                        name,
                        interface,
                        version,
                    } = event
                    {
                        match interface.as_str() {
                            $global => {
                                state.manager = Some(registry.bind(name, version.min(1), qh, ()))
                            }
                            "wl_seat" if state.seat.is_none() => {
                                state.seat = Some(registry.bind(name, version.min(1), qh, ()))
                            }
                            _ => {}
                        }
                    }
                }
            }

            impl Dispatch<wl_seat::WlSeat, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &wl_seat::WlSeat,
                    _: wl_seat::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<$Manager, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &$Manager,
                    _: proto::$mm::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<$Device, ()> for State {
                fn event(
                    state: &mut Self,
                    _: &$Device,
                    event: device_ev::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    match event {
                        device_ev::Event::Selection { id: Some(offer) } => state.on_selection(offer),
                        // The primary selection is not synchronized: its offers are only released.
                        device_ev::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
                        _ => {}
                    }
                }

                event_created_child!(State, $Device, [
                    device_ev::EVT_DATA_OFFER_OPCODE => ($Offer, OfferMimes::default()),
                ]);
            }

            impl Dispatch<$Offer, OfferMimes> for State {
                fn event(
                    _: &mut Self,
                    _: &$Offer,
                    event: offer_ev::Event,
                    data: &OfferMimes,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    let offer_ev::Event::Offer { mime_type } = event;
                    data.0.lock().expect("mimes").push(mime_type);
                }
            }

            impl Dispatch<$Source, SourceText> for State {
                fn event(
                    _: &mut Self,
                    source: &$Source,
                    event: source_ev::Event,
                    data: &SourceText,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    match event {
                        source_ev::Event::Send { fd, mime_type } => {
                            if let Some((_, bytes)) = data.0.iter().find(|(m, _)| *m == mime_type) {
                                serve_bytes(fd, bytes.clone());
                            }
                        }
                        source_ev::Event::Cancelled => source.destroy(),
                    }
                }
            }

            impl State {
                fn on_selection(&mut self, offer: $Offer) {
                    let mimes = offer
                        .data::<OfferMimes>()
                        .map(|m| m.0.lock().expect("mimes").clone())
                        .unwrap_or_default();
                    if mimes.contains(&self.instance_mime) {
                        offer.destroy(); // our own selection
                        return;
                    }
                    let Some((mime, kind)) = classify(&mimes) else {
                        offer.destroy();
                        return;
                    };
                    let Ok((reader, writer)) = std::io::pipe() else {
                        offer.destroy();
                        return;
                    };
                    offer.receive(mime.to_string(), OwnedFd::from(writer).as_fd());
                    offer.destroy();
                    // The write descriptor is closed (drop) after sending: the reader sees the end of the stream.
                    let _ = self.conn.flush();
                    read_selection(reader, self.shared.clone(), kind);
                }
            }

            struct Client {
                conn: Connection,
                qh: QueueHandle<State>,
                manager: $Manager,
                device: $Device,
                instance_mime: String,
            }

            impl Backend for Client {
                fn set_selection(
                    &mut self,
                    items: Vec<(String, Vec<u8>)>,
                ) -> Result<(), ClipboardError> {
                    let source = self
                        .manager
                        .create_data_source(&self.qh, SourceText(items.clone()));
                    for (mime, _) in &items {
                        source.offer(mime.clone());
                    }
                    source.offer(self.instance_mime.clone());
                    self.device.set_selection(Some(&source));
                    self.conn
                        .flush()
                        .map_err(|e| ClipboardError::Backend(format!("flush Wayland : {e}")))
                }
            }

            /// Starts the protocol on `conn` if the compositor announces it.
            pub(super) fn start(
                conn: &Connection,
                instance_mime: &str,
                shared: &Arc<Shared>,
            ) -> Result<Box<dyn Backend>, StartError> {
                let failed = |what: &str, e: &dyn std::fmt::Display| {
                    StartError::Failed(format!("{what}: {e}"))
                };
                let mut queue = conn.new_event_queue::<State>();
                let qh = queue.handle();
                let _registry = conn.display().get_registry(&qh, ());
                let mut state = State {
                    shared: shared.clone(),
                    instance_mime: instance_mime.to_string(),
                    manager: None,
                    seat: None,
                    conn: conn.clone(),
                };
                queue
                    .roundtrip(&mut state)
                    .map_err(|e| failed("Wayland registry", &e))?;
                let manager = state
                    .manager
                    .clone()
                    .ok_or(StartError::NotAnnounced($global))?;
                let seat = state
                    .seat
                    .clone()
                    .ok_or_else(|| StartError::Failed("wl_seat missing".into()))?;
                let device = manager.get_data_device(&seat, &qh, ());
                queue
                    .roundtrip(&mut state)
                    .map_err(|e| failed("data device", &e))?;
                std::thread::Builder::new()
                    .name("clipboard-wayland".into())
                    .spawn(move || {
                        // The thread lives as long as the process: the connection closes with it.
                        while queue.blocking_dispatch(&mut state).is_ok() {}
                    })
                    .map_err(|e| StartError::Failed(e.to_string()))?;
                Ok(Box::new(Client {
                    conn: conn.clone(),
                    qh,
                    manager,
                    device,
                    instance_mime: instance_mime.to_string(),
                }))
            }
        }
    };
}

data_control!(
    ext, xml "protocols/ext-data-control-v1.xml", global "ext_data_control_manager_v1",
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1
);
data_control!(
    wlr, xml "protocols/wlr-data-control-unstable-v1.xml", global "zwlr_data_control_manager_v1",
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1
);

pub struct WaylandClipboard {
    backend: Box<dyn Backend>,
    shared: Arc<Shared>,
    /// Mounted when the first remote file list arrives.
    files: Option<RemoteFiles>,
}

/// `$XDG_RUNTIME_DIR/oxydesk-files-<pid>`: one mount per process.
const MOUNT_PREFIX: &str = "oxydesk-files-";

/// Unmounts and removes the mount directories left by processes that no longer exist.
fn clean_stale_mounts(runtime: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir(runtime) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_prefix(MOUNT_PREFIX)
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::process::Command::new("fusermount3")
                .args(["-u", "-z"])
                .arg(entry.path())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = std::fs::remove_dir(entry.path());
        }
    }
}

impl WaylandClipboard {
    /// Connects to the Wayland socket `socket` (name relative to `XDG_RUNTIME_DIR`) or to the environment.
    pub fn connect(socket: Option<&str>) -> Result<Self, ClipboardError> {
        let err = |m: String| ClipboardError::Backend(m);
        let conn = match socket {
            Some(name) => {
                let dir = std::env::var_os("XDG_RUNTIME_DIR")
                    .ok_or_else(|| err("XDG_RUNTIME_DIR missing".into()))?;
                let stream =
                    std::os::unix::net::UnixStream::connect(std::path::Path::new(&dir).join(name))
                        .map_err(|e| err(format!("Wayland socket {name}: {e}")))?;
                Connection::from_socket(stream)
            }
            None => Connection::connect_to_env(),
        }
        .map_err(|e| err(format!("Wayland connection: {e}")))?;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let instance_mime = format!(
            "application/x-oxydesk-instance-{}-{nonce:x}",
            std::process::id()
        );
        let shared = Arc::new(Shared::default());
        let mut missing = Vec::new();
        for start in [ext::start, wlr::start] {
            match start(&conn, &instance_mime, &shared) {
                Ok(backend) => {
                    return Ok(Self {
                        backend,
                        shared,
                        files: None,
                    })
                }
                Err(StartError::NotAnnounced(global)) => missing.push(global),
                Err(StartError::Failed(m)) => return Err(err(m)),
            }
        }
        Err(err(format!(
            "no data-control protocol announced by the compositor ({} missing)",
            missing.join(", ")
        )))
    }
}

impl WaylandClipboard {
    /// Offers real local files as the selection, as a file manager does after a copy.
    pub fn set_uri_list(&mut self, paths: &[std::path::PathBuf]) -> Result<(), ClipboardError> {
        let uris = format_uri_list(paths);
        let gnome = format!("copy\n{}", uris.lines().collect::<Vec<_>>().join("\n"));
        self.backend.set_selection(vec![
            (URI_LIST_MIME.into(), uris.into_bytes()),
            (GNOME_FILES_MIME.into(), gnome.into_bytes()),
            (KDE_CUT_MIME.into(), b"0".to_vec()),
        ])
    }
}

impl ClipboardBackend for WaylandClipboard {
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        let bytes = text.as_bytes().to_vec();
        self.backend.set_selection(
            TEXT_MIMES
                .iter()
                .map(|m| ((*m).to_string(), bytes.clone()))
                .collect(),
        )
    }

    fn poll_text(&mut self) -> Result<Option<String>, ClipboardError> {
        Ok(self.shared.incoming.lock().expect("incoming").take())
    }

    fn set_image(&mut self, png: &[u8]) -> Result<(), ClipboardError> {
        self.backend
            .set_selection(vec![(IMAGE_MIME.into(), png.to_vec())])
    }

    fn poll_image(&mut self) -> Result<Option<Vec<u8>>, ClipboardError> {
        Ok(self.shared.incoming_image.lock().expect("incoming").take())
    }

    fn set_files(&mut self, list: FileList) -> Result<(), ClipboardError> {
        if self.files.is_none() {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                .map(std::path::PathBuf::from)
                .ok_or_else(|| ClipboardError::Backend("XDG_RUNTIME_DIR missing".into()))?;
            clean_stale_mounts(&runtime);
            let dir = runtime.join(format!("{MOUNT_PREFIX}{}", std::process::id()));
            self.files = Some(RemoteFiles::mount(&dir).map_err(|e| {
                ClipboardError::Backend(format!(
                    "mounting the file system of remote files (is fuse3 installed?): {e}"
                ))
            })?);
        }
        let roots = self.files.as_ref().expect("mounted").publish(&list);
        self.set_uri_list(&roots)
    }

    fn poll_files(&mut self) -> Result<Option<FileList>, ClipboardError> {
        Ok(self.shared.incoming_files.lock().expect("incoming").take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|m| (*m).to_string()).collect()
    }

    #[test]
    fn a_file_manager_copy_is_files() {
        let kde = mimes(&[URI_LIST_MIME, KDE_CUT_MIME, "text/plain"]);
        assert_eq!(classify(&kde), Some((URI_LIST_MIME, Kind::Files)));
        let gnome = mimes(&[URI_LIST_MIME, GNOME_FILES_MIME, "text/plain;charset=utf-8"]);
        assert_eq!(classify(&gnome), Some((URI_LIST_MIME, Kind::Files)));
    }

    #[test]
    fn a_file_copy_kept_by_klipper_is_files() {
        // As seen by a data-control client on Plasma: the cut and GNOME markers are gone.
        let klipper = mimes(&[
            URI_LIST_MIME,
            KDE4_URI_LIST_MIME,
            PORTAL_FILES_MIME,
            "application/x-kde-source-id",
        ]);
        assert_eq!(classify(&klipper), Some((URI_LIST_MIME, Kind::Files)));
        let without_portal = mimes(&[URI_LIST_MIME, KDE4_URI_LIST_MIME]);
        assert_eq!(
            classify(&without_portal),
            Some((URI_LIST_MIME, Kind::Files))
        );
    }

    #[test]
    fn a_link_copied_from_a_page_is_text() {
        let link = mimes(&[URI_LIST_MIME, "text/plain", "text/html"]);
        assert_eq!(classify(&link), Some(("text/plain", Kind::Text)));
        let klipper_link = mimes(&[URI_LIST_MIME, KDE4_URI_LIST_MIME, "text/plain"]);
        assert_eq!(classify(&klipper_link), Some(("text/plain", Kind::Text)));
    }

    #[test]
    fn text_wins_over_an_image_and_unknown_is_ignored() {
        let both = mimes(&[IMAGE_MIME, "text/plain;charset=utf-8"]);
        assert_eq!(
            classify(&both),
            Some(("text/plain;charset=utf-8", Kind::Text))
        );
        assert_eq!(
            classify(&mimes(&[IMAGE_MIME])),
            Some((IMAGE_MIME, Kind::Image))
        );
        assert_eq!(classify(&mimes(&["application/x-other"])), None);
        assert_eq!(classify(&[]), None);
    }
}
