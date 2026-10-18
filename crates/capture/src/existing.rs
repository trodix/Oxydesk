//! Capture of an output that belongs to the user's own desktop (a Plasma session that was already running).
//!
//! The frames and the pointer shape pass through; everything that would change the monitors does not: no
//! resize, no disabling when the output is dropped, no move, no primary. A resize is refused, which makes the
//! client's size come from the scaling fallback (`oxydesk-scaling`).

use std::sync::Arc;

use oxydesk_core::cursor::CursorUpdate;
use oxydesk_core::error::CaptureError;
use oxydesk_core::frame::Frame;
use oxydesk_core::layout::Layout;
use oxydesk_core::layout::OutputId;
use oxydesk_core::ports::{CaptureSource, WindowMover};
use oxydesk_core::wake::Wake;

pub struct ExistingOutput<C>(pub C);

impl<C: CaptureSource> CaptureSource for ExistingOutput<C> {
    fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
        self.0.recv_latest(output)
    }

    fn set_waker(&mut self, wake: Arc<Wake>) {
        self.0.set_waker(wake);
    }

    fn set_raw_pixels(&mut self, raw: bool) {
        self.0.set_raw_pixels(raw);
    }

    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        self.0.take_cursor()
    }

    fn output_name(&self) -> Option<String> {
        self.0.output_name()
    }

    // `resize`, `retire`, `place`, `place_group`, `set_primary` and `apply_layout` keep the port's defaults:
    // a resize is refused and the others do nothing, so the monitors are never touched.
}

/// The whole desktop of the user, as a set of outputs: when the client shows fewer monitors than the desktop has,
/// the windows of the monitors that are left out are brought onto the ones that are shown, so that nothing stays
/// out of sight. `physical` names the real outputs, left to right: the first ones are the ones that stay shown.
pub struct ExistingDesktop<C> {
    inner: C,
    physical: Vec<String>,
    mover: Box<dyn WindowMover>,
    /// Tells whether the compositor shows exactly this many monitors (extra ones take a moment to go).
    settled: Option<Settled>,
    /// The monitors the client added stay, with the windows on them, when it leaves.
    keep_virtual: bool,
    /// How many monitors beyond the real ones the client has now (each has a virtual desktop of its own).
    extras: usize,
    /// The stale virtual desktops were removed when the client first set its layout.
    swept: bool,
}

pub type Settled = Box<dyn FnMut(usize) -> bool + Send>;

impl<C> ExistingDesktop<C> {
    /// Names of the extra monitors of ranks `from..to` beyond the real ones.
    fn extra_names(&self, from: usize, to: usize) -> Vec<String> {
        (from..to)
            .map(|i| format!("Virtual-{}", self.physical.len() + i))
            .collect()
    }

    /// The desktops of monitors that no longer exist go, with their windows moved to a real desktop.
    fn sweep_stale(&mut self) {
        if let Err(e) = self.mover.sweep() {
            eprintln!("cannot remove the stale virtual desktops: {e}");
        }
    }

    pub fn new(inner: C, physical: Vec<String>, mover: Box<dyn WindowMover>) -> Self {
        Self {
            inner,
            physical,
            mover,
            settled: None,
            keep_virtual: false,
            extras: 0,
            swept: false,
        }
    }

    /// When the client leaves, its extra monitors stay in the session (as virtual displays) and no window is moved.
    pub fn keep_virtual_outputs(mut self, keep: bool) -> Self {
        self.keep_virtual = keep;
        self
    }

    /// Waits, when the client leaves, until `settled(n)` says the compositor is back to `n` monitors.
    pub fn with_settle(mut self, settled: Settled) -> Self {
        self.settled = Some(settled);
        self
    }
}

