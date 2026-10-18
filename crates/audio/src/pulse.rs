use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};

use oxydesk_core::error::AudioError;
use oxydesk_core::ports::{AudioParams, AudioProvider, AudioSource};

/// Name of the virtual sink created for the session: applications play to it (default sink),
/// the "monitor" of that sink is captured.
const SINK: &str = "oxydesk";

/// The PulseAudio name for the monitor of whatever the default sink is.
const DEFAULT_MONITOR: &str = "@DEFAULT_MONITOR@";

/// Capture of the "monitor" of a PipeWire null sink, through the PulseAudio API of `pipewire-pulse`
/// (`pactl` / `parec`, `pulseaudio-utils` package).
pub struct PulseCapture;

impl PulseCapture {
    /// Creates the null sink if it does not exist and makes it the default.
    pub fn ensure_sink() -> Result<(), AudioError> {
        let backend = |what: &str, e: std::io::Error| {
            AudioError::Backend(format!("{what}: {e} (pulseaudio-utils package required)"))
        };
        let list = Command::new("pactl")
            .args(["list", "short", "sinks"])
            .output()
            .map_err(|e| backend("pactl", e))?;
        if !list.status.success() {
            return Err(AudioError::Backend(format!(
                "pactl list sinks: {} (is pipewire-pulse started?)",
                String::from_utf8_lossy(&list.stderr).trim()
            )));
        }
        let exists = String::from_utf8_lossy(&list.stdout)
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(SINK));
        if !exists {
            let out = Command::new("pactl")
                .args([
                    "load-module",
                    "module-null-sink",
                    &format!("sink_name={SINK}"),
                    "sink_properties=device.description=Oxydesk",
                ])
                .output()
                .map_err(|e| backend("pactl", e))?;
            if !out.status.success() {
                return Err(AudioError::Backend(format!(
                    "sink creation: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }
        let _ = Command::new("pactl")
            .args(["set-default-sink", SINK])
            .status();
        Ok(())
    }
}

/// Arguments of `parec` capturing `device` as raw 16-bit little-endian PCM.
fn parec_args(device: &str, params: AudioParams) -> Vec<String> {
    vec![
        "-d".into(),
        device.into(),
        "--format=s16le".into(),
        "--raw".into(),
        "--latency-msec=20".into(),
        format!("--rate={}", params.sample_rate),
        format!("--channels={}", params.channels),
    ]
}

fn open_parec(device: &str, params: AudioParams) -> Result<Box<dyn AudioSource>, AudioError> {
    let mut child = Command::new("parec")
        .args(parec_args(device, params))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            AudioError::Backend(format!("parec: {e} (pulseaudio-utils package required)"))
        })?;
    let stdout = child.stdout.take().ok_or(AudioError::Closed)?;
    Ok(Box::new(PulseSource { child, stdout }))
}

impl AudioProvider for PulseCapture {
    fn open(&self, params: AudioParams) -> Result<Box<dyn AudioSource>, AudioError> {
        Self::ensure_sink()?;
        open_parec(&format!("{SINK}.monitor"), params)
    }
}

/// Capture of what the user's current default output plays (its "monitor"), for a session that is also in use
/// locally: nothing is created and the default sink is left alone, so the local speakers keep playing.
pub struct PulseMonitor;

impl AudioProvider for PulseMonitor {
    fn open(&self, params: AudioParams) -> Result<Box<dyn AudioSource>, AudioError> {
        open_parec(DEFAULT_MONITOR, params)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capture_command_names_the_device_and_the_format() {
        let params = AudioParams {
            sample_rate: 48_000,
            channels: 2,
        };
        let args = parec_args(DEFAULT_MONITOR, params);
        assert_eq!(&args[..2], ["-d", "@DEFAULT_MONITOR@"]);
        assert!(args.contains(&"--rate=48000".to_owned()));
        assert!(args.contains(&"--channels=2".to_owned()));
        // The private sink's monitor is a different device: the session's own capture is unchanged.
        assert_eq!(parec_args("oxydesk.monitor", params)[1], "oxydesk.monitor");
    }
}
