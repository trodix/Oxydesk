//! Drives redirected by the client (RDPDR). The protocol part: `DriveBackend` receives the
//! completions of the requests the server sends, and `RdpDrive` turns the blocking `RemoteDrive`
//! calls of the desktop side into requests (`ServerEvent::Rdpdr`) and waits for their completion.
//!
//! Requests carry a tag; the channel hands it back with the completion id it assigned, so any
//! number of requests can be in flight and each answer reaches its caller.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_core::impl_as_any;
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::pdu::efs::{
    Boolean, ClientDriveLockControlResponse, ClientDriveNotifyChangeDirectoryResponse,
    ClientDriveQueryDirectoryResponse, ClientDriveQueryInformationResponse,
    ClientDriveQuerySecurityResponse, ClientDriveQueryVolumeInformationResponse,
    ClientDriveSetInformationResponse, ClientDriveSetSecurityResponse, CreateDisposition,
    CreateOptions, DesiredAccess, DeviceAnnounceHeader, DeviceCloseResponse, DeviceControlResponse,
    DeviceCreateResponse, DeviceFlushBuffersResponse, DeviceIoResponse, DeviceReadResponse,
    DeviceType, DeviceWriteResponse, FileAttributes, FileBasicInformation,
    FileDispositionInformation, FileEndOfFileInformation, FileInformationClass,
    FileInformationClassLevel, FileRenameInformation, FileSystemInformationClass,
    FileSystemInformationClassLevel, NtStatus,
};
use ironrdp_server::{
    RdpdrServerBackend, RdpdrServerFactory, RdpdrServerMessage, ServerEvent, ServerEventSender,
};
use rdsktp_core::drive::{DriveEntry, DriveHandle, DriveHost, DriveSpace, DriveStat, OpenMode};
use rdsktp_core::error::DriveError;
use rdsktp_core::printer::{PrinterHost, PrinterInfo, RemotePrinter};
use rdsktp_core::RemoteDrive;
use tokio::sync::mpsc::UnboundedSender;

use crate::printer::{announced_printer, RdpPrinter};
use crate::pump::Shared;

/// Sends a request event to the connection; `false` if it is gone.
pub type EventSender = Arc<dyn Fn(ServerEvent) -> bool + Send + Sync>;

/// A request that gets no answer for this long fails.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest block of one read or write request.
const IRP_BLOCK: u32 = 256 * 1024;
/// Most entries read from one directory.
const MAX_LISTING: usize = 1_000_000;
/// Seconds between 1601-01-01 (FILETIME) and 1970-01-01.
const FILETIME_EPOCH_DELTA: i64 = 11_644_473_600;

fn from_filetime(ft: i64) -> i64 {
    if ft <= 0 {
        0
    } else {
        ft / 10_000_000 - FILETIME_EPOCH_DELTA
    }
}

fn to_filetime(unix: i64) -> i64 {
    (unix + FILETIME_EPOCH_DELTA).max(0) * 10_000_000
}

/// The path as the client expects it: relative to the drive, backslash-separated, leading backslash.
fn windows_path(path: &str) -> String {
    format!("\\{}", path.replace('/', "\\"))
}

fn status_error(status: NtStatus) -> DriveError {
    match status {
        NtStatus::NO_SUCH_FILE | NtStatus::OBJECT_PATH_NOT_FOUND => DriveError::NotFound,
        NtStatus::OBJECT_NAME_COLLISION => DriveError::Exists,
        NtStatus::ACCESS_DENIED
        | NtStatus::MEDIA_WRITE_PROTECTED
        | NtStatus::PRIVILEGE_NOT_HELD => DriveError::Denied,
        NtStatus::NOT_A_DIRECTORY => DriveError::NotDirectory,
        NtStatus::FILE_IS_A_DIRECTORY => DriveError::IsDirectory,
        NtStatus::DIRECTORY_NOT_EMPTY => DriveError::NotEmpty,
        NtStatus::DISK_FULL => DriveError::NoSpace,
        NtStatus::SHARING_VIOLATION | NtStatus::DELETE_PENDING => DriveError::Busy,
        NtStatus::INVALID_PARAMETER | NtStatus::OBJECT_NAME_INVALID | NtStatus::INVALID_HANDLE => {
            DriveError::Invalid
        }
        other => DriveError::Io(format!("{other:?}")),
    }
}

