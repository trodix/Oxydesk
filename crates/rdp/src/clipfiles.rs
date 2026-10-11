//! File transfer over CLIPRDR: descriptors <-> `FileEntry`, serving the ranges the other end asks
//! for, and fetching the ranges of the files it offers.
//!
//! Nothing is buffered beyond one block: a file of several gigabytes goes through in ranges.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardGeneralCapabilityFlags, FileContentsFlags,
    FileContentsRequest, FileContentsResponse, FileDescriptor,
};
use rdsktp_core::error::ClipboardError;
use rdsktp_core::files::{FileEntry, FileList, FileSource};

use crate::clipboard::SendFn;

/// Size of the ranges requested from the other end (one request per block).
pub const BLOCK: u64 = 512 * 1024;
/// A request that gets no answer for this long fails (the reader gets an I/O error).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Seconds between 1601-01-01 (FILETIME) and 1970-01-01.
const FILETIME_EPOCH_DELTA: i64 = 11_644_473_600;

/// What the connection negotiated; files need `STREAM_FILECLIP_ENABLED`.
pub fn capabilities() -> ClipboardGeneralCapabilityFlags {
    ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
        | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
        | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
        | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
        | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED
}

fn to_filetime(unix: i64) -> u64 {
    ((unix + FILETIME_EPOCH_DELTA).max(0) as u64) * 10_000_000
}

fn from_filetime(ft: u64) -> i64 {
    (ft / 10_000_000) as i64 - FILETIME_EPOCH_DELTA
}

/// The descriptors announced for `entries` (same order: the index of a request is the index of the entry).
pub fn descriptors(entries: &[FileEntry]) -> Vec<FileDescriptor> {
    entries
        .iter()
        .map(|e| {
            let (dir, name) = match e.path.rsplit_once('/') {
                Some((d, n)) => (Some(d.replace('/', "\\")), n),
                None => (None, e.path.as_str()),
            };
            let mut attributes = if e.is_dir {
                ClipboardFileAttributes::DIRECTORY
            } else {
                ClipboardFileAttributes::ARCHIVE
            };
            if e.read_only {
                attributes |= ClipboardFileAttributes::READONLY;
            }
            let mut d = FileDescriptor::new(name)
                .with_attributes(attributes)
                .with_last_write_time(to_filetime(e.modified));
            if !e.is_dir {
                d = d.with_file_size(e.size);
            }
            match dir {
                Some(dir) => d.with_relative_path(dir),
                None => d,
            }
        })
        .collect()
}

/// The entries of a received descriptor list, and for each its index in the list the other end
/// numbers its requests with (invalid names are left out, which shifts indices).
pub fn entries(files: &[FileDescriptor]) -> (Vec<FileEntry>, Vec<u32>) {
    let mut entries = Vec::new();
    let mut wire_index = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let path = match &f.relative_path {
            Some(dir) if !dir.is_empty() => format!("{}/{}", dir.replace('\\', "/"), f.name),
            _ => f.name.clone(),
        };
        let attrs = f.attributes.unwrap_or(ClipboardFileAttributes::empty());
        let entry = FileEntry {
            path,
            size: f.file_size.unwrap_or(0),
            is_dir: attrs.contains(ClipboardFileAttributes::DIRECTORY),
            modified: f.last_write_time.map_or(0, from_filetime),
            read_only: attrs.contains(ClipboardFileAttributes::READONLY),
        };
        if entry.is_valid() {
            entries.push(entry);
            wire_index.push(i as u32);
        }
    }
    (entries, wire_index)
}

/// Answers the requests of the other end for the files we offer, on a thread of its own: reading a
/// disk must not stall the connection.
pub struct Server {
    jobs: Sender<(FileContentsRequest, FileList)>,
}

impl Server {
    pub fn new(send: SendFn) -> Self {
        let (jobs, rx): (_, Receiver<(FileContentsRequest, FileList)>) = mpsc::channel();
        std::thread::Builder::new()
            .name("cliprdr-files".into())
            .spawn(move || {
                // Ends when the connection (the `Sender`) is gone.
                while let Ok((req, list)) = rx.recv() {
                    send(ClipboardMessage::SendFileContentsResponse(answer(
                        &req, &list,
                    )));
                }
            })
            .expect("thread");
        Self { jobs }
    }

    pub fn submit(&self, request: FileContentsRequest, list: FileList) {
        let _ = self.jobs.send((request, list));
    }
}

