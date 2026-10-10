use std::process::ExitCode;
use std::time::Duration;

use rdp_test_client::mock::{MockFaults, MockSession};
use rdp_test_client::report::Report;
use rdp_test_client::scenarios::{run_all, Config};
use rdp_test_client::session::Session;
#[cfg(not(feature = "ironrdp"))]
use rdp_test_client::session::UnimplementedSession;

const USAGE: &str =
    "usage: rdp-test-client --out <dir> [--host H] [--port P] [--user U] [--width W] [--height H] [--outputs N] \
[--no-nla] [--key-samples N] [--max-key-latency-ms MS] [--timeout S] [--mock]\n\
mot de passe : variable RDP_TEST_PASSWORD uniquement";

#[cfg_attr(not(feature = "ironrdp"), allow(dead_code))]
struct Args {
    out: String,
    host: String,
    port: u16,
    user: String,
    cfg: Config,
    mock: bool,
    no_nla: bool,
}

fn parse(args: Vec<String>) -> Result<Args, String> {
    let mut out = None;
    let mut cfg = Config::default();
    let mut mock = false;
    let mut no_nla = false;
    let (mut host, mut port, mut user) = ("127.0.0.1".to_string(), 3389u16, "test".to_string());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} attend une valeur"));
        match a.as_str() {
            "--mock" => mock = true,
            "--no-nla" => no_nla = true,
            "--out" => out = Some(val("--out")?),
            "--host" => host = val(&a)?,
            "--port" => port = val(&a)?.parse().map_err(|_| "--port invalide")?,
            "--user" => user = val(&a)?,
            "--width" => cfg.width = val(&a)?.parse().map_err(|_| "--width invalide")?,
            "--outputs" => cfg.outputs = val(&a)?.parse().map_err(|_| "--outputs invalide")?,
            "--height" => cfg.height = val(&a)?.parse().map_err(|_| "--height invalide")?,
            "--key-samples" => {
                cfg.key_samples = val(&a)?.parse().map_err(|_| "--key-samples invalide")?
            }
            "--max-key-latency-ms" => {
                cfg.max_key_latency_ms = val(&a)?
                    .parse()
                    .map_err(|_| "--max-key-latency-ms invalide")?
            }
            "--timeout" => {
                cfg.timeout =
                    Duration::from_secs(val(&a)?.parse().map_err(|_| "--timeout invalide")?)
            }
            other => return Err(format!("argument inconnu : {other}")),
        }
    }
    Ok(Args {
        out: out.ok_or("--out est obligatoire")?,
        host,
        port,
        user,
        cfg,
        mock,
        no_nla,
    })
}

#[cfg(feature = "ironrdp")]
fn real_session(args: &Args) -> Box<dyn Session> {
    use rdp_test_client::ironrdp_backend::{IronRdpSession, Options};
    Box::new(IronRdpSession::new(Options {
        host: args.host.clone(),
        port: args.port,
        user: args.user.clone(),
        password: std::env::var("RDP_TEST_PASSWORD")
            .ok()
            .filter(|p| !p.is_empty()),
        no_nla: args.no_nla,
        width: (args.cfg.width * args.cfg.outputs) as u16,
        height: args.cfg.height as u16,
    }))
}

#[cfg(not(feature = "ironrdp"))]
fn real_session(_: &Args) -> Box<dyn Session> {
    Box::new(UnimplementedSession)
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1).collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let mut session: Box<dyn Session> = if args.mock {
        Box::new(MockSession::new(MockFaults::default()))
    } else {
        real_session(&args)
    };
    let mut report = Report::default();
    run_all(session.as_mut(), &args.cfg, &mut report);

    let path = std::path::Path::new(&args.out).join("client.json");
    let written =
        std::fs::create_dir_all(&args.out).and_then(|_| std::fs::write(&path, report.to_json()));
    if let Err(e) = written {
        eprintln!("écriture de {} impossible : {e}", path.display());
        return ExitCode::from(3);
    }
    ExitCode::SUCCESS
}
