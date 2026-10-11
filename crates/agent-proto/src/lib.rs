//! Protocol between the unprivileged RDP server (`rdsktp`) and the session agent running as
//! the logged-in user (`rdsktp-agent`), over a Unix socket.
//!
//! Frame: `[tag: u8][length: u32 big-endian][payload]`. Hand-written binary format
//! (no serialization dependency); every field is bounded so that a faulty peer cannot
//! trigger a huge allocation.

use std::io::{Read, Write};

use thiserror::Error;

/// Maximum payload size (a 1080p H.264 frame is a few hundred KiB).
pub const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("E/S : {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid message: {0}")]
    Invalid(&'static str),
}

/// Most outputs a layout may carry.
pub const MAX_OUTPUTS: usize = 16;

/// An output of a layout, in desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Most entries of a file list.
pub const MAX_FILES: usize = 100_000;

/// One file or directory of a file list (path relative to the copied collection, `/`-separated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    /// Seconds since the Unix epoch.
    pub modified: i64,
    pub read_only: bool,
}

/// A range of a file of a list, asked to the other end; answered by `FileData` with the same `req_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRange {
    pub req_id: u32,
    pub list_id: u32,
    pub index: u32,
    pub offset: u64,
    pub len: u32,
}

/// Most entries of a drive directory listing.
pub const MAX_DRIVE_ENTRIES: usize = 1_000_000;

/// Metadata of a file of a redirected drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DriveStatInfo {
    pub size: u64,
    pub is_dir: bool,
    pub read_only: bool,
    pub modified: i64,
    pub accessed: i64,
    pub created: i64,
}

/// Failure of a drive operation (mirrors `rdsktp_core::error::DriveError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveFailure {
    NotFound,
    Exists,
    Denied,
    NotDirectory,
    IsDirectory,
    NotEmpty,
    NoSpace,
    Invalid,
    Busy,
    Io(String),
}

/// An operation on a drive, asked by the agent to the server (which holds the client's drive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveOp {
    Stat(String),
    List(String),
    /// `mode`: 0 read, 1 read-write, 2 truncate, 3 create.
    Open {
        path: String,
        mode: u8,
    },
    Read {
        handle: u64,
        offset: u64,
        len: u32,
    },
    Write {
        handle: u64,
        offset: u64,
        data: Vec<u8>,
    },
    Flush(u64),
    Close(u64),
    Mkdir(String),
    Remove {
        path: String,
        is_dir: bool,
    },
    Rename {
        from: String,
        to: String,
    },
    Truncate {
        path: String,
        size: u64,
    },
    SetModified {
        path: String,
        seconds: i64,
    },
    Space,
}

/// Answer to a `DriveOp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveResult {
    Ok,
    Err(DriveFailure),
    Stat(DriveStatInfo),
    List(Vec<(String, DriveStatInfo)>),
    Handle(u64),
    Data(Vec<u8>),
    Written(u32),
    Space { total: u64, available: u64 },
}

/// An operation on a printer of the client, asked by the agent to the server (which holds the client's printer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrintOp {
    /// Starts a job titled `title`; answered by `PrintResult::Job`.
    Begin {
        title: String,
    },
    Write {
        job: u64,
        data: Vec<u8>,
    },
    Finish(u64),
    Abort(u64),
}

/// Answer to a `PrintOp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrintResult {
    Ok,
    /// The client refused (or lost) the job.
    Refused(String),
    /// The link failed.
    Io(String),
    Job(u64),
}

/// Messages from the server to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToAgent {
    Key {
        scancode: u16,
        pressed: bool,
    },
    PointerMove {
        x: i32,
        y: i32,
    },
    /// 0 left, 1 right, 2 middle, 3 back, 4 forward.
    PointerButton {
        button: u8,
        pressed: bool,
    },
    Wheel {
        dx: i32,
        dy: i32,
    },
    RequestKeyframe {
        output: u32,
    },
    ClipboardSet(String),
    /// Image (PNG file) to offer as the desktop selection.
    ClipboardImageSet(Vec<u8>),
    AudioOpen {
        sample_rate: u32,
        channels: u16,
    },
    AudioClose,
    /// New desktop size (single output); answered by `FromAgent::Resized`.
    Resize {
        width: u32,
        height: u32,
    },
    /// New number, sizes and positions of the outputs (the first one is the primary); answered by
    /// `FromAgent::LayoutApplied`.
    SetLayout {
        outputs: Vec<Rect>,
    },
    /// Files copied on the remote side, to offer as the desktop selection; their contents are read with
    /// `FromAgent::FileRead`.
    FilesSet {
        list_id: u32,
        files: Vec<FileInfo>,
    },
    /// A range of a file of a list the agent offered (`FromAgent::Files`).
    FileRead(FileRange),
    /// Answer to `FromAgent::FileRead`; `ok: false` if the range cannot be read.
    FileData {
        req_id: u32,
        ok: bool,
        data: Vec<u8>,
    },
    /// `true`: send the frames as uncompressed pixels (`FromAgent::VideoRaw`) instead of H.264, for a client that
    /// cannot decode H.264. Back to `false`: restarts from keyframes.
    SetPixels {
        raw: bool,
    },
    /// XKB layout and variant of the session, from the client's keyboard layout.
    KeyboardLayout {
        layout: String,
        variant: String,
    },
    /// The client redirected a drive; the agent mounts it and asks its contents with `FromAgent::DriveCall`.
    DriveAdded {
        drive_id: u32,
        name: String,
    },
    /// The drive is gone: the agent unmounts it.
    DriveRemoved {
        drive_id: u32,
    },
    /// Answer to `FromAgent::DriveCall` with the same `req_id`.
    DriveReply {
        req_id: u32,
        result: DriveResult,
    },
    /// The client redirected a printer; the agent makes its socket and its CUPS queue, and sends the jobs
    /// with `FromAgent::PrintCall`. `format`: 0 PostScript, 1 PCL, 2 text (raw jobs), 3 XPS.
    PrinterAdded {
        printer_id: u32,
        name: String,
        driver: String,
        is_default: bool,
        format: u8,
    },
    /// The printer is gone: the agent removes its socket and its queue.
    PrinterRemoved {
        printer_id: u32,
    },
    /// Answer to `FromAgent::PrintCall` with the same `req_id`.
    PrintReply {
        req_id: u32,
        result: PrintResult,
    },
}

