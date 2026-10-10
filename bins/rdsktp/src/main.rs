//! Serveur RDP RustyDesktop. Composition root : choisit les adaptateurs et les injecte dans `rdsktp-rdp`.
//! Mode actuel : `--test-mode` (bureau synthétique, voir le contrat du client de test).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rdsktp_capture::SyntheticDesktop;
use rdsktp_encode::{OpenH264Config, OpenH264Encoder};
use rdsktp_rdp::{Ports, ServerConfig};

const USAGE: &str = "usage: rdsktp --test-mode [--listen ADDR:PORT] [--state-dir DIR] [--width W] [--height H] [--fps F] [--nla-user U]\n\
  --test-mode   bureau synthétique (seul mode disponible pour l'instant)\n\
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
    if args.iter().any(|a| a == "--help" || a == "-h") || !args.iter().any(|a| a == "--test-mode") {
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

    let desktop = SyntheticDesktop::new(u32::from(width), u32::from(height));
    let encoder = OpenH264Encoder::new(&OpenH264Config {
        fps,
        ..Default::default()
    })?;
    let ports = Ports {
        capture: Box::new(desktop.clone()),
        encoder: Box::new(encoder),
        input: Box::new(desktop.clone()),
        clipboard: Box::new(desktop),
    };
    let config = ServerConfig {
        addr,
        width,
        height,
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
