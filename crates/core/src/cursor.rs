//! Shape of the remote pointer. The client draws it itself at its own pointer position (no latency, never part
//! of the video image): the server only tells which bitmap to use.

/// A pointer bitmap: straight (not premultiplied) RGBA rows, `width * 4` bytes each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u16,
    pub height: u16,
    /// The pixel that clicks, relative to the top-left corner of the bitmap.
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// What the desktop asks the clients to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorUpdate {
    Shape(CursorShape),
    /// No pointer to show (the application hid it).
    Hidden,
}

/// Largest pointer bitmap RDP carries (96x96 for the new-pointer PDU).
pub const MAX_CURSOR_SIZE: u16 = 96;

impl CursorShape {
    /// `None` when the dimensions or the buffer do not describe a bitmap RDP can carry.
    pub fn new(width: u16, height: u16, hot_x: u16, hot_y: u16, rgba: Vec<u8>) -> Option<Self> {
        let ok = (1..=MAX_CURSOR_SIZE).contains(&width)
            && (1..=MAX_CURSOR_SIZE).contains(&height)
            && rgba.len() == usize::from(width) * usize::from(height) * 4;
        ok.then(|| Self {
            width,
            height,
            hot_x: hot_x.min(width - 1),
            hot_y: hot_y.min(height - 1),
            rgba,
        })
    }
}

/// Turns the shapes the compositor reports into the ones to show. KWin's virtual backend paints its own
/// cursor into the captured image whatever the pointer mode, so the session runs a transparent cursor theme
/// whose files only tell *which* cursor is wanted (see `rdsktp-session`); the resolver swaps those markers for
/// the real theme's bitmaps. Shapes it does not recognise (application cursors) pass through.
pub trait CursorResolver: Send + Sync {
    /// `None`: the marker is known but has no bitmap to show (the client keeps its current cursor).
    fn resolve(&self, shape: CursorShape) -> Option<CursorShape>;
}

static RESOLVER: std::sync::OnceLock<Box<dyn CursorResolver>> = std::sync::OnceLock::new();

/// Installed once per process, by whoever starts the session (the resolver depends on its cursor theme).
/// Later calls are ignored.
pub fn install_resolver(resolver: Box<dyn CursorResolver>) {
    let _ = RESOLVER.set(resolver);
}

/// The update with its shape resolved; `None` when there is nothing to send.
pub fn resolve(update: CursorUpdate) -> Option<CursorUpdate> {
    match (update, RESOLVER.get()) {
        (CursorUpdate::Shape(shape), Some(r)) => r.resolve(shape).map(CursorUpdate::Shape),
        (other, _) => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_inconsistent_or_oversized_bitmaps() {
        assert!(CursorShape::new(2, 2, 0, 0, vec![0; 16]).is_some());
        assert!(CursorShape::new(2, 2, 0, 0, vec![0; 15]).is_none());
        assert!(CursorShape::new(0, 2, 0, 0, vec![]).is_none());
        let big = usize::from(MAX_CURSOR_SIZE) + 1;
        assert!(CursorShape::new(big as u16, 1, 0, 0, vec![0; big * 4]).is_none());
    }

    #[test]
    fn hotspot_is_kept_inside_the_bitmap() {
        let s = CursorShape::new(4, 4, 99, 7, vec![0; 64]).unwrap();
        assert_eq!((s.hot_x, s.hot_y), (3, 3));
    }
}
