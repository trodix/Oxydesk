//! `drives` scenario: the directory shared by the client (a local directory) appears on the desktop side as a
//! read-write mount; files go both ways through it. Both ends run on the same machine in the tests, so the
//! scenario checks the two sides with `std::fs`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::report::Report;

/// Where the redirected drive is, on both sides.
#[derive(Debug, Clone)]
pub struct DrivesCheck {
    /// Directory the client shares (what the client reads and writes).
    pub share: PathBuf,
    /// Where the server mounts it (`<drives dir>/<name>`).
    pub mount: PathBuf,
    /// Size in MiB of the file read and written through the mount.
    pub big_mib: u64,
}

fn pattern(offset: u64, buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ((offset + i as u64).wrapping_mul(31).wrapping_add(7) % 251) as u8;
    }
}

fn write_pattern(path: &Path, size: u64) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0;
    while done < size {
        let n = buf.len().min((size - done) as usize);
        pattern(done, &mut buf[..n]);
        f.write_all(&buf[..n])?;
        done += n as u64;
    }
    f.sync_all()
}

fn check_pattern(path: &Path, size: u64) -> Result<(), String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    if len != size {
        return Err(format!(
            "{} has {len} bytes, {size} expected",
            path.display()
        ));
    }
    let (mut buf, mut want) = (vec![0u8; 256 * 1024], vec![0u8; 256 * 1024]);
    let mut done = 0u64;
    while done < size {
        let n = buf.len().min((size - done) as usize);
        f.read_exact(&mut buf[..n]).map_err(|e| e.to_string())?;
        pattern(done, &mut want[..n]);
        if buf[..n] != want[..n] {
            let at = buf[..n]
                .iter()
                .zip(&want[..n])
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            return Err(format!(
                "{}: contents differ at byte {}",
                path.display(),
                done + at as u64
            ));
        }
        done += n as u64;
    }
    Ok(())
}

/// Waits until `f` succeeds.
fn eventually(timeout: Duration, mut f: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    let end = Instant::now() + timeout;
    loop {
        match f() {
            Ok(()) => return Ok(()),
            Err(e) if Instant::now() >= end => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn steps(c: &DrivesCheck, timeout: Duration) -> Result<String, String> {
    let (share, mount) = (&c.share, &c.mount);
    std::fs::create_dir_all(share).map_err(|e| e.to_string())?;

    // Client -> desktop: small text file, then the big file.
    std::fs::write(share.join("hello.txt"), b"bonjour du client").map_err(|e| e.to_string())?;
    let big = c.big_mib << 20;
    write_pattern(&share.join("big.bin"), big).map_err(|e| e.to_string())?;
    eventually(timeout, || {
        let got = std::fs::read(mount.join("hello.txt")).map_err(|e| format!("mount: {e}"))?;
        if got == b"bonjour du client" {
            Ok(())
        } else {
            Err("hello.txt differs".into())
        }
    })?;
    let names: Vec<String> = std::fs::read_dir(mount)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    if !(names.contains(&"hello.txt".to_string()) && names.contains(&"big.bin".to_string())) {
        return Err(format!("listing incomplete: {names:?}"));
    }
    let started = Instant::now();
    check_pattern(&mount.join("big.bin"), big)?;
    let read_rate = big as f64 / 1048576.0 / started.elapsed().as_secs_f64().max(1e-3);

    // Desktop -> client: create, write big, rename, mkdir, delete.
    let started = Instant::now();
    write_pattern(&mount.join("written.bin"), big).map_err(|e| format!("write: {e}"))?;
    let write_rate = big as f64 / 1048576.0 / started.elapsed().as_secs_f64().max(1e-3);
    eventually(timeout, || check_pattern(&share.join("written.bin"), big))?;
    std::fs::rename(mount.join("written.bin"), mount.join("renamed.bin"))
        .map_err(|e| format!("rename: {e}"))?;
    eventually(timeout, || {
        if share.join("renamed.bin").exists() && !share.join("written.bin").exists() {
            Ok(())
        } else {
            Err("rename not visible on the client".into())
        }
    })?;
    std::fs::create_dir(mount.join("sub")).map_err(|e| format!("mkdir: {e}"))?;
    std::fs::write(mount.join("sub/note.txt"), "é à ü – accents")
        .map_err(|e| format!("write: {e}"))?;
    eventually(timeout, || {
        let got = std::fs::read_to_string(share.join("sub/note.txt")).map_err(|e| e.to_string())?;
        if got == "é à ü – accents" {
            Ok(())
        } else {
            Err("note.txt differs".into())
        }
    })?;
    // A directory that is not empty is refused, an empty one goes away.
    if std::fs::remove_dir(mount.join("sub")).is_ok() {
        return Err("a non-empty directory was removed".into());
    }
    std::fs::remove_file(mount.join("sub/note.txt")).map_err(|e| format!("unlink: {e}"))?;
    std::fs::remove_dir(mount.join("sub")).map_err(|e| format!("rmdir: {e}"))?;
    std::fs::remove_file(mount.join("renamed.bin")).map_err(|e| format!("unlink: {e}"))?;
    eventually(timeout, || {
        if !share.join("sub").exists() && !share.join("renamed.bin").exists() {
            Ok(())
        } else {
            Err("deletions not visible on the client".into())
        }
    })?;
    // Opening a missing file fails cleanly.
    match std::fs::read(mount.join("absent.txt")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        other => return Err(format!("missing file: {other:?}")),
    }
    Ok(format!(
        "{} MiB read at {read_rate:.0} MiB/s, written at {write_rate:.0} MiB/s",
        c.big_mib
    ))
}

pub fn drives(c: &DrivesCheck, timeout: Duration, r: &mut Report) {
    // The drive is announced after the connection: the mount appears a moment later.
    let ready = eventually(timeout.max(Duration::from_secs(20)), || {
        std::fs::read_dir(&c.mount)
            .map(|_| ())
            .map_err(|e| format!("{} not mounted: {e}", c.mount.display()))
    });
    let result = ready.and_then(|()| steps(c, timeout.max(Duration::from_secs(10))));
    match result {
        Ok(detail) => r.check("drives", true, detail),
        Err(e) => r.check("drives", false, e),
    }
}
