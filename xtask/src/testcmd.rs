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
  --freerdp        then the FreeRDP compatibility test (ci/freerdp-test.sh: sdl-freerdp, the engine of xfreerdp and
                   Remmina, runs headless against a local server; `FREERDP_SESSION=kwin` uses a real KWin session)
  --clients        then the real clients (xfreerdp, Remmina) in a Docker or Podman container, driven in a virtual X
                   server against a local synthetic server (ci/clients-test.sh; CONTAINER_ENGINE=docker|podman)
  --integration    --server, --client, --freerdp and --clients
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
    pub freerdp: bool,
    pub clients: bool,
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
        freerdp: false,
        clients: false,
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
            "--freerdp" => o.freerdp = true,
            "--clients" => o.clients = true,
            "--integration" => {
                o.server = true;
                o.client = true;
                o.freerdp = true;
                o.clients = true;
            }
            "--skip-unit" => o.skip_unit = true,
            "--out" => o.out = PathBuf::from(it.next().ok_or("--out expects a value")?),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if o.skip_unit && !o.server && !o.client && !o.freerdp && !o.clients {
        return Err(
            "--skip-unit needs --server, --client, --freerdp, --clients or --integration (nothing would run)"
                .into(),
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

/// Byte `i` of the files exchanged by the drive test (same pattern as `vm/win11/rdp-activex-test.ps1`).
fn drive_pattern(offset: u64, buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (((offset + i as u64) * 31 + 7) % 251) as u8;
    }
}

fn write_drive_pattern(path: &Path, size: u64) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = fs::File::create(path)?;
    let mut buf = vec![0u8; 65536];
    let mut done = 0;
    while done < size {
        let n = buf.len().min((size - done) as usize);
        drive_pattern(done, &mut buf[..n]);
        f.write_all(&buf[..n])?;
        done += n as u64;
    }
    f.sync_all()
}

fn check_drive_pattern(path: &Path, size: u64) -> Result<(), String> {
    let data = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if data.len() as u64 != size {
        return Err(format!(
            "{} has {} bytes, {size} expected",
            path.display(),
            data.len()
        ));
    }
    let mut want = vec![0u8; data.len()];
    drive_pattern(0, &mut want);
    match data.iter().zip(&want).position(|(a, b)| a != b) {
        Some(at) => Err(format!("{} differs at byte {at}", path.display())),
        None => Ok(()),
    }
}

/// Host side of the mstsc drive test: the guest's drive appears under `mnt`; the guest prepared `rdsktp/drive/guest.bin`
/// and others, which are checked here through the mount, then changed (the guest checks the result).
fn drives_host_check(mnt: &Path, within: Duration) -> Result<String, String> {
    let start = Instant::now();
    let dir = loop {
        let found = fs::read_dir(mnt).ok().and_then(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.path().join("rdsktp").join("drive"))
                .find(|p| p.join("to-rename.bin").is_file())
        });
        if let Some(d) = found {
            break d;
        }
        if start.elapsed() > within {
            return Err(format!("no guest drive appeared under {}", mnt.display()));
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let drive = dir
        .ancestors()
        .nth(2)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    check_drive_pattern(&dir.join("guest.bin"), (3 << 20) + 5)?;
    // PowerShell's UTF8 encoding writes a byte order mark.
    let text = fs::read_to_string(dir.join("guest.txt")).map_err(|e| e.to_string())?;
    if text.trim_start_matches('\u{feff}') != "to delete" {
        return Err("guest.txt differs".into());
    }
    write_drive_pattern(&dir.join("from-host.bin"), (2 << 20) + 17)
        .map_err(|e| format!("write: {e}"))?;
    fs::create_dir(dir.join("hostdir")).map_err(|e| format!("mkdir: {e}"))?;
    fs::write(dir.join("hostdir").join("note.txt"), "salut é à ü")
        .map_err(|e| format!("write: {e}"))?;
    fs::remove_file(dir.join("guest.txt")).map_err(|e| format!("unlink: {e}"))?;
    fs::rename(dir.join("to-rename.bin"), dir.join("renamed.bin"))
        .map_err(|e| format!("rename: {e}"))?;
    fs::write(dir.join("host-done.txt"), "done").map_err(|e| format!("write: {e}"))?;
    Ok(format!(
        "guest drive {drive}: read 3 MiB, wrote 2 MiB, mkdir, delete, rename"
    ))
}