/// Messages from the agent to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromAgent {
    Hello {
        outputs: u32,
        width: u32,
        height: u32,
    },
    Video {
        output: u32,
        keyframe: bool,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    ClipboardText(String),
    /// New file selection of the desktop; the server reads the contents with `ToAgent::FileRead`.
    Files {
        list_id: u32,
        files: Vec<FileInfo>,
    },
    /// A range of a file of a list the server offered (`ToAgent::FilesSet`).
    FileRead(FileRange),
    /// Answer to `ToAgent::FileRead`; `ok: false` if the range cannot be read.
    FileData {
        req_id: u32,
        ok: bool,
        data: Vec<u8>,
    },
    /// New image selection of the desktop (PNG file).
    ClipboardImage(Vec<u8>),
    Audio(Vec<u8>),
    Fatal(String),
    /// Uncompressed frame (after `ToAgent::SetPixels { raw: true }`): compact BGRA rows, `width * 4` bytes each.
    VideoRaw {
        output: u32,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    /// Answer to `ToAgent::Resize`.
    Resized {
        ok: bool,
    },
    /// Answer to `ToAgent::SetLayout`; `outputs` is the number of outputs now captured. Sent before any
    /// frame of the new layout, after the last frame of the old one.
    LayoutApplied {
        ok: bool,
        outputs: u32,
    },
    /// The pointer changed: `None` hides it, otherwise a straight RGBA bitmap (`width * height * 4` bytes) and its
    /// hotspot.
    Cursor(Option<CursorImage>),
    /// An operation on a redirected drive; answered by `ToAgent::DriveReply`.
    DriveCall {
        req_id: u32,
        drive_id: u32,
        op: DriveOp,
    },
    /// An operation on a redirected printer; answered by `ToAgent::PrintReply`.
    PrintCall {
        req_id: u32,
        printer_id: u32,
        op: PrintOp,
    },
}

/// Pointer bitmap of `FromAgent::Cursor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn files(&mut self, list_id: u32, files: &[FileInfo]) {
        self.u32(list_id);
        self.u32(files.len() as u32);
        for f in files {
            self.u16(f.path.len().min(u16::MAX as usize) as u16);
            self.bytes(&f.path.as_bytes()[..f.path.len().min(u16::MAX as usize)]);
            self.u64(f.size);
            self.u8(u8::from(f.is_dir) | u8::from(f.read_only) << 1);
            self.i64(f.modified);
        }
    }
    fn range(&mut self, r: &FileRange) {
        self.u32(r.req_id);
        self.u32(r.list_id);
        self.u32(r.index);
        self.u64(r.offset);
        self.u32(r.len);
    }
    fn bytes(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
    fn str(&mut self, v: &str) {
        let n = v.len().min(u16::MAX as usize);
        self.u16(n as u16);
        self.bytes(&v.as_bytes()[..n]);
    }
    fn stat(&mut self, s: &DriveStatInfo) {
        self.u64(s.size);
        self.u8(u8::from(s.is_dir) | u8::from(s.read_only) << 1);
        self.i64(s.modified);
        self.i64(s.accessed);
        self.i64(s.created);
    }
    fn print_op(&mut self, op: &PrintOp) {
        match op {
            PrintOp::Begin { title } => {
                self.u8(0);
                self.str(title);
            }
            PrintOp::Write { job, data } => {
                self.u8(1);
                self.u64(*job);
                self.bytes(data);
            }
            PrintOp::Finish(job) => {
                self.u8(2);
                self.u64(*job);
            }
            PrintOp::Abort(job) => {
                self.u8(3);
                self.u64(*job);
            }
        }
    }
    fn print_result(&mut self, r: &PrintResult) {
        match r {
            PrintResult::Ok => self.u8(0),
            PrintResult::Refused(m) => {
                self.u8(1);
                self.str(m);
            }
            PrintResult::Io(m) => {
                self.u8(2);
                self.str(m);
            }
            PrintResult::Job(job) => {
                self.u8(3);
                self.u64(*job);
            }
        }
    }
    fn drive_op(&mut self, op: &DriveOp) {
        match op {
            DriveOp::Stat(p) => {
                self.u8(0);
                self.str(p);
            }
            DriveOp::List(p) => {
                self.u8(1);
                self.str(p);
            }
            DriveOp::Open { path, mode } => {
                self.u8(2);
                self.u8(*mode);
                self.str(path);
            }
            DriveOp::Read {
                handle,
                offset,
                len,
            } => {
                self.u8(3);
                self.u64(*handle);
                self.u64(*offset);
                self.u32(*len);
            }
            DriveOp::Write {
                handle,
                offset,
                data,
            } => {
                self.u8(4);
                self.u64(*handle);
                self.u64(*offset);
                self.bytes(data);
            }
            DriveOp::Flush(h) => {
                self.u8(5);
                self.u64(*h);
            }
            DriveOp::Close(h) => {
                self.u8(6);
                self.u64(*h);
            }
            DriveOp::Mkdir(p) => {
                self.u8(7);
                self.str(p);
            }
            DriveOp::Remove { path, is_dir } => {
                self.u8(8);
                self.u8(u8::from(*is_dir));
                self.str(path);
            }
            DriveOp::Rename { from, to } => {
                self.u8(9);
                self.str(from);
                self.str(to);
            }
            DriveOp::Truncate { path, size } => {
                self.u8(10);
                self.u64(*size);
                self.str(path);
            }
            DriveOp::SetModified { path, seconds } => {
                self.u8(11);
                self.i64(*seconds);
                self.str(path);
            }
            DriveOp::Space => self.u8(12),
        }
    }
    fn drive_result(&mut self, r: &DriveResult) {
        match r {
            DriveResult::Ok => self.u8(0),
            DriveResult::Err(f) => {
                self.u8(1);
                let (code, msg) = match f {
                    DriveFailure::NotFound => (0, ""),
                    DriveFailure::Exists => (1, ""),
                    DriveFailure::Denied => (2, ""),
                    DriveFailure::NotDirectory => (3, ""),
                    DriveFailure::IsDirectory => (4, ""),
                    DriveFailure::NotEmpty => (5, ""),
                    DriveFailure::NoSpace => (6, ""),
                    DriveFailure::Invalid => (7, ""),
                    DriveFailure::Busy => (8, ""),
                    DriveFailure::Io(m) => (9, m.as_str()),
                };
                self.u8(code);
                self.str(msg);
            }
            DriveResult::Stat(s) => {
                self.u8(2);
                self.stat(s);
            }
            DriveResult::List(entries) => {
                self.u8(3);
                self.u32(entries.len() as u32);
                for (name, s) in entries {
                    self.str(name);
                    self.stat(s);
                }
            }
            DriveResult::Handle(h) => {
                self.u8(4);
                self.u64(*h);
            }
            DriveResult::Data(d) => {
                self.u8(5);
                self.bytes(d);
            }
            DriveResult::Written(n) => {
                self.u8(6);
                self.u32(*n);
            }
            DriveResult::Space { total, available } => {
                self.u8(7);
                self.u64(*total);
                self.u64(*available);
            }
        }
    }
}

