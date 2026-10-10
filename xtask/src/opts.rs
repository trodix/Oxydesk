use std::path::PathBuf;

pub const HELP: &str = "cargo xtask vm-test [options]
  --no-build            do not build the .deb
  --no-reset            do not start from a fresh overlay
  --keep                do not shut the VM down at the end
  --deb <file>          .deb to deploy (default: the most recent in target/debian/)
  --build-cmd <cmd>     build command (env XTASK_BUILD_CMD, default: testapp release + cargo deb -p rdsktp)
  --client-cmd <cmd>    client command, placeholders {host} {port} {out} (env XTASK_CLIENT_CMD)
  --outputs N                 number of screens (with --session kwin; default 1)
  --multi-resize        client scenario: dynamic number of monitors (Display Control; needs --outputs >= 2)
  --session <synthetic|kwin|login>  desktop source on the server side (default synthetic; kwin = real virtual KWin;
                                                          login = remote PAM login + agent, requires RDP_TEST_PASSWORD)
  --timeout <s>         client step timeout (default 600)
  --out <dir>           artifacts folder (default artifacts/vm-test)
  --help                this help
env: VM_RDP_PORT (default 3390)
exit code: 0 all pass, 1 failure, 2 arguments";

pub const DEFAULT_BUILD_CMD: &str = "cargo build --release -p rdsktp-testapp -p rdsktp-authd && cargo build --release -p rdsktp-agent --features kwin && cargo deb -p rdsktp";
pub const DEFAULT_CLIENT_CMD: &str =
    "cargo run --quiet --release -p rdp-test-client --features ironrdp -- --host {host} --port {port} --out {out}";

#[derive(Debug, Clone, PartialEq)]
pub struct Opts {
    pub no_build: bool,
    pub no_reset: bool,
    pub keep: bool,
    pub kwin: bool,
    /// Remote login (authd + per-user agent); implies `kwin`.
    pub login: bool,
    /// Number of screens (kwin session only).
    pub outputs: u32,
    pub multi_resize: bool,
    pub deb: Option<String>,
    pub build_cmd: String,
    pub client_cmd: String,
    pub client_timeout_s: u64,
    pub out: PathBuf,
    pub rdp_port: u16,
}

#[derive(Debug, PartialEq)]
pub enum Parsed {
    Help,
    Run(Opts),
}

pub fn parse(args: Vec<String>, env: impl Fn(&str) -> Option<String>) -> Result<Parsed, String> {
    let mut o = Opts {
        no_build: false,
        no_reset: false,
        keep: false,
        kwin: false,
        login: false,
        outputs: 1,
        multi_resize: false,
        deb: None,
        build_cmd: env("XTASK_BUILD_CMD").unwrap_or_else(|| DEFAULT_BUILD_CMD.into()),
        client_cmd: env("XTASK_CLIENT_CMD").unwrap_or_else(|| DEFAULT_CLIENT_CMD.into()),
        client_timeout_s: 600,
        out: PathBuf::from("artifacts/vm-test"),
        rdp_port: match env("VM_RDP_PORT") {
            Some(p) => p.parse().map_err(|_| format!("invalid VM_RDP_PORT: {p}"))?,
            None => 3390,
        },
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = |n: &str| it.next().ok_or(format!("{n} expects a value"));
        match a.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--no-build" => o.no_build = true,
            "--no-reset" => o.no_reset = true,
            "--keep" => o.keep = true,
            "--session" => {
                o.kwin = match val("--session")?.as_str() {
                    "kwin" => true,
                    "login" => {
                        o.login = true;
                        true
                    }
                    "synthetic" => false,
                    other => return Err(format!("unknown --session: {other}")),
                }
            }
            "--outputs" => {
                o.outputs = val("--outputs")?
                    .parse()
                    .ok()
                    .filter(|n| (1..=16).contains(n))
                    .ok_or("--outputs invalid (1 to 16)")?
            }
            "--multi-resize" => o.multi_resize = true,
            "--deb" => o.deb = Some(val("--deb")?),
            "--build-cmd" => o.build_cmd = val("--build-cmd")?,
            "--client-cmd" => o.client_cmd = val("--client-cmd")?,
            "--timeout" => {
                o.client_timeout_s = val("--timeout")?.parse().map_err(|_| "--timeout invalid")?
            }
            "--out" => o.out = PathBuf::from(val("--out")?),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Parsed::Run(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str], env: &[(&str, &str)]) -> Result<Parsed, String> {
        let env: Vec<_> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        parse(args.iter().map(|s| s.to_string()).collect(), move |k| {
            env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        })
    }

    #[test]
    fn defaults_and_env() {
        let Parsed::Run(o) =
            p(&[], &[("VM_RDP_PORT", "4000"), ("XTASK_BUILD_CMD", "true")]).unwrap()
        else {
            panic!()
        };
        assert_eq!(o.rdp_port, 4000);
        assert_eq!(o.build_cmd, "true");
        assert_eq!(o.client_timeout_s, 600);
    }

    #[test]
    fn flags_override_env() {
        let Parsed::Run(o) = p(
            &["--build-cmd", "x", "--keep", "--no-build"],
            &[("XTASK_BUILD_CMD", "y")],
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(o.build_cmd, "x");
        assert!(o.keep && o.no_build);
    }

    #[test]
    fn login_session_implies_kwin() {
        let Parsed::Run(o) = p(&["--session", "login"], &[]).unwrap() else {
            panic!()
        };
        assert!(o.login && o.kwin);
    }

    #[test]
    fn session_flag() {
        let Parsed::Run(o) = p(&["--session", "kwin"], &[]).unwrap() else {
            panic!()
        };
        assert!(o.kwin);
        let Parsed::Run(o) = p(&[], &[]).unwrap() else {
            panic!()
        };
        assert!(!o.kwin);
        assert!(p(&["--session", "x"], &[]).is_err());
    }

    #[test]
    fn rejects_bad_args() {
        assert!(p(&["--nope"], &[]).is_err());
        assert!(p(&["--timeout"], &[]).is_err());
        assert!(p(&["--timeout", "abc"], &[]).is_err());
        assert_eq!(p(&["--help"], &[]).unwrap(), Parsed::Help);
    }
}