/// What a completion carried besides its status.
#[derive(Debug)]
pub(crate) enum Body {
    None,
    Create(u32),
    Read(Vec<u8>),
    Write(u32),
    Info(Option<FileInformationClass>),
    Volume(Option<FileSystemInformationClass>),
}

#[derive(Debug)]
pub(crate) struct Completed {
    pub(crate) status: NtStatus,
    pub(crate) body: Body,
}

impl Completed {
    fn ok(self) -> Result<Body, DriveError> {
        if self.status == NtStatus::SUCCESS {
            Ok(self.body)
        } else {
            Err(status_error(self.status))
        }
    }
}

#[derive(Default)]
struct Waiters {
    /// Callers waiting for their completion, by tag.
    by_tag: HashMap<u64, Sender<Completed>>,
    /// Completion id assigned by the channel to each tag.
    by_completion: HashMap<u32, u64>,
    closed: bool,
}

/// What the callers and the backend of one connection share.
pub(crate) struct Link {
    send: EventSender,
    next_tag: AtomicU64,
    waiters: Mutex<Waiters>,
}

impl Link {
    pub(crate) fn call(&self, message: RdpdrServerMessage) -> Result<Completed, DriveError> {
        let tag = self.next_tag.fetch_add(1, Ordering::Relaxed);
        let request = tracing::enabled!(tracing::Level::DEBUG).then(|| {
            let mut text = format!("{message:?}");
            text.truncate(text.floor_char_boundary(200));
            text
        });
        let (tx, rx): (Sender<Completed>, Receiver<Completed>) = mpsc::channel();
        {
            let mut w = self.waiters.lock().expect("waiters");
            if w.closed {
                return Err(DriveError::Io("connection closed".into()));
            }
            w.by_tag.insert(tag, tx);
        }
        if !(self.send)(ServerEvent::Rdpdr(Some(tag), message)) {
            self.forget(tag);
            return Err(DriveError::Io("connection closed".into()));
        }
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(done) => {
                if done.status != NtStatus::SUCCESS {
                    tracing::debug!(status = ?done.status, request, "drive request refused by the client");
                }
                Ok(done)
            }
            Err(RecvTimeoutError::Timeout) => {
                tracing::debug!(request, "the client does not answer a drive request");
                self.forget(tag);
                Err(DriveError::Io("the client does not answer".into()))
            }
            Err(RecvTimeoutError::Disconnected) => Err(DriveError::Io("connection closed".into())),
        }
    }

    fn forget(&self, tag: u64) {
        let mut w = self.waiters.lock().expect("waiters");
        w.by_tag.remove(&tag);
        w.by_completion.retain(|_, t| *t != tag);
    }

    fn sent(&self, completion_id: u32, tag: u64) {
        let mut w = self.waiters.lock().expect("waiters");
        if w.by_tag.contains_key(&tag) {
            w.by_completion.insert(completion_id, tag);
        }
    }

    fn finish(&self, reply: &DeviceIoResponse, body: Body) {
        let tx = {
            let mut w = self.waiters.lock().expect("waiters");
            w.by_completion
                .remove(&reply.completion_id)
                .and_then(|tag| w.by_tag.remove(&tag))
        };
        if let Some(tx) = tx {
            let _ = tx.send(Completed {
                status: reply.io_status,
                body,
            });
        }
    }

    /// The connection is over: every waiting caller fails.
    fn close(&self) {
        let mut w = self.waiters.lock().expect("waiters");
        w.closed = true;
        w.by_tag.clear();
        w.by_completion.clear();
    }
}

/// A drive of the client seen through RDPDR.
pub struct RdpDrive {
    link: Arc<Link>,
    device_id: u32,
}

impl RdpDrive {
    fn call(&self, message: RdpdrServerMessage) -> Result<Body, DriveError> {
        self.link.call(message)?.ok()
    }