struct In<'a>(&'a [u8]);

impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if self.0.len() < n {
            return Err(ProtoError::Invalid("payload too short"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> Result<bool, ProtoError> {
        Ok(self.u8()? != 0)
    }
    fn u16(&mut self) -> Result<u16, ProtoError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }
    fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn i32(&mut self) -> Result<i32, ProtoError> {
        Ok(i32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn u64(&mut self) -> Result<u64, ProtoError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    fn i64(&mut self) -> Result<i64, ProtoError> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    fn files(&mut self) -> Result<(u32, Vec<FileInfo>), ProtoError> {
        let list_id = self.u32()?;
        let n = self.u32()? as usize;
        if n > MAX_FILES {
            return Err(ProtoError::Invalid("too many files"));
        }
        // Each entry takes at least 19 bytes: a lying count cannot trigger a big allocation.
        let mut files = Vec::with_capacity(n.min(self.0.len() / 19));
        for _ in 0..n {
            let len = usize::from(self.u16()?);
            let path = String::from_utf8(self.take(len)?.to_vec())
                .map_err(|_| ProtoError::Invalid("path is not UTF-8"))?;
            let size = self.u64()?;
            let flags = self.u8()?;
            let modified = self.i64()?;
            files.push(FileInfo {
                path,
                size,
                is_dir: flags & 1 != 0,
                modified,
                read_only: flags & 2 != 0,
            });
        }
        Ok((list_id, files))
    }
    fn range(&mut self) -> Result<FileRange, ProtoError> {
        Ok(FileRange {
            req_id: self.u32()?,
            list_id: self.u32()?,
            index: self.u32()?,
            offset: self.u64()?,
            len: self.u32()?,
        })
    }
    fn str(&mut self) -> Result<String, ProtoError> {
        let n = usize::from(self.u16()?);
        String::from_utf8(self.take(n)?.to_vec())
            .map_err(|_| ProtoError::Invalid("text is not UTF-8"))
    }
    fn stat(&mut self) -> Result<DriveStatInfo, ProtoError> {
        let size = self.u64()?;
        let flags = self.u8()?;
        Ok(DriveStatInfo {
            size,
            is_dir: flags & 1 != 0,
            read_only: flags & 2 != 0,
            modified: self.i64()?,
            accessed: self.i64()?,
            created: self.i64()?,
        })
    }
    fn print_op(&mut self) -> Result<PrintOp, ProtoError> {
        Ok(match self.u8()? {
            0 => PrintOp::Begin { title: self.str()? },
            1 => PrintOp::Write {
                job: self.u64()?,
                data: self.rest(),
            },
            2 => PrintOp::Finish(self.u64()?),
            3 => PrintOp::Abort(self.u64()?),
            _ => return Err(ProtoError::Invalid("unknown print operation")),
        })
    }
    fn print_result(&mut self) -> Result<PrintResult, ProtoError> {
        Ok(match self.u8()? {
            0 => PrintResult::Ok,
            1 => PrintResult::Refused(self.str()?),
            2 => PrintResult::Io(self.str()?),
            3 => PrintResult::Job(self.u64()?),
            _ => return Err(ProtoError::Invalid("unknown print result")),
        })
    }
    fn drive_op(&mut self) -> Result<DriveOp, ProtoError> {
        Ok(match self.u8()? {
            0 => DriveOp::Stat(self.str()?),
            1 => DriveOp::List(self.str()?),
            2 => DriveOp::Open {
                mode: self.u8()?,
                path: self.str()?,
            },
            3 => DriveOp::Read {
                handle: self.u64()?,
                offset: self.u64()?,
                len: self.u32()?,
            },
            4 => DriveOp::Write {
                handle: self.u64()?,
                offset: self.u64()?,
                data: self.rest(),
            },
            5 => DriveOp::Flush(self.u64()?),
            6 => DriveOp::Close(self.u64()?),
            7 => DriveOp::Mkdir(self.str()?),
            8 => DriveOp::Remove {
                is_dir: self.bool()?,
                path: self.str()?,
            },
            9 => DriveOp::Rename {
                from: self.str()?,
                to: self.str()?,
            },
            10 => DriveOp::Truncate {
                size: self.u64()?,
                path: self.str()?,
            },
            11 => DriveOp::SetModified {
                seconds: self.i64()?,
                path: self.str()?,
            },
            12 => DriveOp::Space,
            _ => return Err(ProtoError::Invalid("unknown drive operation")),
        })
    }
    fn drive_result(&mut self) -> Result<DriveResult, ProtoError> {
        Ok(match self.u8()? {
            0 => DriveResult::Ok,
            1 => {
                let code = self.u8()?;
                let msg = self.str()?;
                DriveResult::Err(match code {
                    0 => DriveFailure::NotFound,
                    1 => DriveFailure::Exists,
                    2 => DriveFailure::Denied,
                    3 => DriveFailure::NotDirectory,
                    4 => DriveFailure::IsDirectory,
                    5 => DriveFailure::NotEmpty,
                    6 => DriveFailure::NoSpace,
                    7 => DriveFailure::Invalid,
                    8 => DriveFailure::Busy,
                    9 => DriveFailure::Io(msg),
                    _ => return Err(ProtoError::Invalid("unknown drive failure")),
                })
            }
            2 => DriveResult::Stat(self.stat()?),
            3 => {
                let n = self.u32()? as usize;
                if n > MAX_DRIVE_ENTRIES {
                    return Err(ProtoError::Invalid("too many entries"));
                }
                // Each entry takes at least 36 bytes: a lying count cannot trigger a big allocation.
                let mut entries = Vec::with_capacity(n.min(self.0.len() / 36));
                for _ in 0..n {
                    let name = self.str()?;
                    entries.push((name, self.stat()?));
                }
                DriveResult::List(entries)
            }
            4 => DriveResult::Handle(self.u64()?),
            5 => DriveResult::Data(self.rest()),
            6 => DriveResult::Written(self.u32()?),
            7 => DriveResult::Space {
                total: self.u64()?,
                available: self.u64()?,
            },
            _ => return Err(ProtoError::Invalid("unknown drive result")),
        })
    }
    fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0).to_vec()
    }
    fn string(&mut self) -> Result<String, ProtoError> {
        String::from_utf8(self.rest()).map_err(|_| ProtoError::Invalid("text is not UTF-8"))
    }
    fn end(&self) -> Result<(), ProtoError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(ProtoError::Invalid("extra bytes"))
        }
    }
}