fn answer(req: &FileContentsRequest, list: &FileList) -> FileContentsResponse<'static> {
    let id = req.stream_id;
    let Some(entry) = u32::try_from(req.index)
        .ok()
        .and_then(|i| list.entries.get(i as usize).map(|e| (i, e)))
    else {
        return FileContentsResponse::new_error(id);
    };
    let (index, entry) = entry;
    if entry.is_dir {
        return FileContentsResponse::new_error(id);
    }
    if req.flags.contains(FileContentsFlags::SIZE) {
        return FileContentsResponse::new_size_response(id, entry.size);
    }
    match list.source.read(index, req.position, req.requested_size) {
        Ok(data) => FileContentsResponse::new_data_response(id, data),
        Err(e) => {
            tracing::warn!("file not read: {e}");
            FileContentsResponse::new_error(id)
        }
    }
}

type Reply = Result<Vec<u8>, ()>;

/// Requests in flight toward the other end, by stream id.
#[derive(Default)]
pub struct Fetcher {
    next: AtomicU32,
    pending: Mutex<HashMap<u32, Sender<Reply>>>,
    /// Negotiated flags: requests are only sent when files are enabled.
    caps: AtomicU32,
    closed: AtomicBool,
    /// Number of the list offered by the other end now: reads of an older list fail at once.
    generation: AtomicU64,
}

impl Fetcher {
    pub fn set_capabilities(&self, caps: ClipboardGeneralCapabilityFlags) {
        self.caps.store(caps.bits(), Ordering::Relaxed);
    }

    fn enabled(&self) -> bool {
        !self.closed.load(Ordering::Relaxed)
            && ClipboardGeneralCapabilityFlags::from_bits_retain(self.caps.load(Ordering::Relaxed))
                .contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED)
    }

    /// A new list replaces the previous one; returns its number.
    pub fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// An answer from the other end.
    pub fn complete(&self, stream_id: u32, reply: Reply) {
        if let Some(tx) = self.pending.lock().expect("pending").remove(&stream_id) {
            let _ = tx.send(reply);
        }
    }

    /// Fails every request in flight (the connection is gone).
    pub fn fail_all(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.pending.lock().expect("pending").clear();
    }

    fn request(
        &self,
        send: &SendFn,
        index: u32,
        position: u64,
        size: u32,
        data_id: Option<u32>,
    ) -> Result<Vec<u8>, ClipboardError> {
        let fail = |m: &str| ClipboardError::Backend(m.to_string());
        if !self.enabled() {
            return Err(fail("file transfer not negotiated"));
        }
        let stream_id = self.next.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().expect("pending").insert(stream_id, tx);
        send(ClipboardMessage::SendFileContentsRequest(
            FileContentsRequest {
                stream_id,
                index: index as i32,
                flags: FileContentsFlags::RANGE,
                position,
                requested_size: size,
                data_id,
            },
        ));
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(())) => Err(fail("the other end refused the file range")),
            Err(_) => {
                self.pending.lock().expect("pending").remove(&stream_id);
                Err(fail("no answer for a file range (connection lost?)"))
            }
        }
    }
}

/// The files offered by the other end, read through range requests.
pub struct RemoteSource {
    fetcher: Arc<Fetcher>,
    send: SendFn,
    data_id: Option<u32>,
    generation: u64,
    /// Index of each entry in the list the other end numbers its files with.
    wire_index: Vec<u32>,
    sizes: Vec<u64>,
    /// The last block fetched: sequential reads of a file are served from it.
    block: Mutex<Option<(u32, u64, Vec<u8>)>>,
}

impl RemoteSource {
    pub fn new(
        fetcher: Arc<Fetcher>,
        send: SendFn,
        data_id: Option<u32>,
        wire_index: Vec<u32>,
        entries: &[FileEntry],
    ) -> Self {
        Self {
            generation: fetcher.next_generation(),
            fetcher,
            send,
            data_id,
            wire_index,
            sizes: entries.iter().map(|e| e.size).collect(),
            block: Mutex::new(None),
        }
    }
}

