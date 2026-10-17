//! Files of the local file system as a clipboard `FileList`: `text/uri-list` parsing, directory
//! walk, and a source that reads ranges on demand.

use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oxydesk_core::error::ClipboardError;
use oxydesk_core::files::{FileEntry, FileList, FileSource, MAX_ENTRIES};

/// Largest range served by one `read` call.
const MAX_READ: u32 = 8 * 1024 * 1024;

fn unhex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(unhex(*b.get(i + 1)?)? << 4 | unhex(*b.get(i + 2)?)?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// `file:///a%20b` -> `/a b`; `None` for anything but a local absolute file URI.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = uri.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let bytes = percent_decode(path)?;
    if bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// `/a b` -> `file:///a%20b`.
pub fn path_to_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The local paths of a `text/uri-list`; `None` if one line is not a local file URI (a list of web
/// addresses is not a list of files).
pub fn parse_uri_list(text: &str) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        paths.push(uri_to_path(line)?);
    }
    (!paths.is_empty()).then_some(paths)
}

/// Lines of a `text/uri-list` (CRLF-terminated, as the specification wants).
pub fn format_uri_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| path_to_uri(p) + "\r\n")
        .collect::<String>()
}

fn entry_of(path: String, meta: &std::fs::Metadata) -> FileEntry {
    FileEntry {
        path,
        size: if meta.is_dir() { 0 } else { meta.len() },
        is_dir: meta.is_dir(),
        modified: meta.mtime(),
        read_only: meta.mode() & 0o200 == 0,
    }
}

/// Adds `path` (and its content if it is a directory) under the name `rel`.
fn walk(path: &Path, rel: String, out: &mut Vec<(FileEntry, PathBuf)>) {
    if out.len() >= MAX_ENTRIES {
        return;
    }
    // Symbolic links to directories are not followed (loops); links to files are.
    let Ok(link) = std::fs::symlink_metadata(path) else {
        return;
    };
    let meta = if link.file_type().is_symlink() {
        match std::fs::metadata(path) {
            Ok(m) if !m.is_dir() => m,
            _ => return,
        }
    } else {
        link
    };
    if !meta.is_dir() && !meta.is_file() {
        return; // sockets, devices, pipes
    }
    out.push((entry_of(rel.clone(), &meta), path.to_path_buf()));
    if meta.is_dir() {
        let Ok(rd) = std::fs::read_dir(path) else {
            return;
        };
        let mut children: Vec<_> = rd.flatten().collect();
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            walk(&child.path(), format!("{rel}/{name}"), out);
        }
    }
}

/// Reads the files of a list from the local file system.
pub struct LocalSource {
    paths: Vec<PathBuf>,
    /// The file being read (a transfer reads one file sequentially).
    open: Mutex<Option<(u32, Arc<File>)>>,
}

impl FileSource for LocalSource {
    fn read(&self, index: u32, offset: u64, len: u32) -> Result<Vec<u8>, ClipboardError> {
        let err = |e: std::io::Error| ClipboardError::Backend(format!("reading a file: {e}"));
        let file = {
            let mut open = self.open.lock().expect("open file");
            match open.as_ref() {
                Some((i, f)) if *i == index => f.clone(),
                _ => {
                    let path = self.paths.get(index as usize).ok_or_else(|| {
                        ClipboardError::Backend("no such file in the list".into())
                    })?;
                    let f = Arc::new(File::open(path).map_err(err)?);
                    *open = Some((index, f.clone()));
                    f
                }
            }
        };
        let mut buf = vec![0u8; len.min(MAX_READ) as usize];
        let mut done = 0;
        while done < buf.len() {
            match file.read_at(&mut buf[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(err(e)),
            }
        }
        buf.truncate(done);
        Ok(buf)
    }
}

/// The file list of `roots` (files and directories, directories walked recursively). Roots with
/// the same name as an earlier one are dropped: the collection cannot hold both.
pub fn list_from_paths(roots: &[PathBuf]) -> Option<FileList> {
    let mut found = Vec::new();
    let mut names = std::collections::HashSet::new();
    for root in roots {
        let Some(name) = root.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if names.insert(name.clone()) {
            walk(root, name, &mut found);
        }
    }
    found.retain(|(e, _)| e.is_valid());
    if found.is_empty() {
        return None;
    }
    let (entries, paths): (Vec<_>, Vec<_>) = found.into_iter().unzip();
    Some(FileList {
        entries,
        source: Arc::new(LocalSource {
            paths,
            open: Mutex::new(None),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_roundtrip_with_special_characters() {
        let p = PathBuf::from("/tmp/a b/é€#%.txt");
        let uri = path_to_uri(&p);
        assert_eq!(uri, "file:///tmp/a%20b/%C3%A9%E2%82%AC%23%25.txt");
        assert_eq!(uri_to_path(&uri), Some(p));
        assert_eq!(uri_to_path("file://localhost/x"), Some(PathBuf::from("/x")));
    }

    #[test]
    fn only_local_file_uris_make_a_file_list() {
        assert_eq!(
            parse_uri_list("# c\r\nfile:///a\r\n\r\nfile:///b%20c\n"),
            Some(vec![PathBuf::from("/a"), PathBuf::from("/b c")])
        );
        assert_eq!(parse_uri_list("https://example.com/x.png\r\n"), None);
        assert_eq!(parse_uri_list("file://host/x\r\n"), None);
        assert_eq!(parse_uri_list("file:///a%2\r\n"), None);
        assert_eq!(parse_uri_list(""), None);
    }

    #[test]
    fn walks_directories_and_reads_ranges() {
        let dir = std::env::temp_dir().join(format!("oxydesk-lf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("d/sub")).unwrap();
        std::fs::write(dir.join("d/b.txt"), b"0123456789").unwrap();
        std::fs::write(dir.join("d/sub/e"), b"").unwrap();
        std::fs::write(dir.join("top"), b"abc").unwrap();
        std::os::unix::fs::symlink(dir.join("d"), dir.join("d/loop")).unwrap();
        let list = list_from_paths(&[dir.join("d"), dir.join("top")]).unwrap();
        let paths: Vec<_> = list.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["d", "d/b.txt", "d/sub", "d/sub/e", "top"]);
        assert!(list.entries[0].is_dir && !list.entries[1].is_dir);
        assert_eq!(list.entries[1].size, 10);
        assert_eq!(list.source.read(1, 3, 4).unwrap(), b"3456");
        assert_eq!(list.source.read(1, 8, 100).unwrap(), b"89"); // end of file
        assert_eq!(list.source.read(4, 0, 100).unwrap(), b"abc");
        assert!(list.source.read(9, 0, 1).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
