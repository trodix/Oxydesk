//! Server configuration file (`/etc/oxydesk/oxydesk.conf`): pure parsing and merging with the command line.
//!
//! Format: one `key = value` per line, keys are the long options of `oxydesk` without the `--`. `#` or `;` at
//! the start of a line starts a comment (no trailing comments: a value may contain `#`). Blank lines are ignored,
//! a value may be wrapped in double quotes. Unknown keys are an error (a typo must not be silently ignored).
//!
//! Precedence, highest first: command line, drop-in files (`oxydesk.conf.d/*.conf`, in name order), main file,
//! built-in defaults. A key given on the command line replaces the file's value (for a repeatable key such as
//! `session-app`, the whole list); the keys that choose the mode (`test-mode`, `login`, `session`) replace each
//! other as a group.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// `key = value` → `--key value`.
    Value,
    /// `key = yes|no` → `--key` or nothing.
    Flag,
    /// May appear several times: `--key v1 --key v2`.
    Repeated,
}

/// Every key the file knows. `MODE_KEYS` below are the ones that choose what the server does.
const KEYS: &[(&str, Kind)] = &[
    ("test-mode", Kind::Flag),
    ("login", Kind::Flag),
    ("session", Kind::Value),
    ("session-policy", Kind::Value),
    ("virtual-outputs", Kind::Value),
    ("listen", Kind::Value),
    ("state-dir", Kind::Value),
    ("width", Kind::Value),
    ("height", Kind::Value),
    ("outputs", Kind::Value),
    ("fps", Kind::Value),
    ("encoder", Kind::Value),
    ("dmabuf", Kind::Value),
    ("audio", Kind::Value),
    ("adaptive-bitrate", Kind::Value),
    ("nla-user", Kind::Value),
    ("max-login-failures", Kind::Value),
    ("max-user-failures", Kind::Value),
    ("login-lockout-secs", Kind::Value),
    ("reconnect-timeout", Kind::Value),
    ("greeter-theme", Kind::Value),
    ("dead-peer-timeout", Kind::Value),
    ("authd-socket", Kind::Value),
    ("kwin-socket", Kind::Value),
    ("session-app", Kind::Repeated),
    ("drives-dir", Kind::Value),
    ("printers", Kind::Value),
    ("printers-cups", Kind::Value),
    ("printer-mode", Kind::Value),
];

const MODE_KEYS: &[&str] = &["test-mode", "login", "session"];

#[derive(Debug, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

fn kind_of(key: &str) -> Option<Kind> {
    KEYS.iter().find(|(k, _)| *k == key).map(|(_, kind)| *kind)
}

/// One file's `(key, value)` pairs, in order. `source` only names the file in error messages.
pub fn parse(text: &str, source: &str) -> Result<Vec<(String, String)>, ConfigError> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let at = format!("{source}:{}", i + 1);
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| ConfigError(format!("{at}: expected `key = value`, got `{line}`")))?;
        let key = key.trim();
        if kind_of(key).is_none() {
            return Err(ConfigError(format!("{at}: unknown key `{key}`")));
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        if value.is_empty() {
            return Err(ConfigError(format!("{at}: `{key}` has no value")));
        }
        out.push((key.to_owned(), value.to_owned()));
    }
    Ok(out)
}

/// Stacks the layers (main file first, then the drop-ins in order): a later layer replaces the keys it sets.
pub fn combine(layers: &[Vec<(String, String)>]) -> Vec<(String, Vec<String>)> {
    let mut merged: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for layer in layers {
        let mut here: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (key, value) in layer {
            let values = here.entry(key).or_default();
            if kind_of(key) != Some(Kind::Repeated) {
                values.clear();
            }
            values.push(value.clone());
        }
        for (key, values) in here {
            merged.insert(key.to_owned(), values);
        }
    }
    merged.into_iter().collect()
}

fn truthy(key: &str, value: &str) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" | "1" => Ok(true),
        "no" | "false" | "off" | "0" => Ok(false),
        _ => Err(ConfigError(format!("`{key}`: `{value}` (yes|no)"))),
    }
}

