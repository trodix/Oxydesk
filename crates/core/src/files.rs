//! Files exchanged through the clipboard: a list of entries plus a way to read their contents.
//!
//! The contents are never held in memory as a whole: `FileSource::read` is called for ranges, so
//! files of several gigabytes are transferred as a stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::ClipboardError;

/// Longest path (in UTF-16 units, separators included) that CLIPRDR can carry.
pub const MAX_PATH_UNITS: usize = 259;
/// Most entries of one list (same cap as CLIPRDR's).
pub const MAX_ENTRIES: usize = 100_000;

/// One file or directory of a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path relative to the copied collection, `/`-separated, never absolute, no `.` or `..`.
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    /// Last modification, seconds since the Unix epoch.
    pub modified: i64,
    pub read_only: bool,
}

impl FileEntry {
    /// Whether the path is acceptable on every side: relative, without empty or special
    /// components, not too long for CLIPRDR.
    pub fn is_valid(&self) -> bool {
        !self.path.is_empty()
            && self.path.encode_utf16().count() <= MAX_PATH_UNITS
            && self
                .path
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains(['\0', '\\']))
    }

    /// Whether the entry is at the top of the collection (what an application pastes).
    pub fn is_root(&self) -> bool {
        !self.path.contains('/')
    }
}

/// Reads ranges of the files of a list; blocking.
pub trait FileSource: Send + Sync {
    /// Up to `len` bytes of entry `index` starting at `offset`; fewer only at the end of the file.
    fn read(&self, index: u32, offset: u64, len: u32) -> Result<Vec<u8>, ClipboardError>;
}

/// A list of files and where their contents come from.
#[derive(Clone)]
pub struct FileList {
    pub entries: Vec<FileEntry>,
    pub source: Arc<dyn FileSource>,
}

impl std::fmt::Debug for FileList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileList")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl FileList {
    /// Keeps the valid entries only. Indices of the result are the ones every side must use, so the
    /// filtering is done once, by whoever builds the list.
    pub fn retain_valid(mut entries: Vec<FileEntry>) -> Vec<FileEntry> {
        entries.retain(FileEntry::is_valid);
        entries.truncate(MAX_ENTRIES);
        entries
    }
}

/// Longest range read in one request between two processes.
pub const MAX_RANGE: u32 = 4 * 1024 * 1024;
/// A range request that gets no answer for this long fails.
const RANGE_TIMEOUT: Duration = Duration::from_secs(90);
/// Lists kept readable for the other side (the current one and the previous, still being read).
const KEPT_LISTS: usize = 2;

/// A range of a file asked to the other process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeRequest {
    pub req_id: u32,
    pub list_id: u32,
    pub index: u32,
    pub offset: u64,
    pub len: u32,
}

/// Sends a range request to the other process; `false` if it is gone.
pub type RangeSender = Arc<dyn Fn(RangeRequest) -> bool + Send + Sync>;

type Reply = Option<Vec<u8>>;

/// Bookkeeping of the file lists exchanged with another process (server <-> agent): the lists we
/// offer are kept so the other side can read them by range, and the lists it offers are read through
/// requests it answers.
#[derive(Default)]
pub struct FileExchange {
    next_req: AtomicU32,
    next_list: AtomicU32,
    pending: Mutex<HashMap<u32, Sender<Reply>>>,
    served: Mutex<Vec<(u32, FileList)>>,
}

impl FileExchange {
    /// Keeps `list` readable by the other side; returns the id to announce it with.
    pub fn serve(&self, list: FileList) -> u32 {
        let id = self
            .next_list
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let mut served = self.served.lock().expect("served");
        served.push((id, list));
        while served.len() > KEPT_LISTS {
            served.remove(0);
        }
        id
    }

    /// Reads a range of a list we offered (blocking); `None` if the list or the file is unknown or the
    /// read failed.
    pub fn read_served(&self, list_id: u32, index: u32, offset: u64, len: u32) -> Option<Vec<u8>> {
        let list = self
            .served
            .lock()
            .expect("served")
            .iter()
            .find(|(id, _)| *id == list_id)
            .map(|(_, l)| l.clone())?;
        let entry = list.entries.get(index as usize)?;
        if entry.is_dir {
            return None;
        }
        list.source.read(index, offset, len.min(MAX_RANGE)).ok()
    }

    /// The list another process offers (`entries` as it announced them), read through `send`.
    pub fn remote_list(
        self: &Arc<Self>,
        list_id: u32,
        entries: Vec<FileEntry>,
        send: RangeSender,
    ) -> FileList {
        let entries = FileList::retain_valid(entries);
        FileList {
            entries,
            source: Arc::new(RemoteRanges {
                exchange: self.clone(),
                list_id,
                send,
            }),
        }
    }

    /// An answer to a range request (`None`: refused).
    pub fn complete(&self, req_id: u32, data: Reply) {
        if let Some(tx) = self.pending.lock().expect("pending").remove(&req_id) {
            let _ = tx.send(data);
        }
    }

