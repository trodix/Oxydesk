//! The real agent binary (synthetic session) driven through the protocol: file clipboard (the synthetic
//! desktop copies back the files it is given, as the test application does).

use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rdsktp_agent_proto::{read_msg, write_msg, FileInfo, FromAgent, ToAgent};

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

/// Reads until `done` accepts a message (others are skipped); fails after 20 s. The socket has a read timeout:
/// an agent that goes quiet fails the test (with its log) instead of blocking the CI runner for hours.
fn read_until<T>(s: &mut UnixStream, mut done: impl FnMut(FromAgent) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    loop {
        assert!(Instant::now() < deadline, "timed out");
        match read_msg::<_, FromAgent>(s).expect("protocol (read timeout: the agent went quiet)") {
            Some(m) => {
                if let FromAgent::Fatal(e) = &m {
                    panic!("fatal error of the agent: {e}");
                }
                if let Some(v) = done(m) {
                    return v;
                }
            }
            None => panic!("agent closed the connection"),
        }
    }
}

#[test]
fn files_are_echoed_and_read_back_by_range_through_the_agent() {
    let dir = std::env::temp_dir().join(format!("rdsktp-agent-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("agent.sock");
    let _agent = Agent(
        Command::new(env!("CARGO_BIN_EXE_rdsktp-agent"))
            .args([
                "--session",
                "synthetic",
                "--width",
                "320",
                "--height",
                "240",
            ])
            .args(["--audio", "off", "--socket"])
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
        Some(FromAgent::Hello { .. })
    ));
    write_msg(&mut s, &ToAgent::SetPixels { raw: true }).unwrap();

    // The "server" offers a 5 GB file: the agent publishes it as the desktop selection, which the
    // synthetic desktop echoes back as a new selection of its own.
    let size = 5_000_000_000u64;
    write_msg(
        &mut s,
        &ToAgent::FilesSet {
            list_id: 42,
            files: vec![FileInfo {
                path: "gros.bin".into(),
                size,
                is_dir: false,
                modified: 1_700_000_000,
                read_only: false,
            }],
        },
    )
    .unwrap();
    let (echo_id, files) = read_until(&mut s, |m| match m {
        FromAgent::Files { list_id, files } => Some((list_id, files)),
        _ => None,
    });
    assert_eq!(
        (files.len(), files[0].path.as_str(), files[0].size),
        (1, "gros.bin", size)
    );

    // Reading a range of the echoed list makes the agent read it from us, at an offset past 4 GiB.
    let offset = 4_999_999_000u64;
    write_msg(
        &mut s,
        &ToAgent::FileRead(rdsktp_agent_proto::FileRange {
            req_id: 7,
            list_id: echo_id,
            index: 0,
            offset,
            len: 1000,
        }),
    )
    .unwrap();
    let mut writer = s.try_clone().unwrap();
    let mut asked = false;
    let data = read_until(&mut s, |m| match m {
        FromAgent::FileRead(r) => {
            assert_eq!((r.list_id, r.index, r.offset), (42, 0, offset));
            let data: Vec<u8> = (0..u64::from(r.len))
                .map(|i| ((offset + i) % 251) as u8)
                .collect();
            write_msg(
                &mut writer,
                &ToAgent::FileData {
                    req_id: r.req_id,
                    ok: true,
                    data,
                },
            )
            .unwrap();
            asked = true;
            None
        }
        FromAgent::FileData {
            req_id: 7,
            ok,
            data,
        } => {
            assert!(ok && asked, "answered before asking us");
            Some(data)
        }
        _ => None,
    });
    assert_eq!(data.len(), 1000);
    assert!(data
        .iter()
        .enumerate()
        .all(|(i, b)| *b == ((offset + i as u64) % 251) as u8));

    // A list the agent never offered is refused.
    write_msg(
        &mut writer,
        &ToAgent::FileRead(rdsktp_agent_proto::FileRange {
            req_id: 8,
            list_id: echo_id + 100,
            index: 0,
            offset: 0,
            len: 10,
        }),
    )
    .unwrap();
    read_until(&mut s, |m| match m {
        FromAgent::FileData { req_id: 8, ok, .. } => {
            assert!(!ok);
            Some(())
        }
        _ => None,
    });
}
