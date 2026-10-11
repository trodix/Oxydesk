//! The drives redirected by the client, mounted as directories (FUSE, read and write).
//!
//! Every file operation of the desktop becomes a call on the drive's `RemoteDrive`: nothing is
//! stored here, so a file of several gigabytes streams through. Directory listings are kept for a
//! couple of seconds, which makes `ls -l` and file managers cost one listing instead of one
//! round trip per file.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
    WriteFlags,
};
use rdsktp_core::drive::{is_valid_path, join, DriveEntry, DriveHandle, DriveStat};
use rdsktp_core::error::DriveError;
use rdsktp_core::{DriveHost, OpenMode, RemoteDrive};

/// How long the kernel may keep the attributes and names it was given.
const ATTR_TTL: Duration = Duration::from_secs(1);
/// How long a directory listing is reused.
const LISTING_TTL: Duration = Duration::from_secs(2);
/// Listings kept at once.
const MAX_LISTINGS: usize = 64;
const ROOT: u64 = 1;
const BLOCK: u32 = 4096;

fn errno(e: &DriveError) -> Errno {
    match e {
        DriveError::NotFound => Errno::ENOENT,
        DriveError::Exists => Errno::EEXIST,
        DriveError::Denied => Errno::EACCES,
        DriveError::NotDirectory => Errno::ENOTDIR,
        DriveError::IsDirectory => Errno::EISDIR,
        DriveError::NotEmpty => Errno::ENOTEMPTY,
        DriveError::NoSpace => Errno::ENOSPC,
        DriveError::Invalid => Errno::EINVAL,
        DriveError::Busy => Errno::EBUSY,
        DriveError::Io(why) => {
            tracing::debug!(why, "drive input/output error");
            Errno::EIO
        }
    }
}

struct Node {
    path: String,
    /// Lookups the kernel holds; the node goes away when it has forgotten them all.
    lookups: u64,
}

struct Inodes {
    by_ino: HashMap<u64, Node>,
    by_path: HashMap<String, u64>,
    next: u64,
}

impl Inodes {
    fn new() -> Self {
        let mut i = Inodes {
            by_ino: HashMap::new(),
            by_path: HashMap::new(),
            next: ROOT + 1,
        };
        i.by_ino.insert(
            ROOT,
            Node {
                path: String::new(),
                lookups: 1,
            },
        );
        i.by_path.insert(String::new(), ROOT);
        i
    }

    /// The inode of `path`, created if needed, with one more lookup held by the kernel.
    fn lookup(&mut self, path: &str) -> u64 {
        let ino = match self.by_path.get(path) {
            Some(i) => *i,
            None => {
                let i = self.next;
                self.next += 1;
                self.by_ino.insert(
                    i,
                    Node {
                        path: path.to_owned(),
                        lookups: 0,
                    },
                );
                self.by_path.insert(path.to_owned(), i);
                i
            }
        };
        if let Some(n) = self.by_ino.get_mut(&ino) {
            n.lookups += 1;
        }
        ino
    }

    fn forget(&mut self, ino: u64, count: u64) {
        if ino == ROOT {
            return;
        }
        if let Some(n) = self.by_ino.get_mut(&ino) {
            n.lookups = n.lookups.saturating_sub(count);
            if n.lookups == 0 {
                let path = n.path.clone();
                self.by_ino.remove(&ino);
                if self.by_path.get(&path) == Some(&ino) {
                    self.by_path.remove(&path);
                }
            }
        }
    }

    /// `from` (and what is under it) is now `to`.
    fn renamed(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        let moved: Vec<String> = self
            .by_path
            .keys()
            .filter(|p| p.as_str() == from || p.starts_with(&prefix))
            .cloned()
            .collect();
        for old in moved {
            if let Some(ino) = self.by_path.remove(&old) {
                let new = format!("{to}{}", &old[from.len()..]);
                // Whatever was at the destination is replaced.
                if let Some(replaced) = self.by_path.insert(new.clone(), ino) {
                    self.by_ino.remove(&replaced);
                }
                if let Some(n) = self.by_ino.get_mut(&ino) {
                    n.path = new;
                }
            }
        }
    }
}