    fn create(
        &self,
        path: &str,
        access: DesiredAccess,
        disposition: CreateDisposition,
        options: CreateOptions,
    ) -> Result<u32, DriveError> {
        // The Windows client answers STATUS_INVALID_PARAMETER for a path that does not exist.
        let body = self
            .call(RdpdrServerMessage::Create {
                device_id: self.device_id,
                path: windows_path(path),
                desired_access: access | DesiredAccess::SYNCHRONIZE,
                create_disposition: disposition,
                // SYNCHRONIZE is asked: Windows (like its own redirector) wants the synchronous flag with it.
                create_options: options | CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT,
            })
            .map_err(|e| match e {
                DriveError::Invalid if disposition == CreateDisposition::FILE_OPEN => {
                    DriveError::NotFound
                }
                other => other,
            })?;
        match body {
            Body::Create(file_id) => Ok(file_id),
            _ => Err(DriveError::Io("unexpected answer to a create".into())),
        }
    }

    fn close_file(&self, file_id: u32) -> Result<(), DriveError> {
        self.call(RdpdrServerMessage::Close {
            device_id: self.device_id,
            file_id,
        })
        .map(|_| ())
    }

    fn with_file<T>(
        &self,
        path: &str,
        access: DesiredAccess,
        options: CreateOptions,
        f: impl FnOnce(u32) -> Result<T, DriveError>,
    ) -> Result<T, DriveError> {
        let file_id = self.create(path, access, CreateDisposition::FILE_OPEN, options)?;
        let result = f(file_id);
        let _ = self.close_file(file_id);
        result
    }

    fn query(
        &self,
        file_id: u32,
        class: FileInformationClassLevel,
    ) -> Result<FileInformationClass, DriveError> {
        match self.call(RdpdrServerMessage::QueryInformation {
            device_id: self.device_id,
            file_id,
            info_class: class,
        })? {
            Body::Info(Some(info)) => Ok(info),
            _ => Err(DriveError::Io("empty answer to a query".into())),
        }
    }

    fn set(&self, file_id: u32, info: FileInformationClass) -> Result<(), DriveError> {
        self.call(RdpdrServerMessage::SetInformation {
            device_id: self.device_id,
            file_id,
            set_buffer: info,
        })
        .map(|_| ())
    }

    fn query_directory(
        &self,
        file_id: u32,
        pattern: &str,
        initial: bool,
    ) -> Result<Option<FileInformationClass>, DriveError> {
        let done = self.link.call(RdpdrServerMessage::QueryDirectory {
            device_id: self.device_id,
            file_id,
            info_class: FileInformationClassLevel::FILE_BOTH_DIRECTORY_INFORMATION,
            path: pattern.into(),
            initial_query: initial,
        })?;
        if done.status == NtStatus::NO_MORE_FILES
            || done.status == NtStatus::NO_SUCH_FILE && !initial
        {
            return Ok(None);
        }
        match done.ok()? {
            Body::Info(info) => Ok(info),
            _ => Ok(None),
        }
    }
}

fn read_access() -> DesiredAccess {
    DesiredAccess::FILE_READ_DATA_OR_FILE_LIST_DIRECTORY | DesiredAccess::FILE_READ_ATTRIBUTES
}

fn write_access() -> DesiredAccess {
    read_access()
        | DesiredAccess::FILE_WRITE_DATA_OR_FILE_ADD_FILE
        | DesiredAccess::FILE_APPEND_DATA_OR_FILE_ADD_SUBDIRECTORY
        | DesiredAccess::FILE_WRITE_ATTRIBUTES
}

fn stat_of(
    attributes: FileAttributes,
    size: i64,
    modified: i64,
    accessed: i64,
    created: i64,
) -> DriveStat {
    DriveStat {
        size: size.max(0) as u64,
        is_dir: attributes.contains(FileAttributes::FILE_ATTRIBUTE_DIRECTORY),
        read_only: attributes.contains(FileAttributes::FILE_ATTRIBUTE_READONLY),
        modified: from_filetime(modified),
        accessed: from_filetime(accessed),
        created: from_filetime(created),
    }
}

