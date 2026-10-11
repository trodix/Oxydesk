use rdsktp_core::error::CaptureError;
use rdsktp_core::frame::Frame;
use rdsktp_core::layout::{Layout, OutputId, OutputInfo};
use rdsktp_core::ports::CaptureSource;

/// Creates the source of output `index` (size and position in `OutputInfo`) when the layout grows.
pub type SourceFactory =
    Box<dyn FnMut(usize, &OutputInfo) -> Result<Box<dyn CaptureSource>, CaptureError> + Send>;

/// `recv_latest(OutputId(i))` queries the i-th source. The sources are independent (one stream,
/// one connection, one mailbox each): a slow screen does not delay another.
pub struct MultiCapture {
    sources: Vec<Box<dyn CaptureSource>>,
    factory: Option<SourceFactory>,
    waker: Option<std::sync::Arc<rdsktp_core::wake::Wake>>,
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
    fn set_waker(&mut self, wake: std::sync::Arc<rdsktp_core::wake::Wake>) {
        for s in &mut self.sources {
            s.set_waker(wake.clone());
        }
        self.waker = Some(wake);
    }

    fn take_cursor(&mut self) -> Option<rdsktp_core::cursor::CursorUpdate> {
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
                .ok_or(CaptureError::ResizeUnsupported)?;
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
        for (source, o) in self.sources.iter_mut().zip(&layout.outputs) {
            source.place(o.x, o.y)?;
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
    use rdsktp_core::frame::{FrameData, PixelFormat};

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
            ["0:size 800x600", "1:size 800x600", "0:at 0,0", "1:at 800,0"]
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
