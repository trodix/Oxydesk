//! `cargo xtask test`: unit tests by default (fast). Integration tests, which need a VM, are opt-in and
//! independent: server tests (`--server`: our server deployed in a Debian 13 VM, driven by the test client,
//! `cargo xtask vm-test`) and client tests (`--client`: the stock RDP engine of mstsc in a QEMU Windows 11 VM
//! against a local test server, `vm/win11.sh test`); `--integration` selects both.

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

pub const HELP: &str = "cargo xtask test [options]
  (default)        unit tests only: cargo test --workspace
  --server         then the SERVER integration test, on Debian 13 (cargo xtask vm-test, synthetic session: build the
                   .deb, deploy it in the VM, run our test client against it; for the other sessions use
                   `cargo xtask vm-test --session kwin|login`)
  --client         then the CLIENT integration test, with the stock Windows RDP client (mstsc engine) in a QEMU
                   Windows 11 VM against a local test server (slow: connect, frames, clipboard, audio, dynamic resize)
  --integration    both --server and --client
  --skip-unit      do not run the unit tests (needs one of the above)
  --out <dir>      client test logs folder (default artifacts/win-test)
  --help           this help
env: WIN_RDP_PORT (test server port reachable from the Windows VM, default 3391); WIN_SESSION=kwin (--client: the
  server runs a real KWin session on this host with the test application instead of the synthetic desktop); RDP_TEST_PASSWORD (any value in test mode)
prerequisites: /dev/kvm and the VM tools; --server: `bash vm/vm.sh create` once; --client: `bash vm/win11.sh install` once
(the ISO is downloaded into iso/ when missing). A missing prerequisite is an error listing what is missing.
Each integration test runs even if the other failed. exit code: 0 all pass, 1 failure or missing prerequisite, 2 arguments";

const DEBIAN_IMAGE: &str = "vm/state/provisioned.qcow2";
const BASE_DISK: &str = "vm/state/win11/base.qcow2";
const WINDOWS_TOOLS: [&str; 5] = ["qemu-system-x86_64", "swtpm", "xorriso", "socat", "python3"];
const DEBIAN_TOOLS: [&str; 7] = [
    "qemu-img",
    "qemu-system-x86_64",
    "curl",
    "ssh",
    "scp",
    "ssh-keygen",
    "cargo-deb",
];
/// `vm.sh` builds its cloud-init seed with any one of these.
const SEED_TOOLS: [&str; 3] = ["cloud-localds", "genisoimage", "xorriso"];

#[derive(Debug, Clone, PartialEq)]
pub struct TestOpts {
    pub server: bool,
    pub client: bool,
    pub skip_unit: bool,
    pub port: u16,
    pub out: PathBuf,
}

#[derive(Debug, PartialEq)]
pub enum TestParsed {
    Help,
    Run(TestOpts),
}