impl RdpDrive {
    fn stat_as(&self, path: &str, options: CreateOptions) -> Result<DriveStat, DriveError> {
        self.with_file(path, DesiredAccess::FILE_READ_ATTRIBUTES, options, |file| {
            let FileInformationClass::Basic(basic) =
                self.query(file, FileInformationClassLevel::FILE_BASIC_INFORMATION)?
            else {
                return Err(DriveError::Io("unexpected basic information".into()));
            };
            let FileInformationClass::Standard(standard) =
                self.query(file, FileInformationClassLevel::FILE_STANDARD_INFORMATION)?
            else {
                return Err(DriveError::Io("unexpected standard information".into()));
            };
            let mut stat = stat_of(
                basic.file_attributes,
                standard.end_of_file,
                basic.last_write_time,
                basic.last_access_time,
                basic.creation_time,
            );
            stat.is_dir |= standard.directory == Boolean::True;
            Ok(stat)
        })
    }
}

impl RemoteDrive for RdpDrive {
    fn stat(&self, path: &str) -> Result<DriveStat, DriveError> {
        // The Windows client refuses an open of a file that states neither kind (INVALID_PARAMETER, seen here as
        // NotFound), and FreeRDP one that states the wrong kind: both are tried.
        match self.stat_as(path, CreateOptions::empty()) {
            Err(DriveError::NotFound) => self.stat_as(path, CreateOptions::FILE_NON_DIRECTORY_FILE),
            other => other,
        }
    }

    fn list(&self, path: &str) -> Result<Vec<DriveEntry>, DriveError> {
        self.with_file(
            path,
            read_access(),
            CreateOptions::FILE_DIRECTORY_FILE,
            |dir| {
                let mut entries = Vec::new();
                // FreeRDP resolves the pattern from the drive root, so it names the directory; the Windows client
                // applies it inside the directory handle, where only `\*` matches. Both are tried.
                let named = format!("{}\\*", windows_path(path).trim_end_matches('\\'));
                let mut patterns = vec![named.as_str()];
                if !path.is_empty() {
                    patterns.push("\\*");
                }
                for pattern in patterns {
                    let mut initial = true;
                    let mut seen = false;
                    while entries.len() < MAX_LISTING {
                        let Some(info) = self.query_directory(dir, pattern, initial)? else {
                            break;
                        };
                        initial = false;
                        seen = true;
                        if let FileInformationClass::BothDirectory(e) = info {
                            if e.file_name != "." && e.file_name != ".." && !e.file_name.is_empty()
                            {
                                entries.push(DriveEntry {
                                    stat: stat_of(
                                        e.file_attributes,
                                        e.end_of_file,
                                        e.last_write_time,
                                        e.last_access_time,
                                        e.creation_time,
                                    ),
                                    name: e.file_name,
                                });
                            }
                        }
                    }
                    if seen {
                        break;
                    }
                }
                Ok(entries)
            },
        )
    }

    fn open(&self, path: &str, mode: OpenMode) -> Result<DriveHandle, DriveError> {
        let (access, disposition) = match mode {
            OpenMode::Read => (read_access(), CreateDisposition::FILE_OPEN),
            OpenMode::ReadWrite => (write_access(), CreateDisposition::FILE_OPEN),
            OpenMode::Truncate => (write_access(), CreateDisposition::FILE_OVERWRITE),
            OpenMode::Create => (write_access(), CreateDisposition::FILE_CREATE),
        };
        self.create(
            path,
            access,
            disposition,
            CreateOptions::FILE_NON_DIRECTORY_FILE,
        )
        .map(|id| DriveHandle(u64::from(id)))
    }

