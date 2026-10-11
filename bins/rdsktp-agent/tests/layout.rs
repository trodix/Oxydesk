//! The real agent binary (synthetic session) driven through the protocol: the layout changes the number of outputs.

use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rdsktp_agent_proto::{read_msg, write_msg, FromAgent, Rect, ToAgent};

struct Agent(Child, std::path::PathBuf);
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        if std::thread::panicking() {
            eprintln!(
                "--- agent log ---\n{}",
                std::fs::read_to_string(&self.1).unwrap_or_default()
            );
        }
    }
}

fn rect(x: i32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y: 0,
        width,
        height,
    }
}

/// Reads until `done` accepts a message (others are skipped); fails after 20 s. The socket has a read timeout:
/// an agent that goes quiet fails the test (with its log) instead of blocking the CI runner for hours.
fn read_until<T>(s: &mut UnixStream, mut done: impl FnMut(FromAgent) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    loop {
        assert!(Instant::now() < deadline, "timed out");
        match read_msg::<_, FromAgent>(s).expect("protocol (read timeout: the agent went quiet)") {
            Some(m) => {
                if let Some(v) = done(m) {
                    return v;
                }
            }
            None => panic!("agent closed the connection"),
        }
    }
}

#[test]
fn set_layout_changes_the_number_of_outputs() {
    let dir = std::env::temp_dir().join(format!("rdsktp-agent-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("agent.sock");
    let _agent = Agent(
        Command::new(env!("CARGO_BIN_EXE_rdsktp-agent"))
            .args(["--session", "synthetic", "--outputs", "2", "--width", "320"])
            .args(["--height", "240", "--audio", "off", "--socket"])
            .arg(&socket)
            .env("XDG_RUNTIME_DIR", &dir)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.join("agent.log")).unwrap())
            .spawn()
            .expect("agent"),
        dir.join("agent.log"),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut s = loop {
        match UnixStream::connect(&socket) {
            Ok(s) => break s,
            Err(_) => {
                assert!(Instant::now() < deadline, "agent not ready");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert!(matches!(
        read_msg::<_, FromAgent>(&mut s).unwrap(),
        Some(FromAgent::Hello { outputs: 2, .. })
    ));

    // 2 -> 3 outputs of different sizes: answer first, then a keyframe of the right size per output.
    let layout = vec![rect(0, 320, 240), rect(320, 256, 192), rect(576, 400, 300)];
    write_msg(&mut s, &ToAgent::SetLayout { outputs: layout }).unwrap();
    read_until(&mut s, |m| match m {
        FromAgent::LayoutApplied { ok, outputs } => {
            assert!(ok && outputs == 3);
            Some(())
        }
        _ => None,
    });
    let mut seen = [None; 3];
    read_until(&mut s, |m| {
        if let FromAgent::Video {
            output,
            keyframe: true,
            width,
            height,
            ..
        } = m
        {
            seen[output as usize] = Some((width, height));
        }
        seen.iter().all(Option::is_some).then_some(())
    });
    assert_eq!(seen, [Some((320, 240)), Some((256, 192)), Some((400, 300))]);

    // 3 -> 1 output: no frame of an output that is gone after the answer.
    write_msg(
        &mut s,
        &ToAgent::SetLayout {
            outputs: vec![rect(0, 160, 120)],
        },
    )
    .unwrap();
    read_until(&mut s, |m| match m {
        FromAgent::LayoutApplied { ok, outputs } => {
            assert!(ok && outputs == 1);
            Some(())
        }
        _ => None,
    });
    let (w, h) = read_until(&mut s, |m| match m {
        FromAgent::Video {
            output,
            keyframe: true,
            width,
            height,
            ..
        } => {
            assert_eq!(output, 0, "frame of a removed output");
            Some((width, height))
        }
        _ => None,
    });
    assert_eq!((w, h), (160, 120));
    let _ = std::fs::remove_dir_all(&dir);
}