/// Message serializable into a frame.
pub trait Message: Sized {
    fn encode(&self) -> (u8, Vec<u8>);
    fn decode(tag: u8, payload: &[u8]) -> Result<Self, ProtoError>;
}

impl Message for ToAgent {
    fn encode(&self) -> (u8, Vec<u8>) {
        let mut o = Out(Vec::new());
        let tag = match self {
            Self::Key { scancode, pressed } => {
                o.u16(*scancode);
                o.u8(u8::from(*pressed));
                1
            }
            Self::PointerMove { x, y } => {
                o.i32(*x);
                o.i32(*y);
                2
            }
            Self::PointerButton { button, pressed } => {
                o.u8(*button);
                o.u8(u8::from(*pressed));
                3
            }
            Self::Wheel { dx, dy } => {
                o.i32(*dx);
                o.i32(*dy);
                4
            }
            Self::RequestKeyframe { output } => {
                o.u32(*output);
                5
            }
            Self::ClipboardSet(t) => {
                o.bytes(t.as_bytes());
                6
            }
            Self::ClipboardImageSet(d) => {
                o.bytes(d);
                12
            }
            Self::FilesSet { list_id, files } => {
                o.files(*list_id, files);
                13
            }
            Self::FileRead(r) => {
                o.range(r);
                14
            }
            Self::FileData { req_id, ok, data } => {
                o.u32(*req_id);
                o.u8(u8::from(*ok));
                o.bytes(data);
                15
            }
            Self::AudioOpen {
                sample_rate,
                channels,
            } => {
                o.u32(*sample_rate);
                o.u16(*channels);
                7
            }
            Self::AudioClose => 8,
            Self::Resize { width, height } => {
                o.u32(*width);
                o.u32(*height);
                9
            }
            Self::SetLayout { outputs } => {
                o.u32(outputs.len() as u32);
                for r in outputs {
                    o.i32(r.x);
                    o.i32(r.y);
                    o.u32(r.width);
                    o.u32(r.height);
                }
                10
            }
            Self::SetPixels { raw } => {
                o.u8(u8::from(*raw));
                11
            }
            Self::KeyboardLayout { layout, variant } => {
                o.bytes(format!("{layout}\n{variant}").as_bytes());
                16
            }
            Self::DriveAdded { drive_id, name } => {
                o.u32(*drive_id);
                o.bytes(name.as_bytes());
                17
            }
            Self::DriveRemoved { drive_id } => {
                o.u32(*drive_id);
                18
            }
            Self::DriveReply { req_id, result } => {
                o.u32(*req_id);
                o.drive_result(result);
                19
            }
            Self::PrinterAdded {
                printer_id,
                name,
                driver,
                is_default,
                format,
            } => {
                o.u32(*printer_id);
                o.u8(u8::from(*is_default));
                o.u8(*format);
                o.str(name);
                o.str(driver);
                20
            }
            Self::PrinterRemoved { printer_id } => {
                o.u32(*printer_id);
                21
            }
            Self::PrintReply { req_id, result } => {
                o.u32(*req_id);
                o.print_result(result);
                22
            }
        };
        (tag, o.0)
    }