    fn read(&self, file: DriveHandle, offset: u64, len: u32) -> Result<Vec<u8>, DriveError> {
        let mut data = Vec::with_capacity(len.min(IRP_BLOCK) as usize);
        // A short read means the end of the file for the kernel: ask again until the block is full.
        while (data.len() as u32) < len {
            let want = (len - data.len() as u32).min(IRP_BLOCK);
            let done = self.link.call(RdpdrServerMessage::Read {
                device_id: self.device_id,
                file_id: file.0 as u32,
                length: want,
                offset: offset + data.len() as u64,
            })?;
            if done.status == NtStatus::END_OF_FILE {
                break;
            }
            let Body::Read(chunk) = done.ok()? else {
                return Err(DriveError::Io("unexpected answer to a read".into()));
            };
            if chunk.is_empty() {
                break;
            }
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }

    fn write(&self, file: DriveHandle, offset: u64, data: &[u8]) -> Result<u32, DriveError> {
        let mut written = 0usize;
        for block in data.chunks(IRP_BLOCK as usize) {
            let Body::Write(n) = self.call(RdpdrServerMessage::Write {
                device_id: self.device_id,
                file_id: file.0 as u32,
                data: block.to_vec(),
                offset: offset + written as u64,
            })?
            else {
                return Err(DriveError::Io("unexpected answer to a write".into()));
            };
            written += n as usize;
            if (n as usize) < block.len() {
                break;
            }
        }
        Ok(written as u32)
    }

    fn flush(&self, _file: DriveHandle) -> Result<(), DriveError> {
        // Not sent: every write is complete when the client answers it, FreeRDP refuses the request (NOT_SUPPORTED)
        // and the Windows client never answers it.
        Ok(())
    }

    fn close(&self, file: DriveHandle) {
        let _ = self.close_file(file.0 as u32);
    }

    fn mkdir(&self, path: &str) -> Result<(), DriveError> {
        let file = self.create(
            path,
            write_access(),
            CreateDisposition::FILE_CREATE,
            CreateOptions::FILE_DIRECTORY_FILE,
        )?;
        self.close_file(file)
    }

    fn remove(&self, path: &str, is_dir: bool) -> Result<(), DriveError> {
        let kind = if is_dir {
            CreateOptions::FILE_DIRECTORY_FILE
        } else {
            CreateOptions::FILE_NON_DIRECTORY_FILE
        };
        if is_dir && !self.list(path)?.is_empty() {
            return Err(DriveError::NotEmpty);
        }
        // The deletion happens when the handle is closed. Both ways of asking are used: Windows honours either,
        // FreeRDP only the disposition information.
        let file = self.create(
            path,
            DesiredAccess::DELETE | DesiredAccess::FILE_READ_ATTRIBUTES,
            CreateDisposition::FILE_OPEN,
            kind | CreateOptions::FILE_DELETE_ON_CLOSE,
        )?;
        let marked = self.set(
            file,
            FileInformationClass::Disposition(FileDispositionInformation { delete_pending: 1 }),
        );
        let closed = self.close_file(file);
        marked.and(closed)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), DriveError> {
        self.with_file(
            from,
            DesiredAccess::DELETE | DesiredAccess::FILE_READ_ATTRIBUTES,
            CreateOptions::empty(),
            |file| {
                self.set(
                    file,
                    FileInformationClass::Rename(FileRenameInformation {
                        replace_if_exists: Boolean::True,
                        file_name: windows_path(to),
                    }),
                )
            },
        )
    }

    fn truncate(&self, path: &str, size: u64) -> Result<(), DriveError> {
        self.with_file(
            path,
            write_access(),
            CreateOptions::FILE_NON_DIRECTORY_FILE,
            |file| {
                self.set(
                    file,
                    FileInformationClass::EndOfFile(FileEndOfFileInformation {
                        end_of_file: size as i64,
                    }),
                )
            },
        )
    }

    fn set_modified(&self, path: &str, seconds: i64) -> Result<(), DriveError> {
        self.with_file(
            path,
            DesiredAccess::FILE_WRITE_ATTRIBUTES | DesiredAccess::FILE_READ_ATTRIBUTES,
            CreateOptions::empty(),
            |file| {
                // 0 leaves a field unchanged.
                self.set(
                    file,
                    FileInformationClass::Basic(FileBasicInformation {
                        creation_time: 0,
                        last_access_time: 0,
                        last_write_time: to_filetime(seconds),
                        change_time: 0,
                        file_attributes: FileAttributes::empty(),
                    }),
                )
            },
        )
    }

    fn space(&self) -> Result<DriveSpace, DriveError> {
        self.with_file(
            "",
            read_access(),
            CreateOptions::FILE_DIRECTORY_FILE,
            |root| {
                let Body::Volume(Some(FileSystemInformationClass::FileFsSizeInformation(size))) =
                    self.call(RdpdrServerMessage::QueryVolumeInformation {
                        device_id: self.device_id,
                        file_id: root,
                        fs_info_class: FileSystemInformationClassLevel::FILE_FS_SIZE_INFORMATION,
                    })?
                else {
                    return Err(DriveError::Io("unexpected volume information".into()));
                };
                let unit =
                    u64::from(size.sectors_per_alloc_unit) * u64::from(size.bytes_per_sector);
                Ok(DriveSpace {
                    total: size.total_alloc_units.max(0) as u64 * unit,
                    available: size.available_alloc_units.max(0) as u64 * unit,
                })
            },
        )
    }
}

/// What the worker thread does for the host, in order.
enum Command {
    Added(String, Arc<dyn RemoteDrive>),
    Removed(String),
    PrinterAdded(String, PrinterInfo, Arc<dyn RemotePrinter>),
    PrinterRemoved(String),
}

/// Handles the announcements of the client and the completions of the requests.
pub struct DriveBackend {
    link: Arc<Link>,
    host: Option<Arc<dyn DriveHost>>,
    printer_host: Option<Arc<dyn PrinterHost>>,
    /// Drive directory names, by device id.
    drives: HashMap<u32, String>,
    /// Printer names, by device id.
    printers: HashMap<u32, String>,
    commands: Option<Sender<Command>>,
}

impl std::fmt::Debug for DriveBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveBackend")
            .field("drives", &self.drives)
            .field("printers", &self.printers)
            .finish_non_exhaustive()
    }
}

