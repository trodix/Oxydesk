//! Drives redirected by the client (RDPDR): a drive of the client's machine seen as a tree of files.
//!
//! `RemoteDrive` is the port the desktop side uses (a FUSE mount, ...): blocking, addressed by
//! path, and not tied to any protocol. `DriveHost` is what the RDP side calls when the client
//! announces or removes a drive. Nothing is buffered beyond one read or write block, so files of
//! several gigabytes go through as a stream.

use std::sync::Arc;

use crate::error::DriveError;

/// Longest block moved in one read or write (what one RDPDR request carries).
pub const MAX_BLOCK: u32 = 1024 * 1024;

/// Metadata of a file or directory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DriveStat {
    pub size: u64,
    pub is_dir: bool,
    pub read_only: bool,
    /// Seconds since the Unix epoch.
    pub modified: i64,
    pub accessed: i64,
    pub created: i64,
}

/// An entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveEntry {
    /// Name only: no separator, never `.` or `..`.
    pub name: String,
    pub stat: DriveStat,
}

/// Free and total space of a drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DriveSpace {
    pub total: u64,
    pub available: u64,
}

/// How a file is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Existing file, reading.
    Read,
    /// Existing file, reading and writing.
    ReadWrite,
    /// Existing file, emptied, reading and writing.
    Truncate,
    /// New file (fails if it exists), reading and writing.
    Create,
}

/// An open file; opaque to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DriveHandle(pub u64);

/// A drive of the client. Paths are relative to the drive, `/`-separated, without `.` or `..`;
/// the root is the empty string.
pub trait RemoteDrive: Send + Sync {
    fn stat(&self, path: &str) -> Result<DriveStat, DriveError>;
    fn list(&self, path: &str) -> Result<Vec<DriveEntry>, DriveError>;
    fn open(&self, path: &str, mode: OpenMode) -> Result<DriveHandle, DriveError>;
    /// Up to `len` bytes at `offset`; fewer only at the end of the file.
    fn read(&self, file: DriveHandle, offset: u64, len: u32) -> Result<Vec<u8>, DriveError>;
    /// Returns the number of bytes written.
    fn write(&self, file: DriveHandle, offset: u64, data: &[u8]) -> Result<u32, DriveError>;
    fn flush(&self, file: DriveHandle) -> Result<(), DriveError>;
    fn close(&self, file: DriveHandle);
    fn mkdir(&self, path: &str) -> Result<(), DriveError>;
    /// Deletes a file or an empty directory.
    fn remove(&self, path: &str, is_dir: bool) -> Result<(), DriveError>;
    /// Moves `from` to `to`, replacing a file already there.
    fn rename(&self, from: &str, to: &str) -> Result<(), DriveError>;
    fn truncate(&self, path: &str, size: u64) -> Result<(), DriveError>;
    fn set_modified(&self, path: &str, seconds: i64) -> Result<(), DriveError>;
    fn space(&self) -> Result<DriveSpace, DriveError>;
}

/// Where the drives announced by the client are made available to the desktop.
pub trait DriveHost: Send + Sync {
    /// A drive appeared; `name` is a valid directory name. Called outside the connection thread.
    fn added(&self, name: &str, drive: Arc<dyn RemoteDrive>);
    /// The drive is gone (client removed it or disconnected); its files must stop being served.
    fn removed(&self, name: &str);
}

/// Makes `name` usable as the name of a directory (a client name or a DOS name such as `C:`).
pub fn directory_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '.' || c == '_' || c == ' ');
    if cleaned.is_empty() {
        "drive".into()
    } else {
        cleaned.chars().take(64).collect()
    }
}

/// Whether `path` is a valid relative drive path.
pub fn is_valid_path(path: &str) -> bool {
    path.is_empty()
        || path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains(['\0', '\\']))
}

/// Joins a directory path and a name.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_names_are_safe() {
        assert_eq!(directory_name("C:"), "C");
        assert_eq!(directory_name("../etc"), "etc");
        assert_eq!(directory_name("My Docs"), "My Docs");
        assert_eq!(directory_name("a/b\\c"), "a_b_c");
        assert_eq!(directory_name(".."), "drive");
        assert_eq!(directory_name(""), "drive");
    }

    #[test]
    fn paths_are_validated() {
        assert!(is_valid_path(""));
        assert!(is_valid_path("a/b.txt"));
        assert!(!is_valid_path("/a"));
        assert!(!is_valid_path("a//b"));
        assert!(!is_valid_path("a/../b"));
        assert!(!is_valid_path("a\\b"));
        assert_eq!(join("", "x"), "x");
        assert_eq!(join("d", "x"), "d/x");
    }
}
