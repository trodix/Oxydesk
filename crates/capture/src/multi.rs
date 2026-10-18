use oxydesk_core::error::CaptureError;
use oxydesk_core::frame::Frame;
use oxydesk_core::layout::{Layout, OutputId, OutputInfo};
use oxydesk_core::ports::CaptureSource;

/// Creates the source of output `index` (size and position in `OutputInfo`) when the layout grows.
pub type SourceFactory =
    Box<dyn FnMut(usize, &OutputInfo) -> Result<Box<dyn CaptureSource>, CaptureError> + Send>;

/// `recv_latest(OutputId(i))` queries the i-th source. The sources are independent (one stream,
/// one connection, one mailbox each): a slow screen does not delay another.
pub struct MultiCapture {
    sources: Vec<Box<dyn CaptureSource>>,
    factory: Option<SourceFactory>,
    waker: Option<std::sync::Arc<oxydesk_core::wake::Wake>>,
}

impl MultiCapture {
    pub fn new(sources: Vec<Box<dyn CaptureSource>>) -> Self {
        Self {
            sources,
            factory: None,
            waker: None,
        }
    }

    /// Lets the number of outputs grow: `factory` builds the source of each added output.
    pub fn with_factory(mut self, factory: SourceFactory) -> Self {
        self.factory = Some(factory);
        self
    }
}

impl CaptureSource for MultiCapture {
    fn set_waker(&mut self, wake: std::sync::Arc<oxydesk_core::wake::Wake>) {
        for s in &mut self.sources {
            s.set_waker(wake.clone());
        }
        self.waker = Some(wake);
    }

    fn take_cursor(&mut self) -> Option<oxydesk_core::cursor::CursorUpdate> {
        // The pointer is shown on every output's stream; the latest update found wins.
        self.sources
            .iter_mut()
            .filter_map(|s| s.take_cursor())
            .last()
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        match self.sources.as_mut_slice() {
            [single] => single.resize(width, height),
            // Several outputs asked to become one.
            _ => self.apply_layout(&Layout::side_by_side(1, width, height)),
        }
    }

    /// Outputs are added (through the factory), removed (the last ones are retired and dropped), resized,
    /// then positioned. A failure while adding leaves the existing outputs untouched.
    fn apply_layout(&mut self, layout: &Layout) -> Result<(), CaptureError> {
        let wanted = layout.outputs.len();
        if wanted == 0 {
            return Err(CaptureError::ResizeUnsupported);
        }
        let mut added = Vec::new();
        for (i, o) in layout.outputs.iter().enumerate().skip(self.sources.len()) {
            let factory = self
                .factory
                .as_mut()
                .ok_or_else(|| CaptureError::Unavailable("no way to add outputs".into()))?;
            let mut source = factory(i, o)?;
            if let Some(w) = &self.waker {
                source.set_waker(w.clone());
            }
            added.push(source);
        }
        for mut gone in self.sources.drain(wanted.min(self.sources.len())..) {
            gone.retire();
        }
        self.sources.extend(added);
        // Sizes first, so that no output overlaps a neighbour at its new position.
        for (source, o) in self.sources.iter_mut().zip(&layout.outputs) {
            source.resize(o.width, o.height)?;
        }
        // One single change when the compositor allows it: it keeps the outputs adjacent, so that exchanging two
        // positions one output at a time ends with both at the same place.
        let names: Option<Vec<String>> = self.sources.iter().map(|s| s.output_name()).collect();
        let grouped = match (names, self.sources.first_mut()) {
            (Some(names), Some(first)) if wanted > 1 => {
                let moves: Vec<(String, i32, i32)> = names
                    .into_iter()
                    .zip(&layout.outputs)
                    .map(|(n, o)| (n, o.x, o.y))
                    .collect();
                first.place_group(&moves).is_ok()
            }
            _ => false,
        };
        if !grouped {
            for (source, o) in self.sources.iter_mut().zip(&layout.outputs) {
                source.place(o.x, o.y)?;
            }
        }
        // The first output is the client's primary monitor: the shell puts its panel there.
        if wanted > 1 {
            if let Some(source) = self.sources.first_mut() {
                source.set_primary()?;
            }
        }
        Ok(())
    }

    fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
        match self.sources.get_mut(output.0 as usize) {
            Some(source) => source.recv_latest(output),
            None => Err(CaptureError::Unavailable(format!(
                "unknown output {}",
                output.0
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_core::frame::{FrameData, PixelFormat};

    struct Fixed(u8);
    impl CaptureSource for Fixed {
        fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
            Ok(Some(Frame {
                output,
                width: 1,
                height: 1,
                stride: 4,
                format: PixelFormat::Rgba8,
                data: FrameData::Shm(vec![self.0; 4]),
            }))
        }
    }

    #[test]
    fn routes_by_output_and_rejects_unknown() {
        let mut m = MultiCapture::new(vec![Box::new(Fixed(1)), Box::new(Fixed(2))]);
        let FrameData::Shm(d) = m.recv_latest(OutputId(1)).unwrap().unwrap().data else {
            panic!()
        };
        assert_eq!(d[0], 2);
        assert!(m.recv_latest(OutputId(2)).is_err());
    }

    struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<String>>>, u8);
    impl CaptureSource for Recorder {
        fn recv_latest(&mut self, _: OutputId) -> Result<Option<Frame>, CaptureError> {
            Ok(None)
        }
        fn resize(&mut self, w: u32, h: u32) -> Result<(), CaptureError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{}:size {w}x{h}", self.1));
            Ok(())
        }
        fn place(&mut self, x: i32, y: i32) -> Result<(), CaptureError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{}:at {x},{y}", self.1));
            Ok(())
        }
        fn retire(&mut self) {
            self.0.lock().unwrap().push(format!("{}:retired", self.1));
        }
        fn set_primary(&mut self) -> Result<(), CaptureError> {
            self.0.lock().unwrap().push(format!("{}:primary", self.1));
            Ok(())
        }
    }

