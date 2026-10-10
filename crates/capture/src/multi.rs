//! Agrège une source de capture par sortie derrière le port `CaptureSource`.

use rdsktp_core::error::CaptureError;
use rdsktp_core::frame::Frame;
use rdsktp_core::layout::OutputId;
use rdsktp_core::ports::CaptureSource;

/// `recv_latest(OutputId(i))` interroge la i-ème source. Les sources sont indépendantes (un flux,
/// une connexion, une boîte aux lettres chacune) : un écran lent n'en retarde pas un autre.
pub struct MultiCapture {
    sources: Vec<Box<dyn CaptureSource>>,
}

impl MultiCapture {
    pub fn new(sources: Vec<Box<dyn CaptureSource>>) -> Self {
        Self { sources }
    }
}

impl CaptureSource for MultiCapture {
    fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
        match self.sources.get_mut(output.0 as usize) {
            Some(source) => source.recv_latest(output),
            None => Err(CaptureError::Unavailable(format!("sortie {} inconnue", output.0))),
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
            Ok(Some(Frame { output, width: 1, height: 1, stride: 4, format: PixelFormat::Rgba8, data: FrameData::Shm(vec![self.0; 4]) }))
        }
    }

    #[test]
    fn routes_by_output_and_rejects_unknown() {
        let mut m = MultiCapture::new(vec![Box::new(Fixed(1)), Box::new(Fixed(2))]);
        let FrameData::Shm(d) = m.recv_latest(OutputId(1)).unwrap().unwrap().data else { panic!() };
        assert_eq!(d[0], 2);
        assert!(m.recv_latest(OutputId(2)).is_err());
    }
}
