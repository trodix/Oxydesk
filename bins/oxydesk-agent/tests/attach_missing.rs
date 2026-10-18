//! `--session attach` towards a session that is not running fails at once, with a reason, instead of waiting.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn joining_a_session_that_does_not_exist_fails_fast() {
    let dir = std::env::temp_dir().join(format!("oxydesk-attach-missing-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxydesk-agent"))
        .args([
            "--session",
            "attach",
            "--kwin-socket",
            "no-such-session",
            "--audio",
            "off",
            "--socket",
        ])
        .arg(dir.join("agent.sock"))
        .env("XDG_RUNTIME_DIR", &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agent");
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the agent kept waiting for a session that does not exist");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    // With the `kwin` feature the missing socket is named; without it the session kind is not available.
    assert!(
        err.contains("no-such-session") || err.contains("kwin"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
