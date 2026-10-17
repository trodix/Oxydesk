//! Pure conversions and decisions of the connection loop: protocol values to domain values, and the clock-based
//! rule of the video hold.

use std::sync::atomic::{AtomicU64, Ordering};

use rdsktp_agent_proto::{FileInfo, Rect};
use rdsktp_core::layout::{Layout, OutputId, OutputInfo};
use rdsktp_core::ports::PointerButton;
use rdsktp_core::FileEntry;

/// A hold asked for by the server lasts at most this long without being renewed (a lost release must not freeze
/// the video).
pub const HOLD_MAX_MS: u64 = 1000;

/// Protocol button number (0 left … 4 forward) to the domain's.
pub fn button(b: u8) -> Option<PointerButton> {
    Some(match b {
        0 => PointerButton::Left,
        1 => PointerButton::Right,
        2 => PointerButton::Middle,
        3 => PointerButton::Back,
        4 => PointerButton::Forward,
        _ => return None,
    })
}

/// Milliseconds since the first call, plus one so that 0 can mean "not held".
pub fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        + 1
}

/// Is a hold still in force at `now`? `since` is the time of the hold (0 = not held).
pub fn held_at(since: u64, now: u64) -> bool {
    since != 0 && now.saturating_sub(since) < HOLD_MAX_MS
}

pub fn held(since: &AtomicU64) -> bool {
    held_at(since.load(Ordering::Relaxed), now_ms())
}

/// The value stored by `ToAgent::Hold`.
pub fn hold_stamp(held: bool, now: u64) -> u64 {
    if held {
        now
    } else {
        0
    }
}

pub fn to_info(e: &FileEntry) -> FileInfo {
    FileInfo {
        path: e.path.clone(),
        size: e.size,
        is_dir: e.is_dir,
        modified: e.modified,
        read_only: e.read_only,
    }
}

pub fn from_info(i: FileInfo) -> FileEntry {
    FileEntry {
        path: i.path,
        size: i.size,
        is_dir: i.is_dir,
        modified: i.modified,
        read_only: i.read_only,
    }
}

/// The layout asked for by `ToAgent::SetLayout`: outputs numbered in order, the first one primary.
pub fn layout_of(outputs: &[Rect]) -> Layout {
    Layout {
        outputs: outputs
            .iter()
            .enumerate()
            .map(|(i, r)| OutputInfo {
                id: OutputId(i as u32),
                x: r.x,
                y: r.y,
                width: r.width,
                height: r.height,
                primary: i == 0,
            })
            .collect(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_map_and_unknown_ones_are_dropped() {
        assert_eq!(button(0), Some(PointerButton::Left));
        assert_eq!(button(1), Some(PointerButton::Right));
        assert_eq!(button(2), Some(PointerButton::Middle));
        assert_eq!(button(3), Some(PointerButton::Back));
        assert_eq!(button(4), Some(PointerButton::Forward));
        assert_eq!(button(5), None);
        assert_eq!(button(255), None);
    }

    #[test]
    fn a_hold_expires_unless_renewed() {
        assert!(!held_at(0, 5), "0 means not held");
        assert!(held_at(10, 10));
        assert!(held_at(10, 10 + HOLD_MAX_MS - 1));
        assert!(
            !held_at(10, 10 + HOLD_MAX_MS),
            "exactly at the limit it is over"
        );
        // A clock that reads before the stamp (should not happen) is not a reason to freeze forever.
        assert!(held_at(10, 3));
    }

    #[test]
    fn the_hold_stamp_is_the_time_or_zero() {
        assert_eq!(hold_stamp(true, 42), 42);
        assert_eq!(hold_stamp(false, 42), 0);
    }

    #[test]
    fn the_clock_never_returns_zero_and_does_not_go_back() {
        let a = now_ms();
        let b = now_ms();
        assert!(a >= 1 && b >= a);
        let since = AtomicU64::new(0);
        assert!(!held(&since));
        since.store(now_ms(), Ordering::Relaxed);
        assert!(held(&since));
    }

    #[test]
    fn file_entries_survive_the_round_trip() {
        let e = FileEntry {
            path: "a/b.txt".into(),
            size: 12,
            is_dir: false,
            modified: 7,
            read_only: true,
        };
        let back = from_info(to_info(&e));
        assert_eq!(back.path, e.path);
        assert_eq!(
            (back.size, back.is_dir, back.modified, back.read_only),
            (12, false, 7, true)
        );
    }

    #[test]
    fn the_layout_numbers_the_outputs_and_makes_the_first_primary() {
        let l = layout_of(&[
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            Rect {
                x: 1920,
                y: 0,
                width: 1280,
                height: 720,
            },
        ]);
        assert_eq!(l.outputs.len(), 2);
        assert_eq!(l.outputs[0].id, OutputId(0));
        assert!(l.outputs[0].primary && !l.outputs[1].primary);
        assert_eq!((l.outputs[1].x, l.outputs[1].width), (1920, 1280));
        assert!(layout_of(&[]).outputs.is_empty());
    }
}