impl_as_any!(DriveBackend);

impl DriveBackend {
    pub fn new(
        send: EventSender,
        host: Option<Arc<dyn DriveHost>>,
        printer_host: Option<Arc<dyn PrinterHost>>,
    ) -> Self {
        let commands = (host.is_some() || printer_host.is_some()).then(|| {
            let (tx, rx) = mpsc::channel::<Command>();
            let (drive_host, print_host) = (host.clone(), printer_host.clone());
            // Mounting can take a moment and must not stall the connection: a thread, in order.
            std::thread::Builder::new()
                .name("drive-host".into())
                .spawn(move || {
                    for command in rx {
                        match (command, &drive_host, &print_host) {
                            (Command::Added(name, drive), Some(host), _) => {
                                host.added(&name, drive)
                            }
                            (Command::Removed(name), Some(host), _) => host.removed(&name),
                            (Command::PrinterAdded(name, info, printer), _, Some(host)) => {
                                host.added(&name, &info, printer)
                            }
                            (Command::PrinterRemoved(name), _, Some(host)) => host.removed(&name),
                            _ => {}
                        }
                    }
                })
                .ok();
            tx
        });
        Self {
            link: Arc::new(Link {
                send,
                next_tag: AtomicU64::new(1),
                waiters: Mutex::default(),
            }),
            host,
            printer_host,
            drives: HashMap::new(),
            printers: HashMap::new(),
            commands,
        }
    }

    fn unique_printer_name(&self, wanted: &str) -> String {
        let base = rdsktp_core::printer_name(wanted);
        let mut name = base.clone();
        let mut n = 2;
        while self.printers.values().any(|p| *p == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        name
    }

    fn announce_printer(&mut self, id: u32, device: &DeviceAnnounceHeader) -> bool {
        if self.printer_host.is_none() {
            return false;
        }
        let Some(announce) = announced_printer(device) else {
            tracing::warn!(device = id, "printer announcement not understood");
            return false;
        };
        let info = announce.info();
        let name = self.unique_printer_name(&info.name);
        tracing::info!(device = id, %name, driver = %info.driver, "printer redirected by the client");
        let printer = Arc::new(RdpPrinter {
            link: self.link.clone(),
            device_id: id,
        }) as Arc<dyn RemotePrinter>;
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::PrinterAdded(name.clone(), info, printer));
        }
        self.printers.insert(id, name);
        true
    }

    fn unique_name(&self, wanted: &str) -> String {
        let base = rdsktp_core::drive::directory_name(wanted);
        let mut name = base.clone();
        let mut n = 2;
        while self.drives.values().any(|d| *d == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        name
    }
}

