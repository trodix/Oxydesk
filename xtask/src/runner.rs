use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::opts::Opts;
use crate::report::{verdict, Status, StepResult};

const VM: &str = "vm/vm.sh";

fn run_step(
    name: &str,
    idx: usize,
    dir: &Path,
    cmd: &str,
    timeout: Duration,
    always: bool,
) -> StepResult {
    let log = dir.join("steps").join(format!("{idx:02}-{name}.log"));
    let started = Instant::now();
    let mut res = StepResult {
        name: name.into(),
        status: Status::Failed,
        code: None,
        duration_ms: 0,
        log: Some(log.display().to_string()),
        note: None,
        always,
    };
    let spawned = File::create(&log).and_then(|out| {
        let err = out.try_clone()?;
        Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
    });
    match spawned {
        Err(e) => res.note = Some(format!("cannot launch: {e}")),
        Ok(mut child) => loop {
            match child.try_wait() {
                Ok(Some(st)) => {
                    res.code = st.code();
                    res.status = if st.success() {
                        Status::Passed
                    } else {
                        Status::Failed
                    };
                    break;
                }
                Ok(None) if started.elapsed() > timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    res.status = Status::Timeout;
                    res.note = Some(format!("timeout after {}s", timeout.as_secs()));
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => {
                    res.note = Some(e.to_string());
                    break;
                }
            }
        },
    }
    res.duration_ms = started.elapsed().as_millis();
    res
}

fn skipped(name: &str, note: &str) -> StepResult {
    StepResult {
        name: name.into(),
        status: Status::Skipped,
        code: None,
        duration_ms: 0,
        log: None,
        note: Some(note.into()),
        always: false,
    }
}

fn newest_deb() -> Option<PathBuf> {
    fs::read_dir("target/debian")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "deb"))
        .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
}

