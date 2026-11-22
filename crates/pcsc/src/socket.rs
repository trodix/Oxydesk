//! The listening socket: private to the user, never following a symlink.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// Creates the socket at `path`: the parent directory is created (mode 0700) if missing, a stale socket is
/// removed, and the socket is only accessible to the user (mode 0600).
///
/// Fails when `path` is a symlink or another kind of file than a socket: it is not ours to delete.
pub(crate) fn bind_private(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let kind = meta.file_type();
            if kind.is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is a symlink: refusing to follow it", path.display()),
                ));
            }
            if !kind.is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
            std::fs::remove_file(path)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Removes the socket file, if it is still a socket.
pub(crate) fn remove_socket(path: &Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
        let _ = std::fs::remove_file(path);
    }
}

/// `true` when the peer runs as the same user as this process (or as root).
pub(crate) fn peer_is_trusted(stream: &UnixStream) -> bool {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for the size announced.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    // SAFETY: no pointer involved.
    let me = unsafe { libc::geteuid() };
    rc == 0 && (cred.uid == me || cred.uid == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_is_private_and_its_missing_parent_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run/pcsc/pcscd.comm");
        let _l = bind_private(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(path.parent().unwrap().parent().unwrap()), 0o700);
    }

    #[test]
    fn an_existing_directory_keeps_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let _l = bind_private(&dir.path().join("s")).unwrap();
        let m = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o755);
    }

    #[test]
    fn a_stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        drop(bind_private(&path).unwrap()); // leaves the file behind
        assert!(path.exists());
        let l = bind_private(&path).unwrap();
        assert!(UnixStream::connect(&path).is_ok());
        drop(l);
    }

    #[test]
    fn a_symlink_is_not_followed_nor_removed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("precious");
        std::fs::write(&target, b"data").unwrap();
        let link = dir.path().join("s");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let e = bind_private(&link).unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
        assert_eq!(std::fs::read(&target).unwrap(), b"data");
        assert!(std::fs::symlink_metadata(&link).is_ok());
    }

    #[test]
    fn a_regular_file_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s");
        std::fs::write(&file, b"data").unwrap();
        assert!(bind_private(&file).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"data");
        remove_socket(&file);
        assert!(file.exists(), "only sockets are removed");
    }

    #[test]
    fn the_peer_of_a_local_connection_is_trusted() {
        let (a, b) = UnixStream::pair().unwrap();
        assert!(peer_is_trusted(&a) && peer_is_trusted(&b));
    }

    #[test]
    fn a_socket_path_too_long_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join("x".repeat(200));
        assert!(bind_private(&long).is_err());
    }

    #[test]
    fn remove_socket_removes_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        drop(bind_private(&path).unwrap());
        remove_socket(&path);
        assert!(!path.exists());
        remove_socket(&path); // already gone: fine
    }
}