struct Listing {
    at: Instant,
    entries: Arc<Vec<DriveEntry>>,
}

struct DriveFs {
    drive: Arc<dyn RemoteDrive>,
    inodes: Mutex<Inodes>,
    listings: Mutex<HashMap<String, Listing>>,
    uid: u32,
    gid: u32,
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn time(seconds: i64) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds.max(0) as u64)
}

impl DriveFs {
    fn attr(&self, ino: u64, s: &DriveStat) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: s.size,
            blocks: s.size.div_ceil(512),
            atime: time(s.accessed),
            mtime: time(s.modified),
            ctime: time(s.modified),
            crtime: time(s.created),
            kind: if s.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: match (s.is_dir, s.read_only) {
                (true, false) => 0o755,
                (true, true) => 0o555,
                (false, false) => 0o644,
                (false, true) => 0o444,
            },
            nlink: if s.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK,
            flags: 0,
        }
    }

    fn path_of(&self, ino: INodeNo) -> Option<String> {
        self.inodes
            .lock()
            .expect("inodes")
            .by_ino
            .get(&ino.0)
            .map(|n| n.path.clone())
    }

    /// Path of `name` in the directory `parent`; `None` for a name the drive cannot hold.
    fn child_path(&self, parent: INodeNo, name: &OsStr) -> Result<String, Errno> {
        let dir = self.path_of(parent).ok_or(Errno::ENOENT)?;
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        let path = join(&dir, name);
        if is_valid_path(&path) && !name.is_empty() {
            Ok(path)
        } else {
            Err(Errno::EINVAL)
        }
    }

    fn listing(&self, dir: &str) -> Result<Arc<Vec<DriveEntry>>, DriveError> {
        if let Some(l) = self.listings.lock().expect("listings").get(dir) {
            if l.at.elapsed() < LISTING_TTL {
                return Ok(l.entries.clone());
            }
        }
        let mut entries = self.drive.list(dir)?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let entries = Arc::new(entries);
        let mut cache = self.listings.lock().expect("listings");
        if cache.len() >= MAX_LISTINGS {
            cache.clear();
        }
        cache.insert(
            dir.to_owned(),
            Listing {
                at: Instant::now(),
                entries: entries.clone(),
            },
        );
        Ok(entries)
    }

    fn invalidate(&self, dir: &str) {
        self.listings.lock().expect("listings").remove(dir);
    }

    fn stat(&self, path: &str) -> Result<DriveStat, DriveError> {
        if path.is_empty() {
            return Ok(self.drive.stat("").unwrap_or(DriveStat {
                is_dir: true,
                ..DriveStat::default()
            }));
        }
        // A fresh listing of the directory answers without a round trip.
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        if let Some(l) = self.listings.lock().expect("listings").get(dir) {
            if l.at.elapsed() < LISTING_TTL {
                return l
                    .entries
                    .binary_search_by(|e| e.name.as_str().cmp(name))
                    .map(|i| l.entries[i].stat.clone())
                    .map_err(|_| DriveError::NotFound);
            }
        }
        self.drive.stat(path)
    }

    fn entry(&self, path: &str, stat: &DriveStat) -> FileAttr {
        let ino = self.inodes.lock().expect("inodes").lookup(path);
        self.attr(ino, stat)
    }
}

impl Filesystem for DriveFs {
    fn init(&mut self, _: &Request, config: &mut fuser::KernelConfig) -> io::Result<()> {
        let _ = config.set_max_readahead(1024 * 1024);
        Ok(())
    }

    fn forget(&self, _: &Request, ino: INodeNo, nlookup: u64) {
        self.inodes.lock().expect("inodes").forget(ino.0, nlookup);
    }