    /// A source that knows its output's name and moves a whole group at once, like the one of KWin.
    struct Named(std::sync::Arc<std::sync::Mutex<Vec<String>>>, u8);
    impl CaptureSource for Named {
        fn recv_latest(&mut self, _: OutputId) -> Result<Option<Frame>, CaptureError> {
            Ok(None)
        }
        fn resize(&mut self, _: u32, _: u32) -> Result<(), CaptureError> {
            Ok(())
        }
        fn output_name(&self) -> Option<String> {
            Some(format!("Virtual-{}", self.1))
        }
        fn place_group(&mut self, moves: &[(String, i32, i32)]) -> Result<(), CaptureError> {
            self.0.lock().unwrap().push(format!("group {moves:?}"));
            Ok(())
        }
        fn place(&mut self, x: i32, y: i32) -> Result<(), CaptureError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{}:at {x},{y}", self.1));
            Ok(())
        }
        fn set_primary(&mut self) -> Result<(), CaptureError> {
            self.0.lock().unwrap().push(format!("{}:primary", self.1));
            Ok(())
        }
    }

    fn client_arrangement() -> Layout {
        // The client's primary monitor (output 0) is at the right of the other one.
        Layout::from_monitors(&[
            oxydesk_core::layout::MonitorSpec {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                primary: true,
            },
            oxydesk_core::layout::MonitorSpec {
                x: -800,
                y: 0,
                width: 800,
                height: 600,
                primary: false,
            },
        ])
        .unwrap()
    }

    #[test]
    fn outputs_are_exchanged_in_one_change_never_one_by_one() {
        // Moving the outputs one at a time exchanges nothing: the compositor moves an output put on another one.
        let log: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let mut m = MultiCapture::new(vec![
            Box::new(Named(log.clone(), 0)),
            Box::new(Named(log.clone(), 1)),
        ]);
        m.apply_layout(&client_arrangement()).unwrap();
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log[0],
            r#"group [("Virtual-0", 800, 0), ("Virtual-1", 0, 0)]"#
        );
        assert!(!log.iter().any(|l| l.contains(":at ")), "{log:?}");
    }

    #[test]
    fn the_first_output_becomes_the_primary_one_of_the_compositor() {
        // The shell puts its panel on the primary output of the compositor: the client's primary monitor is output 0.
        let log: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let mut m = MultiCapture::new(vec![
            Box::new(Named(log.clone(), 0)),
            Box::new(Named(log.clone(), 1)),
        ]);
        m.apply_layout(&client_arrangement()).unwrap();
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log.iter()
                .filter(|l| l.ends_with(":primary"))
                .collect::<Vec<_>>(),
            ["0:primary"]
        );
    }

    #[test]
    fn apply_layout_sizes_then_places_every_source() {
        let log: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let mut m = MultiCapture::new(vec![
            Box::new(Recorder(log.clone(), 0)),
            Box::new(Recorder(log.clone(), 1)),
        ]);
        m.apply_layout(&Layout::side_by_side(2, 800, 600)).unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [
                "0:size 800x600",
                "1:size 800x600",
                "0:at 0,0",
                "1:at 800,0",
                "0:primary"
            ]
        );
        // Growing needs a factory.
        assert!(m.apply_layout(&Layout::side_by_side(3, 800, 600)).is_err());
    }

    #[test]
    fn adds_outputs_through_the_factory_and_retires_removed_ones() {
        let log: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let factory_log = log.clone();
        let mut m = MultiCapture::new(vec![Box::new(Recorder(log.clone(), 0))]).with_factory(
            Box::new(move |i, _| {
                Ok(Box::new(Recorder(factory_log.clone(), i as u8)) as Box<dyn CaptureSource>)
            }),
        );
        m.apply_layout(&Layout::side_by_side(3, 100, 100)).unwrap();
        assert!(m.recv_latest(OutputId(2)).is_ok());
        log.lock().unwrap().clear();
        // Back to one output through `resize`: the last two are retired, the first one is resized.
        m.resize(640, 480).unwrap();
        let log = log.lock().unwrap().clone();
        assert!(
            log.contains(&"1:retired".into()) && log.contains(&"2:retired".into()),
            "{log:?}"
        );
        assert!(log.contains(&"0:size 640x480".into()));
        assert!(m.recv_latest(OutputId(1)).is_err());
    }
}
