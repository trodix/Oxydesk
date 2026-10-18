//! The real `oxydesk` binary in login mode: the `session-policy` setting from the command line and the file.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("oxydesk-policy-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn oxydesk(state: &Path, config: &Path, extra: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_oxydesk"))
        .arg("--config")
        .arg(config)
        .args(["--login", "--listen", "127.0.0.1:0", "--state-dir"])
        .arg(state)
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("oxydesk")
}

/// `Some(stderr)` if the server exits within the delay, `None` if it is still serving.
fn exits_within(mut child: Child, wait: Duration) -> Option<String> {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            let out = child.wait_with_output().unwrap();
            assert!(!out.status.success());
            return Some(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

fn config(dir: &Path, text: &str) -> PathBuf {
    let file = dir.join("oxydesk.conf");
    std::fs::write(&file, text).unwrap();
    file
}

#[test]
fn an_unknown_policy_on_the_command_line_is_refused_with_its_name() {
    let d = dir("cli");
    let conf = config(&d, "");
    let err = exits_within(
        oxydesk(&d, &conf, &["--session-policy", "both"]),
        Duration::from_secs(10),
    )
    .expect("the server must refuse the policy");
    assert!(
        err.contains("session-policy") && err.contains("both"),
        "{err}"
    );
}

#[test]
fn an_unknown_policy_in_the_file_is_refused() {
    let d = dir("file");
    let conf = config(&d, "session-policy = sometimes\n");
    let err = exits_within(oxydesk(&d, &conf, &[]), Duration::from_secs(10))
        .expect("the server must refuse the policy");
    assert!(err.contains("session-policy"), "{err}");
}

#[test]
fn every_policy_word_starts_the_server_and_the_command_line_wins_over_the_file() {
    for word in ["new", "attach", "auto"] {
        let d = dir(word);
        let conf = config(&d, "");
        assert!(
            exits_within(
                oxydesk(&d, &conf, &["--session-policy", word]),
                Duration::from_millis(1500)
            )
            .is_none(),
            "{word} must be accepted"
        );
    }
    let d = dir("override");
    let conf = config(&d, "session-policy = sometimes\n");
    assert!(
        exits_within(
            oxydesk(&d, &conf, &["--session-policy", "auto"]),
            Duration::from_millis(1500)
        )
        .is_none(),
        "the command line replaces the file"
    );
}

#[test]
fn the_virtual_outputs_setting_accepts_remove_and_keep_and_refuses_other_words() {
    let d = dir("virtual");
    let conf = config(&d, "virtual-outputs = sometimes\n");
    let err = exits_within(oxydesk(&d, &conf, &[]), Duration::from_secs(10))
        .expect("the server must refuse the value");
    assert!(
        err.contains("virtual-outputs") && err.contains("sometimes"),
        "{err}"
    );
    for word in ["remove", "keep"] {
        // On the command line, which wins over the file.
        assert!(
            exits_within(
                oxydesk(&d, &conf, &["--virtual-outputs", word]),
                Duration::from_millis(1500)
            )
            .is_none(),
            "{word} must be accepted"
        );
    }
}
