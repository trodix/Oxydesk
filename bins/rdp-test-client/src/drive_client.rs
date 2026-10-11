//! Client side of RDPDR over a directory of the machine: the "disk" a test client redirects.
//! A small file system (open, read, write, directories, rename, delete, volume information) with the
//! semantics of Windows redirection clients; also what the server's loopback test talks to.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use ironrdp_core::impl_as_any;
use ironrdp_pdu::{pdu_other_err, PduResult};
use ironrdp_rdpdr::pdu::efs::{
    Boolean, Characteristics, ClientDriveLockControlResponse, ClientDriveQueryDirectoryResponse,
    ClientDriveQueryInformationResponse, ClientDriveQueryVolumeInformationResponse,
    ClientDriveSetInformationResponse, CreateDisposition, CreateOptions, DesiredAccess,
    DeviceCloseResponse, DeviceControlRequest, DeviceControlResponse, DeviceCreateRequest,
    DeviceCreateResponse, DeviceFlushBuffersResponse, DeviceIoRequest, DeviceIoResponse,
    DeviceReadResponse, DeviceWriteResponse, FileAttributeTagInformation, FileAttributes,
    FileBasicInformation, FileBothDirectoryInformation, FileFsAttributeInformation,
    FileFsDeviceInformation, FileFsFullSizeInformation, FileFsSizeInformation,
    FileFsVolumeInformation, FileInformationClass, FileInformationClassLevel,
    FileStandardInformation, FileSystemAttributes, FileSystemInformationClass,
    FileSystemInformationClassLevel, Information, NtStatus, ServerDeviceAnnounceResponse,
    ServerDriveIoRequest,
};
use ironrdp_rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
use ironrdp_rdpdr::pdu::RdpdrPdu;
use ironrdp_rdpdr::Rdpdr;
use ironrdp_svc::SvcMessage;

const FILETIME_EPOCH_DELTA: i64 = 11_644_473_600;

fn filetime(t: std::io::Result<std::time::SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| (d.as_secs() as i64 + FILETIME_EPOCH_DELTA) * 10_000_000)
        .unwrap_or(0)
}

fn from_filetime(ft: i64) -> Option<std::time::SystemTime> {
    let secs = ft / 10_000_000 - FILETIME_EPOCH_DELTA;
    u64::try_from(secs)
        .ok()
        .map(|s| UNIX_EPOCH + std::time::Duration::from_secs(s))
}

fn status_of(e: &std::io::Error) -> NtStatus {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => NtStatus::NO_SUCH_FILE,
        PermissionDenied => NtStatus::ACCESS_DENIED,
        AlreadyExists => NtStatus::OBJECT_NAME_COLLISION,
        DirectoryNotEmpty => NtStatus::DIRECTORY_NOT_EMPTY,
        NotADirectory => NtStatus::NOT_A_DIRECTORY,
        IsADirectory => NtStatus::FILE_IS_A_DIRECTORY,
        StorageFull => NtStatus::DISK_FULL,
        _ => NtStatus::UNSUCCESSFUL,
    }
}

struct Open {
    path: PathBuf,
    file: Option<File>,
    is_dir: bool,
    delete_on_close: bool,
    /// Entries left to return to the directory queries.
    listing: Option<std::vec::IntoIter<(String, PathBuf)>>,
}

/// A directory redirected as a drive.
#[derive(Debug)]
pub struct DirectoryDrive {
    root: PathBuf,
    open: HashMap<u32, Open>,
    next_id: u32,
}

impl std::fmt::Debug for Open {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Open")
    }
}

impl_as_any!(DirectoryDrive);

