//! Command line of the agent, parsed apart from the code that builds the desktop: a pure function of the
//! arguments, so every default and every refusal can be tested without a compositor.

use rdsktp_encode::EncoderChoice;

/// Where the audio comes from (`--audio`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioMode {
    Off,
    Tone,
    Pulse,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub session: String,
    pub width: u32,
    pub height: u32,
    pub outputs: u32,
    pub fps: f32,
    pub audio: AudioMode,
    pub encoder: EncoderChoice,
    /// `--dmabuf on|off`; `None` = let the encoder decide.
    pub dmabuf: Option<bool>,
    /// Wake-up by the capture (default) or fixed rate (`--loop tick`).
    pub event: bool,
    pub poll_ms: Option<f32>,
    pub min_interval_ms: f32,
    pub kwin_socket: String,
    /// One program per `--session-app`, split on whitespace.
    pub programs: Vec<Vec<String>>,
}

/// The value following `name`, when present and well formed.
pub fn arg<T: std::str::FromStr>(args: &[String], name: &str) -> Option<T> {
    args.windows(2)
        .find(|w| w[0] == name)
        .and_then(|w| w[1].parse().ok())
}

impl Settings {
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        let session: String = arg(args, "--session").unwrap_or_else(|| "kwin".into());
        let outputs: u32 = arg(args, "--outputs").unwrap_or(1);
        anyhow::ensure!(
            (1..=16).contains(&outputs),
            "--outputs must be between 1 and 16"
        );
        let audio_mode: String = arg(args, "--audio")
            .unwrap_or_else(|| if session == "kwin" { "pulse" } else { "tone" }.into());
        let audio = match audio_mode.as_str() {
            "off" => AudioMode::Off,
            "tone" => AudioMode::Tone,
            "pulse" => AudioMode::Pulse,
            other => anyhow::bail!("unknown --audio: {other}"),
        };
        let encoder: EncoderChoice = arg::<String>(args, "--encoder")
            .map_or(Ok(EncoderChoice::Auto), |v| v.parse())
            .map_err(|e: String| anyhow::anyhow!(e))?;
        let dmabuf = match arg::<String>(args, "--dmabuf").as_deref() {
            Some("on") => Some(true),
            Some("off") => Some(false),
            None | Some("auto") => None,
            Some(other) => anyhow::bail!("unknown --dmabuf: {other} (auto|on|off)"),
        };
        Ok(Self {
            width: arg(args, "--width").unwrap_or(1920),
            height: arg(args, "--height").unwrap_or(1080),
            outputs,
            fps: arg(args, "--fps").unwrap_or(30.0),
            audio,
            encoder,
            dmabuf,
            event: arg::<String>(args, "--loop").is_none_or(|v| v != "tick"),
            poll_ms: arg(args, "--poll-ms"),
            min_interval_ms: arg(args, "--min-interval-ms").unwrap_or(16.0),
            kwin_socket: arg(args, "--kwin-socket").unwrap_or_else(|| "rdsktp-0".into()),
            programs: args
                .windows(2)
                .filter(|w| w[0] == "--session-app")
                .map(|w| w[1].split_whitespace().map(str::to_owned).collect())
                .collect(),
            session,
        })
    }

    /// DMA-BUF capture: explicit choice, else what the encoder wants.
    #[cfg_attr(not(feature = "kwin"), allow(dead_code))]
    pub fn use_dmabuf(&self) -> bool {
        self.dmabuf.unwrap_or_else(|| self.encoder.wants_dmabuf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn defaults_are_a_full_hd_kwin_session_with_pulse_audio() {
        let c = Settings::parse(&[]).unwrap();
        assert_eq!(c.session, "kwin");
        assert_eq!((c.width, c.height, c.outputs), (1920, 1080, 1));
        assert_eq!(c.fps, 30.0);
        assert_eq!(c.audio, AudioMode::Pulse);
        assert_eq!(c.encoder, EncoderChoice::Auto);
        assert_eq!(c.dmabuf, None);
        assert!(c.event);
        assert_eq!(c.poll_ms, None);
        assert_eq!(c.min_interval_ms, 16.0);
        assert_eq!(c.kwin_socket, "rdsktp-0");
        assert!(c.programs.is_empty());
    }

    #[test]
    fn a_synthetic_session_defaults_to_a_test_tone() {
        let c = Settings::parse(&s(&["--session", "synthetic"])).unwrap();
        assert_eq!(c.audio, AudioMode::Tone);
        let c = Settings::parse(&s(&["--session", "synthetic", "--audio", "off"])).unwrap();
        assert_eq!(c.audio, AudioMode::Off);
    }

    #[test]
    fn every_option_is_read() {
        let c = Settings::parse(&s(&[
            "--width",
            "1280",
            "--height",
            "720",
            "--outputs",
            "3",
            "--fps",
            "60",
            "--encoder",
            "openh264",
            "--dmabuf",
            "on",
            "--loop",
            "tick",
            "--poll-ms",
            "8",
            "--min-interval-ms",
            "4",
            "--kwin-socket",
            "x",
            "--session-app",
            "konsole --hold",
            "--session-app",
            "kate",
        ]))
        .unwrap();
        assert_eq!((c.width, c.height, c.outputs, c.fps), (1280, 720, 3, 60.0));
        assert_eq!(c.encoder, EncoderChoice::OpenH264);
        assert_eq!(c.dmabuf, Some(true));
        assert!(!c.event);
        assert_eq!((c.poll_ms, c.min_interval_ms), (Some(8.0), 4.0));
        assert_eq!(c.kwin_socket, "x");
        assert_eq!(c.programs, vec![s(&["konsole", "--hold"]), s(&["kate"])]);
    }

    #[test]
    fn the_number_of_outputs_is_bounded() {
        assert!(Settings::parse(&s(&["--outputs", "0"])).is_err());
        assert!(Settings::parse(&s(&["--outputs", "17"])).is_err());
        assert!(Settings::parse(&s(&["--outputs", "16"])).is_ok());
        // Unparseable numbers fall back to the default instead of failing the start.
        assert_eq!(Settings::parse(&s(&["--outputs", "x"])).unwrap().outputs, 1);
    }

    #[test]
    fn unknown_choices_are_refused() {
        let e = Settings::parse(&s(&["--audio", "alsa"])).unwrap_err();
        assert!(e.to_string().contains("--audio: alsa"));
        let e = Settings::parse(&s(&["--encoder", "nvenc"])).unwrap_err();
        assert!(e.to_string().contains("nvenc"));
        let e = Settings::parse(&s(&["--dmabuf", "maybe"])).unwrap_err();
        assert!(e.to_string().contains("--dmabuf: maybe"));
    }

    #[test]
    fn dmabuf_follows_the_encoder_unless_forced() {
        let c = Settings::parse(&s(&["--encoder", "openh264"])).unwrap();
        assert!(!c.use_dmabuf());
        let c = Settings::parse(&s(&["--encoder", "openh264", "--dmabuf", "on"])).unwrap();
        assert!(c.use_dmabuf());
        let c = Settings::parse(&s(&["--encoder", "vaapi", "--dmabuf", "off"])).unwrap();
        assert!(!c.use_dmabuf());
        let c = Settings::parse(&s(&["--encoder", "vaapi", "--dmabuf", "auto"])).unwrap();
        assert!(c.use_dmabuf());
    }

    #[test]
    fn arg_ignores_a_trailing_option_without_value() {
        assert_eq!(arg::<u32>(&s(&["--width"]), "--width"), None);
        assert_eq!(arg::<u32>(&s(&["--width", "5"]), "--width"), Some(5));
    }
}