/// The command line followed by the file's settings that it does not already give. The program reads the first
/// occurrence of an option, and the command line is first: it wins.
pub fn merge_args(
    cli: &[String],
    file: &[(String, Vec<String>)],
) -> Result<Vec<String>, ConfigError> {
    let given = |key: &str| cli.iter().any(|a| a.strip_prefix("--") == Some(key));
    let mode_on_cli = MODE_KEYS.iter().any(|k| given(k));
    let mut args = cli.to_vec();
    for (key, values) in file {
        if given(key) || (mode_on_cli && MODE_KEYS.contains(&key.as_str())) {
            continue;
        }
        match kind_of(key) {
            Some(Kind::Flag) => {
                if truthy(key, &values[0])? {
                    args.push(format!("--{key}"));
                }
            }
            _ => {
                for v in values {
                    args.push(format!("--{key}"));
                    args.push(v.clone());
                }
            }
        }
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn file(text: &str) -> Vec<(String, Vec<String>)> {
        combine(&[parse(text, "t").unwrap()])
    }

    #[test]
    fn parses_comments_blanks_quotes_and_spaces() {
        let text = "# c\n; c\n\nlisten = 0.0.0.0:3390\n  fps=\"24\"  \nprinter-mode = xps,HP*=raw:pcl # kept\n";
        assert_eq!(
            parse(text, "t").unwrap(),
            vec![
                ("listen".into(), "0.0.0.0:3390".into()),
                ("fps".into(), "24".into()),
                ("printer-mode".into(), "xps,HP*=raw:pcl # kept".into()),
            ]
        );
    }

    #[test]
    fn reconnection_keys_are_accepted_and_become_options() {
        let args = merge_args(&[], &file("reconnect-timeout = 15\ndead-peer-timeout = 0")).unwrap();
        assert_eq!(
            args,
            s(&["--dead-peer-timeout", "0", "--reconnect-timeout", "15"])
        );
    }

    #[test]
    fn the_session_policy_is_a_plain_option_not_a_mode() {
        let args = merge_args(&s(&["--login"]), &file("session-policy = auto")).unwrap();
        assert_eq!(args, s(&["--login", "--session-policy", "auto"]));
    }

    #[test]
    fn rejects_unknown_keys_missing_equals_and_empty_values() {
        assert!(parse("lisen = 1", "t")
            .unwrap_err()
            .0
            .contains("t:1: unknown key `lisen`"));
        assert!(parse("\nlisten", "t").unwrap_err().0.contains("t:2"));
        assert!(parse("fps =", "t").unwrap_err().0.contains("no value"));
    }

    #[test]
    fn command_line_wins_over_file() {
        let args = merge_args(&s(&["--fps", "60"]), &file("fps = 24\nwidth = 1280")).unwrap();
        assert_eq!(args, s(&["--fps", "60", "--width", "1280"]));
        // The program takes the first occurrence: the command line's.
        let first = args.windows(2).find(|w| w[0] == "--fps").unwrap();
        assert_eq!(first[1], "60");
    }

    #[test]
    fn flags_follow_yes_and_no() {
        assert_eq!(
            merge_args(&[], &file("login = yes")).unwrap(),
            s(&["--login"])
        );
        assert!(merge_args(&[], &file("login = no")).unwrap().is_empty());
        assert!(merge_args(&[], &file("login = maybe")).is_err());
    }

    #[test]
    fn a_mode_on_the_command_line_replaces_the_files_mode() {
        let f = file("login = yes\nlisten = 0.0.0.0:3390");
        let args = merge_args(&s(&["--test-mode"]), &f).unwrap();
        assert_eq!(args, s(&["--test-mode", "--listen", "0.0.0.0:3390"]));
        let args = merge_args(&s(&["--session", "kwin"]), &file("test-mode = yes")).unwrap();
        assert_eq!(args, s(&["--session", "kwin"]));
    }

    #[test]
    fn a_repeated_key_on_the_command_line_replaces_the_whole_list() {
        let f = file("session-app = a\nsession-app = b");
        assert_eq!(
            merge_args(&[], &f).unwrap(),
            s(&["--session-app", "a", "--session-app", "b"])
        );
        assert_eq!(
            merge_args(&s(&["--session-app", "c"]), &f).unwrap(),
            s(&["--session-app", "c"])
        );
    }

    #[test]
    fn dropins_override_the_main_file_key_by_key() {
        let main = parse("fps = 24\nsession-app = a\nsession-app = b", "m").unwrap();
        let drop = parse("fps = 30\nsession-app = z", "d").unwrap();
        let merged = combine(&[main, drop]);
        assert_eq!(
            merge_args(&[], &merged).unwrap(),
            s(&["--fps", "30", "--session-app", "z"])
        );
    }
}
