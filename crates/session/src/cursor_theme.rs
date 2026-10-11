//! Cursor shapes for the remote clients.
//!
//! KWin's virtual backend paints its own cursor into the captured image whatever the screencast pointer mode,
//! which would double the one the RDP client draws itself (with a trail). The session therefore runs a
//! transparent cursor theme, and the screencast's cursor metadata would only ever carry transparent bitmaps.
//! So each cursor *name* gets its own transparent file whose width is the name's rank (`rank + 1` px wide, one
//! row): the bitmap reported by the metadata then says which cursor KWin wants, and [`ThemeResolver`] swaps it
//! for the bitmap of the user's real cursor theme.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rdsktp_core::cursor::{CursorResolver, CursorShape};

pub(crate) const ICONS_DIR: &str = "rdsktp-icons";
pub(crate) const THEME: &str = "rdsktp-blank";
/// Size the bitmaps are taken at (RDP pointers go up to 96 px; 32 is the classic size).
const WANTED_SIZE: u32 = 32;

/// Cursor names requested by KWin/Qt/GTK. The rank in this list is the marker's width minus one.
const NAMES: &[&str] = &[
    "left_ptr",
    "default",
    "arrow",
    "top_left_arrow",
    "pointer",
    "text",
    "xterm",
    "ibeam",
    "hand1",
    "hand2",
    "pointing_hand",
    "watch",
    "wait",
    "progress",
    "crosshair",
    "cross",
    "fleur",
    "move",
    "size_all",
    "size_hor",
    "size_ver",
    "sb_h_double_arrow",
    "sb_v_double_arrow",
    "size_fdiag",
    "size_bdiag",
    "not-allowed",
    "forbidden",
    "question_arrow",
    "help",
    "grab",
    "grabbing",
    "openhand",
    "closedhand",
    "copy",
    "alias",
    "context-menu",
    "cell",
    "vertical-text",
    "col-resize",
    "row-resize",
    "e-resize",
    "w-resize",
    "n-resize",
    "s-resize",
    "ne-resize",
    "nw-resize",
    "se-resize",
    "sw-resize",
    "ew-resize",
    "ns-resize",
    "nesw-resize",
    "nwse-resize",
];

fn le(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Xcursor file of a fully transparent `width` x 1 image (nominal size 24).
fn marker_xcursor(width: u32) -> Vec<u8> {
    let mut out = b"Xcur".to_vec();
    out.extend(le(&[16, 0x1_0000, 1])); // header size, version, 1 entry
    out.extend(le(&[0xfffd_0002, 24, 28])); // image type, nominal size, chunk offset
    out.extend(le(&[36, 0xfffd_0002, 24, 1, width, 1, 0, 0, 0])); // chunk: w x 1, hotspot 0, delay 0
    out.extend(vec![0u8; width as usize * 4]); // transparent ARGB pixels
    out
}

/// Writes the marker theme under the session's runtime directory.
pub(crate) fn write_marker_theme(runtime: &Path) -> std::io::Result<()> {
    let dir = runtime.join(ICONS_DIR).join(THEME);
    let cursors = dir.join("cursors");
    std::fs::create_dir_all(&cursors)?;
    std::fs::write(
        dir.join("index.theme"),
        format!("[Icon Theme]\nName={THEME}\n"),
    )?;
    for (rank, name) in NAMES.iter().enumerate() {
        let file = cursors.join(name);
        let _ = std::fs::remove_file(&file);
        std::fs::write(file, marker_xcursor(rank as u32 + 1))?;
    }
    Ok(())
}

/// Reads the image of an Xcursor file whose nominal size is the closest to `wanted`, as straight RGBA.
/// Animated cursors give their first frame.
fn read_xcursor(data: &[u8], wanted: u32) -> Option<CursorShape> {
    let u32_at = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?))
    };
    if data.get(..4)? != b"Xcur" {
        return None;
    }
    let ntoc = u32_at(12)? as usize;
    // (distance to the wanted size, chunk offset) of the best image; the first one wins a tie.
    let mut best: Option<(u32, usize)> = None;
    for i in 0..ntoc.min(256) {
        let entry = 16 + i * 12;
        if u32_at(entry)? != 0xfffd_0002 {
            continue;
        }
        let nominal = u32_at(entry + 4)?;
        let distance = nominal.abs_diff(wanted);
        if best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, u32_at(entry + 8)? as usize));
        }
    }
    let at = best?.1;
    let (width, height) = (u32_at(at + 16)?, u32_at(at + 20)?);
    let (hot_x, hot_y) = (u32_at(at + 24)?, u32_at(at + 28)?);
    let count = (width as usize).checked_mul(height as usize)?;
    let pixels = data.get(at + 36..at + 36 + count.checked_mul(4)?)?;
    let mut rgba = Vec::with_capacity(pixels.len());
    for p in pixels.as_chunks::<4>().0 {
        // Premultiplied ARGB (little-endian word: B G R A in memory) to straight RGBA.
        let (b, g, r, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
        let un = |c: u32| (c * 255 + a / 2).checked_div(a).unwrap_or(0).min(255) as u8;
        rgba.extend([un(r), un(g), un(b), a as u8]);
    }
    CursorShape::new(
        u16::try_from(width).ok()?,
        u16::try_from(height).ok()?,
        u16::try_from(hot_x).ok()?,
        u16::try_from(hot_y).ok()?,
        rgba,
    )
}