/// The display name carried by the announcement: UTF-16 (Windows clients) or plain bytes (FreeRDP sends its name as
/// a C string). Text without any zero byte before its terminator is not UTF-16.
fn announced_name(data: &[u8]) -> String {
    let text = data
        .strip_suffix(&[0, 0])
        .or_else(|| data.strip_suffix(&[0]))
        .unwrap_or(data);
    if text.is_empty() {
        return String::new();
    }
    if !text.contains(&0) {
        return String::from_utf8_lossy(text).into_owned();
    }
    let units: Vec<u16> = text
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// The display name of a drive: its announced name, else its DOS name.
fn drive_label(device: &DeviceAnnounceHeader) -> String {
    let name = announced_name(device.device_data());
    if name.trim().is_empty() {
        device.preferred_dos_name().to_owned()
    } else {
        name
    }
}

impl Drop for DriveBackend {
    fn drop(&mut self) {
        self.link.close();
        if let Some(commands) = &self.commands {
            for name in self.drives.values() {
                let _ = commands.send(Command::Removed(name.clone()));
            }
            for name in self.printers.values() {
                let _ = commands.send(Command::PrinterRemoved(name.clone()));
            }
        }
    }
}

impl RdpdrServerBackend for DriveBackend {
    fn on_device_announce(&mut self, devices: &[DeviceAnnounceHeader]) -> Vec<(u32, bool)> {
        devices
            .iter()
            .map(|device| {
                let id = device.device_id();
                if device.device_type() == DeviceType::Print {
                    return (id, self.announce_printer(id, device));
                }
                if device.device_type() != DeviceType::Filesystem || self.host.is_none() {
                    return (id, false);
                }
                let name = self.unique_name(&drive_label(device));
                tracing::info!(device = id, %name, "drive redirected by the client");
                let drive = Arc::new(RdpDrive {
                    link: self.link.clone(),
                    device_id: id,
                }) as Arc<dyn RemoteDrive>;
                if let Some(commands) = &self.commands {
                    let _ = commands.send(Command::Added(name.clone(), drive));
                }
                self.drives.insert(id, name);
                (id, true)
            })
            .collect()
    }

    fn on_device_remove(&mut self, device_ids: &[u32]) {
        for id in device_ids {
            if let (Some(name), Some(commands)) = (self.drives.remove(id), &self.commands) {
                let _ = commands.send(Command::Removed(name));
            }
            if let (Some(name), Some(commands)) = (self.printers.remove(id), &self.commands) {
                let _ = commands.send(Command::PrinterRemoved(name));
            }
        }
    }

    fn on_client_name(&mut self, computer_name: &str) {
        tracing::debug!(client = computer_name, "rdpdr client");
    }

    fn on_request_sent(&mut self, completion_id: u32, tag: Option<u64>) {
        if let Some(tag) = tag {
            self.link.sent(completion_id, tag);
        }
    }

    fn on_create_complete(&mut self, r: &DeviceCreateResponse) -> PduResult<()> {
        self.link
            .finish(&r.device_io_reply, Body::Create(r.file_id));
        Ok(())
    }

    fn on_close_complete(&mut self, r: &DeviceCloseResponse) -> PduResult<()> {
        self.link.finish(&r.device_io_response, Body::None);
        Ok(())
    }

    fn on_read_complete(&mut self, r: &DeviceReadResponse) -> PduResult<()> {
        self.link
            .finish(&r.device_io_reply, Body::Read(r.read_data.clone()));
        Ok(())
    }

    fn on_write_complete(&mut self, r: &DeviceWriteResponse) -> PduResult<()> {
        self.link.finish(&r.device_io_reply, Body::Write(r.length));
        Ok(())
    }

    fn on_flush_buffers_complete(&mut self, r: &DeviceFlushBuffersResponse) -> PduResult<()> {
        self.link.finish(&r.device_io_response, Body::None);
        Ok(())
    }

    fn on_device_control_complete(&mut self, r: &DeviceControlResponse) -> PduResult<()> {
        self.link.finish(&r.device_io_reply, Body::None);
        Ok(())
    }

    fn on_query_information_complete(
        &mut self,
        r: &ClientDriveQueryInformationResponse,
    ) -> PduResult<()> {
        self.link
            .finish(&r.device_io_response, Body::Info(r.buffer.clone()));
        Ok(())
    }

    fn on_set_information_complete(
        &mut self,
        r: &ClientDriveSetInformationResponse,
    ) -> PduResult<()> {
        self.link.finish(&r.device_io_reply, Body::None);
        Ok(())
    }

    fn on_query_directory_complete(
        &mut self,
        r: &ClientDriveQueryDirectoryResponse,
    ) -> PduResult<()> {
        self.link
            .finish(&r.device_io_reply, Body::Info(r.buffer.clone()));
        Ok(())
    }

    fn on_notify_change_directory_complete(
        &mut self,
        r: &ClientDriveNotifyChangeDirectoryResponse,
    ) -> PduResult<()> {
        self.link.finish(&r.device_io_reply, Body::None);
        Ok(())
    }

    fn on_query_volume_information_complete(
        &mut self,
        r: &ClientDriveQueryVolumeInformationResponse,
    ) -> PduResult<()> {
        self.link
            .finish(&r.device_io_reply, Body::Volume(r.buffer.clone()));
        Ok(())
    }

    fn on_lock_control_complete(&mut self, r: &ClientDriveLockControlResponse) -> PduResult<()> {
        self.link.finish(&r.device_io_reply, Body::None);
        Ok(())
    }

    fn on_query_security_complete(
        &mut self,
        r: &ClientDriveQuerySecurityResponse,
    ) -> PduResult<()> {
        self.link.finish(&r.device_io_response, Body::None);
        Ok(())
    }

    fn on_set_security_complete(&mut self, r: &ClientDriveSetSecurityResponse) -> PduResult<()> {
        self.link.finish(&r.device_io_response, Body::None);
        Ok(())
    }
}

type HostSlot = Arc<Mutex<Option<Arc<dyn DriveHost>>>>;
type PrinterSlot = Arc<Mutex<Option<Arc<dyn PrinterHost>>>>;

/// Creates the backend of each connection; the host is the one of the current session.
pub struct DriveFactory {
    pub shared: Arc<Shared>,
    pub host: HostSlot,
    pub printers: PrinterSlot,
}

impl ServerEventSender for DriveFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.shared.sender.lock().expect("sender mutex") = Some(sender);
    }
}

impl RdpdrServerFactory for DriveFactory {
    fn build_backend(&self) -> Box<dyn RdpdrServerBackend> {
        let shared = self.shared.clone();
        let send: EventSender = Arc::new(move |event| {
            shared
                .sender
                .lock()
                .expect("sender mutex")
                .as_ref()
                .is_some_and(|s| s.send(event).is_ok())
        });
        Box::new(DriveBackend::new(
            send,
            self.host.lock().expect("host mutex").clone(),
            self.printers.lock().expect("printers mutex").clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::announced_name;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain([0])
            .flat_map(|u| u.to_le_bytes())
            .collect()
    }

    #[test]
    fn names_are_read_as_utf16_or_plain_bytes() {
        assert_eq!(announced_name(&utf16("Documents é")), "Documents é");
        assert_eq!(announced_name(&utf16("C")), "C");
        assert_eq!(announced_name(b"Partage\0"), "Partage");
        assert_eq!(announced_name(b"Partage"), "Partage");
        assert_eq!(announced_name(b""), "");
        assert_eq!(announced_name(&[0, 0]), "");
    }
}
