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
use oxydesk_core::drive::{DriveEntry, DriveHandle, DriveHost, DriveSpace, DriveStat, OpenMode};
use oxydesk_core::error::DriveError;
use oxydesk_core::printer::{PrintFormat, PrinterHost, PrinterInfo, PrinterModes, RemotePrinter};
use oxydesk_core::RemoteDrive;
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

    /// Sends a message that has no answer.
    fn notify(&self, message: RdpdrServerMessage) {
        let _ = (self.send)(ServerEvent::Rdpdr(None, message));
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
    Added(Arc<dyn DriveHost>, String, Arc<dyn RemoteDrive>),
    Removed(Arc<dyn DriveHost>, String),
    PrinterAdded(
        Arc<dyn PrinterHost>,
        String,
        PrinterInfo,
        Arc<dyn RemotePrinter>,
    ),
    PrinterRemoved(Arc<dyn PrinterHost>, String),
}

/// Where the host of the session is found when the client announces a device: in broker mode the session (and its
/// host) only exists once the login is accepted, which can be after this backend was built.
type Source<T> = Arc<dyn Fn() -> Option<Arc<T>> + Send + Sync>;

/// Handles the announcements of the client and the completions of the requests.
pub struct DriveBackend {
    link: Arc<Link>,
    host: Source<dyn DriveHost>,
    printer_host: Source<dyn PrinterHost>,
    /// Drive directory names (and the host that has them), by device id.
    drives: HashMap<u32, (String, Arc<dyn DriveHost>)>,
    /// Printer names (and the host that has them), by device id.
    printers: HashMap<u32, (String, Arc<dyn PrinterHost>)>,
    modes: PrinterModes,
    commands: Sender<Command>,
}

