//! RustyDesktop RDP server. Composition root: chooses the adapters and injects them into `rdsktp-rdp`.
//! Modes: `--test-mode` (synthetic desktop, test client contract) or `--session kwin`
//! (real virtual KWin session, `kwin` feature, autologin for the current user).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rdsktp_capture::SyntheticDesktop;
use rdsktp_core::layout::Layout;
use rdsktp_encode::{AutoEncoder, EncoderChoice};
use rdsktp_rdp::{Ports, ServerConfig};

const USAGE: &str = "usage: rdsktp (--test-mode | --login [--authd-socket PATH] | --session kwin [--kwin-socket NAME] [--session-app \"CMD ARGS\"]...) [--listen ADDR:PORT]
  --test-mode   synthetic desktop\n\
  --login       remote login: PAM through rdsktp-authd, one session per user (TLS, no NLA)\n\
  --session kwin  starts kwin_wayland --virtual (+ --session-app programs) and captures it\n\
  --state-dir   self-signed TLS certificate generated if absent (default: /var/lib/rdsktp)\n\
  --max-login-failures N   --login-lockout-secs S   --max-user-failures N   login mode: failed attempts before an address (default 5) or an account (default 10) is blocked, and for how long (default 300)\n\
  --nla-user    enables NLA; password only through the RDSKTP_PASSWORD variable
  --encoder     auto (default) | vaapi | openh264; auto uses the GPU when VA-API is usable
  --dmabuf      auto (default) | on | off: offer DMA-BUF capture buffers (auto: only with a GPU encoder)";

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let kwin_mode = arg::<String>(&args, "--session").as_deref() == Some("kwin");
    let login_mode = args.iter().any(|a| a == "--login");
    if args.iter().any(|a| a == "--help" || a == "-h")
        || !(kwin_mode || login_mode || args.iter().any(|a| a == "--test-mode"))
    {
        eprintln!("{USAGE}");
        std::process::exit(if args.iter().any(|a| a == "--help") {
            0
        } else {
            2
        });
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
            Box::new(AutoEncoder::new(choice, fps, 8_000_000))
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
    let ports = Ports {
        capture,
        encoders,
        make_encoder: Some(Box::new(move |_| {
            Box::new(AutoEncoder::new(choice, fps, 8_000_000))
                as Box<dyn rdsktp_core::ports::FrameEncoder>
        })),
        input,
        clipboard,
        audio,
    };
    let config = ServerConfig {
        addr,
        layout,
        fps,
        tls_cert,
        tls_key,
        nla,
        throttle,
    };
    tokio::select! {
        r = rdsktp_rdp::run(config, ports) => r,
        _ = shutdown_signal() => { tracing::info!("shutdown"); Ok(()) }
    }
}