fn git(args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Escapes for insertion into a bash command.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn vm_test(o: &Opts) -> ExitCode {
    let started = Instant::now();
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = o.out.join(format!("run-{unix}"));
    if let Err(e) = fs::create_dir_all(dir.join("steps")) {
        eprintln!("cannot create {}: {e}", dir.display());
        return ExitCode::from(1);
    }
    let abs_dir = fs::canonicalize(&dir).unwrap_or(dir.clone());
    let min = |m: u64| Duration::from_secs(m * 60);

    // Normal steps: (name, command, timeout, ignored?)
    let deb = o
        .deb
        .clone()
        .or_else(|| newest_deb().map(|p| p.display().to_string()));
    let deploy_cmd = match &deb {
        Some(d) => format!("bash {VM} deploy {}", sq(d)),
        None => format!("bash {VM} deploy"),
    };
    if o.outputs > 1 && !o.kwin {
        eprintln!("--outputs > 1 requires --session kwin");
        return ExitCode::from(2);
    }
    if o.login && std::env::var("RDP_TEST_PASSWORD").map_or(true, |p| p.is_empty()) {
        eprintln!("--session login requires RDP_TEST_PASSWORD (password of the VM test account)");
        return ExitCode::from(2);
    }
    let mut client_cmd = o
        .client_cmd
        .replace("{host}", "127.0.0.1")
        .replace("{port}", &o.rdp_port.to_string())
        .replace("{out}", &abs_dir.display().to_string());
    if o.outputs > 1 {
        client_cmd.push_str(&format!(" --outputs {}", o.outputs));
    }
    if o.multi_resize {
        client_cmd.push_str(" --multi-resize");
    }
    if o.login {
        client_cmd.push_str(" --no-nla --user dev");
    }
    // Login negative control: a wrong password must be refused (no image received: the `first_frame` check fails; the RDP handshake itself succeeds before validation).
    let deny_cmd = o.login.then(|| {
        let deny_dir = abs_dir.join("deny");
        let base = o
            .client_cmd
            .replace("{host}", "127.0.0.1")
            .replace("{port}", &o.rdp_port.to_string())
            .replace("{out}", &deny_dir.display().to_string());
        format!(
            "RDP_TEST_PASSWORD=\"wrong-$(head -c 9 /dev/urandom | base64)\" {base} --no-nla --user dev --timeout 60; \
             python3 -I -c 'import json,sys; c=json.load(open(sys.argv[1]))[\"checks\"]; sys.exit(0 if any(x[\"name\"]==\"first_frame\" and not x[\"passed\"] for x in c) else 1)' {}/client.json",
            deny_dir.display()
        )
    });
    let normal: Vec<(&str, String, Duration, Option<&str>)> = vec![
        (
            "build",
            o.build_cmd.clone(),
            min(30),
            o.no_build.then_some("--no-build"),
        ),
        (
            "vm-reset",
            format!("bash {VM} reset"),
            min(3),
            o.no_reset.then_some("--no-reset"),
        ),
        ("vm-up", format!("bash {VM} up"), min(8), None),
        ("deploy", deploy_cmd, min(5), None),
        (
            "configure-kwin",
            format!(
                "VM_OUTPUTS={} bash {VM} {}",
                o.outputs,
                if o.login {
                    "login-config"
                } else {
                    "kwin-config"
                }
            ),
            min(if o.login { 6 } else { 3 }),
            (!o.kwin).then_some("--session synthetic"),
        ),
        (
            "deny-control",
            deny_cmd.clone().unwrap_or_default(),
            min(3),
            deny_cmd.is_none().then_some("not in login mode"),
        ),
        (
            "client",
            client_cmd,
            Duration::from_secs(o.client_timeout_s),
            None,
        ),
    ];

    let mut steps = Vec::new();
    let mut idx = 0;
    let mut broken = false;
    for (name, cmd, t, skip) in normal {
        idx += 1;
        if let Some(why) = skip {
            steps.push(skipped(name, why));
        } else if broken {
            steps.push(skipped(name, "previous step failed"));
        } else {
            let r = run_step(name, idx, &dir, &cmd, t, false);
            broken = r.status != Status::Passed;
            eprintln!("[{name}] {:?} ({} ms)", r.status, r.duration_ms);
            steps.push(r);
        }
    }
    // Collection and shutdown: always run.
    let mut always = vec![
        (
            "journal-rdsktp",
            format!("bash {VM} logs journal rdsktp"),
            min(2),
        ),
        (
            "journal-authd",
            format!("bash {VM} logs journal rdsktp-authd"),
            min(2),
        ),
        (
            "journal-agent",
            format!("bash {VM} logs journal rdsktp-agent@dev"),
            min(2),
        ),
        ("journal-all", format!("bash {VM} logs journal"), min(2)),
        ("console", format!("bash {VM} logs console"), min(2)),
    ];
    if !o.keep {
        always.push(("vm-down", format!("bash {VM} down"), min(3)));
    }
    for (name, cmd, t) in always {
        idx += 1;
        let r = run_step(name, idx, &dir, &cmd, t, true);
        eprintln!("[{name}] {:?}", r.status);
        steps.push(r);
    }

    let client_json = fs::read_to_string(dir.join("client.json")).ok();
    let v = verdict(&steps, client_json.as_deref());
    let client_value = client_json
        .as_deref()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok());
    let report = json!({
        "schema": 1,
        "verdict": if v.passed { "passed" } else { "failed" },
        "reason": v.reason,
        "started_unix": unix,
        "duration_ms": started.elapsed().as_millis() as u64,
        "git": { "rev": git(&["rev-parse", "--short", "HEAD"]), "dirty": !git(&["status", "--porcelain"]).is_empty() },
        "config": {
            "no_build": o.no_build, "no_reset": o.no_reset, "keep": o.keep, "deb": deb,
            "build_cmd": o.build_cmd, "client_cmd": o.client_cmd, "client_timeout_s": o.client_timeout_s,
            "rdp_port": o.rdp_port,
        },
        "steps": steps,
        "failed_checks": v.failed_checks,
        "client": client_value,
        "artifacts_dir": abs_dir.display().to_string(),
    });
    let text = serde_json::to_string_pretty(&report).expect("report serializable");
    let path = dir.join("report.json");
    if fs::write(&path, &text).is_err() || fs::write(o.out.join("latest.json"), &text).is_err() {
        eprintln!("cannot write the report");
        return ExitCode::from(1);
    }
    println!(
        "REPORT: {}",
        fs::canonicalize(&path).unwrap_or(path).display()
    );
    if v.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
