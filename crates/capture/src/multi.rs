//! Aggregates one capture source per output behind the `CaptureSource` port.

use rdsktp_core::error::CaptureError;
use rdsktp_core::frame::Frame;
use rdsktp_core::layout::OutputId;
use rdsktp_core::ports::CaptureSource;

/// `recv_latest(OutputId(i))` queries the i-th source. The sources are independent (one stream,
/// one connection, one mailbox each): a slow screen does not delay another.
pub struct MultiCapture {
    sources: Vec<Box<dyn CaptureSource>>,
}

impl MultiCapture {
    pub fn new(sources: Vec<Box<dyn CaptureSource>>) -> Self {
        Self { sources }
    }
}

impl CaptureSource for MultiCapture {
    fn set_waker(&mut self, wake: std::sync::Arc<rdsktp_core::wake::Wake>) {
        for s in &mut self.sources {
            s.set_waker(wake.clone());
        }
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
}
