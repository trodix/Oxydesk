//! The real agent joining a real KWin it did not start (`--session attach`), with real application windows.
//!
//! A private `kwin_wayland --virtual` with two outputs plays the user's desktop, on a private D-Bus session so that
//! the host's own KWin is never involved. Skipped when KWin, `kscreen-doctor`, `konsole` or `dbus-daemon` is missing.

#![cfg(feature = "kwin")]

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use oxydesk_agent_proto::{read_msg, write_msg, FromAgent, Rect, ToAgent};
use oxydesk_core::ports::SessionProvider;
use oxydesk_session::{KwinSession, KwinSessionConfig};

fn have(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Kills the child at the end of the test, whatever happens.
struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for<T>(what: &str, secs: u64, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn busctl(args: &[&str]) -> String {
    let out = Command::new("busctl")
        .args(["--user", "call"])
        .args(args)
        .output()
        .expect("busctl");
    assert!(
        out.status.success(),
        "busctl {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Loads, runs and unloads a KWin script on the private bus.
fn run_script(dir: &Path, name: &str, source: &str) {
    let file = dir.join(format!("{name}.js"));
    std::fs::write(&file, source).unwrap();
    let reply = busctl(&[
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
        "loadScript",
        "ss",
        file.to_str().unwrap(),
        name,
    ]);
    let id = reply
        .split_whitespace()
        .nth(1)
        .expect("script id")
        .to_owned();
    busctl(&[
        "org.kde.KWin",
        &format!("/Scripting/Script{id}"),
        "org.kde.kwin.Script",
        "run",
    ]);
    busctl(&[
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
        "unloadScript",
        "s",
        name,
    ]);
}

/// Where the application windows are: (class, output, x, y), read back through a script that calls D-Bus.
fn windows(dir: &Path, round: &mut u32) -> Vec<(String, String, i32, i32, String)> {
    *round += 1;
    let monitor_out = dir.join(format!("monitor-{round}.txt"));
    let mut monitor = Guard(
        Command::new("dbus-monitor")
            .args(["--session", "member='Pos'"])
            .stdout(std::fs::File::create(&monitor_out).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-monitor"),
    );
    std::thread::sleep(Duration::from_millis(500));
    run_script(
        dir,
        &format!("oxydesk-it-query-{round}"),
        "for (const w of workspace.windowList()) {\n\
         \tif (!w.normalWindow) continue;\n\
         \tcallDBus('org.oxydesk.Probe', '/p', 'org.oxydesk.Probe', 'Pos', String(w.resourceClass), String(w.output.name), Math.round(w.frameGeometry.x), Math.round(w.frameGeometry.y), w.desktops.map(d => d.name).join('|'));\n\
         }\n\
         callDBus('org.oxydesk.Probe', '/p', 'org.oxydesk.Probe', 'Pos', 'END', 'END', 0, 0, '');\n",
    );
    wait_for("the window list", 10, || {
        std::fs::read_to_string(&monitor_out)
            .ok()
            .filter(|t| t.contains("\"END\""))
    });
    let _ = monitor.0.kill();
    let text = std::fs::read_to_string(&monitor_out).unwrap();
    let mut found = Vec::new();
    let mut args: Vec<String> = Vec::new();
    for line in text.lines().chain(std::iter::once("method call")) {
        if line.starts_with("method call") {
            if let [class, output, x, y, desks] = args.as_slice() {
                if class != "END" {
                    found.push((
                        class.clone(),
                        output.clone(),
                        x.parse().unwrap(),
                        y.parse().unwrap(),
                        desks.clone(),
                    ));
                }
            }
            args.clear();
        } else if let Some(v) = line.trim().strip_prefix("string \"") {
            args.push(v.trim_end_matches('"').to_owned());
        } else if let Some(v) = line.trim().strip_prefix("int32 ") {
            args.push(v.to_owned());
        }
    }
    found
}

/// The names of the virtual desktops of the session.
fn desktop_names(dir: &Path, round: &mut u32) -> Vec<String> {
    *round += 1;
    let monitor_out = dir.join(format!("monitor-{round}.txt"));
    let mut monitor = Guard(
        Command::new("dbus-monitor")
            .args(["--session", "member='Pos'"])
            .stdout(std::fs::File::create(&monitor_out).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-monitor"),
    );
    std::thread::sleep(Duration::from_millis(500));
    run_script(
        dir,
        &format!("oxydesk-it-desktops-{round}"),
        "callDBus('org.oxydesk.Probe', '/p', 'org.oxydesk.Probe', 'Pos', 'DESKTOPS', workspace.desktops.map(d => d.name).join('|'), 0, 0, 'END');\n",
    );
    let text = wait_for("the desktop list", 10, || {
        std::fs::read_to_string(&monitor_out)
            .ok()
            .filter(|t| t.contains("\"END\""))
    });
    let _ = monitor.0.kill();
    let names = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("string \""))
        .map(|v| v.trim_end_matches('"'))
        .skip_while(|v| *v != "DESKTOPS")
        .nth(1)
        .unwrap_or("");
    names.split('|').map(str::to_owned).collect()
}

fn rect(x: i32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y: 0,
        width,
        height,
    }
}

fn read_until<T>(s: &mut UnixStream, mut done: impl FnMut(FromAgent) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    loop {
        assert!(Instant::now() < deadline, "timed out");
        match read_msg::<_, FromAgent>(s).expect("protocol") {
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

fn set_layout(s: &mut UnixStream, outputs: Vec<Rect>) -> (bool, u32) {
    write_msg(s, &ToAgent::SetLayout { outputs }).unwrap();
    read_until(s, |m| match m {
        FromAgent::LayoutApplied { ok, outputs } => Some((ok, outputs)),
        _ => None,
    })
}

#[test]
fn a_client_with_other_monitors_than_the_desktop_joins_it_and_leaves_it_as_it_was() {
    for tool in [
        "kwin_wayland",
        "kscreen-doctor",
        "konsole",
        "dbus-daemon",
        "dbus-monitor",
        "busctl",
        "kbuildsycoca6",
    ] {
        if !have(tool) {
            eprintln!("skipped: {tool} is not installed");
            return;
        }
    }
    let tmp: PathBuf =
        std::env::temp_dir().join(format!("oxydesk-attach-it-{}", std::process::id()));
    std::fs::create_dir_all(tmp.join("share/applications")).unwrap();
    std::fs::create_dir_all(tmp.join("cache")).unwrap();

    // A private session bus: the KWin below owns org.kde.KWin there, not on the user's bus.
    let mut bus = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("dbus-daemon");
    let mut address = String::new();
    BufReader::new(bus.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let _bus = Guard(bus);
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", address.trim());

    // The same authorization the package installs, for this very binary (ADR 004), in a cache of its own.
    let agent_bin = env!("CARGO_BIN_EXE_oxydesk-agent");
    let desktop = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist/oxydesk-agent.desktop"),
    )
    .unwrap()
    .replace("/usr/bin/oxydesk-agent", agent_bin);
    std::fs::write(
        tmp.join("share/applications/oxydesk-agent.desktop"),
        desktop,
    )
    .unwrap();
    std::env::set_var(
        "XDG_DATA_DIRS",
        format!(
            "{}:/usr/local/share:/usr/share",
            tmp.join("share").display()
        ),
    );
    std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));

    // The "user's desktop": two 800x600 outputs.
    let socket = format!("oxydesk-it-{}", std::process::id());
    let mut kwin = KwinSession::new(KwinSessionConfig {
        socket: socket.clone(),
        width: 800,
        height: 600,
        outputs: 2,
        programs: vec![],
    });
    kwin.start().expect("private KWin");
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR"));
    let _konsole = Guard(
        Command::new("konsole")
            .env("WAYLAND_DISPLAY", &socket)
            .env("QT_QPA_PLATFORM", "wayland")
            .env_remove("DISPLAY")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("konsole"),
    );
    let mut round = 0;
    wait_for("the konsole window", 30, || {
        (!windows(&tmp, &mut round).is_empty()).then_some(())
    });
    // The window is on the second output, as if the user had put it there.
    run_script(
        &tmp,
        "oxydesk-it-place",
        "const out = workspace.screens.find(o => o.name === 'Virtual-1');\n\
         for (const w of workspace.windowList()) if (w.normalWindow) workspace.sendClientToScreen(w, out);\n",
    );
    let before = wait_for("the window on Virtual-1", 10, || {
        let w = windows(&tmp, &mut round);
        (w.len() == 1 && w[0].1 == "Virtual-1").then(|| w[0].clone())
    });

    // The agent joins that KWin.
    let agent_socket = tmp.join("agent.sock");
    let log = tmp.join("agent.log");
    let spawn_agent = |extra: &[&str]| {
        Guard(
            Command::new(agent_bin)
                .args(["--session", "attach", "--kwin-socket", &socket])
                .args(["--audio", "off", "--socket"])
                .arg(&agent_socket)
                .args(extra)
                .env("XDG_RUNTIME_DIR", &runtime)
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .expect("agent"),
        )
    };
    let agent = spawn_agent(&["--virtual-outputs", "remove"]);

    let mut s = wait_for("the agent socket", 30, || {
        UnixStream::connect(&agent_socket).ok()
    });
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let hello = match read_msg::<_, FromAgent>(&mut s).unwrap() {
        Some(FromAgent::Hello {
            outputs,
            width,
            height,
        }) => (outputs, width, height),
        other => panic!(
            "no Hello: {other:?}\n{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        ),
    };
    assert_eq!(
        hello,
        (2, 800, 600),
        "the agent announces the monitors of the desktop"
    );
    write_msg(&mut s, &ToAgent::SetPixels { raw: true }).unwrap();

    // One monitor in the client: scaled image, and the window comes over to the monitor that stays.
    assert_eq!(set_layout(&mut s, vec![rect(0, 640, 480)]), (true, 1));
    let frame = read_until(&mut s, |m| match m {
        FromAgent::VideoRaw {
            output,
            width,
            height,
            ..
        } => Some((output, width, height)),
        _ => None,
    });
    assert_eq!(frame, (0, 640, 480));
    let moved = wait_for("the window on Virtual-0", 10, || {
        let w = windows(&tmp, &mut round);
        (w.len() == 1 && w[0].1 == "Virtual-0").then(|| w[0].clone())
    });
    assert_eq!(moved.0, before.0);

    // The monitors of the desktop were not touched.
    let listing = String::from_utf8_lossy(
        &Command::new("kscreen-doctor")
            .arg("-o")
            .env("WAYLAND_DISPLAY", &socket)
            .output()
            .unwrap()
            .stdout,
    )
    .into_owned();
    assert_eq!(
        listing.matches("enabled").count() - listing.matches("disabled").count(),
        2,
        "{listing}"
    );
    assert!(
        listing.contains("Virtual-0") && listing.contains("Virtual-1"),
        "{listing}"
    );

    // Three monitors in the client (the desktop has two): the third is a virtual output, and nothing is moved.
    let (ok, outputs) = set_layout(
        &mut s,
        vec![rect(0, 800, 600), rect(800, 800, 600), rect(1600, 800, 600)],
    );
    assert!(
        ok && outputs == 3,
        "{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let mut seen = [false; 3];
    read_until(&mut s, |m| {
        if let FromAgent::VideoRaw { output, .. } = m {
            seen[output as usize] = true;
        }
        seen.iter().all(|s| *s).then_some(())
    });

    // The client leaves: the window goes back where it was.
    drop(s);
    let back = wait_for("the window back on Virtual-1", 20, || {
        let w = windows(&tmp, &mut round);
        (w.len() == 1 && w[0].1 == "Virtual-1").then(|| w[0].clone())
    });
    assert_eq!(back, before, "the window is back at its place");
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(!log_text.contains("cannot move"), "{log_text}");
    assert!(!log_text.contains("cannot put"), "{log_text}");

    // The client comes back with three monitors and leaves a window on the third one: with its monitor gone, the
    // window must be on a real monitor, and the desktop must have its two monitors again.
    let mut s = wait_for("the agent socket again", 30, || {
        UnixStream::connect(&agent_socket).ok()
    });
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    assert!(matches!(
        read_msg::<_, FromAgent>(&mut s).unwrap(),
        Some(FromAgent::Hello { outputs: 2, .. })
    ));
    write_msg(&mut s, &ToAgent::SetPixels { raw: true }).unwrap();
    let (ok, outputs) = set_layout(
        &mut s,
        vec![rect(0, 800, 600), rect(800, 800, 600), rect(1600, 800, 600)],
    );
    assert!(ok && outputs == 3);
    run_script(
        &tmp,
        "oxydesk-it-extra",
        "const out = workspace.screens.find(o => o.name !== 'Virtual-0' && o.name !== 'Virtual-1');\n\
         for (const w of workspace.windowList()) if (w.normalWindow) workspace.sendClientToScreen(w, out);\n",
    );
    wait_for("the window on the extra monitor", 10, || {
        let w = windows(&tmp, &mut round);
        (w.len() == 1 && w[0].1 != "Virtual-0" && w[0].1 != "Virtual-1").then_some(())
    });
    drop(s);
    wait_for(
        "the window on a real monitor, the extra one gone",
        20,
        || {
            let w = windows(&tmp, &mut round);
            let screens = Command::new("kscreen-doctor")
                .arg("-o")
                .env("WAYLAND_DISPLAY", &socket)
                .output()
                .unwrap();
            let screens = String::from_utf8_lossy(&screens.stdout).into_owned();
            (w.len() == 1
                && (w[0].1 == "Virtual-0" || w[0].1 == "Virtual-1")
                && screens.matches("Output:").count() == 2)
                .then_some(())
        },
    );
    // The extra monitor goes, and so does the virtual desktop that stood for it.
    let desks = desktop_names(&tmp, &mut round);
    assert!(
        !desks.iter().any(|d| d.starts_with("Oxydesk ")),
        "desktop kept: {desks:?}"
    );

    // Without `--virtual-outputs` (keep is the default), the extra monitor stays in the session after the client leaves, and so does
    // the window that is on it.
    drop(agent);
    let _ = std::fs::remove_file(&agent_socket);
    // The default: the extra monitors stay.
    let _agent = spawn_agent(&[]);
    let mut s = wait_for("the agent socket (keep)", 30, || {
        UnixStream::connect(&agent_socket).ok()
    });
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    assert!(matches!(
        read_msg::<_, FromAgent>(&mut s).unwrap(),
        Some(FromAgent::Hello { outputs: 2, .. })
    ));
    write_msg(&mut s, &ToAgent::SetPixels { raw: true }).unwrap();
    let (ok, outputs) = set_layout(
        &mut s,
        vec![rect(0, 800, 600), rect(800, 800, 600), rect(1600, 800, 600)],
    );
    assert!(ok && outputs == 3);
    run_script(
        &tmp,
        "oxydesk-it-extra-keep",
        "const out = workspace.screens.find(o => o.name !== 'Virtual-0' && o.name !== 'Virtual-1');\n\
         for (const w of workspace.windowList()) if (w.normalWindow) workspace.sendClientToScreen(w, out);\n",
    );
    let kept = wait_for("the window on the extra monitor", 10, || {
        let w = windows(&tmp, &mut round);
        (w.len() == 1 && w[0].1 != "Virtual-0" && w[0].1 != "Virtual-1").then(|| w[0].clone())
    });
    drop(s);
    std::thread::sleep(Duration::from_secs(3));
    let listing = |socket: &str| {
        let out = Command::new("kscreen-doctor")
            .arg("-o")
            .env("WAYLAND_DISPLAY", socket)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert_eq!(
        listing(&socket).matches("Output:").count(),
        3,
        "the extra monitor stays"
    );
    // The window is parked on a real monitor, on the virtual desktop of the extra monitor, which stays too.
    let parked = wait_for("the window parked on its desktop", 20, || {
        let w = windows(&tmp, &mut round);
        (w.len() == 1 && w[0].4 == "Oxydesk Virtual-2").then(|| w[0].clone())
    });
    assert!(
        parked.1 == "Virtual-0" || parked.1 == "Virtual-1",
        "{parked:?} (was {kept:?})"
    );
    assert!(desktop_names(&tmp, &mut round).contains(&"Oxydesk Virtual-2".to_string()));
    // The next client finds the monitor as it was left, and takes the window back onto it.
    let mut s = UnixStream::connect(&agent_socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    assert!(matches!(
        read_msg::<_, FromAgent>(&mut s).unwrap(),
        Some(FromAgent::Hello { outputs: 3, .. })
    ));
    write_msg(&mut s, &ToAgent::SetPixels { raw: true }).unwrap();
    let (ok, outputs) = set_layout(
        &mut s,
        vec![rect(0, 800, 600), rect(800, 800, 600), rect(1600, 800, 600)],
    );
    assert!(ok && outputs == 3);
    let back = wait_for("the window back on the extra monitor", 10, || {
        let w = windows(&tmp, &mut round);
        (w.len() == 1 && w[0].1 == kept.1 && !w[0].4.starts_with("Oxydesk ")).then(|| w[0].clone())
    });
    assert_eq!(back.1, kept.1);
    drop(s);
    drop(kwin);
    let _ = std::fs::remove_dir_all(&tmp);
}
