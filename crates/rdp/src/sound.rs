//! Outgoing audio: RDPSND in 16-bit PCM. A source (`AudioProvider`) is opened in the negotiated format
//! when the client is ready; a thread reads it in 20 ms packets and pushes them to the client.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError};
use ironrdp_server::{
    RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory,
};
use oxydesk_core::ports::{AudioParams, AudioProvider};
use tokio::sync::mpsc::UnboundedSender;

use crate::pump::Shared;

/// Packet duration: short enough for latency, long enough to limit the event loop load.
const PACKET_MS: u32 = 20;

fn pcm(rate: u32) -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: 2,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * 4,
        n_block_align: 4,
        bits_per_sample: 16,
        data: None,
    }
}

pub struct SoundFactory {
    pub shared: Arc<Shared>,
    pub provider: Arc<dyn AudioProvider>,
}

impl ServerEventSender for SoundFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.shared.sender.lock().expect("sender mutex") = Some(sender);
    }
}

impl SoundServerFactory for SoundFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(SoundHandler {
            shared: self.shared.clone(),
            provider: self.provider.clone(),
            // 44.1 kHz first: measured with the stock Windows client (mstscax, Win11 26300), a 48 kHz
            // stream is played at 44.1/48 of its pitch on a 44.1 kHz device (440 Hz heard as 405 Hz),
            // while a 44.1 kHz stream is exact. The server picks the first format both sides share.
            formats: vec![pcm(44_100), pcm(48_000)],
            running: None,
        })
    }
}

struct SoundHandler {
    shared: Arc<Shared>,
    provider: Arc<dyn AudioProvider>,
    formats: Vec<AudioFormat>,
    running: Option<Arc<AtomicBool>>,
}

impl std::fmt::Debug for SoundHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundHandler")
            .field("formats", &self.formats)
            .finish_non_exhaustive()
    }
}

impl RdpsndServerHandler for SoundHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let f = format.format();
        let params = AudioParams {
            sample_rate: f.n_samples_per_sec,
            channels: f.n_channels,
        };
        let mut source = self
            .provider
            .open(params)
            .map_err(|e| Box::new(e) as Box<dyn RdpsndError>)?;
        tracing::info!(
            rate = params.sample_rate,
            channels = params.channels,
            "audio: stream started"
        );
        let run = Arc::new(AtomicBool::new(true));
        self.running = Some(run.clone());
        let shared = self.shared.clone();
        // Bound to this connection's channel: after a reconnection the shared sender belongs to the
        // new connection, which a stale pump must never feed.
        let sender = shared.sender.lock().expect("sender mutex").clone();
        std::thread::Builder::new()
            .name("audio-pump".into())
            .spawn(move || {
                let started = Instant::now();
                let mut buf = vec![0u8; params.bytes_for_ms(PACKET_MS)];
                while run.load(Ordering::Relaxed) && !shared.stop.load(Ordering::Relaxed) {
                    if let Err(e) = source.read_exact(&mut buf) {
                        tracing::warn!("audio: {e}");
                        break;
                    }
                    // `read_exact` blocks for one packet: the connection may have ended meanwhile,
                    // and its events would reach the next connection (one shared channel).
                    if !run.load(Ordering::Relaxed) {
                        break;
                    }
                    let ts = started.elapsed().as_millis() as u32;
                    if let Some(tx) = &sender {
                        if tx
                            .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
                                buf.clone(),
                                ts,
                            )))
                            .is_err()
                        {
                            break; // connection gone
                        }
                    }
                }
            })
            .map_err(|e| Box::new(e) as Box<dyn RdpsndError>)?;
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(run) = self.running.take() {
            run.store(false, Ordering::Relaxed);
        }
    }
}

impl Drop for SoundHandler {
    /// A connection lost without a clean RDPSND shutdown must not leave its pump running.
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_core::error::AudioError;
    use oxydesk_core::ports::AudioSource;

    struct Nothing;

    impl AudioProvider for Nothing {
        fn open(&self, _: AudioParams) -> Result<Box<dyn AudioSource>, AudioError> {
            Err(AudioError::Backend("no device".into()))
        }
    }

    fn handler() -> Box<dyn RdpsndServerHandler> {
        SoundFactory {
            shared: Arc::new(Shared::default()),
            provider: Arc::new(Nothing),
        }
        .build_backend()
    }

    #[test]
    fn the_formats_offered_are_pcm_stereo_with_44_1_khz_first() {
        let h = handler();
        let formats = h.get_formats();
        assert_eq!(formats.len(), 2);
        assert_eq!(formats[0].n_samples_per_sec, 44_100);
        assert_eq!(formats[1].n_samples_per_sec, 48_000);
        for f in formats {
            assert_eq!(f.n_channels, 2);
            assert_eq!(f.bits_per_sample, 16);
            assert_eq!(f.n_block_align, 4);
            assert_eq!(f.n_avg_bytes_per_sec, f.n_samples_per_sec * 4);
        }
    }

    #[test]
    fn without_a_common_format_nothing_is_chosen() {
        assert!(handler().choose_format(&[]).is_none());
    }

    #[test]
    fn stopping_a_stream_that_never_started_is_harmless() {
        let mut h = handler();
        h.stop();
        h.stop();
        assert!(format!("{h:?}").contains("SoundHandler"));
    }

    #[test]
    fn the_factory_gives_the_sender_of_the_connection_to_the_shared_state() {
        let shared = Arc::new(Shared::default());
        let mut factory = SoundFactory {
            shared: shared.clone(),
            provider: Arc::new(Nothing),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        factory.set_sender(tx);
        assert!(shared.sender.lock().unwrap().is_some());
    }
}
