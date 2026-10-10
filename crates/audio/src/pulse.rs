use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};

use rdsktp_core::error::AudioError;
use rdsktp_core::ports::{AudioParams, AudioProvider, AudioSource};

/// Nom du puits virtuel créé pour la session : les applications y jouent (puits par défaut),
/// on capture son moniteur.
const SINK: &str = "rdsktp";

/// Capture du « moniteur » d'un puits nul PipeWire, via l'API PulseAudio de `pipewire-pulse`
/// (`pactl` / `parec`, paquet `pulseaudio-utils`).
pub struct PulseCapture;

impl PulseCapture {
    /// Crée le puits nul s'il n'existe pas et le rend par défaut.
    pub fn ensure_sink() -> Result<(), AudioError> {
        let backend = |what: &str, e: std::io::Error| AudioError::Backend(format!("{what} : {e} (paquet pulseaudio-utils requis)"));
        let list = Command::new("pactl").args(["list", "short", "sinks"]).output().map_err(|e| backend("pactl", e))?;
        if !list.status.success() {
            return Err(AudioError::Backend(format!(
                "pactl list sinks : {} (pipewire-pulse démarré ?)",
                String::from_utf8_lossy(&list.stderr).trim()
            )));
        }
        let exists = String::from_utf8_lossy(&list.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some(SINK));
        if !exists {
            let out = Command::new("pactl")
                .args(["load-module", "module-null-sink", &format!("sink_name={SINK}"), "sink_properties=device.description=RustyDesktop"])
                .output()
                .map_err(|e| backend("pactl", e))?;
            if !out.status.success() {
                return Err(AudioError::Backend(format!("création du puits : {}", String::from_utf8_lossy(&out.stderr).trim())));
            }
        }
        let _ = Command::new("pactl").args(["set-default-sink", SINK]).status();
        Ok(())
    }
}

impl AudioProvider for PulseCapture {
    fn open(&self, params: AudioParams) -> Result<Box<dyn AudioSource>, AudioError> {
        Self::ensure_sink()?;
        let mut child = Command::new("parec")
            .args(["-d", &format!("{SINK}.monitor"), "--format=s16le", "--raw", "--latency-msec=20"])
            .arg(format!("--rate={}", params.sample_rate))
            .arg(format!("--channels={}", params.channels))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| AudioError::Backend(format!("parec : {e} (paquet pulseaudio-utils requis)")))?;
        let stdout = child.stdout.take().ok_or(AudioError::Closed)?;
        Ok(Box::new(PulseSource { child, stdout }))
    }
}

struct PulseSource {
    child: Child,
    stdout: ChildStdout,
}

impl AudioSource for PulseSource {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), AudioError> {
        self.stdout.read_exact(buf).map_err(|_| AudioError::Closed)
    }
}

impl Drop for PulseSource {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