impl FileSource for RemoteSource {
    fn read(&self, index: u32, offset: u64, len: u32) -> Result<Vec<u8>, ClipboardError> {
        let size = *self
            .sizes
            .get(index as usize)
            .ok_or_else(|| ClipboardError::Backend("no such file in the list".into()))?;
        let wire = self.wire_index[index as usize];
        if self.fetcher.generation.load(Ordering::SeqCst) != self.generation {
            return Err(ClipboardError::Backend(
                "the files were replaced by a newer copy".into(),
            ));
        }
        let end = offset.saturating_add(u64::from(len)).min(size);
        let mut out = Vec::with_capacity(end.saturating_sub(offset) as usize);
        let mut pos = offset;
        while pos < end {
            let start = pos / BLOCK * BLOCK;
            let cached = {
                let b = self.block.lock().expect("block");
                match b.as_ref() {
                    Some((i, s, data)) if *i == index && *s == start => Some(data.clone()),
                    _ => None,
                }
            };
            let data = match cached {
                Some(d) => d,
                None => {
                    let want = (size - start).min(BLOCK) as u32;
                    let d = self
                        .fetcher
                        .request(&self.send, wire, start, want, self.data_id)?;
                    if d.is_empty() {
                        break; // shorter file than announced
                    }
                    *self.block.lock().expect("block") = Some((index, start, d.clone()));
                    d
                }
            };
            let from = (pos - start) as usize;
            if from >= data.len() {
                break;
            }
            let take = ((end - pos) as usize).min(data.len() - from);
            out.extend_from_slice(&data[from..from + take]);
            pos += take as u64;
            if data.len() < BLOCK.min(size - start) as usize {
                break; // short block: end of what the other end has
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64, dir: bool) -> FileEntry {
        FileEntry {
            path: path.into(),
            size,
            is_dir: dir,
            modified: 1_700_000_000,
            read_only: false,
        }
    }

    #[test]
    fn descriptors_and_entries_are_inverse() {
        let list = vec![
            entry("d", 0, true),
            entry("d/sub/é.txt", 5_000_000_000, false),
            entry("top", 3, false),
        ];
        let d = descriptors(&list);
        assert_eq!(d[1].relative_path.as_deref(), Some("d\\sub"));
        assert_eq!(d[1].name, "é.txt");
        assert_eq!(d[1].file_size, Some(5_000_000_000));
        let (back, idx) = entries(&d);
        assert_eq!(back, list);
        assert_eq!(idx, [0, 1, 2]);
    }

    #[test]
    fn invalid_names_from_the_other_end_shift_the_indices() {
        let mut d = descriptors(&[
            entry("a", 1, false),
            entry("b", 2, false),
            entry("c", 3, false),
        ]);
        d[1].name = "..".into();
        let (e, idx) = entries(&d);
        assert_eq!(
            e.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(idx, [0, 2]);
    }

    /// The other end, in a thread: answers range requests from a generated file.
    fn fake_peer(fetcher: Arc<Fetcher>, size: u64, requests: Arc<AtomicU32>) -> SendFn {
        Arc::new(move |msg| {
            if let ClipboardMessage::SendFileContentsRequest(r) = msg {
                requests.fetch_add(1, Ordering::SeqCst);
                let end = (r.position + u64::from(r.requested_size)).min(size);
                let data: Vec<u8> = (r.position..end).map(|p| (p % 251) as u8).collect();
                fetcher.complete(r.stream_id, Ok(data));
            }
        })
    }

    #[test]
    fn remote_reads_are_served_by_blocks() {
        let fetcher = Arc::new(Fetcher::default());
        fetcher.set_capabilities(capabilities());
        let requests = Arc::new(AtomicU32::new(0));
        let size = BLOCK * 3 + 17;
        let send = fake_peer(fetcher.clone(), size, requests.clone());
        let src = RemoteSource::new(fetcher, send, Some(7), vec![0], &[entry("f", size, false)]);
        // Sequential 128 KiB reads: 4 requests for 4 blocks.
        let mut got = Vec::new();
        let mut off = 0;
        while off < size {
            let part = src.read(0, off, 128 * 1024).unwrap();
            assert!(!part.is_empty());
            off += part.len() as u64;
            got.extend(part);
        }
        assert_eq!(got.len() as u64, size);
        assert!(got
            .iter()
            .enumerate()
            .all(|(i, b)| *b == (i as u64 % 251) as u8));
        assert_eq!(requests.load(Ordering::SeqCst), 4);
        // A read across a block boundary, and past the end.
        let across = src.read(0, BLOCK - 5, 10).unwrap();
        assert_eq!(across.len(), 10);
        assert!(src.read(0, size, 10).unwrap().is_empty());
    }

    #[test]
    fn reads_fail_when_files_are_not_negotiated() {
        let fetcher = Arc::new(Fetcher::default());
        let send: SendFn = Arc::new(|_| {});
        let src = RemoteSource::new(fetcher, send, None, vec![0], &[entry("f", 10, false)]);
        assert!(src.read(0, 0, 10).is_err());
    }
}
