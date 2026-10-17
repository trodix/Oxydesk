//! Drives redirected by the client, for a session served by an agent: the agent mounts them in the user's
//! session (it runs as that user, the server cannot) and asks their contents through the link.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use oxydesk_agent_proto::{DriveFailure, DriveOp, DriveResult, DriveStatInfo, ToAgent};
use oxydesk_core::error::DriveError;
use oxydesk_core::{DriveHandle, DriveHost, DriveStat, OpenMode, RemoteDrive};

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
            if len > oxydesk_core::drive::MAX_BLOCK {
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

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_core::{DriveEntry, DriveSpace};

    /// A drive that records the calls and fails for the names it does not know.
    #[derive(Default)]
    struct Fake(Mutex<Vec<String>>);

    impl Fake {
        fn note(&self, s: String) {
            self.0.lock().unwrap().push(s);
        }
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl RemoteDrive for Fake {
        fn stat(&self, path: &str) -> Result<DriveStat, DriveError> {
            self.note(format!("stat {path}"));
            match path {
                "missing" => Err(DriveError::NotFound),
                _ => Ok(DriveStat {
                    size: 7,
                    is_dir: path.is_empty(),
                    read_only: true,
                    modified: 1,
                    accessed: 2,
                    created: 3,
                }),
            }
        }
        fn list(&self, _: &str) -> Result<Vec<DriveEntry>, DriveError> {
            Ok(vec![DriveEntry {
                name: "a".into(),
                stat: DriveStat::default(),
            }])
        }
        fn open(&self, path: &str, mode: OpenMode) -> Result<DriveHandle, DriveError> {
            self.note(format!("open {path} {mode:?}"));
            Ok(DriveHandle(42))
        }
        fn read(&self, f: DriveHandle, offset: u64, len: u32) -> Result<Vec<u8>, DriveError> {
            self.note(format!("read {} {offset} {len}", f.0));
            Ok(vec![1; len.min(4) as usize])
        }
        fn write(&self, _: DriveHandle, _: u64, data: &[u8]) -> Result<u32, DriveError> {
            Ok(data.len() as u32)
        }
        fn flush(&self, _: DriveHandle) -> Result<(), DriveError> {
            Ok(())
        }
        fn close(&self, f: DriveHandle) {
            self.note(format!("close {}", f.0));
        }
        fn mkdir(&self, _: &str) -> Result<(), DriveError> {
            Err(DriveError::Exists)
        }
        fn remove(&self, _: &str, is_dir: bool) -> Result<(), DriveError> {
            if is_dir {
                Err(DriveError::NotEmpty)
            } else {
                Ok(())
            }
        }
        fn rename(&self, _: &str, _: &str) -> Result<(), DriveError> {
            Err(DriveError::Denied)
        }
        fn truncate(&self, _: &str, _: u64) -> Result<(), DriveError> {
            Err(DriveError::NoSpace)
        }
        fn set_modified(&self, _: &str, _: i64) -> Result<(), DriveError> {
            Err(DriveError::Io("boom".into()))
        }
        fn space(&self) -> Result<DriveSpace, DriveError> {
            Ok(DriveSpace {
                total: 100,
                available: 40,
            })
        }
    }

    #[test]
    fn every_error_of_the_drive_has_a_wire_counterpart() {
        let pairs = [
            (DriveError::NotFound, DriveFailure::NotFound),
            (DriveError::Exists, DriveFailure::Exists),
            (DriveError::Denied, DriveFailure::Denied),
            (DriveError::NotDirectory, DriveFailure::NotDirectory),
            (DriveError::IsDirectory, DriveFailure::IsDirectory),
            (DriveError::NotEmpty, DriveFailure::NotEmpty),
            (DriveError::NoSpace, DriveFailure::NoSpace),
            (DriveError::Invalid, DriveFailure::Invalid),
            (DriveError::Busy, DriveFailure::Busy),
            (DriveError::Io("x".into()), DriveFailure::Io("x".into())),
        ];
        for (error, wire) in pairs {
            assert_eq!(failure(error), wire);
        }
    }

    #[test]
    fn operations_asked_by_the_agent_run_on_the_clients_drive() {
        let d = Fake::default();
        assert_eq!(
            execute(&d, DriveOp::Stat("f".into())),
            DriveResult::Stat(DriveStatInfo {
                size: 7,
                is_dir: false,
                read_only: true,
                modified: 1,
                accessed: 2,
                created: 3
            })
        );
        assert!(matches!(
            execute(&d, DriveOp::List("".into())),
            DriveResult::List(l) if l.len() == 1 && l[0].0 == "a"
        ));
        assert_eq!(
            execute(
                &d,
                DriveOp::Open {
                    path: "f".into(),
                    mode: 3
                }
            ),
            DriveResult::Handle(42)
        );
        assert_eq!(
            execute(
                &d,
                DriveOp::Read {
                    handle: 42,
                    offset: 8,
                    len: 10
                }
            ),
            DriveResult::Data(vec![1; 4])
        );
        assert_eq!(
            execute(
                &d,
                DriveOp::Write {
                    handle: 42,
                    offset: 0,
                    data: vec![0; 5]
                }
            ),
            DriveResult::Written(5)
        );
        assert_eq!(execute(&d, DriveOp::Flush(42)), DriveResult::Ok);
        assert_eq!(execute(&d, DriveOp::Close(42)), DriveResult::Ok);
        assert_eq!(
            execute(&d, DriveOp::Space),
            DriveResult::Space {
                total: 100,
                available: 40
            }
        );
        assert!(d.calls().contains(&"open f Create".to_string()));
        assert!(d.calls().contains(&"close 42".to_string()));
    }

    #[test]
    fn failures_come_back_as_typed_errors() {
        let d = Fake::default();
        assert_eq!(
            execute(&d, DriveOp::Stat("missing".into())),
            DriveResult::Err(DriveFailure::NotFound)
        );
        assert_eq!(
            execute(&d, DriveOp::Mkdir("x".into())),
            DriveResult::Err(DriveFailure::Exists)
        );
        assert_eq!(
            execute(
                &d,
                DriveOp::Remove {
                    path: "d".into(),
                    is_dir: true
                }
            ),
            DriveResult::Err(DriveFailure::NotEmpty)
        );
        assert_eq!(
            execute(
                &d,
                DriveOp::Remove {
                    path: "f".into(),
                    is_dir: false
                }
            ),
            DriveResult::Ok
        );
        assert_eq!(
            execute(
                &d,
                DriveOp::Rename {
                    from: "a".into(),
                    to: "b".into()
                }
            ),
            DriveResult::Err(DriveFailure::Denied)
        );
        assert_eq!(
            execute(
                &d,
                DriveOp::Truncate {
                    path: "a".into(),
                    size: 1
                }
            ),
            DriveResult::Err(DriveFailure::NoSpace)
        );
        assert_eq!(
            execute(
                &d,
                DriveOp::SetModified {
                    path: "a".into(),
                    seconds: 1
                }
            ),
            DriveResult::Err(DriveFailure::Io("boom".into()))
        );
    }

    #[test]
    fn an_unknown_open_mode_and_an_oversized_read_are_refused_before_touching_the_drive() {
        let d = Fake::default();
        assert_eq!(
            execute(
                &d,
                DriveOp::Open {
                    path: "f".into(),
                    mode: 9
                }
            ),
            DriveResult::Err(DriveFailure::Invalid)
        );
        let too_big = oxydesk_core::drive::MAX_BLOCK + 1;
        assert_eq!(
            execute(
                &d,
                DriveOp::Read {
                    handle: 1,
                    offset: 0,
                    len: too_big
                }
            ),
            DriveResult::Err(DriveFailure::Invalid)
        );
        let max = oxydesk_core::drive::MAX_BLOCK;
        assert!(matches!(
            execute(
                &d,
                DriveOp::Read {
                    handle: 1,
                    offset: 0,
                    len: max
                }
            ),
            DriveResult::Data(_)
        ));
        assert!(!d.calls().iter().any(|c| c.starts_with("open")));
    }

    #[test]
    fn registered_drives_get_increasing_ids_and_are_found_by_name_when_removed() {
        let drives = Drives::default();
        let a = drives.add("C", Arc::new(Fake::default()));
        let b = drives.add("D", Arc::new(Fake::default()));
        assert!(b > a);
        assert!(drives.get(a).is_some());
        assert_eq!(drives.remove("C"), Some(a));
        assert!(drives.get(a).is_none());
        assert_eq!(drives.remove("C"), None, "already gone");
        assert!(drives.get(b).is_some());
        // An id is never reused after a removal.
        let c = drives.add("C", Arc::new(Fake::default()));
        assert!(c > b);
    }
}