    fn decode(tag: u8, payload: &[u8]) -> Result<Self, ProtoError> {
        let mut i = In(payload);
        let msg = match tag {
            1 => Self::Key {
                scancode: i.u16()?,
                pressed: i.bool()?,
            },
            2 => Self::PointerMove {
                x: i.i32()?,
                y: i.i32()?,
            },
            3 => Self::PointerButton {
                button: i.u8()?,
                pressed: i.bool()?,
            },
            4 => Self::Wheel {
                dx: i.i32()?,
                dy: i.i32()?,
            },
            5 => Self::RequestKeyframe { output: i.u32()? },
            6 => Self::ClipboardSet(i.string()?),
            7 => Self::AudioOpen {
                sample_rate: i.u32()?,
                channels: i.u16()?,
            },
            8 => Self::AudioClose,
            9 => Self::Resize {
                width: i.u32()?,
                height: i.u32()?,
            },
            10 => {
                let n = i.u32()? as usize;
                if n > MAX_OUTPUTS {
                    return Err(ProtoError::Invalid("too many outputs"));
                }
                let mut outputs = Vec::with_capacity(n);
                for _ in 0..n {
                    outputs.push(Rect {
                        x: i.i32()?,
                        y: i.i32()?,
                        width: i.u32()?,
                        height: i.u32()?,
                    });
                }
                Self::SetLayout { outputs }
            }
            11 => Self::SetPixels { raw: i.bool()? },
            16 => {
                let text = i.string()?;
                let (layout, variant) = text
                    .split_once('\n')
                    .ok_or(ProtoError::Invalid("layout without variant"))?;
                Self::KeyboardLayout {
                    layout: layout.into(),
                    variant: variant.into(),
                }
            }
            12 => Self::ClipboardImageSet(i.rest()),
            13 => {
                let (list_id, files) = i.files()?;
                Self::FilesSet { list_id, files }
            }
            14 => Self::FileRead(i.range()?),
            15 => Self::FileData {
                req_id: i.u32()?,
                ok: i.bool()?,
                data: i.rest(),
            },
            17 => Self::DriveAdded {
                drive_id: i.u32()?,
                name: i.string()?,
            },
            18 => Self::DriveRemoved { drive_id: i.u32()? },
            19 => Self::DriveReply {
                req_id: i.u32()?,
                result: i.drive_result()?,
            },
            20 => Self::PrinterAdded {
                printer_id: i.u32()?,
                is_default: i.bool()?,
                format: i.u8()?,
                name: i.str()?,
                driver: i.str()?,
            },
            21 => Self::PrinterRemoved {
                printer_id: i.u32()?,
            },
            22 => Self::PrintReply {
                req_id: i.u32()?,
                result: i.print_result()?,
            },
            _ => return Err(ProtoError::Invalid("unknown tag")),
        };
        i.end()?;
        Ok(msg)
    }
}