    fn lookup(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let path = match self.child_path(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        match self.stat(&path) {
            Ok(stat) => reply.entry(&ATTR_TTL, &self.entry(&path, &stat), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn getattr(&self, _: &Request, ino: INodeNo, _: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.path_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.stat(&path) {
            Ok(stat) => reply.attr(&ATTR_TTL, &self.attr(ino.0, &stat)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn setattr(
        &self,
        _: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.path_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let dir = parent_of(&path).to_owned();
        if let Some(size) = size {
            if let Err(e) = self.drive.truncate(&path, size) {
                return reply.error(errno(&e));
            }
        }
        if let Some(mtime) = mtime {
            let seconds = match mtime {
                TimeOrNow::SpecificTime(t) => t
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64),
                TimeOrNow::Now => UNIX_EPOCH.elapsed().map_or(0, |_| {
                    std::time::SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs() as i64)
                }),
            };
            if let Err(e) = self.drive.set_modified(&path, seconds) {
                return reply.error(errno(&e));
            }
        }
        self.invalidate(&dir);
        match self.drive.stat(&path) {
            Ok(stat) => reply.attr(&ATTR_TTL, &self.attr(ino.0, &stat)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn open(&self, _: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.path_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let mode = if flags.0 & libc::O_TRUNC != 0 {
            OpenMode::Truncate
        } else if flags.0 & libc::O_ACCMODE == libc::O_RDONLY {
            OpenMode::Read
        } else {
            OpenMode::ReadWrite
        };
        match self.drive.open(&path, mode) {
            Ok(h) => reply.opened(FileHandle(h.0), FopenFlags::empty()),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn create(
        &self,
        _: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let path = match self.child_path(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        self.invalidate(parent_of(&path));
        match self.drive.open(&path, OpenMode::Create) {
            Ok(h) => {
                let stat = DriveStat {
                    modified: now(),
                    accessed: now(),
                    created: now(),
                    ..DriveStat::default()
                };
                reply.created(
                    &ATTR_TTL,
                    &self.entry(&path, &stat),
                    Generation(0),
                    FileHandle(h.0),
                    FopenFlags::empty(),
                )
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn read(
        &self,
        _: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        match self.drive.read(DriveHandle(fh.0), offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => {
                tracing::warn!("reading a redirected file: {e}");
                reply.error(errno(&e))
            }
        }
    }

    fn write(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _: WriteFlags,
        _: OpenFlags,
        _: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        if let Some(path) = self.path_of(ino) {
            self.invalidate(parent_of(&path));
        }
        match self.drive.write(DriveHandle(fh.0), offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => {
                tracing::warn!("writing a redirected file: {e}");
                reply.error(errno(&e))
            }
        }
    }

    fn flush(
        &self,
        _: &Request,
        _: INodeNo,
        _: FileHandle,
        _: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(&self, _: &Request, _: INodeNo, fh: FileHandle, _: bool, reply: ReplyEmpty) {
        match self.drive.flush(DriveHandle(fh.0)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn release(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _: OpenFlags,
        _: Option<fuser::LockOwner>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        self.drive.close(DriveHandle(fh.0));
        if let Some(path) = self.path_of(ino) {
            self.invalidate(parent_of(&path));
        }
        reply.ok();
    }

    fn mkdir(&self, _: &Request, parent: INodeNo, name: &OsStr, _: u32, _: u32, reply: ReplyEntry) {
        let path = match self.child_path(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        self.invalidate(parent_of(&path));
        match self.drive.mkdir(&path) {
            Ok(()) => {
                let stat = DriveStat {
                    is_dir: true,
                    modified: now(),
                    accessed: now(),
                    created: now(),
                    ..DriveStat::default()
                };
                reply.entry(&ATTR_TTL, &self.entry(&path, &stat), Generation(0))
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, false, reply);
    }

    fn rmdir(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, true, reply);
    }

    fn rename(
        &self,
        _: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if !flags.is_empty() {
            return reply.error(Errno::EINVAL);
        }
        let (from, to) = match (
            self.child_path(parent, name),
            self.child_path(newparent, newname),
        ) {
            (Ok(f), Ok(t)) => (f, t),
            (Err(e), _) | (_, Err(e)) => return reply.error(e),
        };
        self.invalidate(parent_of(&from));
        self.invalidate(parent_of(&to));
        match self.drive.rename(&from, &to) {
            Ok(()) => {
                self.inodes.lock().expect("inodes").renamed(&from, &to);
                reply.ok()
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readdir(
        &self,
        _: &Request,
        ino: INodeNo,
        _: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(path) = self.path_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let entries = match self.listing(&path) {
            Ok(e) => e,
            Err(e) => return reply.error(errno(&e)),
        };
        let inodes = self.inodes.lock().expect("inodes");
        let number = |p: &str| inodes.by_path.get(p).copied().unwrap_or_else(|| hint(p));
        let mut all: Vec<(u64, FileType, &str)> = vec![
            (ino.0, FileType::Directory, "."),
            (number(parent_of(&path)), FileType::Directory, ".."),
        ];
        let paths: Vec<String> = entries.iter().map(|e| join(&path, &e.name)).collect();
        for (e, p) in entries.iter().zip(&paths) {
            let kind = if e.stat.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            all.push((number(p), kind, &e.name));
        }
        for (i, (child, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _: &Request, _: INodeNo, reply: ReplyStatfs) {
        match self.drive.space() {
            Ok(s) => {
                let block = u64::from(BLOCK);
                reply.statfs(
                    s.total / block,
                    s.available / block,
                    s.available / block,
                    0,
                    0,
                    BLOCK,
                    255,
                    BLOCK,
                )
            }
            Err(_) => reply.statfs(0, 0, 0, 0, 0, BLOCK, 255, BLOCK),
        }
    }
}

impl DriveFs {
    fn remove(&self, parent: INodeNo, name: &OsStr, is_dir: bool, reply: ReplyEmpty) {
        let path = match self.child_path(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        // The kind is checked here: the drive deletes whatever is at the path.
        match self.stat(&path) {
            Ok(s) if s.is_dir != is_dir => {
                return reply.error(if is_dir {
                    Errno::ENOTDIR
                } else {
                    Errno::EISDIR
                })
            }
            Err(e) => return reply.error(errno(&e)),
            Ok(_) => {}
        }
        self.invalidate(parent_of(&path));
        match self.drive.remove(&path, is_dir) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Inode number given by `readdir` for a path the kernel has not looked up yet (a hint only).
fn hint(path: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
    }
    (h | (1 << 63)) & !1 | 2
}

fn mount_one(dir: &Path, name: &str, drive: Arc<dyn RemoteDrive>) -> io::Result<BackgroundSession> {
    let _ = std::process::Command::new("fusermount3")
        .args(["-u", "-z"])
        .arg(dir)
        .stderr(std::process::Stdio::null())
        .status();
    std::fs::create_dir_all(dir)?;
    let fs = DriveFs {
        drive,
        inodes: Mutex::new(Inodes::new()),
        listings: Mutex::new(HashMap::new()),
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    };
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::NoAtime,
        MountOption::FSName(format!("rdsktp-drive:{name}")),
    ];
    config.n_threads = Some(4);
    config.clone_fd = true;
    fuser::spawn_mount(fs, dir, &config)
}

/// Mounts each drive announced by the client under `base/<name>`.
pub struct DriveMounts {
    base: PathBuf,
    mounted: Mutex<HashMap<String, BackgroundSession>>,
}

impl DriveMounts {
    /// `base` is created if needed; mounts left by a previous process are released.
    pub fn new(base: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&base)?;
        if let Ok(stale) = std::fs::read_dir(&base) {
            for entry in stale.flatten() {
                let _ = std::process::Command::new("fusermount3")
                    .args(["-u", "-z"])
                    .arg(entry.path())
                    .stderr(std::process::Stdio::null())
                    .status();
                let _ = std::fs::remove_dir(entry.path());
            }
        }
        Ok(Self {
            base,
            mounted: Mutex::default(),
        })
    }
}

impl DriveHost for DriveMounts {
    fn added(&self, name: &str, drive: Arc<dyn RemoteDrive>) {
        let dir = self.base.join(name);
        match mount_one(&dir, name, drive) {
            Ok(session) => {
                tracing::info!(drive = name, dir = %dir.display(), "drive mounted");
                // Replacing a drive of the same name unmounts the old one.
                self.mounted
                    .lock()
                    .expect("mounted")
                    .insert(name.to_owned(), session);
            }
            Err(e) => tracing::warn!(drive = name, "cannot mount the redirected drive: {e}"),
        }
    }

    fn removed(&self, name: &str) {
        let session = self.mounted.lock().expect("mounted").remove(name);
        if session.is_some() {
            drop(session);
            let _ = std::fs::remove_dir(self.base.join(name));
            tracing::info!(drive = name, "drive unmounted");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    /// A `RemoteDrive` over a local directory.
    struct Local(
        PathBuf,
        Mutex<HashMap<u64, std::fs::File>>,
        std::sync::atomic::AtomicU64,
    );

    fn io_err(e: io::Error) -> DriveError {
        match e.kind() {
            io::ErrorKind::NotFound => DriveError::NotFound,
            io::ErrorKind::AlreadyExists => DriveError::Exists,
            io::ErrorKind::DirectoryNotEmpty => DriveError::NotEmpty,
            _ => DriveError::Io(e.to_string()),
        }
    }

    fn st(m: &std::fs::Metadata) -> DriveStat {
        let secs = |t: io::Result<std::time::SystemTime>| {
            t.ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64)
        };
        DriveStat {
            size: m.len(),
            is_dir: m.is_dir(),
            read_only: false,
            modified: secs(m.modified()),
            accessed: secs(m.accessed()),
            created: secs(m.created()),
        }
    }

    impl RemoteDrive for Local {
        fn stat(&self, path: &str) -> Result<DriveStat, DriveError> {
            std::fs::metadata(self.0.join(path))
                .map(|m| st(&m))
                .map_err(io_err)
        }
        fn list(&self, path: &str) -> Result<Vec<DriveEntry>, DriveError> {
            let mut out = Vec::new();
            for e in std::fs::read_dir(self.0.join(path))
                .map_err(io_err)?
                .flatten()
            {
                out.push(DriveEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    stat: st(&e.metadata().map_err(io_err)?),
                });
            }
            Ok(out)
        }
        fn open(&self, path: &str, mode: OpenMode) -> Result<DriveHandle, DriveError> {
            let mut o = std::fs::OpenOptions::new();
            match mode {
                OpenMode::Read => o.read(true),
                OpenMode::ReadWrite => o.read(true).write(true),
                OpenMode::Truncate => o.read(true).write(true).truncate(true),
                OpenMode::Create => o.read(true).write(true).create_new(true),
            };
            let f = o.open(self.0.join(path)).map_err(io_err)?;
            let id = self.2.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            self.1.lock().unwrap().insert(id, f);
            Ok(DriveHandle(id))
        }
        fn read(&self, h: DriveHandle, offset: u64, len: u32) -> Result<Vec<u8>, DriveError> {
            let files = self.1.lock().unwrap();
            let f = files.get(&h.0).ok_or(DriveError::Invalid)?;
            let mut buf = vec![0; len as usize];
            let n = f.read_at(&mut buf, offset).map_err(io_err)?;
            buf.truncate(n);
            Ok(buf)
        }
        fn write(&self, h: DriveHandle, offset: u64, data: &[u8]) -> Result<u32, DriveError> {
            let files = self.1.lock().unwrap();
            let f = files.get(&h.0).ok_or(DriveError::Invalid)?;
            f.write_all_at(data, offset).map_err(io_err)?;
            Ok(data.len() as u32)
        }
        fn flush(&self, _: DriveHandle) -> Result<(), DriveError> {
            Ok(())
        }
        fn close(&self, h: DriveHandle) {
            self.1.lock().unwrap().remove(&h.0);
        }
        fn mkdir(&self, path: &str) -> Result<(), DriveError> {
            std::fs::create_dir(self.0.join(path)).map_err(io_err)
        }
        fn remove(&self, path: &str, is_dir: bool) -> Result<(), DriveError> {
            let p = self.0.join(path);
            if is_dir {
                std::fs::remove_dir(p)
            } else {
                std::fs::remove_file(p)
            }
            .map_err(io_err)
        }
        fn rename(&self, from: &str, to: &str) -> Result<(), DriveError> {
            std::fs::rename(self.0.join(from), self.0.join(to)).map_err(io_err)
        }
        fn truncate(&self, path: &str, size: u64) -> Result<(), DriveError> {
            std::fs::OpenOptions::new()
                .write(true)
                .open(self.0.join(path))
                .and_then(|f| f.set_len(size))
                .map_err(io_err)
        }
        fn set_modified(&self, path: &str, seconds: i64) -> Result<(), DriveError> {
            std::fs::File::options()
                .write(true)
                .open(self.0.join(path))
                .and_then(|f| f.set_modified(time(seconds)))
                .map_err(io_err)
        }
        fn space(&self) -> Result<rdsktp_core::DriveSpace, DriveError> {
            Ok(rdsktp_core::DriveSpace {
                total: 1 << 30,
                available: 1 << 29,
            })
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rdsktp-drivefs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn inodes_follow_renames_and_forgets() {
        let mut i = Inodes::new();
        let a = i.lookup("d");
        let b = i.lookup("d/x");
        i.renamed("d", "e");
        assert_eq!(i.by_ino[&a].path, "e");
        assert_eq!(i.by_ino[&b].path, "e/x");
        assert_eq!(i.by_path.get("e/x"), Some(&b));
        assert!(!i.by_path.contains_key("d"));
        i.forget(a, 1);
        assert!(!i.by_ino.contains_key(&a));
        i.forget(ROOT, 5);
        assert!(i.by_ino.contains_key(&ROOT));
        assert_ne!(hint("a") & 1, 1);
    }

    /// Everyday operations through a real mount (skipped where FUSE is not usable).
    #[test]
    fn a_mounted_drive_behaves_like_a_directory() {
        let source = tmp("src");
        let base = tmp("mnt");
        let mounts = match DriveMounts::new(base.clone()) {
            Ok(m) => m,
            Err(e) => return eprintln!("cannot prepare mounts, test skipped: {e}"),
        };
        std::fs::write(source.join("a.txt"), b"alpha").unwrap();
        std::fs::create_dir(source.join("d")).unwrap();
        let drive = Arc::new(Local(source.clone(), Mutex::default(), Default::default()));
        mounts.added("Share", drive);
        let mnt = base.join("Share");
        let mounted = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            std::fs::read_dir(&mnt).is_ok_and(|mut d| d.next().is_some())
        });
        if !mounted {
            return eprintln!("FUSE unavailable, test skipped");
        }

        assert_eq!(std::fs::read(mnt.join("a.txt")).unwrap(), b"alpha");
        let mut names: Vec<_> = std::fs::read_dir(&mnt)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["a.txt", "d"]);
        assert!(std::fs::metadata(mnt.join("d")).unwrap().is_dir());
        assert!(std::fs::metadata(mnt.join("missing")).is_err());

        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(mnt.join("d/big.bin"), &data).unwrap();
        assert_eq!(std::fs::read(source.join("d/big.bin")).unwrap(), data);
        assert_eq!(
            std::fs::metadata(mnt.join("d/big.bin")).unwrap().len(),
            3_000_000
        );
        assert_eq!(std::fs::read(mnt.join("d/big.bin")).unwrap(), data);

        std::fs::rename(mnt.join("d/big.bin"), mnt.join("moved.bin")).unwrap();
        assert!(source.join("moved.bin").exists() && !source.join("d/big.bin").exists());
        assert_eq!(
            std::fs::read(mnt.join("moved.bin")).unwrap().len(),
            3_000_000
        );
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(mnt.join("moved.bin"))
            .unwrap();
        f.set_len(10).unwrap();
        drop(f);
        assert_eq!(
            std::fs::metadata(source.join("moved.bin")).unwrap().len(),
            10
        );

        std::fs::create_dir(mnt.join("new")).unwrap();
        assert!(std::fs::create_dir(mnt.join("new")).is_err());
        std::fs::remove_file(mnt.join("moved.bin")).unwrap();
        assert!(std::fs::remove_dir(mnt.join("d")).is_ok());
        std::fs::remove_dir(mnt.join("new")).unwrap();
        assert!(std::fs::remove_file(mnt.join("a.txt")).is_ok());
        assert!(std::fs::read_dir(&source).unwrap().next().is_none());

        mounts.removed("Share");
        assert!(!mnt.exists());
        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&base);
    }
}
