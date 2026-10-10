use std::path::PathBuf;

pub const HELP: &str = "cargo xtask vm-test [options]
  --no-build            ne pas construire le .deb
  --no-reset            ne pas repartir d'un overlay neuf
  --keep                ne pas éteindre la VM à la fin
  --deb <fichier>       .deb à déployer (défaut : le plus récent de target/debian/)
  --build-cmd <cmd>     commande de build (env XTASK_BUILD_CMD, défaut : testapp release + cargo deb -p rdsktp)
  --client-cmd <cmd>    commande client, placeholders {host} {port} {out} (env XTASK_CLIENT_CMD)
  --outputs N                 nombre d'écrans (avec --session kwin ; défaut 1)
  --session <synthetic|kwin|login>  source du bureau côté serveur (défaut synthetic ; kwin = vrai KWin virtuel ;
                              login = login distant PAM + agent, exige RDP_TEST_PASSWORD)
  --timeout <s>         timeout de l'étape client (défaut 600)
  --out <dir>           dossier des artefacts (défaut artifacts/vm-test)
  --help                cette aide
env : VM_RDP_PORT (défaut 3390)
code de sortie : 0 tout passe, 1 échec, 2 arguments";

pub const DEFAULT_BUILD_CMD: &str = "cargo build --release -p rdsktp-testapp -p rdsktp-authd && cargo build --release -p rdsktp-agent --features kwin && cargo deb -p rdsktp";
pub const DEFAULT_CLIENT_CMD: &str =
    "cargo run --quiet --release -p rdp-test-client --features ironrdp -- --host {host} --port {port} --out {out}";

#[derive(Debug, Clone, PartialEq)]
pub struct Opts {
    pub no_build: bool,
    pub no_reset: bool,
    pub keep: bool,
    pub kwin: bool,
    /// Login distant (authd + agent par utilisateur) ; implique `kwin`.
    pub login: bool,
    /// Nombre d'écrans (session kwin uniquement).
    pub outputs: u32,
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
        deb: None,
        build_cmd: env("XTASK_BUILD_CMD").unwrap_or_else(|| DEFAULT_BUILD_CMD.into()),
        client_cmd: env("XTASK_CLIENT_CMD").unwrap_or_else(|| DEFAULT_CLIENT_CMD.into()),
        client_timeout_s: 600,
        out: PathBuf::from("artifacts/vm-test"),
        rdp_port: match env("VM_RDP_PORT") {
            Some(p) => p
                .parse()
                .map_err(|_| format!("VM_RDP_PORT invalide : {p}"))?,
            None => 3390,
        },
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = |n: &str| it.next().ok_or(format!("{n} attend une valeur"));
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
                    other => return Err(format!("--session inconnu : {other}")),
                }
            }
            "--outputs" => {
                o.outputs = val("--outputs")?
                    .parse()
                    .ok()
                    .filter(|n| (1..=16).contains(n))
                    .ok_or("--outputs invalide (1 à 16)")?
            }
            "--deb" => o.deb = Some(val("--deb")?),
            "--build-cmd" => o.build_cmd = val("--build-cmd")?,
            "--client-cmd" => o.client_cmd = val("--client-cmd")?,
            "--timeout" => {
                o.client_timeout_s = val("--timeout")?
                    .parse()
                    .map_err(|_| "--timeout invalide")?
            }
            "--out" => o.out = PathBuf::from(val("--out")?),
            other => return Err(format!("argument inconnu : {other}")),
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
        let Parsed::Run(o) = p(&["--session", "login"], &[]).unwrap() else { panic!() };
        assert!(o.login && o.kwin);
    }

    #[test]
    fn session_flag() {
        let Parsed::Run(o) = p(&["--session", "kwin"], &[]).unwrap() else { panic!() };
        assert!(o.kwin);
        let Parsed::Run(o) = p(&[], &[]).unwrap() else { panic!() };
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