/// Directories where cursor themes live, like libXcursor looks for them.
fn theme_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut roots = Vec::new();
    if let Some(path) = std::env::var_os("XCURSOR_PATH") {
        roots.extend(std::env::split_paths(&path));
    }
    if let Some(home) = &home {
        roots.push(home.join(".local/share/icons"));
        roots.push(home.join(".icons"));
    }
    for dir in [
        "/usr/local/share/icons",
        "/usr/share/icons",
        "/usr/share/pixmaps",
    ] {
        roots.push(PathBuf::from(dir));
    }
    roots
}

/// The user's cursor theme: KDE's setting, else the Xcursor environment, else a common default.
fn configured_theme() -> Vec<String> {
    let mut names = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(text) = std::fs::read_to_string(Path::new(&home).join(".config/kcminputrc")) {
            if let Some(v) = text.lines().find_map(|l| l.strip_prefix("cursorTheme=")) {
                names.push(v.trim().to_string());
            }
        }
    }
    if let Ok(v) = std::env::var("XCURSOR_THEME") {
        names.push(v);
    }
    names.extend(["breeze_cursors", "Breeze_Snow", "Adwaita", "default"].map(String::from));
    names
}

/// `cursors/` directories of a theme and of the themes it inherits from, closest first.
fn theme_chain(roots: &[PathBuf], start: &[String]) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut queue: Vec<String> = start.to_vec();
    let mut seen = Vec::new();
    while !queue.is_empty() && seen.len() < 16 {
        let name = queue.remove(0);
        if seen.contains(&name) {
            continue;
        }
        for root in roots {
            let dir = root.join(&name);
            if !dir.is_dir() {
                continue;
            }
            if dir.join("cursors").is_dir() {
                chain.push(dir.join("cursors"));
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("index.theme")) {
                if let Some(v) = text.lines().find_map(|l| l.strip_prefix("Inherits=")) {
                    queue.extend(v.split(',').map(|s| s.trim().to_string()));
                }
            }
        }
        seen.push(name);
    }
    chain
}

/// Replaces the markers of the transparent theme with the bitmaps of the user's real theme.
pub(crate) struct ThemeResolver {
    chain: Vec<PathBuf>,
    cache: Mutex<HashMap<usize, Option<CursorShape>>>,
}

impl ThemeResolver {
    fn new(chain: Vec<PathBuf>) -> Self {
        Self {
            chain,
            cache: Mutex::default(),
        }
    }

    fn load(&self, name: &str) -> Option<CursorShape> {
        self.chain.iter().find_map(|dir| {
            let data = std::fs::read(dir.join(name)).ok()?;
            read_xcursor(&data, WANTED_SIZE)
        })
    }
}

impl CursorResolver for ThemeResolver {
    fn resolve(&self, shape: CursorShape) -> Option<CursorShape> {
        let marker = shape.height == 1
            && usize::from(shape.width) <= NAMES.len()
            && shape.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
        if !marker {
            return Some(shape);
        }
        let rank = usize::from(shape.width) - 1;
        let mut cache = self.cache.lock().expect("cursor cache");
        let found = cache.entry(rank).or_insert_with(|| {
            let name = NAMES[rank];
            // The default arrow stands in for names the theme lacks.
            self.load(name).or_else(|| self.load("left_ptr"))
        });
        found.clone()
    }
}