impl<C: CaptureSource> CaptureSource for ExistingDesktop<C> {
    fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
        self.inner.recv_latest(output)
    }

    fn set_waker(&mut self, wake: Arc<Wake>) {
        self.inner.set_waker(wake);
    }

    fn set_raw_pixels(&mut self, raw: bool) {
        self.inner.set_raw_pixels(raw);
    }

    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        self.inner.take_cursor()
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        self.apply_layout(&Layout::side_by_side(1, width, height))
    }

    /// The client left: what it had on its extra (virtual) monitors goes to a real monitor, those monitors are
    /// removed, and the windows it had brought over go back where they were, so that the local screen shows
    /// everything as before. The monitors go first: KWin remembers the places of the windows for each set of
    /// monitors and puts them back there when the set changes, which would undo an earlier restore.
    fn release(&mut self) {
        let cannot = |what: &str, e: CaptureError| eprintln!("cannot {what}: {e}");
        // The windows of the extra monitors go to the first real one: parked on the virtual desktop of their monitor
        // (which stays, like the monitor) or, when the monitors go, with that desktop removed.
        if let Some(home) = self.physical.first().cloned() {
            let extra = self.extra_names(0, self.extras);
            if let Err(e) = self.mover.park(&extra, &home, self.keep_virtual) {
                cannot("park the windows of the extra monitors", e);
            }
            self.extras = 0;
        }
        if self.keep_virtual {
            self.inner.release();
            self.sweep_stale();
            return;
        }
        if !self.physical.is_empty() {
            // Windows on an output outside the real ones are brought onto the first real one before it vanishes.
            if let Err(e) = self.mover.gather(&self.physical) {
                cannot("bring the windows of the extra monitors back", e);
            }
            // The inner desktop drops its sources beyond the real outputs, and refuses what it cannot resize.
            let _ =
                self.inner
                    .apply_layout(&Layout::side_by_side(self.physical.len() as u32, 1, 1));
            if let Some(settled) = self.settled.as_mut() {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !settled(self.physical.len()) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
        if let Err(e) = self.mover.restore() {
            cannot("put the windows back", e);
        }
        self.inner.release();
        self.sweep_stale();
    }

    fn apply_layout(&mut self, layout: &Layout) -> Result<(), CaptureError> {
        let shown = layout.outputs.len();
        if !self.swept {
            self.swept = true;
            self.sweep_stale();
        }
        if shown > 0 && shown < self.physical.len() {
            // Not fatal: the windows stay where they are, and the client sees only part of them.
            if let Err(e) = self.mover.gather(&self.physical[..shown]) {
                eprintln!("cannot move the windows onto the shown monitors: {e}");
            }
        }
        // A desktop without a monitor (the screens are unplugged) is shown entirely on virtual outputs: they are its
        // monitors, not extra ones, so there is nothing to park or bring over.
        let extras = if self.physical.is_empty() {
            0
        } else {
            shown.saturating_sub(self.physical.len())
        };
        if extras < self.extras {
            // Monitors going away mid-session: their windows are parked before the monitors are dropped.
            if let Some(home) = self.physical.first().cloned() {
                let gone = self.extra_names(extras, self.extras);
                if let Err(e) = self.mover.park(&gone, &home, self.keep_virtual) {
                    eprintln!("cannot park the windows of the extra monitors: {e}");
                }
            }
        }
        // The real monitors cannot be resized (the answer is `ResizeUnsupported`, which makes the caller scale), but
        // the extra ones are created all the same.
        let answer = self.inner.apply_layout(layout);
        if !matches!(answer, Ok(()) | Err(CaptureError::ResizeUnsupported)) {
            return answer;
        }
        if extras > self.extras {
            let added = self.extra_names(self.extras, extras);
            if let Err(e) = self.mover.claim(&added) {
                eprintln!("cannot bring the windows of the extra monitors: {e}");
            }
        }
        self.extras = extras;
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_core::layout::{Layout, OutputInfo};
    use std::sync::Mutex;

    /// Records every call that would act on a monitor.
    struct Recording(Arc<Mutex<Vec<&'static str>>>);

    impl CaptureSource for Recording {
        fn recv_latest(&mut self, _: OutputId) -> Result<Option<Frame>, CaptureError> {
            self.0.lock().unwrap().push("recv");
            Ok(None)
        }
        fn resize(&mut self, _: u32, _: u32) -> Result<(), CaptureError> {
            self.0.lock().unwrap().push("resize");
            Ok(())
        }
        fn retire(&mut self) {
            self.0.lock().unwrap().push("retire");
        }
        fn place(&mut self, _: i32, _: i32) -> Result<(), CaptureError> {
            self.0.lock().unwrap().push("place");
            Ok(())
        }
        fn place_group(&mut self, _: &[(String, i32, i32)]) -> Result<(), CaptureError> {
            self.0.lock().unwrap().push("place_group");
            Ok(())
        }
        fn set_primary(&mut self) -> Result<(), CaptureError> {
            self.0.lock().unwrap().push("set_primary");
            Ok(())
        }
        fn output_name(&self) -> Option<String> {
            Some("DP-1".into())
        }
    }

    #[test]
    fn frames_pass_through_and_no_call_reaches_the_monitor() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut c = ExistingOutput(Recording(log.clone()));
        assert!(c.recv_latest(OutputId(0)).unwrap().is_none());
        assert_eq!(c.output_name().as_deref(), Some("DP-1"));
        assert!(matches!(
            c.resize(800, 600),
            Err(CaptureError::ResizeUnsupported)
        ));
        let layout = Layout {
            outputs: vec![OutputInfo {
                id: OutputId(0),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                primary: true,
            }],
            ..Layout::default()
        };
        assert!(matches!(
            c.apply_layout(&layout),
            Err(CaptureError::ResizeUnsupported)
        ));
        c.retire();
        c.place(10, 10).unwrap();
        assert!(matches!(
            c.place_group(&[("DP-1".into(), 0, 0)]),
            Err(CaptureError::ResizeUnsupported)
        ));
        c.set_primary().unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["recv"]);
    }

    struct Mover(Arc<Mutex<Vec<Vec<String>>>>);

    impl WindowMover for Mover {
        fn gather(&mut self, keep: &[String]) -> Result<(), CaptureError> {
            self.0.lock().unwrap().push(keep.to_vec());
            Ok(())
        }
        fn restore(&mut self) -> Result<(), CaptureError> {
            self.0.lock().unwrap().push(vec!["restore".into()]);
            Ok(())
        }
    }

    struct Inner;

    impl CaptureSource for Inner {
        fn recv_latest(&mut self, _: OutputId) -> Result<Option<Frame>, CaptureError> {
            Ok(None)
        }
    }

    fn desktop(moves: &Arc<Mutex<Vec<Vec<String>>>>) -> ExistingDesktop<Inner> {
        ExistingDesktop::new(
            Inner,
            vec!["DP-1".into(), "DP-2".into()],
            Box::new(Mover(moves.clone())),
        )
    }

    #[test]
    fn fewer_client_monitors_bring_the_windows_onto_the_shown_ones() {
        let moves = Arc::new(Mutex::new(Vec::new()));
        let mut d = desktop(&moves);
        let _ = d.apply_layout(&Layout::side_by_side(1, 800, 600));
        assert_eq!(*moves.lock().unwrap(), vec![vec!["DP-1".to_string()]]);
        // A single size is a one-monitor layout too.
        let _ = d.resize(800, 600);
        assert_eq!(moves.lock().unwrap().len(), 2);
    }

    #[test]
    fn as_many_or_more_client_monitors_leave_the_windows_alone() {
        let moves = Arc::new(Mutex::new(Vec::new()));
        let mut d = desktop(&moves);
        let _ = d.apply_layout(&Layout::side_by_side(2, 800, 600));
        let _ = d.apply_layout(&Layout::side_by_side(3, 800, 600));
        assert!(moves.lock().unwrap().is_empty());
    }

    #[test]
    fn the_windows_are_put_back_when_the_client_leaves() {
        let moves = Arc::new(Mutex::new(Vec::new()));
        let mut d = desktop(&moves);
        let _ = d.apply_layout(&Layout::side_by_side(1, 800, 600));
        d.release();
        // What is on the extra monitors is brought to the real ones, the monitors go, then the windows go back.
        let seen = moves.lock().unwrap().clone();
        assert_eq!(
            seen[1..],
            [
                vec!["DP-1".to_string(), "DP-2".to_string()],
                vec!["restore".to_string()],
            ]
        );
    }

    #[test]
    fn the_windows_go_back_only_once_the_extra_monitors_are_gone() {
        let moves = Arc::new(Mutex::new(Vec::new()));
        let polls = Arc::new(Mutex::new(Vec::new()));
        let seen = polls.clone();
        let log = moves.clone();
        let mut d = desktop(&moves).with_settle(Box::new(move |n| {
            let mut p = seen.lock().unwrap();
            p.push((n, log.lock().unwrap().len()));
            p.len() >= 3
        }));
        d.release();
        let polls = polls.lock().unwrap();
        // Polled for the real number of monitors, and always before the restore (the last move).
        assert!(polls.len() >= 3 && polls.iter().all(|&(n, moved)| n == 2 && moved == 1));
        assert_eq!(
            moves.lock().unwrap().last(),
            Some(&vec!["restore".to_string()])
        );
    }

    #[test]
    fn kept_virtual_monitors_leave_the_session_and_its_windows_as_they_are() {
        let moves = Arc::new(Mutex::new(Vec::new()));
        let mut d = desktop(&moves)
            .with_settle(Box::new(|_| panic!("nothing to wait for")))
            .keep_virtual_outputs(true);
        let _ = d.apply_layout(&Layout::side_by_side(1, 800, 600));
        d.release();
        // Only the move made when the client arrived: nothing on leaving.
        assert_eq!(moves.lock().unwrap().len(), 1);
    }

    /// Accepts every layout, as the multi-monitor capture does.
    struct Accepting;

    impl CaptureSource for Accepting {
        fn recv_latest(&mut self, _: OutputId) -> Result<Option<Frame>, CaptureError> {
            Ok(None)
        }
        fn apply_layout(&mut self, _: &Layout) -> Result<(), CaptureError> {
            Ok(())
        }
    }

    struct Desks(Arc<Mutex<Vec<String>>>);

    impl WindowMover for Desks {
        fn gather(&mut self, _: &[String]) -> Result<(), CaptureError> {
            Ok(())
        }
        fn claim(&mut self, extra: &[String]) -> Result<(), CaptureError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("claim {}", extra.join(",")));
            Ok(())
        }
        fn park(&mut self, extra: &[String], home: &str, keep: bool) -> Result<(), CaptureError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("park {} -> {home} keep={keep}", extra.join(",")));
            Ok(())
        }
    }

    fn with_desks(keep: bool) -> (ExistingDesktop<Accepting>, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let d = ExistingDesktop::new(
            Accepting,
            vec!["DP-1".into(), "DP-2".into()],
            Box::new(Desks(log.clone())),
        )
        .keep_virtual_outputs(keep);
        (d, log)
    }

    struct Sweeper(Arc<Mutex<u32>>);

    impl WindowMover for Sweeper {
        fn gather(&mut self, _: &[String]) -> Result<(), CaptureError> {
            Ok(())
        }
        fn sweep(&mut self) -> Result<(), CaptureError> {
            *self.0.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[test]
    fn the_desktops_of_monitors_that_are_gone_are_swept_once_at_the_start_and_when_the_client_leaves(
    ) {
        let swept = Arc::new(Mutex::new(0));
        let mut d = ExistingDesktop::new(
            Accepting,
            vec!["DP-1".into(), "DP-2".into()],
            Box::new(Sweeper(swept.clone())),
        );
        let layout = Layout::side_by_side(2, 800, 600);
        d.apply_layout(&layout).unwrap();
        d.apply_layout(&layout).unwrap();
        assert_eq!(*swept.lock().unwrap(), 1);
        d.release();
        assert_eq!(*swept.lock().unwrap(), 2);
    }

    #[test]
    fn each_extra_monitor_takes_the_windows_of_its_desktop() {
        let (mut d, log) = with_desks(true);
        d.apply_layout(&Layout::side_by_side(2, 8, 8)).unwrap();
        assert!(log.lock().unwrap().is_empty());
        d.apply_layout(&Layout::side_by_side(4, 8, 8)).unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["claim Virtual-2,Virtual-3"]);
        // Only what is new is claimed.
        d.apply_layout(&Layout::side_by_side(5, 8, 8)).unwrap();
        assert_eq!(log.lock().unwrap()[1], "claim Virtual-4");
    }

    #[test]
    fn a_desktop_without_a_monitor_lives_on_virtual_outputs_and_moves_no_window() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut d = ExistingDesktop::new(Accepting, Vec::new(), Box::new(Desks(log.clone())));
        d.apply_layout(&Layout::side_by_side(2, 8, 8)).unwrap();
        d.apply_layout(&Layout::side_by_side(1, 8, 8)).unwrap();
        d.release();
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn monitors_that_go_mid_session_park_their_windows_first() {
        let (mut d, log) = with_desks(false);
        d.apply_layout(&Layout::side_by_side(4, 8, 8)).unwrap();
        d.apply_layout(&Layout::side_by_side(3, 8, 8)).unwrap();
        assert_eq!(log.lock().unwrap()[1], "park Virtual-3 -> DP-1 keep=false");
    }

    #[test]
    fn leaving_parks_the_extra_windows_on_their_desktops_or_drops_them() {
        for keep in [true, false] {
            let (mut d, log) = with_desks(keep);
            d.apply_layout(&Layout::side_by_side(3, 8, 8)).unwrap();
            d.release();
            assert_eq!(
                log.lock().unwrap()[1],
                format!("park Virtual-2 -> DP-1 keep={keep}")
            );
        }
    }

    struct Failing;

    impl WindowMover for Failing {
        fn gather(&mut self, _: &[String]) -> Result<(), CaptureError> {
            Err(CaptureError::Unavailable("no script".into()))
        }
    }

    #[test]
    fn a_window_mover_failure_does_not_stop_the_layout() {
        let mut d = ExistingDesktop::new(Inner, vec!["A".into(), "B".into()], Box::new(Failing));
        // The inner source refuses (the port's default), which is the answer given, not the mover's error.
        assert!(matches!(
            d.apply_layout(&Layout::side_by_side(1, 8, 8)),
            Err(CaptureError::ResizeUnsupported)
        ));
    }
}