impl DirectoryDrive {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            open: HashMap::new(),
            next_id: 1,
        }
    }

    /// The channel announcing this directory as drive `device_id` named `name`.
    pub fn channel(root: PathBuf, device_id: u32, name: &str, computer: &str) -> Rdpdr {
        Rdpdr::new(Box::new(Self::new(root)), computer.to_owned())
            .with_drives(Some(vec![(device_id, name.to_owned())]))
    }

    /// Maps a request path (`\dir\file`) under the root; nothing can leave it.
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let mut out = self.root.clone();
        for part in path.split('\\').filter(|p| !p.is_empty()) {
            let part = Path::new(part);
            match part.components().next() {
                Some(Component::Normal(c)) if part.components().count() == 1 => out.push(c),
                _ => return None,
            }
        }
        Some(out)
    }

    fn attributes(meta: &std::fs::Metadata) -> FileAttributes {
        let mut a = if meta.is_dir() {
            FileAttributes::FILE_ATTRIBUTE_DIRECTORY
        } else {
            FileAttributes::FILE_ATTRIBUTE_ARCHIVE
        };
        if meta.permissions().readonly() {
            a |= FileAttributes::FILE_ATTRIBUTE_READONLY;
        }
        a
    }

    fn create(&mut self, req: &DeviceCreateRequest) -> (NtStatus, u32, Information) {
        let Some(path) = self.resolve(&req.path) else {
            return (NtStatus::OBJECT_NAME_INVALID, 0, Information::file_opened());
        };
        let meta = std::fs::metadata(&path).ok();
        let wants_dir = req
            .create_options
            .contains(CreateOptions::FILE_DIRECTORY_FILE);
        let wants_file = req
            .create_options
            .contains(CreateOptions::FILE_NON_DIRECTORY_FILE);
        let d = req.create_disposition;
        let exists = meta.is_some();
        let creates = d == CreateDisposition::FILE_CREATE
            || d == CreateDisposition::FILE_OPEN_IF
            || d == CreateDisposition::FILE_OVERWRITE_IF
            || d == CreateDisposition::FILE_SUPERSEDE;
        if exists && d == CreateDisposition::FILE_CREATE {
            return (
                NtStatus::OBJECT_NAME_COLLISION,
                0,
                Information::file_opened(),
            );
        }
        if !exists && !creates {
            let parent_ok = path.parent().is_some_and(Path::is_dir);
            let status = if parent_ok {
                NtStatus::NO_SUCH_FILE
            } else {
                NtStatus::OBJECT_PATH_NOT_FOUND
            };
            return (status, 0, Information::file_opened());
        }
        if let Some(m) = &meta {
            if m.is_dir() && wants_file {
                return (NtStatus::FILE_IS_A_DIRECTORY, 0, Information::file_opened());
            }
            if m.is_file() && wants_dir {
                return (NtStatus::NOT_A_DIRECTORY, 0, Information::file_opened());
            }
        }
        let mut information = Information::file_opened();
        let is_dir = meta.as_ref().map_or(wants_dir, |m| m.is_dir());
        let mut file = None;
        if is_dir {
            if !exists {
                if let Err(e) = std::fs::create_dir(&path) {
                    return (status_of(&e), 0, information);
                }
                information = Information::file_created();
            }
        } else {
            let write = req.desired_access.intersects(
                DesiredAccess::FILE_WRITE_DATA_OR_FILE_ADD_FILE
                    | DesiredAccess::FILE_APPEND_DATA_OR_FILE_ADD_SUBDIRECTORY
                    | DesiredAccess::GENERIC_WRITE
                    | DesiredAccess::GENERIC_ALL,
            );
            let truncate = exists
                && (d == CreateDisposition::FILE_OVERWRITE
                    || d == CreateDisposition::FILE_OVERWRITE_IF
                    || d == CreateDisposition::FILE_SUPERSEDE);
            let mut options = OpenOptions::new();
            options.read(true).write(write || !exists || truncate);
            if !exists {
                options.create_new(true);
                information = Information::file_created();
            } else if truncate {
                options.truncate(true);
                information = Information::file_overwritten();
            }
            match options.open(&path) {
                Ok(f) => file = Some(f),
                Err(e) => return (status_of(&e), 0, information),
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.open.insert(
            id,
            Open {
                path,
                file,
                is_dir,
                delete_on_close: req
                    .create_options
                    .contains(CreateOptions::FILE_DELETE_ON_CLOSE),
                listing: None,
            },
        );
        (NtStatus::SUCCESS, id, information)
    }

    fn list(path: &Path) -> Vec<(String, PathBuf)> {
        let mut entries = vec![
            (".".to_owned(), path.to_path_buf()),
            ("..".to_owned(), path.to_path_buf()),
        ];
        if let Ok(rd) = std::fs::read_dir(path) {
            let mut names: Vec<_> = rd.flatten().collect();
            names.sort_by_key(|e| e.file_name());
            for e in names {
                entries.push((e.file_name().to_string_lossy().into_owned(), e.path()));
            }
        }
        entries
    }

    fn query_information(
        &mut self,
        id: u32,
        class: FileInformationClassLevel,
    ) -> (NtStatus, Option<FileInformationClass>) {
        let Some(open) = self.open.get(&id) else {
            return (NtStatus::INVALID_HANDLE, None);
        };
        let meta = match std::fs::metadata(&open.path) {
            Ok(m) => m,
            Err(e) => return (status_of(&e), None),
        };
        let size = if meta.is_dir() { 0 } else { meta.len() as i64 };
        let info: FileInformationClass = match class {
            FileInformationClassLevel::FILE_BASIC_INFORMATION => FileBasicInformation {
                creation_time: filetime(meta.created()),
                last_access_time: filetime(meta.accessed()),
                last_write_time: filetime(meta.modified()),
                change_time: filetime(meta.modified()),
                file_attributes: Self::attributes(&meta),
            }
            .into(),
            FileInformationClassLevel::FILE_STANDARD_INFORMATION => FileStandardInformation {
                allocation_size: size,
                end_of_file: size,
                number_of_links: 1,
                delete_pending: if open.delete_on_close {
                    Boolean::True
                } else {
                    Boolean::False
                },
                directory: if meta.is_dir() {
                    Boolean::True
                } else {
                    Boolean::False
                },
            }
            .into(),
            FileInformationClassLevel::FILE_ATTRIBUTE_TAG_INFORMATION => {
                FileAttributeTagInformation {
                    file_attributes: Self::attributes(&meta),
                    reparse_tag: 0,
                }
                .into()
            }
            _ => return (NtStatus::NOT_SUPPORTED, None),
        };
        (NtStatus::SUCCESS, Some(info))
    }

    fn set_information(&mut self, id: u32, info: FileInformationClass) -> NtStatus {
        let Some(open) = self.open.get_mut(&id) else {
            return NtStatus::INVALID_HANDLE;
        };
        match info {
            FileInformationClass::EndOfFile(e) => match &open.file {
                Some(f) => f
                    .set_len(e.end_of_file.max(0) as u64)
                    .map_or_else(|e| status_of(&e), |_| NtStatus::SUCCESS),
                None => NtStatus::INVALID_PARAMETER,
            },
            FileInformationClass::Allocation(_) => NtStatus::SUCCESS,
            FileInformationClass::Disposition(d) => {
                open.delete_on_close = d.delete_pending != 0;
                NtStatus::SUCCESS
            }
            FileInformationClass::Basic(b) => {
                if let (Some(t), Some(f)) = (from_filetime(b.last_write_time), &open.file) {
                    if b.last_write_time > 0 {
                        let _ = f.set_modified(t);
                    }
                } else if b.last_write_time > 0 {
                    if let (Some(t), Ok(d)) =
                        (from_filetime(b.last_write_time), File::open(&open.path))
                    {
                        let _ = d.set_modified(t);
                    }
                }
                NtStatus::SUCCESS
            }
            FileInformationClass::Rename(r) => {
                let Some(to) = self.resolve(&r.file_name) else {
                    return NtStatus::OBJECT_NAME_INVALID;
                };
                let open = self.open.get_mut(&id).expect("open");
                if to.exists() && matches!(r.replace_if_exists, Boolean::False) {
                    return NtStatus::OBJECT_NAME_COLLISION;
                }
                match std::fs::rename(&open.path, &to) {
                    Ok(()) => {
                        open.path = to;
                        NtStatus::SUCCESS
                    }
                    Err(e) => status_of(&e),
                }
            }
            _ => NtStatus::NOT_SUPPORTED,
        }
    }

    fn volume(
        &self,
        class: FileSystemInformationClassLevel,
    ) -> (NtStatus, Option<FileSystemInformationClass>) {
        let (total, free) = (1_000_000i64, 250_000i64);
        let info = match class {
            FileSystemInformationClassLevel::FILE_FS_VOLUME_INFORMATION => {
                FileSystemInformationClass::FileFsVolumeInformation(FileFsVolumeInformation {
                    volume_creation_time: 0,
                    volume_serial_number: 0x1234_5678,
                    supports_objects: Boolean::False,
                    volume_label: "TESTDRIVE".into(),
                })
            }
            FileSystemInformationClassLevel::FILE_FS_SIZE_INFORMATION => {
                FileSystemInformationClass::FileFsSizeInformation(FileFsSizeInformation {
                    total_alloc_units: total,
                    available_alloc_units: free,
                    sectors_per_alloc_unit: 8,
                    bytes_per_sector: 512,
                })
            }
            FileSystemInformationClassLevel::FILE_FS_FULL_SIZE_INFORMATION => {
                FileSystemInformationClass::FileFsFullSizeInformation(FileFsFullSizeInformation {
                    total_alloc_units: total,
                    caller_available_alloc_units: free,
                    actual_available_alloc_units: free,
                    sectors_per_alloc_unit: 8,
                    bytes_per_sector: 512,
                })
            }
            FileSystemInformationClassLevel::FILE_FS_ATTRIBUTE_INFORMATION => {
                FileSystemInformationClass::FileFsAttributeInformation(FileFsAttributeInformation {
                    file_system_attributes: FileSystemAttributes::FILE_CASE_PRESERVED_NAMES
                        | FileSystemAttributes::FILE_UNICODE_ON_DISK,
                    max_component_name_len: 255,
                    file_system_name: "FAT32".into(),
                })
            }
            FileSystemInformationClassLevel::FILE_FS_DEVICE_INFORMATION => {
                FileSystemInformationClass::FileFsDeviceInformation(FileFsDeviceInformation {
                    device_type: 0x07,
                    characteristics: Characteristics::FILE_REMOTE_DEVICE,
                })
            }
            _ => return (NtStatus::NOT_SUPPORTED, None),
        };
        (NtStatus::SUCCESS, Some(info))
    }

    fn close(&mut self, id: u32) -> NtStatus {
        match self.open.remove(&id) {
            Some(open) => {
                drop(open.file);
                if open.delete_on_close {
                    let r = if open.is_dir {
                        std::fs::remove_dir(&open.path)
                    } else {
                        std::fs::remove_file(&open.path)
                    };
                    if let Err(e) = r {
                        return status_of(&e);
                    }
                }
                NtStatus::SUCCESS
            }
            None => NtStatus::INVALID_HANDLE,
        }
    }
}

fn message(pdu: RdpdrPdu) -> PduResult<Vec<SvcMessage>> {
    Ok(vec![SvcMessage::from(pdu)])
}

fn reply(req: &DeviceIoRequest, status: NtStatus) -> DeviceIoResponse {
    DeviceIoResponse::new(req.clone(), status)
}

impl ironrdp_rdpdr::RdpdrBackend for DirectoryDrive {
    fn handle_server_device_announce_response(
        &mut self,
        _: ServerDeviceAnnounceResponse,
    ) -> PduResult<()> {
        Ok(())
    }

    fn handle_scard_call(
        &mut self,
        _: DeviceControlRequest<ScardIoCtlCode>,
        _: ScardCall,
    ) -> PduResult<Vec<SvcMessage>> {
        Ok(Vec::new())
    }

    fn handle_drive_io_request(&mut self, req: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        match req {
            ServerDriveIoRequest::ServerCreateDriveRequest(r) => {
                let (status, file_id, information) = self.create(&r);
                message(RdpdrPdu::DeviceCreateResponse(DeviceCreateResponse {
                    device_io_reply: reply(&r.device_io_request, status),
                    file_id,
                    information,
                }))
            }
            ServerDriveIoRequest::DeviceCloseRequest(r) => {
                let status = self.close(r.device_io_request.file_id);
                message(RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse {
                    device_io_response: reply(&r.device_io_request, status),
                }))
            }
            ServerDriveIoRequest::DeviceReadRequest(r) => {
                let io = &r.device_io_request;
                let (status, data) = match self.open.get(&io.file_id).and_then(|o| o.file.as_ref())
                {
                    Some(f) => {
                        let mut buf = vec![0u8; r.length as usize];
                        match f.read_at(&mut buf, r.offset) {
                            Ok(n) => {
                                buf.truncate(n);
                                (NtStatus::SUCCESS, buf)
                            }
                            Err(e) => (status_of(&e), Vec::new()),
                        }
                    }
                    None => (NtStatus::INVALID_HANDLE, Vec::new()),
                };
                message(RdpdrPdu::DeviceReadResponse(DeviceReadResponse {
                    device_io_reply: reply(io, status),
                    read_data: data,
                }))
            }
            ServerDriveIoRequest::DeviceWriteRequest(r) => {
                let io = &r.device_io_request;
                let (status, length) =
                    match self.open.get(&io.file_id).and_then(|o| o.file.as_ref()) {
                        Some(f) => match f.write_all_at(&r.write_data, r.offset) {
                            Ok(()) => (NtStatus::SUCCESS, r.write_data.len() as u32),
                            Err(e) => (status_of(&e), 0),
                        },
                        None => (NtStatus::INVALID_HANDLE, 0),
                    };
                message(RdpdrPdu::DeviceWriteResponse(DeviceWriteResponse {
                    device_io_reply: reply(io, status),
                    length,
                }))
            }
            ServerDriveIoRequest::DeviceFlushBuffersRequest(r) => {
                let status = match self.open.get(&r.device_io_request.file_id) {
                    Some(o) => o.file.as_ref().map_or(NtStatus::SUCCESS, |f| {
                        f.sync_data()
                            .map_or_else(|e| status_of(&e), |_| NtStatus::SUCCESS)
                    }),
                    None => NtStatus::INVALID_HANDLE,
                };
                message(RdpdrPdu::DeviceFlushBuffersResponse(
                    DeviceFlushBuffersResponse {
                        device_io_response: reply(&r.device_io_request, status),
                    },
                ))
            }
            ServerDriveIoRequest::ServerDriveQueryInformationRequest(r) => {
                let (status, buffer) = self
                    .query_information(r.device_io_request.file_id, r.file_info_class_lvl.clone());
                message(RdpdrPdu::ClientDriveQueryInformationResponse(
                    ClientDriveQueryInformationResponse {
                        device_io_response: reply(&r.device_io_request, status),
                        buffer,
                    },
                ))
            }
            ServerDriveIoRequest::ServerDriveSetInformationRequest(r) => {
                let status =
                    self.set_information(r.device_io_request.file_id, r.set_buffer.clone());
                let response = ClientDriveSetInformationResponse::new(&r, status)
                    .map_err(|_| pdu_other_err!("set information response"))?;
                message(RdpdrPdu::ClientDriveSetInformationResponse(response))
            }
            ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(r) => {
                let io = &r.device_io_request;
                let Some(open) = self.open.get_mut(&io.file_id) else {
                    return message(RdpdrPdu::ClientDriveQueryDirectoryResponse(
                        ClientDriveQueryDirectoryResponse {
                            device_io_reply: reply(io, NtStatus::INVALID_HANDLE),
                            buffer: None,
                        },
                    ));
                };
                if r.initial_query != 0 || open.listing.is_none() {
                    open.listing = Some(Self::list(&open.path).into_iter());
                }
                let next = open.listing.as_mut().and_then(Iterator::next);
                let (status, buffer) = match next {
                    Some((name, path)) => {
                        let meta = if name == "." || name == ".." {
                            std::fs::metadata(&open.path)
                        } else {
                            std::fs::metadata(&path)
                        };
                        match meta {
                            Ok(m) => (
                                NtStatus::SUCCESS,
                                Some(FileInformationClass::from(
                                    FileBothDirectoryInformation::new(
                                        filetime(m.created()),
                                        filetime(m.accessed()),
                                        filetime(m.modified()),
                                        filetime(m.modified()),
                                        if m.is_dir() { 0 } else { m.len() as i64 },
                                        Self::attributes(&m),
                                        name,
                                    ),
                                )),
                            ),
                            Err(e) => (status_of(&e), None),
                        }
                    }
                    None => (NtStatus::NO_MORE_FILES, None),
                };
                message(RdpdrPdu::ClientDriveQueryDirectoryResponse(
                    ClientDriveQueryDirectoryResponse {
                        device_io_reply: reply(io, status),
                        buffer,
                    },
                ))
            }
            ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(r) => {
                let (status, buffer) = self.volume(r.fs_info_class_lvl.clone());
                message(RdpdrPdu::ClientDriveQueryVolumeInformationResponse(
                    ClientDriveQueryVolumeInformationResponse {
                        device_io_reply: reply(&r.device_io_request, status),
                        buffer,
                    },
                ))
            }
            ServerDriveIoRequest::ServerDriveLockControlRequest(r) => {
                message(RdpdrPdu::ClientDriveLockControlResponse(
                    ClientDriveLockControlResponse::new(r.device_io_request, NtStatus::SUCCESS),
                ))
            }
            ServerDriveIoRequest::DeviceControlRequest(r) => {
                message(RdpdrPdu::DeviceControlResponse(DeviceControlResponse {
                    device_io_reply: reply(&r.header, NtStatus::NOT_SUPPORTED),
                    output_buffer: None,
                }))
            }
            _ => Ok(Vec::new()),
        }
    }
}
