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
ou : rdp-test-client --analyze-wav FILE [HZ MIN_WINDOWS NAME]\n\
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
        let mut val = |name: &str| it.next().ok_or(format!("{name} expects a value"));
        match a.as_str() {
            "--mock" => mock = true,
            "--no-nla" => no_nla = true,
            "--out" => out = Some(val("--out")?),
            "--host" => host = val(&a)?,
            "--port" => port = val(&a)?.parse().map_err(|_| "--port invalid")?,
            "--user" => user = val(&a)?,
            "--width" => cfg.width = val(&a)?.parse().map_err(|_| "--width invalid")?,
            "--outputs" => cfg.outputs = val(&a)?.parse().map_err(|_| "--outputs invalid")?,
            "--height" => cfg.height = val(&a)?.parse().map_err(|_| "--height invalid")?,
            "--key-samples" => {
                cfg.key_samples = val(&a)?.parse().map_err(|_| "--key-samples invalid")?
            }
            "--max-key-latency-ms" => {
                cfg.max_key_latency_ms = val(&a)?
                    .parse()
                    .map_err(|_| "--max-key-latency-ms invalid")?
            }
            "--timeout" => {
                cfg.timeout =
                    Duration::from_secs(val(&a)?.parse().map_err(|_| "--timeout invalid")?)
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        out: out.ok_or("--out is required")?,
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

/// `--analyze-wav FILE [HZ MIN_WINDOWS NAME]`: checks that a recording (QEMU `wav` backend) carries a tone
/// (default: the 440 Hz test tone, 3 half-second windows). Prints one JSON line like a report check;
/// exit code 0 iff the tone is present.
fn analyze_wav(path: &str, hz: f64, min_windows: usize, name: &str) -> ExitCode {
    let (passed, detail) = match std::fs::read(path) {
        Ok(bytes) => rdp_test_client::audio::analyze_wav(&bytes, hz, min_windows),
        Err(e) => (false, format!("cannot read {path}: {e}")),
    };
    println!(
        "{}",
        serde_json::json!({"name": name, "passed": passed, "detail": detail})
    );
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().map(String::as_str) == Some("--analyze-wav") {
        return match raw.as_slice() {
            [_, path] => analyze_wav(
                path,
                rdp_test_client::audio::TONE_HZ,
                3,
                "audio_tone_in_guest",
            ),
            [_, path, hz, min, name] => match (hz.parse(), min.parse()) {
                (Ok(hz), Ok(min)) => analyze_wav(path, hz, min, name),
                _ => ExitCode::from(2),
            },
            _ => ExitCode::from(2),
        };
    }
    let args = match parse(raw) {
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
        eprintln!("cannot write {}: {e}", path.display());
        return ExitCode::from(3);
    }
    ExitCode::SUCCESS
}