    /// Fails every request in flight (the other process is gone).
    pub fn fail_all(&self) {
        self.pending.lock().expect("pending").clear();
    }
}

struct RemoteRanges {
    exchange: Arc<FileExchange>,
    list_id: u32,
    send: RangeSender,
}

impl FileSource for RemoteRanges {
    fn read(&self, index: u32, offset: u64, len: u32) -> Result<Vec<u8>, ClipboardError> {
        let fail = |m: &str| ClipboardError::Backend(m.to_string());
        let len = len.min(MAX_RANGE);
        let mut out = Vec::new();
        let mut offset = offset;
        // Several requests when the caller wants more than one range at a time.
        while (out.len() as u64) < u64::from(len) {
            let want = len - out.len() as u32;
            let req_id = self
                .exchange
                .next_req
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1);
            let (tx, rx) = mpsc::channel();
            self.exchange
                .pending
                .lock()
                .expect("pending")
                .insert(req_id, tx);
            let request = RangeRequest {
                req_id,
                list_id: self.list_id,
                index,
                offset,
                len: want,
            };
            if !(self.send)(request) {
                self.exchange
                    .pending
                    .lock()
                    .expect("pending")
                    .remove(&req_id);
                return Err(fail("the other process is gone"));
            }
            match rx.recv_timeout(RANGE_TIMEOUT) {
                Ok(Some(data)) => {
                    let n = data.len();
                    out.extend(data);
                    offset += n as u64;
                    if n < want as usize {
                        break; // end of the file
                    }
                }
                Ok(None) => return Err(fail("file range refused")),
                Err(_) => {
                    self.exchange
                        .pending
                        .lock()
                        .expect("pending")
                        .remove(&req_id);
                    return Err(fail("no answer for a file range"));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> FileEntry {
        FileEntry {
            path: path.into(),
            size: 0,
            is_dir: false,
            modified: 0,
            read_only: false,
        }
    }

    #[test]
    fn paths_must_be_relative_and_clean() {
        for ok in ["a", "dir/a.txt", "é€😀/x"] {
            assert!(entry(ok).is_valid(), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "a//b",
            "a/../b",
            "..",
            ".",
            "a\\b",
            "a/",
            "a\0b",
        ] {
            assert!(!entry(bad).is_valid(), "{bad:?}");
        }
        assert!(!entry(&"x".repeat(260)).is_valid());
        assert!(entry(&"x".repeat(259)).is_valid());
    }

    #[test]
    fn roots_are_the_top_level_entries() {
        assert!(entry("a").is_root());
        assert!(!entry("d/a").is_root());
    }

    struct Mem(Vec<u8>);
    impl FileSource for Mem {
        fn read(&self, _i: u32, offset: u64, len: u32) -> Result<Vec<u8>, ClipboardError> {
            let s = (offset as usize).min(self.0.len());
            let e = (s + len as usize).min(self.0.len());
            Ok(self.0[s..e].to_vec())
        }
    }

    #[test]
    fn a_list_offered_through_the_exchange_is_read_back_by_range() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let mut e = entry("f");
        e.size = data.len() as u64;
        let local = Arc::new(FileExchange::default());
        let list = FileList {
            entries: vec![e.clone()],
            source: Arc::new(Mem(data.clone())),
        };
        let id = local.serve(list);

        // The "other process": reads the list through requests answered from `local`.
        let remote = Arc::new(FileExchange::default());
        let (r2, l2) = (remote.clone(), local.clone());
        let send: RangeSender = Arc::new(move |q| {
            let (r3, l3) = (r2.clone(), l2.clone());
            std::thread::spawn(move || {
                r3.complete(
                    q.req_id,
                    l3.read_served(q.list_id, q.index, q.offset, q.len),
                );
            });
            true
        });
        let got = remote.remote_list(id, vec![e], send);
        assert_eq!(got.source.read(0, 100, 50).unwrap(), data[100..150]);
        assert_eq!(got.source.read(0, 9_990, 100).unwrap(), data[9_990..]);
        // Unknown list: refused.
        assert!(local.read_served(id + 7, 0, 0, 1).is_none());
    }

    #[test]
    fn only_the_latest_lists_stay_readable() {
        let x = FileExchange::default();
        let mk = || FileList {
            entries: vec![entry("f")],
            source: Arc::new(Mem(vec![1, 2, 3])),
        };
        let a = x.serve(mk());
        let b = x.serve(mk());
        let c = x.serve(mk());
        assert!(x.read_served(a, 0, 0, 1).is_none());
        assert!(x.read_served(b, 0, 0, 1).is_some());
        assert!(x.read_served(c, 0, 0, 1).is_some());
    }

    #[test]
    fn a_gone_peer_fails_the_read() {
        let x = Arc::new(FileExchange::default());
        let l = x.remote_list(1, vec![entry("f")], Arc::new(|_| false));
        assert!(l.source.read(0, 0, 10).is_err());
    }
}