impl Message for FromAgent {
    fn encode(&self) -> (u8, Vec<u8>) {
        let mut o = Out(Vec::new());
        let tag = match self {
            Self::Hello {
                outputs,
                width,
                height,
            } => {
                o.u32(*outputs);
                o.u32(*width);
                o.u32(*height);
                1
            }
            Self::Video {
                output,
                keyframe,
                width,
                height,
                data,
            } => {
                o.u32(*output);
                o.u8(u8::from(*keyframe));
                o.u32(*width);
                o.u32(*height);
                o.bytes(data);
                2
            }
            Self::ClipboardText(t) => {
                o.bytes(t.as_bytes());
                3
            }
            Self::ClipboardImage(d) => {
                o.bytes(d);
                10
            }
            Self::Files { list_id, files } => {
                o.files(*list_id, files);
                11
            }
            Self::FileRead(r) => {
                o.range(r);
                12
            }
            Self::FileData { req_id, ok, data } => {
                o.u32(*req_id);
                o.u8(u8::from(*ok));
                o.bytes(data);
                13
            }
            Self::Audio(d) => {
                o.bytes(d);
                4
            }
            Self::Fatal(t) => {
                o.bytes(t.as_bytes());
                5
            }
            Self::Resized { ok } => {
                o.u8(u8::from(*ok));
                6
            }
            Self::LayoutApplied { ok, outputs } => {
                o.u8(u8::from(*ok));
                o.u32(*outputs);
                7
            }
            Self::VideoRaw {
                output,
                width,
                height,
                data,
            } => {
                o.u32(*output);
                o.u32(*width);
                o.u32(*height);
                o.bytes(data);
                8
            }
            Self::Cursor(None) => {
                o.u8(0);
                9
            }
            Self::Cursor(Some(c)) => {
                o.u8(1);
                o.u16(c.width);
                o.u16(c.height);
                o.u16(c.hot_x);
                o.u16(c.hot_y);
                o.bytes(&c.rgba);
                9
            }
            Self::DriveCall {
                req_id,
                drive_id,
                op,
            } => {
                o.u32(*req_id);
                o.u32(*drive_id);
                o.drive_op(op);
                14
            }
            Self::PrintCall {
                req_id,
                printer_id,
                op,
            } => {
                o.u32(*req_id);
                o.u32(*printer_id);
                o.print_op(op);
                15
            }
        };
        (tag, o.0)
    }

    fn decode(tag: u8, payload: &[u8]) -> Result<Self, ProtoError> {
        let mut i = In(payload);
        let msg = match tag {
            1 => {
                let m = Self::Hello {
                    outputs: i.u32()?,
                    width: i.u32()?,
                    height: i.u32()?,
                };
                i.end()?;
                m
            }
            2 => Self::Video {
                output: i.u32()?,
                keyframe: i.bool()?,
                width: i.u32()?,
                height: i.u32()?,
                data: i.rest(),
            },
            3 => Self::ClipboardText(i.string()?),
            4 => Self::Audio(i.rest()),
            10 => Self::ClipboardImage(i.rest()),
            11 => {
                let (list_id, files) = i.files()?;
                i.end()?;
                Self::Files { list_id, files }
            }
            12 => {
                let r = i.range()?;
                i.end()?;
                Self::FileRead(r)
            }
            13 => Self::FileData {
                req_id: i.u32()?,
                ok: i.bool()?,
                data: i.rest(),
            },
            5 => Self::Fatal(i.string()?),
            6 => Self::Resized { ok: i.bool()? },
            7 => Self::LayoutApplied {
                ok: i.bool()?,
                outputs: i.u32()?,
            },
            8 => Self::VideoRaw {
                output: i.u32()?,
                width: i.u32()?,
                height: i.u32()?,
                data: i.rest(),
            },
            9 => match i.u8()? {
                0 => Self::Cursor(None),
                _ => {
                    let (width, height) = (i.u16()?, i.u16()?);
                    let (hot_x, hot_y) = (i.u16()?, i.u16()?);
                    let rgba = i.rest();
                    if rgba.len() != usize::from(width) * usize::from(height) * 4 {
                        return Err(ProtoError::Invalid("cursor size"));
                    }
                    Self::Cursor(Some(CursorImage {
                        width,
                        height,
                        hot_x,
                        hot_y,
                        rgba,
                    }))
                }
            },
            14 => Self::DriveCall {
                req_id: i.u32()?,
                drive_id: i.u32()?,
                op: i.drive_op()?,
            },
            15 => Self::PrintCall {
                req_id: i.u32()?,
                printer_id: i.u32()?,
                op: i.print_op()?,
            },
            _ => return Err(ProtoError::Invalid("unknown tag")),
        };
        Ok(msg)
    }
}