pub fn parse(
    args: Vec<String>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<TestParsed, String> {
    let mut o = TestOpts {
        server: false,
        client: false,
        skip_unit: false,
        port: match env("WIN_RDP_PORT") {
            Some(p) => p
                .parse()
                .map_err(|_| format!("invalid WIN_RDP_PORT: {p}"))?,
            None => 3391,
        },
        out: PathBuf::from("artifacts/win-test"),
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => return Ok(TestParsed::Help),
            "--server" => o.server = true,
            "--client" => o.client = true,
            "--integration" => {
                o.server = true;
                o.client = true;
            }
            "--skip-unit" => o.skip_unit = true,
            "--out" => o.out = PathBuf::from(it.next().ok_or("--out expects a value")?),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if o.skip_unit && !o.server && !o.client {
        return Err(
            "--skip-unit needs --server, --client or --integration (nothing would run)".into(),
        );
    }
    Ok(TestParsed::Run(o))
}

/// What prevents the Windows integration test from running; empty when everything is there.
pub fn windows_prerequisites(
    has_tool: impl Fn(&str) -> bool,
    kvm_usable: bool,
    base_disk: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if !kvm_usable {
        out.push("/dev/kvm is not readable and writable".to_string());
    }
    out.extend(missing_tools(&has_tool, &WINDOWS_TOOLS));
    if !base_disk {
        out.push(format!(
            "no Windows VM ({BASE_DISK}): run `bash vm/win11.sh install` once"
        ));
    }
    out
}

/// Same for the Debian VM test.
pub fn debian_prerequisites(
    has_tool: impl Fn(&str) -> bool,
    kvm_usable: bool,
    image: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if !kvm_usable {
        out.push("/dev/kvm is not readable and writable".to_string());
    }
    out.extend(missing_tools(&has_tool, &DEBIAN_TOOLS));
    if !SEED_TOOLS.iter().any(|t| has_tool(t)) {
        out.push(format!("none of {} found in PATH", SEED_TOOLS.join(", ")));
    }
    if !image {
        out.push(format!(
            "no Debian VM image ({DEBIAN_IMAGE}): run `bash vm/vm.sh create` once"
        ));
    }
    out
}

fn missing_tools(has_tool: &impl Fn(&str) -> bool, tools: &[&str]) -> Vec<String> {
    tools
        .iter()
        .filter(|t| !has_tool(t))
        .map(|t| format!("`{t}` not found in PATH"))
        .collect()
}

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

fn kvm_usable() -> bool {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

fn cargo(args: &[&str]) -> bool {
    Command::new("cargo")
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

/// Test server killed on drop, whatever the way out.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        // SIGINT first: the server then stops its own KWin session (children of a killed process would leak).
        let _ = Command::new("kill")
            .args(["-INT", &self.0.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && matches!(self.0.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_port(port: u16, within: Duration) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let start = Instant::now();
    while start.elapsed() < within {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn refuse_if_missing(what: &str, missing: Vec<String>) -> Result<(), String> {
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{what} cannot run:\n  - {}",
            missing.join("\n  - ")
        ))
    }
}

fn server() -> Result<(), String> {
    refuse_if_missing(
        "server integration test (Debian 13)",
        debian_prerequisites(on_path, kvm_usable(), Path::new(DEBIAN_IMAGE).is_file()),
    )?;
    println!("== server: Debian 13 VM (cargo xtask vm-test, synthetic session)");
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = Command::new(exe)
        .arg("vm-test")
        .status()
        .map_err(|e| format!("cannot run vm-test: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("server test (Debian 13 VM) failed".into())
    }
}

fn client(o: &TestOpts) -> Result<(), String> {
    refuse_if_missing(
        "client integration test (Windows)",
        windows_prerequisites(on_path, kvm_usable(), Path::new(BASE_DISK).is_file()),
    )?;
    fs::create_dir_all(o.out.join("state")).map_err(|e| e.to_string())?;
    // WIN_SESSION=kwin: the server runs a real KWin session on this host (feature `kwin`) with the test
    // application, instead of the synthetic desktop.
    let kwin = std::env::var("WIN_SESSION").is_ok_and(|v| v == "kwin");
    println!("== build (server and client)");
    let server_build: &[&str] = if kwin {
        &[
            "build",
            "-q",
            "-p",
            "rdsktp",
            "-p",
            "rdsktp-testapp",
            "--features",
            "rdsktp/kwin",
        ]
    } else {
        &["build", "-q", "-p", "rdsktp"]
    };
    if !cargo(server_build)
        || !cargo(&[
            "build",
            "-q",
            "-p",
            "rdp-test-client",
            "--features",
            "ironrdp",
        ])
    {
        return Err("build failed".into());
    }
    let log = fs::File::create(o.out.join("server.log")).map_err(|e| e.to_string())?;
    let mut server = Command::new("target/debug/rdsktp");
    if kwin {
        let app = fs::canonicalize("target/debug/rdsktp-testapp").map_err(|e| e.to_string())?;
        server.args([
            "--session",
            "kwin",
            "--kwin-socket",
            "rdsktp-wintest",
            "--session-app",
            &format!("{} --tone --clipboard-echo", app.display()),
        ]);
    } else {
        server.arg("--test-mode");
    }
    let server = server
        .args(["--width", "1280", "--height", "720"])
        .arg("--listen")
        .arg(format!("0.0.0.0:{}", o.port))
        .arg("--state-dir")
        .arg(o.out.join("state"))
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| format!("cannot start the test server: {e}"))?;
    let _server = Server(server);
    if !wait_port(o.port, Duration::from_secs(10)) {
        return Err(format!(
            "test server not listening on port {} (see {}/server.log)",
            o.port,
            o.out.display()
        ));
    }
    println!(
        "== windows (the VM reaches the server at 10.0.2.2:{})",
        o.port
    );
    let out = Command::new("bash")
        .args(["vm/win11.sh", "test"])
        .arg(format!("10.0.2.2:{}", o.port))
        .arg("tester")
        .env(
            "RDP_TEST_PASSWORD",
            std::env::var("RDP_TEST_PASSWORD").unwrap_or_else(|_| "test-mode".into()),
        )
        .output()
        .map_err(|e| format!("cannot run vm/win11.sh: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::write(o.out.join("win11.log"), &text);
    print!("{text}");
    if out.status.success() {
        Ok(())
    } else {
        Err("client test (Windows) failed".into())
    }
}

pub fn run(o: &TestOpts) -> ExitCode {
    if !o.skip_unit {
        println!("== unit tests");
        if !cargo(&["test", "--workspace"]) {
            eprintln!("unit tests failed");
            return ExitCode::from(1);
        }
    }
    // Independent: each requested integration test runs even if the other failed.
    let mut failed = false;
    if o.server {
        match server() {
            Ok(()) => println!("== server: passed"),
            Err(e) => {
                eprintln!("{e}");
                failed = true;
            }
        }
    }
    if o.client {
        match client(o) {
            Ok(()) => println!("== client: passed"),
            Err(e) => {
                eprintln!("{e}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str], env: &[(&str, &str)]) -> Result<TestParsed, String> {
        let env: Vec<_> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        parse(args.iter().map(|s| s.to_string()).collect(), move |k| {
            env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        })
    }

    #[test]
    fn default_is_unit_tests_only() {
        let TestParsed::Run(o) = p(&[], &[]).unwrap() else {
            panic!()
        };
        assert!(!o.server && !o.client && !o.skip_unit);
        assert_eq!(o.port, 3391);
    }

    #[test]
    fn server_and_client_are_independent() {
        let TestParsed::Run(d) = p(&["--server"], &[]).unwrap() else {
            panic!()
        };
        assert!(d.server && !d.client);
        let TestParsed::Run(w) =
            p(&["--client", "--skip-unit"], &[("WIN_RDP_PORT", "4100")]).unwrap()
        else {
            panic!()
        };
        assert!(!w.server && w.client && w.skip_unit);
        assert_eq!(w.port, 4100);
    }

    #[test]
    fn integration_selects_both() {
        let TestParsed::Run(o) = p(&["--integration"], &[]).unwrap() else {
            panic!()
        };
        assert!(o.server && o.client);
    }

    #[test]
    fn rejects_bad_args() {
        assert!(p(&["--nope"], &[]).is_err());
        assert!(p(&["--out"], &[]).is_err());
        assert!(p(&["--skip-unit"], &[]).is_err());
        assert!(p(&[], &[("WIN_RDP_PORT", "x")]).is_err());
        assert_eq!(p(&["--help"], &[]).unwrap(), TestParsed::Help);
    }

    #[test]
    fn windows_prerequisites_are_listed() {
        assert!(windows_prerequisites(|_| true, true, true).is_empty());
        let m = windows_prerequisites(|t| t != "swtpm", false, false);
        assert_eq!(m.len(), 3);
        assert!(m.iter().any(|s| s.contains("swtpm")));
        assert!(m.iter().any(|s| s.contains("/dev/kvm")));
        assert!(m.iter().any(|s| s.contains("win11.sh install")));
    }

    #[test]
    fn debian_prerequisites_accept_any_seed_tool() {
        assert!(debian_prerequisites(|_| true, true, true).is_empty());
        assert!(
            debian_prerequisites(|t| t == "xorriso" || !SEED_TOOLS.contains(&t), true, true)
                .is_empty()
        );
        let none = debian_prerequisites(|t| !SEED_TOOLS.contains(&t), true, true);
        assert_eq!(none.len(), 1);
        let m = debian_prerequisites(|t| t != "cargo-deb", true, false);
        assert!(m.iter().any(|s| s.contains("cargo-deb")));
        assert!(m.iter().any(|s| s.contains("vm.sh create")));
    }
}
