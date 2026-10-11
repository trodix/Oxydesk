//! Keyboard layout of the KWin session: `kxkbrc` (group `Layout`) written with `kwriteconfig6`, then KWin
//! is told to reload it (signal `reloadConfig` of `org.kde.keyboard`, the one sent by the keyboard KCM).
//!
//! The layout belongs to the session user (service or login user). `RDSKTP_KEYBOARD_LAYOUT=off` disables
//! the adoption of the client's layout, for a user who shares the D-Bus session of a real desktop.

use std::process::{Command, Stdio};

use rdsktp_core::{InputError, XkbLayout};

/// `kwriteconfig6` arguments (one command per key) for a single-layout configuration.
pub fn config_commands(layout: &XkbLayout) -> Vec<Vec<String>> {
    let key = |k: &str, v: &str| {
        ["--file", "kxkbrc", "--group", "Layout", "--key", k]
            .iter()
            .map(|s| s.to_string())
            .chain([v.to_string()])
            .collect::<Vec<_>>()
    };
    vec![
        key("Use", "true"),
        key("LayoutList", &layout.layout),
        key("VariantList", &layout.variant),
        key("DisplayNames", ""),
    ]
}

pub fn disabled() -> bool {
    std::env::var("RDSKTP_KEYBOARD_LAYOUT").is_ok_and(|v| v == "off")
}

pub fn apply(layout: &XkbLayout) -> Result<(), InputError> {
    if disabled() {
        return Ok(());
    }
    let run = |prog: &str, args: &[String]| -> Result<(), InputError> {
        let status = Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| InputError::Backend(format!("{prog}: {e}")))?;
        status
            .success()
            .then_some(())
            .ok_or_else(|| InputError::Backend(format!("{prog}: {status}")))
    };
    for args in config_commands(layout) {
        run("kwriteconfig6", &args)?;
    }
    run(
        "dbus-send",
        &[
            "--session".into(),
            "--type=signal".into(),
            "/Layouts".into(),
            "org.kde.keyboard.reloadConfig".into(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_for_swiss_french() {
        let cmds = config_commands(&XkbLayout::new("ch", "fr"));
        let find = |k: &str| {
            cmds.iter()
                .find(|c| c.contains(&k.to_string()))
                .unwrap()
                .last()
                .unwrap()
                .clone()
        };
        assert_eq!(find("LayoutList"), "ch");
        assert_eq!(find("VariantList"), "fr");
        assert_eq!(find("Use"), "true");
    }
}
