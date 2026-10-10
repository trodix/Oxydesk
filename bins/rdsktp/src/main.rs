//! Serveur RDP RustyDesktop. Composition root : choisit les adaptateurs et les injecte dans `rdsktp-rdp`.
//! Modes : `--test-mode` (bureau synthétique, contrat du client de test) ou `--session kwin`
//! (session KWin virtuelle réelle, feature `kwin`, autologin pour l'utilisateur courant).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rdsktp_capture::SyntheticDesktop;
use rdsktp_encode::{OpenH264Config, OpenH264Encoder};
use rdsktp_core::layout::Layout;
use rdsktp_rdp::{Ports, ServerConfig};

const USAGE: &str = "usage: rdsktp (--test-mode | --login [--authd-socket PATH] | --session kwin [--kwin-socket NOM] [--session-app \"CMD ARGS\"]...) [--listen ADDR:PORT] [--state-dir DIR] [--outputs N] [--audio tone|pulse|off] [--width W] [--height H] [--fps F] [--nla-user U]\n\
  --test-mode   bureau synthétique\n\
  --login       login distant : PAM via rdsktp-authd, une session par utilisateur (TLS, sans NLA)\n\
  --session kwin  démarre kwin_wayland --virtual (+ programmes --session-app) et le capture\n\
  --state-dir   certificat TLS auto-signé généré s'il est absent (défaut : /var/lib/rdsktp)\n\
  --nla-user    active NLA ; mot de passe via la variable RDSKTP_PASSWORD uniquement";

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
        tracing::info!(dir = %dir.display(), "certificat TLS auto-signé généré");
    }
    Ok((cert, key))
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
    anyhow::ensure!((1..=16).contains(&outputs), "--outputs doit être entre 1 et 16");
    let layout = Layout::side_by_side(outputs, u32::from(width), u32::from(height));
    let fps: f32 = arg(&args, "--fps").unwrap_or(30.0);
    let state_dir: PathBuf = arg(&args, "--state-dir").unwrap_or_else(|| "/var/lib/rdsktp".into());
    let (tls_cert, tls_key) = ensure_cert(&state_dir)?;
    let nla = match arg::<String>(&args, "--nla-user") {
        Some(user) => Some((
            user,
            std::env::var("RDSKTP_PASSWORD")
                .map_err(|_| anyhow::anyhow!("RDSKTP_PASSWORD requis avec --nla-user"))?,
        )),
        None => None,
    };

    if login_mode {
        anyhow::ensure!(nla.is_none(), "--login est incompatible avec --nla-user (pas de NLA pour les comptes dynamiques)");
        let socket: PathBuf = arg(&args, "--authd-socket").unwrap_or_else(|| "/run/rdsktp/authd.sock".into());
        let broker = std::sync::Arc::new(rdsktp_remote::RemoteBroker::new(socket, outputs));
        let config = ServerConfig { addr, layout, fps, tls_cert, tls_key, nla };
        return tokio::select! {
            r = rdsktp_rdp::run_with_broker(config, broker) => r,
            _ = tokio::signal::ctrl_c() => { tracing::info!("arrêt"); Ok(()) }
        };
    }

    let encoders = (0..outputs)
        .map(|_| {
            OpenH264Encoder::new(&OpenH264Config { fps, ..Default::default() })
                .map(|e| Box::new(e) as Box<dyn rdsktp_core::ports::FrameEncoder>)
        })
        .collect::<Result<Vec<_>, _>>()?;
    #[cfg(feature = "kwin")]
    let mut _session = None;
    // Audio : tonalité de test en mode synthétique, capture PipeWire en session KWin.
    let audio: Option<std::sync::Arc<dyn rdsktp_core::ports::AudioProvider>> =
        match arg::<String>(&args, "--audio").unwrap_or_else(|| if kwin_mode { "pulse" } else { "tone" }.into()).as_str() {
            "off" => None,
            "tone" => Some(std::sync::Arc::new(rdsktp_audio::ToneProvider)),
            "pulse" => Some(std::sync::Arc::new(rdsktp_audio::PulseCapture)),
            other => anyhow::bail!("--audio inconnu : {other}"),
        };
    let (capture, input, clipboard): (
        Box<dyn rdsktp_core::ports::CaptureSource>,
        Box<dyn rdsktp_core::ports::InputSink>,
        Box<dyn rdsktp_core::ports::ClipboardBackend>,
    ) =
        if kwin_mode {
            #[cfg(feature = "kwin")]
            {
                use rdsktp_core::ports::SessionProvider;
                if audio.is_some() {
                    // Le puits doit exister avant les applications de la session (elles y jouent par défaut).
                    if let Err(e) = rdsktp_audio::PulseCapture::ensure_sink() {
                        tracing::warn!("audio indisponible : {e}");
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
                session.start().map_err(|e| anyhow::anyhow!("session KWin : {e}"))?;
                _session = Some(session);
                let sources = (0..outputs as usize)
                    .map(|i| {
                        rdsktp_capture::KwinCapture::start(Some(&socket), i)
                            .map(|c| Box::new(c) as Box<dyn rdsktp_core::ports::CaptureSource>)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let capture = rdsktp_capture::MultiCapture::new(sources);
                let input = rdsktp_input::KwinFakeInput::connect(Some(&socket))?;
                let clipboard = rdsktp_clipboard::WaylandClipboard::connect(Some(&socket))?;
                (Box::new(capture), Box::new(input), Box::new(clipboard))
            }
            #[cfg(not(feature = "kwin"))]
            anyhow::bail!("binaire compilé sans la feature `kwin`");
        } else {
            anyhow::ensure!(outputs == 1, "--outputs > 1 nécessite --session kwin");
            let desktop = SyntheticDesktop::new(u32::from(width), u32::from(height));
            // Mode test : le « bureau » synthétique renvoie en écho le texte reçu.
            (Box::new(desktop.clone()), Box::new(desktop.clone()), Box::new(desktop))
        };
    let ports = Ports {
        capture,
        encoders,
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
    };
    tokio::select! {
        r = rdsktp_rdp::run(config, ports) => r,
        _ = tokio::signal::ctrl_c() => { tracing::info!("arrêt"); Ok(()) }
    }
}
