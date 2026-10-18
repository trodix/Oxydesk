//! Moving the windows of the user's own KWin (a KWin script, loaded over D-Bus on the session bus).
//!
//! A script that moved windows stays loaded: it keeps where each one was and puts it back when its global shortcut
//! is invoked (`kglobalaccel`), which is how the windows return to their place when the client leaves.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use oxydesk_core::error::CaptureError;
use oxydesk_core::ports::WindowMover;

/// Scripts loaded by `gather` and not yet unloaded: (script name, shortcut name).
#[derive(Default)]
pub struct KwinWindowMover {
    loaded: Vec<(String, String)>,
}

fn quoted_list(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|n| format!("\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("[{}]", quoted.join(","))
}

/// Script that sends every application window found on an output outside `keep` to the first output of `keep`,
/// remembering where each one was. Invoking the global shortcut `shortcut` sends back the ones that are still on
/// that first output (a window the user moved elsewhere since is left alone). Panels, desktops and other shell
/// surfaces are not "normal" windows and stay where they are.
fn gather_script(keep: &[String], shortcut: &str, name: &str) -> String {
    let names = quoted_list(keep);
    let key = quoted_list(&[shortcut.to_owned(), name.to_owned()]);
    format!(
        "const keep = {names};\n\
         const shortcut = {key}[0];\n\
         const scriptName = {key}[1];\n\
         const saved = [];\n\
         const target = workspace.screens.find(o => o.name === keep[0]);\n\
         if (target) {{\n\
         \tfor (const w of workspace.windowList()) {{\n\
         \t\tif (!(w.normalWindow || w.dialog) || !w.output || keep.includes(w.output.name)) continue;\n\
         \t\tconst g = w.frameGeometry;\n\
         \t\tsaved.push({{id: w.internalId.toString(), output: w.output.name, x: g.x, y: g.y, width: g.width, height: g.height}});\n\
         \t\tworkspace.sendClientToScreen(w, target);\n\
         \t}}\n\
         }}\n\
         registerShortcut(shortcut, shortcut, \"\", function () {{\n\
         \tconst all = workspace.windowList();\n\
         \tfor (const s of saved) {{\n\
         \t\tconst w = all.find(c => c.internalId.toString() === s.id);\n\
         \t\tconst out = workspace.screens.find(o => o.name === s.output);\n\
         \t\tif (!w || !out || !target || !w.output || w.output.name !== target.name) continue;\n\
         \t\tworkspace.sendClientToScreen(w, out);\n\
         \t\tif (!w.fullScreen) w.frameGeometry = {{x: s.x, y: s.y, width: s.width, height: s.height}};\n\
         \t}}\n\
         \tsaved.length = 0;\n\
         \tcallDBus(\"org.kde.KWin\", \"/Scripting\", \"org.kde.kwin.Scripting\", \"unloadScript\", scriptName);\n\
         }});\n"
    )
}

/// Identifier of the script KWin loaded, from the reply of `busctl` (`i 7`).
fn script_id(reply: &str) -> Option<u32> {
    let id: i64 = reply.trim().strip_prefix("i ")?.trim().parse().ok()?;
    u32::try_from(id).ok()
}

fn busctl(args: &[&str]) -> Result<String, CaptureError> {
    let out = Command::new("busctl")
        .arg("--user")
        .arg("call")
        .args(args)
        .output()
        .map_err(|e| CaptureError::Unavailable(format!("busctl: {e}")))?;
    if !out.status.success() {
        return Err(CaptureError::Unavailable(format!(
            "busctl: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn unload(name: &str) {
    let _ = busctl(&[
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
        "unloadScript",
        "s",
        name,
    ]);
}

/// Script for `claim`: the desktop of each extra monitor exists, and its windows come to the monitor.
fn claim_script(extra: &[String]) -> String {
    let outputs = quoted_list(extra);
    format!(
        "for (const name of {outputs}) {{\n\
         \tconst title = \"Oxydesk \" + name;\n\
         \tlet desk = workspace.desktops.find(d => d.name === title);\n\
         \tif (!desk) {{ workspace.createDesktop(workspace.desktops.length, title); desk = workspace.desktops.find(d => d.name === title); }}\n\
         \tconst out = workspace.screens.find(o => o.name === name);\n\
         \tif (!desk || !out) continue;\n\
         \tfor (const w of workspace.windowList()) {{\n\
         \t\tif (w.desktops.length !== 1 || w.desktops[0].name !== title) continue;\n\
         \t\tw.desktops = [workspace.currentDesktop];\n\
         \t\tworkspace.sendClientToScreen(w, out);\n\
         \t}}\n\
         }}\n"
    )
}

/// Script for `park`: the windows of each extra monitor go to `home`; kept, they are parked on the desktop of their
/// monitor (created if missing), else that desktop is removed.
fn park_script(extra: &[String], home: &str, keep: bool) -> String {
    let outputs = quoted_list(extra);
    let home = quoted_list(&[home.to_owned()]);
    format!(
        "const keep = {keep};\n\
         const home = workspace.screens.find(o => o.name === {home}[0]);\n\
         for (const name of {outputs}) {{\n\
         \tconst title = \"Oxydesk \" + name;\n\
         \tlet desk = workspace.desktops.find(d => d.name === title);\n\
         \tif (keep && !desk) {{ workspace.createDesktop(workspace.desktops.length, title); desk = workspace.desktops.find(d => d.name === title); }}\n\
         \tfor (const w of workspace.windowList()) {{\n\
         \t\tif (!(w.normalWindow || w.dialog) || !w.output || w.output.name !== name) continue;\n\
         \t\tif (home) workspace.sendClientToScreen(w, home);\n\
         \t\tif (keep && desk) w.desktops = [desk];\n\
         \t}}\n\
         \tif (!keep && desk) workspace.removeDesktop(desk);\n\
         }}\n"
    )
}

/// Loads, runs and unloads a script that has nothing to keep.
fn run_once(source: &str) -> Result<(), CaptureError> {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let name = format!("oxydesk-once-{}-{n}", std::process::id());
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let file = dir.join(format!("{name}.js"));
    std::fs::write(&file, source)
        .map_err(|e| CaptureError::Unavailable(format!("{}: {e}", file.display())))?;
    let result = (|| {
        let path = file.to_string_lossy();
        let reply = busctl(&[
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
            "loadScript",
            "ss",
            &path,
            &name,
        ])?;
        let id = script_id(&reply).ok_or_else(|| {
            CaptureError::Unavailable(format!("KWin refused the script ({})", reply.trim()))
        })?;
        let run = busctl(&[
            "org.kde.KWin",
            &format!("/Scripting/Script{id}"),
            "org.kde.kwin.Script",
            "run",
        ]);
        unload(&name);
        run.map(|_| ())
    })();
    let _ = std::fs::remove_file(&file);
    result
}

/// Waits until KWin no longer holds the script `name` (it removes itself when its windows are back).
fn wait_unloaded(name: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let held = busctl(&[
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
            "isScriptLoaded",
            "s",
            name,
        ]);
        if held.is_ok_and(|r| r.trim() == "b false") {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

impl WindowMover for KwinWindowMover {
    fn gather(&mut self, keep: &[String]) -> Result<(), CaptureError> {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        if keep.is_empty() {
            return Ok(());
        }
        // KWin refuses a script name it already holds: each run has its own.
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let name = format!("oxydesk-gather-{}-{n}", std::process::id());
        let shortcut = format!("oxydesk-restore-{}-{n}", std::process::id());
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let file = dir.join(format!("{name}.js"));
        std::fs::write(&file, gather_script(keep, &shortcut, &name))
            .map_err(|e| CaptureError::Unavailable(format!("{}: {e}", file.display())))?;
        let result = (|| {
            let path = file.to_string_lossy();
            let reply = busctl(&[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting",
                "loadScript",
                "ss",
                &path,
                &name,
            ])?;
            let id = script_id(&reply).ok_or_else(|| {
                CaptureError::Unavailable(format!("KWin refused the script ({})", reply.trim()))
            })?;
            let object = format!("/Scripting/Script{id}");
            match busctl(&["org.kde.KWin", &object, "org.kde.kwin.Script", "run"]) {
                Ok(_) => {
                    // Left loaded: it holds the places the windows came from.
                    self.loaded.push((name.clone(), shortcut.clone()));
                    Ok(())
                }
                Err(e) => {
                    unload(&name);
                    Err(e)
                }
            }
        })();
        let _ = std::fs::remove_file(&file);
        result
    }

    fn claim(&mut self, extra: &[String]) -> Result<(), CaptureError> {
        if extra.is_empty() {
            return Ok(());
        }
        run_once(&claim_script(extra))
    }

    fn park(&mut self, extra: &[String], home: &str, keep: bool) -> Result<(), CaptureError> {
        if extra.is_empty() {
            return Ok(());
        }
        run_once(&park_script(extra, home, keep))
    }

    fn restore(&mut self) -> Result<(), CaptureError> {
        let mut failure = None;
        // Last move first: a window moved twice ends where it began.
        for (name, shortcut) in self.loaded.drain(..).rev() {
            // The shortcut reaches KWin through kglobalaccel, after this call has returned: the script unloads itself
            // when it is done. Unloading it from here could remove it before it ran.
            if let Err(e) = busctl(&[
                "org.kde.kglobalaccel",
                "/component/kwin",
                "org.kde.kglobalaccel.Component",
                "invokeShortcut",
                "s",
                &shortcut,
            ]) {
                failure = Some(e);
                unload(&name);
            } else if !wait_unloaded(&name) {
                // It never ran (no kglobalaccel?): removed here, and the windows stay where they are.
                failure = Some(CaptureError::Unavailable(
                    "KWin did not run the restore shortcut".into(),
                ));
                unload(&name);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_names_the_outputs_that_stay() {
        let s = gather_script(&["HDMI-A-1".into(), "DP \"2\"".into()], "restore-1", "g-1");
        assert!(s.contains(r#"const keep = ["HDMI-A-1","DP \"2\""];"#));
        assert!(s.contains("sendClientToScreen"));
        assert!(s.contains("keep[0]"));
    }

    #[test]
    fn the_script_remembers_the_places_and_puts_them_back_on_its_shortcut() {
        let s = gather_script(&["A".into()], "oxydesk-restore-9-0", "oxydesk-gather-9-0");
        assert!(s.contains(r#"const shortcut = ["oxydesk-restore-9-0","oxydesk-gather-9-0"][0];"#));
        assert!(s.contains("saved.push"));
        assert!(s.contains("registerShortcut(shortcut"));
        // It removes itself once the windows are back.
        assert!(s.contains("unloadScript\", scriptName"));
        // Only a window still on the output it was brought to is sent back.
        assert!(s.contains("w.output.name !== target.name"));
    }

    #[test]
    fn each_extra_monitor_has_a_desktop_that_gives_its_windows_back_to_it() {
        let s = claim_script(&["Virtual-2".into(), "Virtual-3".into()]);
        assert!(s.contains(r#"["Virtual-2","Virtual-3"]"#));
        assert!(s.contains("createDesktop") && s.contains(r#""Oxydesk " + name"#));
        // Only the windows parked on that desktop alone come over, onto the current desktop.
        assert!(s.contains("w.desktops.length !== 1"));
        assert!(s.contains("w.desktops = [workspace.currentDesktop]"));
        assert!(s.contains("sendClientToScreen(w, out)"));
    }

    #[test]
    fn leaving_parks_the_windows_on_the_desktop_or_removes_it() {
        let kept = park_script(&["Virtual-2".into()], "HDMI-A-1", true);
        assert!(kept.contains("const keep = true;"));
        assert!(kept.contains(r#"["HDMI-A-1"][0]"#));
        assert!(kept.contains("w.desktops = [desk]"));
        let removed = park_script(&["Virtual-2".into()], "HDMI-A-1", false);
        assert!(removed.contains("const keep = false;"));
        assert!(removed.contains("removeDesktop(desk)"));
    }

    #[test]
    fn the_script_id_comes_from_the_busctl_reply() {
        assert_eq!(script_id("i 7\n"), Some(7));
        assert_eq!(script_id("i 0\n"), Some(0));
        assert_eq!(script_id("i -1\n"), None);
        assert_eq!(script_id("b true"), None);
        assert_eq!(script_id(""), None);
    }
}