/// Writes a frame (a single `write_all`, the stream is not interleaved if the caller holds the lock).
pub fn write_msg<W: Write, M: Message>(w: &mut W, msg: &M) -> Result<(), ProtoError> {
    let (tag, payload) = msg.encode();
    if payload.len() > MAX_PAYLOAD {
        return Err(ProtoError::Invalid("payload too large"));
    }
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(tag);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

/// Reads a frame; `Ok(None)` if the peer closed cleanly between two frames.
pub fn read_msg<R: Read, M: Message>(r: &mut R) -> Result<Option<M>, ProtoError> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(ProtoError::Invalid("payload too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    M::decode(head[0], &payload).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<M: Message + PartialEq + std::fmt::Debug>(m: M) {
        let mut buf = Vec::new();
        write_msg(&mut buf, &m).unwrap();
        let back: M = read_msg(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(back, m);
    }

    fn sample_files() -> Vec<FileInfo> {
        vec![
            FileInfo {
                path: "dossier".into(),
                size: 0,
                is_dir: true,
                modified: 1_700_000_000,
                read_only: false,
            },
            FileInfo {
                path: "dossier/é😀.bin".into(),
                size: 5_000_000_000,
                is_dir: false,
                modified: -5,
                read_only: true,
            },
        ]
    }

    fn sample_range() -> FileRange {
        FileRange {
            req_id: 7,
            list_id: 2,
            index: 1,
            offset: 4_999_999_999,
            len: 65536,
        }
    }

    #[test]
    fn a_lying_file_count_is_refused_without_allocating() {
        let mut o = Out(Vec::new());
        o.u32(1);
        o.u32(u32::MAX);
        assert!(ToAgent::decode(13, &o.0).is_err());
        let mut o = Out(Vec::new());
        o.u32(1);
        o.u32(50_000);
        assert!(ToAgent::decode(13, &o.0).is_err());
    }

    #[test]
    fn all_messages_roundtrip() {
        roundtrip(ToAgent::Key {
            scancode: 0xE01D,
            pressed: true,
        });
        roundtrip(ToAgent::PointerMove { x: -5, y: 1079 });
        roundtrip(ToAgent::PointerButton {
            button: 4,
            pressed: false,
        });
        roundtrip(ToAgent::Wheel { dx: 0, dy: -120 });
        roundtrip(ToAgent::RequestKeyframe { output: 1 });
        roundtrip(ToAgent::ClipboardSet("Café 😀 日本".into()));
        roundtrip(ToAgent::ClipboardImageSet(vec![137, 80, 78, 71]));
        roundtrip(ToAgent::FilesSet {
            list_id: 3,
            files: sample_files(),
        });
        roundtrip(ToAgent::FileRead(sample_range()));
        roundtrip(ToAgent::FileData {
            req_id: 9,
            ok: true,
            data: vec![0, 1, 2],
        });
        roundtrip(ToAgent::AudioOpen {
            sample_rate: 48_000,
            channels: 2,
        });
        roundtrip(ToAgent::AudioClose);
        roundtrip(ToAgent::Resize {
            width: 1024,
            height: 576,
        });
        roundtrip(FromAgent::Resized { ok: true });
        roundtrip(ToAgent::KeyboardLayout {
            layout: "ch".into(),
            variant: "fr".into(),
        });
        roundtrip(ToAgent::SetPixels { raw: true });
        roundtrip(ToAgent::SetPixels { raw: false });
        roundtrip(ToAgent::SetLayout {
            outputs: vec![
                Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                Rect {
                    x: -1280,
                    y: 100,
                    width: 1280,
                    height: 720,
                },
            ],
        });
        roundtrip(FromAgent::LayoutApplied {
            ok: true,
            outputs: 2,
        });
        roundtrip(FromAgent::Hello {
            outputs: 2,
            width: 1920,
            height: 1080,
        });
        roundtrip(FromAgent::Video {
            output: 1,
            keyframe: true,
            width: 1920,
            height: 1080,
            data: vec![0, 0, 0, 1, 9],
        });
        roundtrip(FromAgent::VideoRaw {
            output: 1,
            width: 2,
            height: 1,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        });
        roundtrip(FromAgent::Cursor(None));
        roundtrip(FromAgent::Cursor(Some(CursorImage {
            width: 2,
            height: 1,
            hot_x: 1,
            hot_y: 0,
            rgba: vec![1, 2, 3, 4, 5, 6, 7, 8],
        })));
        roundtrip(FromAgent::ClipboardText("x".into()));
        roundtrip(FromAgent::ClipboardImage(vec![1, 2, 3]));
        roundtrip(FromAgent::Files {
            list_id: 4,
            files: sample_files(),
        });
        roundtrip(FromAgent::FileRead(sample_range()));
        roundtrip(FromAgent::FileData {
            req_id: 1,
            ok: false,
            data: vec![],
        });
        roundtrip(FromAgent::Audio(vec![1, 2, 3, 4]));
        roundtrip(FromAgent::Fatal("boom".into()));
    }

    #[test]
    fn clean_eof_between_frames_is_none() {
        let r: Option<ToAgent> = read_msg(&mut [].as_slice()).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn rejects_garbage() {
        // unknown tag
        let mut b = vec![99, 0, 0, 0, 0];
        assert!(read_msg::<_, ToAgent>(&mut b.as_slice()).is_err());
        // oversized length: rejected without allocation
        b = vec![1, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(read_msg::<_, ToAgent>(&mut b.as_slice()).is_err());
        // payload too short and extra bytes
        assert!(ToAgent::decode(1, &[0]).is_err());
        assert!(ToAgent::decode(8, &[0]).is_err());
        // layout: too many outputs (no allocation), truncated list
        assert!(ToAgent::decode(10, &1000u32.to_be_bytes()).is_err());
        assert!(ToAgent::decode(10, &[0, 0, 0, 1, 0, 0]).is_err());
        // text is not UTF-8
        assert!(ToAgent::decode(6, &[0xFF, 0xFE]).is_err());
    }

    #[test]
    fn drive_messages_roundtrip() {
        let stat = DriveStatInfo {
            size: 1 << 40,
            is_dir: false,
            read_only: true,
            modified: -5,
            accessed: 6,
            created: 7,
        };
        roundtrip(ToAgent::DriveAdded {
            drive_id: 3,
            name: "Documents".into(),
        });
        roundtrip(ToAgent::DriveRemoved { drive_id: 3 });
        for result in [
            DriveResult::Ok,
            DriveResult::Err(DriveFailure::NotEmpty),
            DriveResult::Err(DriveFailure::Io("boom".into())),
            DriveResult::Stat(stat),
            DriveResult::List(vec![("a.txt".into(), stat), ("é".into(), stat)]),
            DriveResult::List(vec![]),
            DriveResult::Handle(u64::MAX),
            DriveResult::Data(vec![1, 2, 3]),
            DriveResult::Written(9),
            DriveResult::Space {
                total: 10,
                available: 4,
            },
        ] {
            roundtrip(ToAgent::DriveReply { req_id: 8, result });
        }
        for op in [
            DriveOp::Stat("a/b".into()),
            DriveOp::List(String::new()),
            DriveOp::Open {
                path: "x".into(),
                mode: 3,
            },
            DriveOp::Read {
                handle: 1,
                offset: 2,
                len: 3,
            },
            DriveOp::Write {
                handle: 1,
                offset: 2,
                data: vec![9; 100],
            },
            DriveOp::Flush(4),
            DriveOp::Close(5),
            DriveOp::Mkdir("d".into()),
            DriveOp::Remove {
                path: "d".into(),
                is_dir: true,
            },
            DriveOp::Rename {
                from: "a".into(),
                to: "b/c".into(),
            },
            DriveOp::Truncate {
                path: "t".into(),
                size: 77,
            },
            DriveOp::SetModified {
                path: "t".into(),
                seconds: -1,
            },
            DriveOp::Space,
        ] {
            roundtrip(FromAgent::DriveCall {
                req_id: 1,
                drive_id: 2,
                op,
            });
        }
    }

    #[test]
    fn print_messages_roundtrip() {
        roundtrip(ToAgent::PrinterAdded {
            printer_id: 4,
            name: "Bureau é".into(),
            driver: "MS Publisher Imagesetter".into(),
            is_default: true,
            format: 3,
        });
        roundtrip(ToAgent::PrinterRemoved { printer_id: 4 });
        for result in [
            PrintResult::Ok,
            PrintResult::Refused("papier".into()),
            PrintResult::Io("lien".into()),
            PrintResult::Job(u64::MAX),
        ] {
            roundtrip(ToAgent::PrintReply { req_id: 3, result });
        }
        for op in [
            PrintOp::Begin {
                title: "doc.pdf".into(),
            },
            PrintOp::Write {
                job: 1,
                data: vec![7; 1000],
            },
            PrintOp::Write {
                job: 1,
                data: vec![],
            },
            PrintOp::Finish(2),
            PrintOp::Abort(3),
        ] {
            roundtrip(FromAgent::PrintCall {
                req_id: 1,
                printer_id: 2,
                op,
            });
        }
    }

    #[test]
    fn drive_list_with_a_lying_count_is_refused() {
        let mut payload = vec![0u8; 0];
        payload.extend_from_slice(&1u32.to_be_bytes());
        payload.push(3);
        payload.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(ToAgent::decode(19, &payload).is_err());
    }
}
