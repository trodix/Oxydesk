//! RustyDesktop RDP server. Composition root: chooses the adapters and injects them into `rdsktp-rdp`.
//! Modes: `--test-mode` (synthetic desktop, test client contract) or `--session kwin`
//! (real virtual KWin session, `kwin` feature, autologin for the current user).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rdsktp_capture::SyntheticDesktop;
use rdsktp_core::layout::Layout;
use rdsktp_core::DEFAULT_BITRATE_BPS;
use rdsktp_encode::{AutoEncoder, EncoderChoice};
use rdsktp_rdp::{Ports, ServerConfig};

const USAGE: &str = "usage: rdsktp (--test-mode | --login [--authd-socket PATH] | --session kwin [--kwin-socket NAME] [--session-app \"CMD ARGS\"]...) [--listen ADDR:PORT]
  --config FILE   configuration file (default: /etc/rdsktp/rdsktp.conf if present, plus FILE.d/*.conf); the command line wins over it\n\
  --test-mode   synthetic desktop\n\
  --login       remote login: PAM through rdsktp-authd, one session per user (TLS, no NLA)\n\
  --session kwin  starts kwin_wayland --virtual (+ --session-app programs) and captures it\n\
  --state-dir   self-signed TLS certificate generated if absent (default: /var/lib/rdsktp)\n\
  --max-login-failures N   --login-lockout-secs S   --max-user-failures N   login mode: failed attempts before an address (default 5) or an account (default 10) is blocked, and for how long (default 300)\n\
  --nla-user    enables NLA; password only through the RDSKTP_PASSWORD variable
  --encoder     auto (default) | vaapi | openh264; auto uses the GPU when VA-API is usable
  --dmabuf      auto (default) | on | off: offer DMA-BUF capture buffers (auto: only with a GPU encoder)
  --drives-dir DIR   drive redirection: the client's drives are mounted (FUSE) under DIR/<name> (default with --session kwin: $XDG_RUNTIME_DIR/rdsktp-drives; `off` disables)
  --printers DIR     printer redirection: one socket per printer of the client in DIR (default with --session kwin: $XDG_RUNTIME_DIR/rdsktp-printers; `off` disables)
  --adaptive-bitrate on|off  bitrate and frames in flight follow what the client's link carries (default: on)
  --printers-cups on|off  also make a CUPS queue per printer (default: on if CUPS answers)
  --reconnect-timeout SECS  login mode: a client that loses its network resumes its session by itself for this long, without
                          typing the password again (default: 10; 0 turns it off)
  --dead-peer-timeout SECS  a client that stops answering is dropped after this long, which frees the server for its
                          reconnection (default: 8; 0 leaves it to the kernel, minutes)
  --printer-mode SPEC     format of the jobs of the printers: auto (default), raw, raw:ps|pcl|text, xps; per printer with
                          PATTERN=MODE (`*` matches anything), comma-separated: xps,HP*=raw:pcl. auto = raw when the
                          driver name states a language, XPS otherwise (if the client takes it), else the printer is refused";

/// Default configuration file; its drop-ins are in `<file>.d/*.conf`.
const DEFAULT_CONFIG: &str = "/etc/rdsktp/rdsktp.conf";

/// The command line completed with the configuration file's settings it does not give (command line first, so it wins).
fn with_config_file(cli: &[String]) -> anyhow::Result<Vec<String>> {
    let explicit: Option<PathBuf> = arg(cli, "--config");
    let path = explicit.clone().unwrap_or_else(|| DEFAULT_CONFIG.into());
    let mut layers = Vec::new();
    match std::fs::read_to_string(&path) {
        Ok(text) => layers.push(rdsktp_core::config::parse(
            &text,
            &path.display().to_string(),
        )?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {}
        Err(e) => anyhow::bail!("configuration file {}: {e}", path.display()),
    }
    let mut dropins: Vec<PathBuf> = match std::fs::read_dir(format!("{}.d", path.display())) {
        Ok(dir) => dir
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "conf"))
            .collect(),
        Err(_) => Vec::new(),
    };
    dropins.sort();
    for p in dropins {
        let text = std::fs::read_to_string(&p)
            .map_err(|e| anyhow::anyhow!("configuration file {}: {e}", p.display()))?;
        layers.push(rdsktp_core::config::parse(&text, &p.display().to_string())?);
    }
    if !layers.is_empty() {
        tracing::info!(file = %path.display(), "configuration file loaded");
    }
    Ok(rdsktp_core::config::merge_args(
        cli,
        &rdsktp_core::config::combine(&layers),
    )?)
}

fn arg<T: std::str::FromStr>(args: &[String], name: &str) -> Option<T> {
    args.windows(2)
        .find(|w| w[0] == name)
        .and_then(|w| w[1].parse().ok())
}

fn ensure_cert(dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let (cert, key) = (dir.join("tls-cert.pem"), dir.join("tls-key.pem"));
    if !cert.exists() || !key.exists() {
        std::fs::create_dir_all(dir)?;
        let generated = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "rdsktp".to_string(),
        ])?;
        std::fs::write(&cert, generated.cert.pem())?;
        std::fs::write(&key, generated.signing_key.serialize_pem())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
        }
        tracing::info!(dir = %dir.display(), "self-signed TLS certificate generated");
    }
    Ok((cert, key))
}

/// Ctrl-C or SIGTERM (service stop): returning lets the KWin session and its applications be stopped.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli: Vec<String> = std::env::args().skip(1).collect();
    if cli.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("{USAGE}");
        std::process::exit(0);
    }
    let args = with_config_file(&cli)?;
    let kwin_mode = arg::<String>(&args, "--session").as_deref() == Some("kwin");
    let login_mode = args.iter().any(|a| a == "--login");
    if !(kwin_mode || login_mode || args.iter().any(|a| a == "--test-mode")) {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let addr: SocketAddr =
        arg(&args, "--listen").unwrap_or_else(|| "0.0.0.0:3389".parse().expect("addr"));
    let (width, height): (u16, u16) = (
        arg(&args, "--width").unwrap_or(1920),
        arg(&args, "--height").unwrap_or(1080),
    );
    let outputs: u32 = arg(&args, "--outputs").unwrap_or(1);
    anyhow::ensure!(
        (1..=16).contains(&outputs),
        "--outputs must be between 1 and 16"
    );
    let layout = Layout::side_by_side(outputs, u32::from(width), u32::from(height));
    let fps: f32 = arg(&args, "--fps").unwrap_or(30.0);
    let state_dir: PathBuf = arg(&args, "--state-dir").unwrap_or_else(|| "/var/lib/rdsktp".into());
    let (tls_cert, tls_key) = ensure_cert(&state_dir)?;
    let nla = match arg::<String>(&args, "--nla-user") {
        Some(user) => Some((
            user,
            std::env::var("RDSKTP_PASSWORD")
                .map_err(|_| anyhow::anyhow!("RDSKTP_PASSWORD required with --nla-user"))?,
        )),
        None => None,
    };

    let defaults = rdsktp_core::ThrottleConfig::default();
    let throttle = rdsktp_core::ThrottleConfig {
        max_ip_failures: arg(&args, "--max-login-failures").unwrap_or(defaults.max_ip_failures),
        max_user_failures: arg(&args, "--max-user-failures").unwrap_or(defaults.max_user_failures),
        window: defaults.window,
        lockout: std::time::Duration::from_secs(
            arg(&args, "--login-lockout-secs").unwrap_or(defaults.lockout.as_secs()),
        ),
    };
    anyhow::ensure!(
        throttle.max_ip_failures >= 1 && throttle.max_user_failures >= 1,
        "login failure limits must be at least 1"
    );
    if login_mode {
        anyhow::ensure!(
            nla.is_none(),
            "--login is incompatible with --nla-user (no NLA for dynamic accounts)"
        );
        let socket: PathBuf =
            arg(&args, "--authd-socket").unwrap_or_else(|| "/run/rdsktp/authd.sock".into());
        let broker = std::sync::Arc::new(rdsktp_remote::RemoteBroker::new(socket, outputs));
        let config = ServerConfig {
            addr,
            layout,
            fps,
            tls_cert,
            tls_key,
            nla,
            throttle,
            printer_modes: printer_modes(&args)?,
            adaptive_bitrate: adaptive_bitrate(&args)?,
            reconnect_timeout: reconnect_timeout(&args)?,
            dead_peer_timeout: dead_peer_timeout(&args),
        };
        return tokio::select! {
            r = rdsktp_rdp::run_with_broker(config, broker) => r,
            _ = shutdown_signal() => { tracing::info!("shutdown"); Ok(()) }
        };
    }

    let choice: EncoderChoice = match arg::<String>(&args, "--encoder") {
        Some(v) => v.parse().map_err(|e: String| anyhow::anyhow!(e))?,
        None => EncoderChoice::Auto,
    };
    #[cfg_attr(not(feature = "kwin"), allow(unused_variables))]
    let dmabuf = match arg::<String>(&args, "--dmabuf").as_deref() {
        Some("on") => true,
        Some("off") => false,
        None | Some("auto") => choice.wants_dmabuf(),
        Some(other) => anyhow::bail!("unknown --dmabuf: {other} (auto|on|off)"),
    };
    let encoders = (0..outputs)
        .map(|_| {
            Box::new(AutoEncoder::new(choice, fps, DEFAULT_BITRATE_BPS))
                as Box<dyn rdsktp_core::ports::FrameEncoder>
        })
        .collect::<Vec<_>>();
    #[cfg(feature = "kwin")]
    let mut _session = None;
    // Audio: test tone in synthetic mode, PipeWire capture in KWin session.
    let audio: Option<std::sync::Arc<dyn rdsktp_core::ports::AudioProvider>> =
        match arg::<String>(&args, "--audio")
            .unwrap_or_else(|| if kwin_mode { "pulse" } else { "tone" }.into())
            .as_str()
        {
            "off" => None,
            "tone" => Some(std::sync::Arc::new(rdsktp_audio::ToneProvider)),
            "pulse" => Some(std::sync::Arc::new(rdsktp_audio::PulseCapture)),
            other => anyhow::bail!("unknown --audio: {other}"),
        };
    let (capture, input, clipboard): (
        Box<dyn rdsktp_core::ports::CaptureSource>,
        Box<dyn rdsktp_core::ports::InputSink>,
        Box<dyn rdsktp_core::ports::ClipboardBackend>,
    ) = if kwin_mode {
        #[cfg(feature = "kwin")]
        {
            use rdsktp_core::ports::SessionProvider;
            if audio.is_some() {
                // The sink must exist before the session applications (they play to it by default).
                if let Err(e) = rdsktp_audio::PulseCapture::ensure_sink() {
                    tracing::warn!("audio unavailable: {e}");
                }
            }
            let socket: String = arg(&args, "--kwin-socket").unwrap_or_else(|| "rdsktp-0".into());
            let programs = args
                .windows(2)
                .filter(|w| w[0] == "--session-app")
                .map(|w| w[1].split_whitespace().map(str::to_owned).collect())
                .collect();
            let mut session = rdsktp_session::KwinSession::new(rdsktp_session::KwinSessionConfig {
                socket: socket.clone(),
                width: u32::from(width),
                height: u32::from(height),
                outputs,
                programs,
            });
            session
                .start()
                .map_err(|e| anyhow::anyhow!("KWin session: {e}"))?;
            _session = Some(session);
            let sources = (0..outputs as usize)
                .map(|i| {
                    rdsktp_capture::KwinCapture::start_with(Some(&socket), i, dmabuf)
                        .map(|c| Box::new(c) as Box<dyn rdsktp_core::ports::CaptureSource>)
                })
                .collect::<Result<Vec<_>, _>>()?;
            // More outputs than the session started with (KWin 6.7): virtual outputs, which vanish with
            // their capture; the session's own outputs are disabled and enabled again.
            let capture = rdsktp_capture::MultiCapture::new(sources).with_factory(
                rdsktp_capture::source_factory(socket.clone(), outputs as usize, dmabuf),
            );
            let input = rdsktp_input::KwinFakeInput::connect(Some(&socket))?;
            let clipboard: Box<dyn rdsktp_core::ports::ClipboardBackend> =
                match rdsktp_clipboard::WaylandClipboard::connect(Some(&socket)) {
                    Ok(c) => Box::new(c),
                    Err(e) => {
                        tracing::warn!("clipboard sharing disabled: {e}");
                        Box::new(rdsktp_clipboard::UnavailableClipboard)
                    }
                };
            // KWin that cannot resize its desktop (6.3): the client's size is obtained by scaling.
            let (capture, input) = rdsktp_scaling::wrap(Box::new(capture), Box::new(input));
            (Box::new(capture), Box::new(input), clipboard)
        }
        #[cfg(not(feature = "kwin"))]
        anyhow::bail!("binary built without the `kwin` feature");
    } else {
        let desktop = SyntheticDesktop::with_outputs(outputs, u32::from(width), u32::from(height));
        // Test mode: the synthetic "desktop" echoes back the received text.
        (
            Box::new(desktop.clone()),
            Box::new(desktop.clone()),
            Box::new(desktop),
        )
    };
    let drives_dir: Option<PathBuf> = match arg::<String>(&args, "--drives-dir").as_deref() {
        Some("off") => None,
        Some(dir) => Some(dir.into()),
        None if kwin_mode => {
            std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("rdsktp-drives"))
        }
        None => None,
    };
    let drives: Option<std::sync::Arc<dyn rdsktp_core::DriveHost>> = match drives_dir {
        Some(dir) => match rdsktp_drivefs::DriveMounts::new(dir.clone()) {
            Ok(m) => {
                tracing::info!(dir = %dir.display(), "drive redirection enabled");
                Some(std::sync::Arc::new(m))
            }
            Err(e) => {
                tracing::warn!("drive redirection disabled: {e}");
                None
            }
        },
        None => None,
    };
    let printers = printers_host(&args, kwin_mode);
    let ports = Ports {
        capture,
        encoders,
        make_encoder: Some(Box::new(move |_| {
            Box::new(AutoEncoder::new(choice, fps, DEFAULT_BITRATE_BPS))
                as Box<dyn rdsktp_core::ports::FrameEncoder>
        })),
        input,
        clipboard,
        audio,
        drives,
        printers,
    };
    let config = ServerConfig {
        addr,
        layout,
        fps,
        tls_cert,
        tls_key,
        nla,
        throttle,
        printer_modes: printer_modes(&args)?,
        adaptive_bitrate: adaptive_bitrate(&args)?,
        reconnect_timeout: reconnect_timeout(&args)?,
        dead_peer_timeout: dead_peer_timeout(&args),
    };
    tokio::select! {
        r = rdsktp_rdp::run(config, ports) => r,
        _ = shutdown_signal() => { tracing::info!("shutdown"); Ok(()) }
    }
}

/// `--dead-peer-timeout SECS` (default 8, 0 = left to the kernel).
fn dead_peer_timeout(args: &[String]) -> std::time::Duration {
    std::time::Duration::from_secs(arg::<u64>(args, "--dead-peer-timeout").unwrap_or(8))
}

/// `--reconnect-timeout SECS` (default 10, 0 = never).
fn reconnect_timeout(args: &[String]) -> anyhow::Result<std::time::Duration> {
    Ok(std::time::Duration::from_secs(
        arg::<u64>(args, "--reconnect-timeout").unwrap_or(10),
    ))
}

/// `--adaptive-bitrate on|off` (default on).
fn adaptive_bitrate(args: &[String]) -> anyhow::Result<bool> {
    match arg::<String>(args, "--adaptive-bitrate").as_deref() {
        None | Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => anyhow::bail!("--adaptive-bitrate: '{other}' (on|off)"),
    }
}

/// The mode of each printer, per `--printer-mode`.
fn printer_modes(args: &[String]) -> anyhow::Result<rdsktp_core::PrinterModes> {
    match arg::<String>(args, "--printer-mode") {
        Some(spec) => rdsktp_core::PrinterModes::parse(&spec)
            .map_err(|e| anyhow::anyhow!("--printer-mode: {e}")),
        None => Ok(rdsktp_core::PrinterModes::default()),
    }
}

/// The host of the printers of the client, per `--printers` and `--printers-cups`.
fn printers_host(
    args: &[String],
    kwin_mode: bool,
) -> Option<std::sync::Arc<dyn rdsktp_core::PrinterHost>> {
    let dir: PathBuf = match arg::<String>(args, "--printers").as_deref() {
        Some("off") => return None,
        Some(dir) => dir.into(),
        None if kwin_mode => std::env::var_os("XDG_RUNTIME_DIR")
            .map(|d| PathBuf::from(d).join(rdsktp_spool::SOCKET_DIR))?,
        None => return None,
    };
    let cups = match arg::<String>(args, "--printers-cups").as_deref() {
        Some("off") => None,
        _ => rdsktp_spool::Cups::detect(&rdsktp_spool::current_user()),
    };
    match rdsktp_spool::PrinterSpool::new(dir.clone(), cups) {
        Ok(spool) => {
            let spool = spool.with_xps(rdsktp_spool::GhostscriptXps::detect().map(|gs| {
                std::sync::Arc::new(gs) as std::sync::Arc<dyn rdsktp_core::XpsConverter>
            }));
            tracing::info!(dir = %dir.display(), "printer redirection enabled");
            Some(std::sync::Arc::new(spool))
        }
        Err(e) => {
            tracing::warn!("printer redirection disabled: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rdsktp-main-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn options_are_parsed_by_name_and_type() {
        let a = args(&[
            "--fps",
            "24.5",
            "--width",
            "abc",
            "--listen",
            "127.0.0.1:3390",
        ]);
        assert_eq!(arg::<f32>(&a, "--fps"), Some(24.5));
        // A value of the wrong type is absent (the default applies), not a crash.
        assert_eq!(arg::<u16>(&a, "--width"), None);
        assert_eq!(
            arg::<SocketAddr>(&a, "--listen"),
            Some("127.0.0.1:3390".parse().unwrap())
        );
        assert_eq!(arg::<String>(&a, "--missing"), None);
    }

    #[test]
    fn the_timeouts_have_documented_defaults_and_can_be_switched_off() {
        assert_eq!(reconnect_timeout(&[]).unwrap(), Duration::from_secs(10));
        assert_eq!(dead_peer_timeout(&[]), Duration::from_secs(8));
        let a = args(&["--reconnect-timeout", "0", "--dead-peer-timeout", "30"]);
        assert_eq!(reconnect_timeout(&a).unwrap(), Duration::ZERO);
        assert_eq!(dead_peer_timeout(&a), Duration::from_secs(30));
    }

    #[test]
    fn adaptive_bitrate_is_on_unless_switched_off_and_rejects_other_words() {
        assert!(adaptive_bitrate(&[]).unwrap());
        assert!(adaptive_bitrate(&args(&["--adaptive-bitrate", "on"])).unwrap());
        assert!(!adaptive_bitrate(&args(&["--adaptive-bitrate", "off"])).unwrap());
        let e = adaptive_bitrate(&args(&["--adaptive-bitrate", "maybe"])).unwrap_err();
        assert!(e.to_string().contains("maybe"));
    }

    #[test]
    fn printer_modes_come_from_the_option_and_a_bad_spec_names_the_option() {
        assert!(printer_modes(&[]).is_ok());
        assert!(printer_modes(&args(&["--printer-mode", "xps,HP*=raw:pcl"])).is_ok());
        let e = printer_modes(&args(&["--printer-mode", "nonsense"])).unwrap_err();
        assert!(e.to_string().starts_with("--printer-mode:"));
    }

    #[test]
    fn printers_are_not_redirected_when_switched_off_or_not_asked_for() {
        assert!(printers_host(&args(&["--printers", "off"]), true).is_none());
        // Outside a KWin session nothing is redirected unless a directory is given.
        assert!(printers_host(&[], false).is_none());
    }

    #[test]
    fn the_certificate_is_generated_once_with_a_private_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("cert");
        let (cert, key) = ensure_cert(&dir.join("state")).unwrap();
        assert!(std::fs::read_to_string(&cert)
            .unwrap()
            .contains("BEGIN CERTIFICATE"));
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // A second start keeps the identity the clients have already trusted.
        let before = std::fs::read(&cert).unwrap();
        ensure_cert(&dir.join("state")).unwrap();
        assert_eq!(std::fs::read(&cert).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_configuration_file_completes_the_command_line_which_wins() {
        let dir = temp("conf");
        let file = dir.join("rdsktp.conf");
        std::fs::write(&file, "fps = 24\nwidth = 1280\n").unwrap();
        std::fs::create_dir_all(dir.join("rdsktp.conf.d")).unwrap();
        std::fs::write(
            dir.join("rdsktp.conf.d/10-b.conf"),
            "width = 1600\nheight = 900\n",
        )
        .unwrap();
        std::fs::write(dir.join("rdsktp.conf.d/ignored.txt"), "width = 1\n").unwrap();
        let cli = args(&["--config", file.to_str().unwrap(), "--fps", "60"]);
        let merged = with_config_file(&cli).unwrap();
        assert_eq!(arg::<u32>(&merged, "--fps"), Some(60), "command line first");
        assert_eq!(
            arg::<u32>(&merged, "--width"),
            Some(1600),
            "drop-in overrides the file"
        );
        assert_eq!(arg::<u32>(&merged, "--height"), Some(900));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_explicit_file_or_a_bad_key_stops_the_start() {
        let dir = temp("conf-bad");
        let missing = dir.join("absent.conf");
        let e = with_config_file(&args(&["--config", missing.to_str().unwrap()])).unwrap_err();
        assert!(e.to_string().contains("absent.conf"));
        let bad = dir.join("bad.conf");
        std::fs::write(&bad, "lisen = 1\n").unwrap();
        let e = with_config_file(&args(&["--config", bad.to_str().unwrap()])).unwrap_err();
        assert!(e.to_string().contains("unknown key"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