/// Byte `i` of the job of the printer test: a line, then 100000 bytes `A..Z` repeated (same as `rdp-activex-test.ps1`).
fn printer_job() -> Vec<u8> {
    let mut job = b"Bonjour imprimante\n".to_vec();
    job.extend((0..100_000usize).map(|i| b'A' + (i % 26) as u8));
    job
}

/// Host side of the mstsc printer test: the guest's printer `RdsktpTest` appears as a socket under `dir`; one job is
/// sent through it (the guest compares what its spooler wrote to the file port).
fn printers_host_check(dir: &Path, within: Duration) -> Result<String, String> {
    use std::io::{Read, Write};
    let start = Instant::now();
    let socket = loop {
        let found = fs::read_dir(dir).ok().and_then(|d| {
            d.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
                p.extension().is_some_and(|e| e == "sock")
                    && p.to_string_lossy().contains("RdsktpTest")
            })
        });
        if let Some(s) = found {
            break s;
        }
        if start.elapsed() > within {
            return Err(format!("no guest printer appeared under {}", dir.display()));
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let job = printer_job();
    let mut s =
        std::os::unix::net::UnixStream::connect(&socket).map_err(|e| format!("connect: {e}"))?;
    s.write_all(&job).map_err(|e| format!("send: {e}"))?;
    s.shutdown(std::net::Shutdown::Write)
        .map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(60))).ok();
    let mut answer = String::new();
    s.read_to_string(&mut answer)
        .map_err(|e| format!("answer: {e}"))?;
    if answer.trim() != "OK" {
        return Err(format!("the spool answered {:?}", answer.trim()));
    }
    Ok(format!(
        "job of {} bytes sent to {}",
        job.len(),
        socket
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ))
}

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
    let drives_dir = fs::canonicalize(&o.out)
        .map_err(|e| e.to_string())?
        .join("drives");
    fs::create_dir_all(&drives_dir).map_err(|e| e.to_string())?;
    // The directory is named like the real one: the CUPS backend only accepts such a directory.
    let printers_dir = fs::canonicalize(&o.out)
        .map_err(|e| e.to_string())?
        .join("rdsktp-printers");
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
    // WIN_MONITORS=2: the guest uses both its monitors (multimon) and the server exposes two outputs.
    let monitors = std::env::var("WIN_MONITORS").unwrap_or_else(|_| "1".into());
    if kwin {
        // The session shares the user's D-Bus: adopting the client's layout would change the layout of the host.
        server.env("RDSKTP_KEYBOARD_LAYOUT", "off");
    }
    let server = server
        // Files echoed by the test desktop are copied aside: mstsc drops the contents of its old clipboard.
        .env("RDSKTP_ECHO_FILES", "copy")
        .args(["--width", "1280", "--height", "720"])
        .arg("--outputs")
        .arg(&monitors)
        .arg("--listen")
        .arg(format!("0.0.0.0:{}", o.port))
        .arg("--state-dir")
        .arg(o.out.join("state"))
        // Drive redirection: the drives of the guest are mounted here (checked by `drives_host_check`).
        .arg("--drives-dir")
        .arg(&drives_dir)
        // Printer redirection: the printers of the guest are sockets here (checked by `printers_host_check`).
        .arg("--printers")
        .arg(&printers_dir)
        .args(["--printers-cups", "off"])
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
    // The guest redirects its drives (WIN_DRIVES=1); this thread checks and changes them through the server's mount.
    let drives_check = {
        let dir = drives_dir.clone();
        std::thread::spawn(move || drives_host_check(&dir, Duration::from_secs(240)))
    };
    let printers_check = {
        let dir = printers_dir.clone();
        std::thread::spawn(move || printers_host_check(&dir, Duration::from_secs(240)))
    };
    let out = Command::new("bash")
        .args(["vm/win11.sh", "test"])
        .env("WIN_DRIVES", "1")
        .env("WIN_PRINTERS", "1")
        .arg(format!("10.0.2.2:{}", o.port))
        .arg("tester")
        .env(
            "RDP_TEST_PASSWORD",
            std::env::var("RDP_TEST_PASSWORD").unwrap_or_else(|_| "test-mode".into()),
        )
        // The marker of the layout exists only on the synthetic desktop.
        .env("WIN_LAYOUT_CHECK", if kwin { "0" } else { "1" })
        .output()
        .map_err(|e| format!("cannot run vm/win11.sh: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::write(o.out.join("win11.log"), &text);
    print!("{text}");
    let drives = drives_check
        .join()
        .unwrap_or_else(|_| Err("drive check panicked".into()));
    match &drives {
        Ok(d) => println!("PASS drives_host {d}"),
        Err(e) => println!("FAIL drives_host {e}"),
    }
    let printers = printers_check
        .join()
        .unwrap_or_else(|_| Err("printer check panicked".into()));
    match &printers {
        Ok(d) => println!("PASS printers_host {d}"),
        Err(e) => println!("FAIL printers_host {e}"),
    }
    if out.status.success() && drives.is_ok() && printers.is_ok() {
        Ok(())
    } else {
        Err("client test (Windows) failed".into())
    }
}

/// `cargo xtask host-check --drives DIR --printers DIR [--within S]`: the host side of the mstsc drive and printer
/// tests against a server that is not started by xtask (the login mode of the Debian VM: `ci/mstsc-login-test.sh`
/// exposes its mount and its printer socket under these directories).
pub fn host_check(args: Vec<String>) -> ExitCode {
    let mut drives = None;
    let mut printers = None;
    let mut within = 240;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match (a.as_str(), it.next()) {
            ("--drives", Some(v)) => drives = Some(PathBuf::from(v)),
            ("--printers", Some(v)) => printers = Some(PathBuf::from(v)),
            ("--within", Some(v)) => within = v.parse().unwrap_or(240),
            _ => {
                eprintln!("usage: cargo xtask host-check [--drives DIR] [--printers DIR] [--within SECONDS]");
                return ExitCode::from(2);
            }
        }
    }
    let within = Duration::from_secs(within);
    let drives = drives.map(|d| std::thread::spawn(move || drives_host_check(&d, within)));
    let printers = printers.map(|d| std::thread::spawn(move || printers_host_check(&d, within)));
    let mut ok = true;
    for (name, handle) in [("drives_host", drives), ("printers_host", printers)] {
        let Some(handle) = handle else { continue };
        match handle
            .join()
            .unwrap_or_else(|_| Err("check panicked".into()))
        {
            Ok(d) => println!("PASS {name} {d}"),
            Err(e) => {
                println!("FAIL {name} {e}");
                ok = false;
            }
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
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
    if o.freerdp {
        let ok = Command::new("bash")
            .arg("ci/freerdp-test.sh")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("== freerdp: passed");
        } else {
            eprintln!("FreeRDP compatibility test failed (logs in artifacts/freerdp-test)");
            failed = true;
        }
    }
    if o.clients {
        let ok = Command::new("bash")
            .arg("ci/clients-test.sh")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("== clients: passed");
        } else {
            eprintln!("Real clients test failed (logs in artifacts/clients-test)");
            failed = true;
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
        assert!(o.server && o.client && o.freerdp && o.clients);
    }

    #[test]
    fn freerdp_is_independent_and_enough_with_skip_unit() {
        let TestParsed::Run(o) = p(&["--freerdp", "--skip-unit"], &[]).unwrap() else {
            panic!()
        };
        assert!(o.freerdp && !o.server && !o.client && o.skip_unit);
    }

    #[test]
    fn clients_is_independent_and_enough_with_skip_unit() {
        let TestParsed::Run(o) = p(&["--clients", "--skip-unit"], &[]).unwrap() else {
            panic!()
        };
        assert!(o.clients && !o.freerdp && !o.server && !o.client && o.skip_unit);
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