/// Installs the resolver of the user's cursor theme, if there is one to read.
pub(crate) fn install_resolver() {
    let chain = theme_chain(&theme_roots(), &configured_theme());
    if chain.is_empty() {
        tracing::warn!("no cursor theme found: remote cursor shapes unavailable");
        return;
    }
    rdsktp_core::cursor::install_resolver(Box::new(ThemeResolver::new(chain)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Xcursor file with one image per `(nominal size, width, height)`; premultiplied ARGB `pixel`.
    fn xcursor(images: &[(u32, u32, u32)], pixel: u32) -> Vec<u8> {
        let mut out = b"Xcur".to_vec();
        out.extend(le(&[16, 0x1_0000, images.len() as u32]));
        let mut offset = 16 + 12 * images.len() as u32;
        let mut chunks = Vec::new();
        for &(nominal, w, h) in images {
            out.extend(le(&[0xfffd_0002, nominal, offset]));
            chunks.extend(le(&[36, 0xfffd_0002, nominal, 1, w, h, 2, 3, 0]));
            chunks.extend(le(&vec![pixel; (w * h) as usize]));
            offset += 36 + w * h * 4;
        }
        out.extend(chunks);
        out
    }

    #[test]
    fn marker_is_well_formed_and_encodes_its_width() {
        let b = marker_xcursor(5);
        assert_eq!(&b[..4], b"Xcur");
        assert_eq!(b.len(), 16 + 12 + 36 + 5 * 4);
        // the announced offset (28) does point at the chunk (16 header bytes + 12 of table)
        assert_eq!(u32::from_le_bytes(b[28..32].try_into().unwrap()), 36);
        let shape = read_xcursor(&b, 32).unwrap();
        assert_eq!((shape.width, shape.height), (5, 1));
        assert!(shape.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0));
    }

    #[test]
    fn picks_the_closest_size_and_unpremultiplies() {
        // 50% alpha, premultiplied red 0x40 -> straight red 128 (rounded)
        let data = xcursor(&[(24, 2, 2), (48, 3, 3)], 0x8040_0000);
        let shape = read_xcursor(&data, 32).unwrap();
        assert_eq!((shape.width, shape.height), (2, 2)); // 24 is 8 away, 48 is 16 away
        assert_eq!((shape.hot_x, shape.hot_y), (1, 1)); // hotspot (2, 3) clamped inside 2x2
        assert_eq!(&shape.rgba[..4], &[128, 0, 0, 128]);
        assert_eq!(read_xcursor(&data, 60).unwrap().width, 3);
    }

    #[test]
    fn truncated_or_foreign_files_are_refused() {
        let data = xcursor(&[(32, 4, 4)], 0xff00_0000);
        assert!(read_xcursor(&data[..data.len() - 1], 32).is_none());
        assert!(read_xcursor(b"nope", 32).is_none());
        assert!(read_xcursor(&[], 32).is_none());
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rdsktp-cursor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn markers_become_the_real_themes_bitmaps() {
        let root = temp_dir("resolve");
        let cursors = root.join("T/cursors");
        std::fs::create_dir_all(&cursors).unwrap();
        std::fs::write(
            cursors.join("left_ptr"),
            xcursor(&[(32, 4, 4)], 0xffff_ffff),
        )
        .unwrap();
        std::fs::write(cursors.join("text"), xcursor(&[(32, 6, 8)], 0xffff_ffff)).unwrap();
        let chain = theme_chain(std::slice::from_ref(&root), &["T".to_string()]);
        let resolver = ThemeResolver::new(chain);

        let marker = |name: &str| {
            let rank = NAMES.iter().position(|n| *n == name).unwrap();
            read_xcursor(&marker_xcursor(rank as u32 + 1), 32).unwrap()
        };
        let text = resolver.resolve(marker("text")).unwrap();
        assert_eq!((text.width, text.height), (6, 8));
        // a name the theme lacks falls back to the arrow
        let other = resolver.resolve(marker("help")).unwrap();
        assert_eq!((other.width, other.height), (4, 4));
        // an application cursor is not a marker: untouched
        let app = CursorShape::new(16, 16, 1, 1, vec![9; 16 * 16 * 4]).unwrap();
        assert_eq!(resolver.resolve(app.clone()), Some(app));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_theme_chain_follows_inheritance_once() {
        let root = temp_dir("chain");
        for (name, inherits) in [("A", "B,A"), ("B", "")] {
            let dir = root.join(name);
            std::fs::create_dir_all(dir.join("cursors")).unwrap();
            std::fs::write(
                dir.join("index.theme"),
                format!("[Icon Theme]\nInherits={inherits}\n"),
            )
            .unwrap();
        }
        let chain = theme_chain(std::slice::from_ref(&root), &["A".to_string()]);
        assert_eq!(chain, vec![root.join("A/cursors"), root.join("B/cursors")]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn marker_theme_has_a_file_per_name() {
        let root = temp_dir("write");
        write_marker_theme(&root).unwrap();
        let cursors = root.join(ICONS_DIR).join(THEME).join("cursors");
        for (rank, name) in NAMES.iter().enumerate() {
            let shape = read_xcursor(&std::fs::read(cursors.join(name)).unwrap(), 32).unwrap();
            assert_eq!(usize::from(shape.width), rank + 1, "{name}");
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
