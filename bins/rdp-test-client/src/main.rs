use std::process::ExitCode;
use std::time::Duration;

use rdp_test_client::mock::{MockFaults, MockSession};
use rdp_test_client::report::Report;
use rdp_test_client::scenarios::{run_all, Config};
use rdp_test_client::session::{Session, UnimplementedSession};

const USAGE: &str =
    "usage: rdp-test-client --out <dir> [--host H] [--port P] [--user U] [--width W] [--height H] \
[--key-samples N] [--max-key-latency-ms MS] [--timeout S] [--mock]\n\
mot de passe : variable RDP_TEST_PASSWORD uniquement";

struct Args {
    out: String,
    cfg: Config,
    mock: bool,
}

fn parse(args: Vec<String>) -> Result<Args, String> {
    let mut out = None;
    let mut cfg = Config::default();
    let mut mock = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} attend une valeur"));
        match a.as_str() {
            "--mock" => mock = true,
            "--out" => out = Some(val("--out")?),
            // Acceptés pour le backend IronRDP (M2) ; ignorés par le mock.
            "--host" | "--port" | "--user" => {
                val(&a)?;
            }
            "--width" => cfg.width = val(&a)?.parse().map_err(|_| "--width invalide")?,
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
        cfg,
        mock,
    })
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
        Box::new(UnimplementedSession)
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