impl std::fmt::Debug for DriveBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveBackend")
            .field(
                "drives",
                &self.drives.values().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .field(
                "printers",
                &self.printers.values().map(|(n, _)| n).collect::<Vec<_>>(),
            )
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
        Self::with_sources(
            send,
            Arc::new(move || host.clone()),
            Arc::new(move || printer_host.clone()),
        )
    }

    /// Like `new`, the hosts being looked up when a device is announced.
    pub fn with_sources(
        send: EventSender,
        host: Source<dyn DriveHost>,
        printer_host: Source<dyn PrinterHost>,
    ) -> Self {
        let (commands, rx) = mpsc::channel::<Command>();
        // Mounting can take a moment and must not stall the connection: a thread, in order.
        std::thread::Builder::new()
            .name("drive-host".into())
            .spawn(move || {
                for command in rx {
                    match command {
                        Command::Added(host, name, drive) => host.added(&name, drive),
                        Command::Removed(host, name) => host.removed(&name),
                        Command::PrinterAdded(host, name, info, printer) => {
                            host.added(&name, &info, printer)
                        }
                        Command::PrinterRemoved(host, name) => host.removed(&name),
                    }
                }
            })
            .ok();
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
            modes: PrinterModes::default(),
            commands,
        }
    }

    /// The mode of each printer (`auto` for all by default).
    #[must_use]
    pub fn with_modes(mut self, modes: PrinterModes) -> Self {
        self.modes = modes;
        self
    }

    fn unique_printer_name(&self, wanted: &str) -> String {
        let base = oxydesk_core::printer_name(wanted);
        let mut name = base.clone();
        let mut n = 2;
        while self.printers.values().any(|(p, _)| *p == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        name
    }

    fn announce_printer(&mut self, id: u32, device: &DeviceAnnounceHeader) -> bool {
        let Some(host) = (self.printer_host)() else {
            return false;
        };
        let Some(announce) = announced_printer(device) else {
            tracing::warn!(device = id, "printer announcement not understood");
            return false;
        };
        // The format is decided here, once: no fallback to another format after a job is sent.
        let info = match announce.info(self.modes.mode_for(&announce.name)) {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(device = id, name = %announce.name, driver = %announce.driver, xps = announce.xps, "printer not redirected: {e}");
                return false;
            }
        };
        let name = self.unique_printer_name(&info.name);
        tracing::info!(device = id, %name, driver = %info.driver, xps = announce.xps, format = ?info.format, "printer redirected by the client");
        let printer = Arc::new(RdpPrinter {
            link: self.link.clone(),
            device_id: id,
        }) as Arc<dyn RemotePrinter>;
        // Before any write: from now on the client takes the jobs of this printer as XPS.
        if info.format == PrintFormat::Xps {
            self.link
                .notify(RdpdrServerMessage::UsingXps { device_id: id });
        }
        let _ = self.commands.send(Command::PrinterAdded(
            host.clone(),
            name.clone(),
            info,
            printer,
        ));
        self.printers.insert(id, (name, host));
        true
    }

    fn unique_name(&self, wanted: &str) -> String {
        let base = oxydesk_core::drive::directory_name(wanted);
        let mut name = base.clone();
        let mut n = 2;
        while self.drives.values().any(|(d, _)| *d == name) {
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
        for (name, host) in self.drives.values() {
            let _ = self
                .commands
                .send(Command::Removed(host.clone(), name.clone()));
        }
        for (name, host) in self.printers.values() {
            let _ = self
                .commands
                .send(Command::PrinterRemoved(host.clone(), name.clone()));
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
                if device.device_type() != DeviceType::Filesystem {
                    return (id, false);
                }
                let Some(host) = (self.host)() else {
                    return (id, false);
                };
                let name = self.unique_name(&drive_label(device));
                tracing::info!(device = id, %name, "drive redirected by the client");
                let drive = Arc::new(RdpDrive {
                    link: self.link.clone(),
                    device_id: id,
                }) as Arc<dyn RemoteDrive>;
                let _ = self
                    .commands
                    .send(Command::Added(host.clone(), name.clone(), drive));
                self.drives.insert(id, (name, host));
                (id, true)
            })
            .collect()
    }

    fn on_device_remove(&mut self, device_ids: &[u32]) {
        for id in device_ids {
            if let Some((name, host)) = self.drives.remove(id) {
                let _ = self.commands.send(Command::Removed(host, name));
            }
            if let Some((name, host)) = self.printers.remove(id) {
                let _ = self.commands.send(Command::PrinterRemoved(host, name));
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
    pub modes: PrinterModes,
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
        let (host, printers) = (self.host.clone(), self.printers.clone());
        Box::new(
            DriveBackend::with_sources(
                send,
                Arc::new(move || host.lock().expect("host mutex").clone()),
                Arc::new(move || printers.lock().expect("printers mutex").clone()),
            )
            .with_modes(self.modes.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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

    #[test]
    fn filetimes_convert_both_ways_and_clamp_unset_values() {
        assert_eq!(from_filetime(0), 0);
        assert_eq!(from_filetime(-5), 0);
        assert_eq!(from_filetime(116_444_736_000_000_000), 0);
        assert_eq!(from_filetime(116_444_736_010_000_000), 1);
        assert_eq!(to_filetime(0), 116_444_736_000_000_000);
        assert_eq!(from_filetime(to_filetime(1_700_000_000)), 1_700_000_000);
        // Before 1601 cannot be expressed: clamped, never negative.
        assert_eq!(to_filetime(-20_000_000_000), 0);
    }

    #[test]
    fn paths_are_sent_relative_to_the_drive_with_backslashes() {
        assert_eq!(windows_path(""), "\\");
        assert_eq!(windows_path("a/b/c.txt"), "\\a\\b\\c.txt");
    }

    #[test]
    fn client_statuses_map_to_the_errors_of_the_desktop_side() {
        let cases = [
            (NtStatus::NO_SUCH_FILE, DriveError::NotFound),
            (NtStatus::OBJECT_PATH_NOT_FOUND, DriveError::NotFound),
            (NtStatus::OBJECT_NAME_COLLISION, DriveError::Exists),
            (NtStatus::ACCESS_DENIED, DriveError::Denied),
            (NtStatus::MEDIA_WRITE_PROTECTED, DriveError::Denied),
            (NtStatus::NOT_A_DIRECTORY, DriveError::NotDirectory),
            (NtStatus::FILE_IS_A_DIRECTORY, DriveError::IsDirectory),
            (NtStatus::DIRECTORY_NOT_EMPTY, DriveError::NotEmpty),
            (NtStatus::DISK_FULL, DriveError::NoSpace),
            (NtStatus::SHARING_VIOLATION, DriveError::Busy),
            (NtStatus::INVALID_PARAMETER, DriveError::Invalid),
        ];
        for (status, expected) in cases {
            assert_eq!(status_error(status), expected);
        }
        assert!(matches!(
            status_error(NtStatus::NOT_SUPPORTED),
            DriveError::Io(_)
        ));
    }

    // --- a client in memory -------------------------------------------------------------------------------------

    #[derive(Clone)]
    enum Node {
        Dir,
        File { data: Vec<u8>, mtime: i64 },
    }

    #[derive(Default)]
    struct Client {
        nodes: BTreeMap<String, Node>,
        /// Open handles: path, delete on close, directory listing cursor.
        handles: HashMap<u32, (String, bool, usize)>,
        next_handle: u32,
        /// The Windows client answers INVALID_PARAMETER when an open names the wrong kind (or none).
        strict_kinds: bool,
        /// Requests seen, as text.
        log: Vec<String>,
        /// Answer every request with this status.
        refuse_with: Option<NtStatus>,
        /// Do not answer requests of this kind (a client that hangs): the connection is closed instead.
        short_writes: bool,
    }

    fn key(path: &str) -> String {
        path.trim_matches('\\').replace('\\', "/")
    }

    fn parent_of(key: &str) -> &str {
        key.rsplit_once('/').map_or("", |(p, _)| p)
    }

    impl Client {
        fn new() -> Self {
            let mut c = Self::default();
            c.nodes.insert(String::new(), Node::Dir);
            c
        }

        fn with_file(mut self, path: &str, data: &[u8]) -> Self {
            self.nodes.insert(
                path.into(),
                Node::File {
                    data: data.to_vec(),
                    mtime: 1_700_000_000,
                },
            );
            self
        }

        fn with_dir(mut self, path: &str) -> Self {
            self.nodes.insert(path.into(), Node::Dir);
            self
        }

        fn io(device_id: u32, completion_id: u32, io_status: NtStatus) -> DeviceIoResponse {
            DeviceIoResponse {
                device_id,
                completion_id,
                io_status,
            }
        }

        fn basic(attributes: FileAttributes, mtime: i64) -> FileBasicInformation {
            FileBasicInformation {
                creation_time: to_filetime(mtime),
                last_access_time: to_filetime(mtime),
                last_write_time: to_filetime(mtime),
                change_time: 0,
                file_attributes: attributes,
            }
        }

        /// Answers one request through the backend, as the channel would.
        fn answer(
            &mut self,
            backend: &mut DriveBackend,
            completion_id: u32,
            tag: u64,
            message: RdpdrServerMessage,
        ) {
            self.log.push(format!("{message:?}"));
            backend.on_request_sent(completion_id, Some(tag));
            let ok = NtStatus::SUCCESS;
            match message {
                RdpdrServerMessage::Create {
                    device_id,
                    path,
                    create_disposition,
                    create_options,
                    ..
                } => {
                    let k = key(&path);
                    let existing = self.nodes.get(&k).cloned();
                    let wants_dir = create_options.contains(CreateOptions::FILE_DIRECTORY_FILE);
                    let wants_file =
                        create_options.contains(CreateOptions::FILE_NON_DIRECTORY_FILE);
                    let status = if let Some(refusal) = self.refuse_with {
                        refusal
                    } else if !self.nodes.contains_key(parent_of(&k)) && !k.is_empty() {
                        NtStatus::OBJECT_PATH_NOT_FOUND
                    } else if self.strict_kinds && !wants_dir && !wants_file {
                        NtStatus::INVALID_PARAMETER
                    } else if create_disposition == CreateDisposition::FILE_CREATE {
                        if existing.is_some() {
                            NtStatus::OBJECT_NAME_COLLISION
                        } else {
                            self.nodes.insert(
                                k.clone(),
                                if wants_dir {
                                    Node::Dir
                                } else {
                                    Node::File {
                                        data: Vec::new(),
                                        mtime: 0,
                                    }
                                },
                            );
                            ok
                        }
                    } else {
                        match existing {
                            None => NtStatus::NO_SUCH_FILE,
                            Some(Node::Dir) if wants_file => NtStatus::FILE_IS_A_DIRECTORY,
                            Some(Node::File { .. }) if wants_dir => NtStatus::NOT_A_DIRECTORY,
                            Some(Node::File { mtime, .. })
                                if create_disposition == CreateDisposition::FILE_OVERWRITE =>
                            {
                                self.nodes.insert(
                                    k.clone(),
                                    Node::File {
                                        data: Vec::new(),
                                        mtime,
                                    },
                                );
                                ok
                            }
                            Some(_) => ok,
                        }
                    };
                    let file_id = if status == ok {
                        self.next_handle += 1;
                        let delete = create_options.contains(CreateOptions::FILE_DELETE_ON_CLOSE);
                        self.handles.insert(self.next_handle, (k, delete, 0));
                        self.next_handle
                    } else {
                        0
                    };
                    backend
                        .on_create_complete(&DeviceCreateResponse {
                            device_io_reply: Self::io(device_id, completion_id, status),
                            file_id,
                            information: ironrdp_rdpdr::pdu::efs::Information::FILE_OPENED,
                        })
                        .unwrap();
                }
                RdpdrServerMessage::Close { device_id, file_id } => {
                    if let Some((k, delete, _)) = self.handles.remove(&file_id) {
                        if delete {
                            self.nodes.remove(&k);
                        }
                    }
                    backend
                        .on_close_complete(&DeviceCloseResponse {
                            device_io_response: Self::io(device_id, completion_id, ok),
                        })
                        .unwrap();
                }
                RdpdrServerMessage::Read {
                    device_id,
                    file_id,
                    length,
                    offset,
                } => {
                    let (status, data) = match self
                        .handles
                        .get(&file_id)
                        .and_then(|(k, ..)| self.nodes.get(k))
                    {
                        Some(Node::File { data, .. }) if offset >= data.len() as u64 => {
                            (NtStatus::END_OF_FILE, Vec::new())
                        }
                        Some(Node::File { data, .. }) => {
                            let from = offset as usize;
                            let to = (from + length as usize).min(data.len());
                            (ok, data[from..to].to_vec())
                        }
                        _ => (NtStatus::INVALID_HANDLE, Vec::new()),
                    };
                    backend
                        .on_read_complete(&DeviceReadResponse {
                            device_io_reply: Self::io(device_id, completion_id, status),
                            read_data: data,
                        })
                        .unwrap();
                }
                RdpdrServerMessage::Write {
                    device_id,
                    file_id,
                    data,
                    offset,
                } => {
                    let short = self.short_writes;
                    let (status, written) = match self
                        .handles
                        .get(&file_id)
                        .and_then(|(k, ..)| self.nodes.get_mut(k))
                    {
                        Some(Node::File { data: file, .. }) => {
                            let take = if short { data.len() / 2 } else { data.len() };
                            let end = offset as usize + take;
                            if file.len() < end {
                                file.resize(end, 0);
                            }
                            file[offset as usize..end].copy_from_slice(&data[..take]);
                            (ok, take as u32)
                        }
                        _ => (NtStatus::INVALID_HANDLE, 0),
                    };
                    backend
                        .on_write_complete(&DeviceWriteResponse {
                            device_io_reply: Self::io(device_id, completion_id, status),
                            length: written,
                        })
                        .unwrap();
                }
                RdpdrServerMessage::QueryInformation {
                    device_id,
                    file_id,
                    info_class,
                } => {
                    let node = self
                        .handles
                        .get(&file_id)
                        .and_then(|(k, ..)| self.nodes.get(k))
                        .cloned();
                    let (status, buffer) = match (node, info_class) {
                        (None, _) => (NtStatus::INVALID_HANDLE, None),
                        (Some(Node::Dir), FileInformationClassLevel::FILE_BASIC_INFORMATION) => (
                            ok,
                            Some(FileInformationClass::Basic(Self::basic(
                                FileAttributes::FILE_ATTRIBUTE_DIRECTORY,
                                1_700_000_000,
                            ))),
                        ),
                        (
                            Some(Node::File { mtime, .. }),
                            FileInformationClassLevel::FILE_BASIC_INFORMATION,
                        ) => (
                            ok,
                            Some(FileInformationClass::Basic(Self::basic(
                                FileAttributes::FILE_ATTRIBUTE_NORMAL,
                                mtime,
                            ))),
                        ),
                        (Some(node), FileInformationClassLevel::FILE_STANDARD_INFORMATION) => {
                            let (size, dir) = match node {
                                Node::Dir => (0, Boolean::True),
                                Node::File { data, .. } => (data.len() as i64, Boolean::False),
                            };
                            (
                                ok,
                                Some(FileInformationClass::Standard(
                                    ironrdp_rdpdr::pdu::efs::FileStandardInformation {
                                        allocation_size: size,
                                        end_of_file: size,
                                        number_of_links: 1,
                                        delete_pending: Boolean::False,
                                        directory: dir,
                                    },
                                )),
                            )
                        }
                        _ => (NtStatus::NOT_SUPPORTED, None),
                    };
                    backend
                        .on_query_information_complete(&ClientDriveQueryInformationResponse {
                            device_io_response: Self::io(device_id, completion_id, status),
                            buffer,
                        })
                        .unwrap();
                }
                RdpdrServerMessage::SetInformation {
                    device_id,
                    file_id,
                    set_buffer,
                } => {
                    let status = self.set_information(file_id, set_buffer);
                    // The response type cannot be built outside its crate: what the backend does with it is one call.
                    backend
                        .link
                        .finish(&Self::io(device_id, completion_id, status), Body::None);
                }
                RdpdrServerMessage::QueryDirectory {
                    device_id,
                    file_id,
                    path,
                    initial_query,
                    ..
                } => {
                    let (status, buffer) = self.next_entry(file_id, &path, initial_query);
                    backend
                        .on_query_directory_complete(&ClientDriveQueryDirectoryResponse {
                            device_io_reply: Self::io(device_id, completion_id, status),
                            buffer,
                        })
                        .unwrap();
                }
                RdpdrServerMessage::QueryVolumeInformation { device_id, .. } => {
                    backend
                        .on_query_volume_information_complete(
                            &ClientDriveQueryVolumeInformationResponse {
                                device_io_reply: Self::io(device_id, completion_id, ok),
                                buffer: Some(FileSystemInformationClass::FileFsSizeInformation(
                                    ironrdp_rdpdr::pdu::efs::FileFsSizeInformation {
                                        total_alloc_units: 1000,
                                        available_alloc_units: 250,
                                        sectors_per_alloc_unit: 8,
                                        bytes_per_sector: 512,
                                    },
                                )),
                            },
                        )
                        .unwrap();
                }
                other => panic!("request not handled by the test client: {other:?}"),
            }
        }

        fn set_information(&mut self, file_id: u32, info: FileInformationClass) -> NtStatus {
            let Some((k, ..)) = self.handles.get(&file_id).cloned() else {
                return NtStatus::INVALID_HANDLE;
            };
            match info {
                FileInformationClass::Disposition(d) => {
                    if d.delete_pending != 0 {
                        self.handles.get_mut(&file_id).unwrap().1 = true;
                    }
                    NtStatus::SUCCESS
                }
                FileInformationClass::Rename(r) => {
                    let to = key(&r.file_name);
                    match self.nodes.remove(&k) {
                        Some(node) => {
                            self.nodes.insert(to, node);
                            NtStatus::SUCCESS
                        }
                        None => NtStatus::NO_SUCH_FILE,
                    }
                }
                FileInformationClass::EndOfFile(e) => match self.nodes.get_mut(&k) {
                    Some(Node::File { data, .. }) => {
                        data.resize(e.end_of_file as usize, 0);
                        NtStatus::SUCCESS
                    }
                    _ => NtStatus::INVALID_PARAMETER,
                },
                FileInformationClass::Basic(b) => match self.nodes.get_mut(&k) {
                    Some(Node::File { mtime, .. }) => {
                        *mtime = from_filetime(b.last_write_time);
                        NtStatus::SUCCESS
                    }
                    _ => NtStatus::SUCCESS,
                },
                _ => NtStatus::NOT_SUPPORTED,
            }
        }

        /// The next entry of a listing. FreeRDP wants the pattern to name the directory, the Windows client only
        /// matches `\*` (`strict_kinds` stands for the Windows behaviour here).
        fn next_entry(
            &mut self,
            file_id: u32,
            pattern: &str,
            initial: bool,
        ) -> (NtStatus, Option<FileInformationClass>) {
            let Some((dir, _, cursor)) = self.handles.get(&file_id).cloned() else {
                return (NtStatus::INVALID_HANDLE, None);
            };
            let windows_pattern = pattern == "\\*";
            if windows_pattern != self.strict_kinds && !dir.is_empty() {
                return (NtStatus::NO_MORE_FILES, None);
            }
            let cursor = if initial { 0 } else { cursor };
            let names: Vec<(String, Node)> = std::iter::once((".".to_string(), Node::Dir))
                .chain(
                    self.nodes
                        .iter()
                        .filter(|(k, _)| !k.is_empty() && parent_of(k) == dir)
                        .map(|(k, n)| (k.rsplit('/').next().unwrap().to_string(), n.clone())),
                )
                .collect();
            let Some((name, node)) = names.get(cursor).cloned() else {
                return (NtStatus::NO_MORE_FILES, None);
            };
            self.handles.get_mut(&file_id).unwrap().2 = cursor + 1;
            let (attributes, size, mtime) = match node {
                Node::Dir => (FileAttributes::FILE_ATTRIBUTE_DIRECTORY, 0, 1_700_000_000),
                Node::File { data, mtime } => {
                    (FileAttributes::FILE_ATTRIBUTE_NORMAL, data.len(), mtime)
                }
            };
            (
                NtStatus::SUCCESS,
                Some(FileInformationClass::BothDirectory(
                    ironrdp_rdpdr::pdu::efs::FileBothDirectoryInformation {
                        next_entry_offset: 0,
                        file_index: 0,
                        creation_time: to_filetime(mtime),
                        last_access_time: to_filetime(mtime),
                        last_write_time: to_filetime(mtime),
                        change_time: 0,
                        end_of_file: size as i64,
                        allocation_size: size as i64,
                        file_attributes: attributes,
                        ea_size: 0,
                        short_name_length: 0,
                        short_name: [0; 24],
                        file_name: name,
                    },
                )),
            )
        }
    }

    /// A drive wired to a client in memory: the requests of the desktop side are answered by a thread that feeds
    /// the completions to the backend, exactly as the channel does.
    struct Rig {
        drive: RdpDrive,
        client: Arc<Mutex<Client>>,
        _backend: Arc<Mutex<DriveBackend>>,
    }

    fn rig(client: Client) -> Rig {
        let client = Arc::new(Mutex::new(client));
        let (events, requests) = mpsc::channel::<(u64, RdpdrServerMessage)>();
        let events = Mutex::new(events);
        let send: EventSender = Arc::new(move |event| match event {
            ServerEvent::Rdpdr(Some(tag), message) => {
                events.lock().unwrap().send((tag, message)).is_ok()
            }
            _ => true,
        });
        let backend = Arc::new(Mutex::new(DriveBackend::new(send, None, None)));
        let link = backend.lock().unwrap().link.clone();
        {
            let (backend, client) = (backend.clone(), client.clone());
            std::thread::spawn(move || {
                let mut completion_id = 100;
                for (tag, message) in requests {
                    completion_id += 1;
                    client.lock().unwrap().answer(
                        &mut backend.lock().unwrap(),
                        completion_id,
                        tag,
                        message,
                    );
                }
            });
        }
        Rig {
            drive: RdpDrive { link, device_id: 7 },
            client,
            _backend: backend,
        }
    }

    fn file_data(rig: &Rig, path: &str) -> Option<Vec<u8>> {
        match rig.client.lock().unwrap().nodes.get(path) {
            Some(Node::File { data, .. }) => Some(data.clone()),
            _ => None,
        }
    }

    // --- stat and listing ---------------------------------------------------------------------------------------

    #[test]
    fn stat_reports_kind_size_and_dates_of_files_and_directories() {
        let r = rig(Client::new().with_file("a.txt", b"hello").with_dir("d"));
        let file = r.drive.stat("a.txt").unwrap();
        assert_eq!(
            (file.size, file.is_dir, file.modified),
            (5, false, 1_700_000_000)
        );
        let dir = r.drive.stat("d").unwrap();
        assert!(dir.is_dir);
        assert!(r.drive.stat("").unwrap().is_dir);
        assert_eq!(r.drive.stat("missing"), Err(DriveError::NotFound));
    }

    #[test]
    fn stat_still_works_with_a_client_that_wants_the_kind_of_the_file_stated() {
        let mut client = Client::new().with_file("a.txt", b"xyz");
        client.strict_kinds = true;
        let r = rig(client);
        // The first open states no kind (refused as INVALID_PARAMETER); the second states "not a directory".
        assert_eq!(r.drive.stat("a.txt").unwrap().size, 3);
    }

    #[test]
    fn a_listing_skips_dot_entries_and_returns_every_file() {
        let r = rig(Client::new()
            .with_file("one", b"1")
            .with_file("two", b"22")
            .with_dir("sub")
            .with_file("sub/inner", b"x"));
        let mut names: Vec<_> = r
            .drive
            .list("")
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.stat.is_dir))
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                ("one".into(), false),
                ("sub".into(), true),
                ("two".into(), false)
            ]
        );
        let inner = r.drive.list("sub").unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].name, "inner");
    }

    #[test]
    fn a_listing_works_with_either_style_of_pattern() {
        // FreeRDP wants the directory named in the pattern, Windows only matches `\*` inside the handle.
        for strict in [false, true] {
            let mut client = Client::new().with_dir("sub").with_file("sub/f", b"1");
            client.strict_kinds = strict;
            let r = rig(client);
            assert_eq!(r.drive.list("sub").unwrap().len(), 1, "strict={strict}");
        }
    }

    #[test]
    fn listing_a_file_or_a_missing_directory_fails_cleanly() {
        let r = rig(Client::new().with_file("f", b"1"));
        assert_eq!(r.drive.list("f"), Err(DriveError::NotDirectory));
        assert!(r.drive.list("nope").is_err());
    }

    // --- content ------------------------------------------------------------------------------------------------

    #[test]
    fn read_returns_the_requested_range_and_stops_at_the_end() {
        let r = rig(Client::new().with_file("f", b"0123456789"));
        let h = r.drive.open("f", OpenMode::Read).unwrap();
        assert_eq!(r.drive.read(h, 2, 4).unwrap(), b"2345");
        assert_eq!(r.drive.read(h, 8, 100).unwrap(), b"89");
        assert_eq!(r.drive.read(h, 10, 5).unwrap(), b"");
        assert_eq!(r.drive.read(h, 0, 0).unwrap(), b"");
        r.drive.close(h);
        assert!(r.client.lock().unwrap().handles.is_empty());
    }

    #[test]
    fn a_read_larger_than_one_block_is_split_and_reassembled() {
        let data: Vec<u8> = (0..(IRP_BLOCK as usize * 2 + 1234))
            .map(|i| (i % 251) as u8)
            .collect();
        let r = rig(Client::new().with_file("big", &data));
        let h = r.drive.open("big", OpenMode::Read).unwrap();
        assert_eq!(r.drive.read(h, 0, data.len() as u32).unwrap(), data);
        let reads = r
            .client
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|l| l.starts_with("Read"))
            .count();
        assert_eq!(reads, 3);
    }

    #[test]
    fn write_creates_extends_and_splits_large_blocks() {
        let r = rig(Client::new());
        let h = r.drive.open("new.bin", OpenMode::Create).unwrap();
        let data: Vec<u8> = (0..(IRP_BLOCK as usize + 10)).map(|i| i as u8).collect();
        assert_eq!(r.drive.write(h, 0, &data).unwrap(), data.len() as u32);
        assert_eq!(r.drive.write(h, data.len() as u64, b"tail").unwrap(), 4);
        r.drive.close(h);
        let mut expected = data;
        expected.extend_from_slice(b"tail");
        assert_eq!(file_data(&r, "new.bin").unwrap(), expected);
    }

    #[test]
    fn a_short_write_is_reported_instead_of_hidden() {
        let mut client = Client::new();
        client.short_writes = true;
        let r = rig(client);
        let h = r.drive.open("f", OpenMode::Create).unwrap();
        assert_eq!(r.drive.write(h, 0, b"abcdefgh").unwrap(), 4);
    }

    #[test]
    fn open_modes_follow_the_semantics_of_the_kernel() {
        let r = rig(Client::new().with_file("f", b"content"));
        assert_eq!(r.drive.open("f", OpenMode::Create), Err(DriveError::Exists));
        assert_eq!(
            r.drive.open("missing", OpenMode::Read),
            Err(DriveError::NotFound)
        );
        assert_eq!(
            r.drive.open("missing", OpenMode::ReadWrite),
            Err(DriveError::NotFound)
        );
        let h = r.drive.open("f", OpenMode::Truncate).unwrap();
        r.drive.close(h);
        assert_eq!(file_data(&r, "f").unwrap(), b"");
        // An open of a directory as a file is refused by the client.
        let r = rig(Client::new().with_dir("d"));
        assert_eq!(
            r.drive.open("d", OpenMode::Read),
            Err(DriveError::IsDirectory)
        );
    }

    #[test]
    fn an_invalid_parameter_answer_to_a_plain_open_means_not_found() {
        let mut client = Client::new();
        client.refuse_with = Some(NtStatus::INVALID_PARAMETER);
        let r = rig(client);
        assert_eq!(
            r.drive.open("whatever", OpenMode::Read),
            Err(DriveError::NotFound)
        );
        // …but not for a creation, where it is a genuine refusal.
        assert_eq!(
            r.drive.open("whatever", OpenMode::Create),
            Err(DriveError::Invalid)
        );
    }

    #[test]
    fn flush_needs_no_request() {
        let r = rig(Client::new().with_file("f", b"1"));
        let h = r.drive.open("f", OpenMode::ReadWrite).unwrap();
        r.drive.flush(h).unwrap();
        assert!(!r
            .client
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|l| l.starts_with("FlushBuffers")));
    }

    // --- namespace changes --------------------------------------------------------------------------------------

    #[test]
    fn mkdir_creates_once_and_refuses_an_existing_name() {
        let r = rig(Client::new());
        r.drive.mkdir("d").unwrap();
        assert!(matches!(
            r.client.lock().unwrap().nodes.get("d"),
            Some(Node::Dir)
        ));
        assert_eq!(r.drive.mkdir("d"), Err(DriveError::Exists));
        assert!(r.drive.mkdir("missing/child").is_err());
    }

    #[test]
    fn remove_deletes_files_and_empty_directories_only() {
        let r = rig(Client::new()
            .with_file("f", b"1")
            .with_dir("empty")
            .with_dir("full")
            .with_file("full/x", b"1"));
        r.drive.remove("f", false).unwrap();
        r.drive.remove("empty", true).unwrap();
        assert_eq!(r.drive.remove("full", true), Err(DriveError::NotEmpty));
        assert_eq!(r.drive.remove("gone", false), Err(DriveError::NotFound));
        let c = r.client.lock().unwrap();
        assert!(!c.nodes.contains_key("f") && !c.nodes.contains_key("empty"));
        assert!(c.nodes.contains_key("full") && c.nodes.contains_key("full/x"));
        assert!(c.handles.is_empty(), "no handle left open");
    }

    #[test]
    fn rename_moves_a_file_and_closes_its_handle() {
        let r = rig(Client::new().with_file("a", b"data").with_dir("d"));
        r.drive.rename("a", "d/b").unwrap();
        assert_eq!(file_data(&r, "d/b").unwrap(), b"data");
        assert!(file_data(&r, "a").is_none());
        assert_eq!(r.drive.rename("nope", "x"), Err(DriveError::NotFound));
        assert!(r.client.lock().unwrap().handles.is_empty());
    }

    #[test]
    fn truncate_and_set_modified_change_the_file_in_place() {
        let r = rig(Client::new().with_file("f", b"0123456789"));
        r.drive.truncate("f", 4).unwrap();
        assert_eq!(file_data(&r, "f").unwrap(), b"0123");
        r.drive.truncate("f", 6).unwrap();
        assert_eq!(file_data(&r, "f").unwrap(), b"0123\0\0");
        r.drive.set_modified("f", 1_600_000_000).unwrap();
        assert_eq!(r.drive.stat("f").unwrap().modified, 1_600_000_000);
        assert_eq!(r.drive.truncate("nope", 1), Err(DriveError::NotFound));
    }

    #[test]
    fn space_multiplies_the_units_of_the_volume() {
        let r = rig(Client::new());
        let space = r.drive.space().unwrap();
        assert_eq!(space.total, 1000 * 8 * 512);
        assert_eq!(space.available, 250 * 8 * 512);
    }

    // --- the link -----------------------------------------------------------------------------------------------

    #[test]
    fn a_refusal_of_the_client_reaches_the_caller_as_a_typed_error() {
        let mut client = Client::new();
        client.refuse_with = Some(NtStatus::ACCESS_DENIED);
        let r = rig(client);
        assert_eq!(r.drive.mkdir("x"), Err(DriveError::Denied));
        assert_eq!(r.drive.open("x", OpenMode::Read), Err(DriveError::Denied));
    }

    #[test]
    fn calls_fail_at_once_when_the_connection_is_gone() {
        let r = rig(Client::new().with_file("f", b"1"));
        r.drive.link.close();
        assert!(matches!(r.drive.stat("f"), Err(DriveError::Io(m)) if m.contains("closed")));
        // A sender that reports the connection gone fails the call too, and leaves no waiter behind.
        let link = Link {
            send: Arc::new(|_| false),
            next_tag: AtomicU64::new(1),
            waiters: Mutex::default(),
        };
        let result = link.call(RdpdrServerMessage::Close {
            device_id: 1,
            file_id: 1,
        });
        assert!(matches!(result, Err(DriveError::Io(_))));
        assert!(link.waiters.lock().unwrap().by_tag.is_empty());
    }

    #[test]
    fn dropping_the_backend_fails_the_requests_still_waiting() {
        let (tx, rx) = mpsc::channel();
        let send: EventSender = Arc::new(move |event| tx.send(event).is_ok());
        let backend = DriveBackend::new(send, None, None);
        let link = backend.link.clone();
        let waiting = std::thread::spawn({
            let link = link.clone();
            move || {
                link.call(RdpdrServerMessage::Close {
                    device_id: 1,
                    file_id: 1,
                })
            }
        });
        // The request is out; nobody will answer it.
        let _ = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("request sent");
        drop(backend);
        let result = waiting.join().unwrap();
        assert!(matches!(result, Err(DriveError::Io(m)) if m.contains("closed")));
        assert!(link.waiters.lock().unwrap().closed);
    }

    #[test]
    fn an_answer_for_an_unknown_request_is_ignored() {
        let send: EventSender = Arc::new(|_| true);
        let mut backend = DriveBackend::new(send, None, None);
        backend
            .on_close_complete(&DeviceCloseResponse {
                device_io_response: DeviceIoResponse {
                    device_id: 1,
                    completion_id: 999,
                    io_status: NtStatus::SUCCESS,
                },
            })
            .unwrap();
        backend.on_request_sent(5, Some(12345)); // tag nobody waits for
        assert!(backend
            .link
            .waiters
            .lock()
            .unwrap()
            .by_completion
            .is_empty());
    }

    #[test]
    fn devices_that_are_not_drives_or_printers_are_declined() {
        let send: EventSender = Arc::new(|_| true);
        let mut backend = DriveBackend::new(send, None, None);
        let answers = backend.on_device_announce(&[DeviceAnnounceHeader::new_smartcard(3)]);
        assert_eq!(answers, vec![(3, false)]);
        // No printer host: the printer is declined too.
        let answers =
            backend.on_device_announce(&[DeviceAnnounceHeader::new_printer(4, "p".into())]);
        assert_eq!(answers, vec![(4, false)]);
        backend.on_device_remove(&[3, 4, 99]);
    }
}
