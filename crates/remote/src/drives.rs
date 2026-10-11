//! Drives redirected by the client, for a session served by an agent: the agent mounts them in the user's
//! session (it runs as that user, the server cannot) and asks their contents through the link.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use rdsktp_agent_proto::{DriveFailure, DriveOp, DriveResult, DriveStatInfo, ToAgent};
use rdsktp_core::error::DriveError;
use rdsktp_core::{DriveHandle, DriveHost, DriveStat, OpenMode, RemoteDrive};

use crate::link::AgentLink;

pub(crate) fn failure(e: DriveError) -> DriveFailure {
    match e {
        DriveError::NotFound => DriveFailure::NotFound,
        DriveError::Exists => DriveFailure::Exists,
        DriveError::Denied => DriveFailure::Denied,
        DriveError::NotDirectory => DriveFailure::NotDirectory,
        DriveError::IsDirectory => DriveFailure::IsDirectory,
        DriveError::NotEmpty => DriveFailure::NotEmpty,
        DriveError::NoSpace => DriveFailure::NoSpace,
        DriveError::Invalid => DriveFailure::Invalid,
        DriveError::Busy => DriveFailure::Busy,
        DriveError::Io(m) => DriveFailure::Io(m),
    }
}

fn info(s: DriveStat) -> DriveStatInfo {
    DriveStatInfo {
        size: s.size,
        is_dir: s.is_dir,
        read_only: s.read_only,
        modified: s.modified,
        accessed: s.accessed,
        created: s.created,
    }
}

/// Runs the operation asked by the agent on the client's drive. Blocking: not for the reader thread.
pub(crate) fn execute(drive: &dyn RemoteDrive, op: DriveOp) -> DriveResult {
    let result = match op {
        DriveOp::Stat(p) => drive.stat(&p).map(|s| DriveResult::Stat(info(s))),
        DriveOp::List(p) => drive.list(&p).map(|entries| {
            DriveResult::List(
                entries
                    .into_iter()
                    .map(|e| (e.name, info(e.stat)))
                    .collect(),
            )
        }),
        DriveOp::Open { path, mode } => {
            let mode = match mode {
                0 => OpenMode::Read,
                1 => OpenMode::ReadWrite,
                2 => OpenMode::Truncate,
                3 => OpenMode::Create,
                _ => return DriveResult::Err(DriveFailure::Invalid),
            };
            drive.open(&path, mode).map(|h| DriveResult::Handle(h.0))
        }
        DriveOp::Read {
            handle,
            offset,
            len,
        } => {
            // The agent cannot make this side allocate more than one block.
            if len > rdsktp_core::drive::MAX_BLOCK {
                return DriveResult::Err(DriveFailure::Invalid);
            }
            drive
                .read(DriveHandle(handle), offset, len)
                .map(DriveResult::Data)
        }
        DriveOp::Write {
            handle,
            offset,
            data,
        } => drive
            .write(DriveHandle(handle), offset, &data)
            .map(DriveResult::Written),
        DriveOp::Flush(h) => drive.flush(DriveHandle(h)).map(|()| DriveResult::Ok),
        DriveOp::Close(h) => {
            drive.close(DriveHandle(h));
            Ok(DriveResult::Ok)
        }
        DriveOp::Mkdir(p) => drive.mkdir(&p).map(|()| DriveResult::Ok),
        DriveOp::Remove { path, is_dir } => drive.remove(&path, is_dir).map(|()| DriveResult::Ok),
        DriveOp::Rename { from, to } => drive.rename(&from, &to).map(|()| DriveResult::Ok),
        DriveOp::Truncate { path, size } => drive.truncate(&path, size).map(|()| DriveResult::Ok),
        DriveOp::SetModified { path, seconds } => {
            drive.set_modified(&path, seconds).map(|()| DriveResult::Ok)
        }
        DriveOp::Space => drive.space().map(|s| DriveResult::Space {
            total: s.total,
            available: s.available,
        }),
    };
    match result {
        Ok(r) => r,
        Err(e) => DriveResult::Err(failure(e)),
    }
}

struct Registered {
    name: String,
    drive: Arc<dyn RemoteDrive>,
}

/// The drives of the connection, as the agent knows them.
#[derive(Default)]
pub(crate) struct Drives {
    inner: Mutex<(u32, HashMap<u32, Registered>)>,
}

impl Drives {
    pub(crate) fn get(&self, id: u32) -> Option<Arc<dyn RemoteDrive>> {
        let g = self.inner.lock().expect("drives mutex");
        g.1.get(&id).map(|r| r.drive.clone())
    }

    fn add(&self, name: &str, drive: Arc<dyn RemoteDrive>) -> u32 {
        let mut g = self.inner.lock().expect("drives mutex");
        g.0 += 1;
        let id = g.0;
        g.1.insert(
            id,
            Registered {
                name: name.to_owned(),
                drive,
            },
        );
        id
    }

    fn remove(&self, name: &str) -> Option<u32> {
        let mut g = self.inner.lock().expect("drives mutex");
        let id =
            g.1.iter()
                .find(|(_, r)| r.name == name)
                .map(|(id, _)| *id)?;
        g.1.remove(&id);
        Some(id)
    }
}

/// `DriveHost` that hands the drives to the agent of the session.
pub struct AgentDrives(pub(crate) Weak<AgentLink>);

impl DriveHost for AgentDrives {
    fn added(&self, name: &str, drive: Arc<dyn RemoteDrive>) {
        let Some(link) = self.0.upgrade() else { return };
        let drive_id = link.drives.add(name, drive);
        link.send(&ToAgent::DriveAdded {
            drive_id,
            name: name.to_owned(),
        });
    }

    fn removed(&self, name: &str) {
        let Some(link) = self.0.upgrade() else { return };
        if let Some(drive_id) = link.drives.remove(name) {
            link.send(&ToAgent::DriveRemoved { drive_id });
        }
    }
}
