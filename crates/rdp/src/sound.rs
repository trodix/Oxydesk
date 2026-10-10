//! Outgoing audio: RDPSND in 16-bit PCM. A source (`AudioProvider`) is opened in the negotiated format
//! when the client is ready; a thread reads it in 20 ms packets and pushes them to the client.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError};
use ironrdp_server::{RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory};
use rdsktp_core::ports::{AudioParams, AudioProvider};
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
            formats: vec![pcm(48_000), pcm(44_100)],
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
        f.debug_struct("SoundHandler").field("formats", &self.formats).finish_non_exhaustive()
    }
}

impl RdpsndServerHandler for SoundHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(&mut self, common: &'a [NegotiatedFormat]) -> Option<&'a NegotiatedFormat> {
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let f = format.format();
        let params = AudioParams { sample_rate: f.n_samples_per_sec, channels: f.n_channels };
        let mut source = self.provider.open(params).map_err(|e| Box::new(e) as Box<dyn RdpsndError>)?;
        tracing::info!(rate = params.sample_rate, channels = params.channels, "audio: stream started");
        let run = Arc::new(AtomicBool::new(true));
        self.running = Some(run.clone());
        let shared = self.shared.clone();
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
                    let ts = started.elapsed().as_millis() as u32;
                    if let Some(tx) = shared.sender.lock().expect("sender mutex").as_ref() {
                        let _ = tx.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(buf.clone(), ts)));
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
