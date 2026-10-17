# ADR 013: drive redirection (RDPDR), client folders mounted in the session

## Context
An RDP client can share folders (mstsc: "Local resources > Drives", xfreerdp: `/drive:`, Remmina: "Share folder"). Goal: see them in the Linux session as ordinary folders, readable and writable, with files of several gigabytes and no intermediate storage.

## Decision
- **Port** (`crates/core/src/drive.rs`): `RemoteDrive` (blocking, relative `/`-separated paths: `stat`, `list`, `open`/`read`/`write`/`flush`/`close`, `mkdir`, `remove`, `rename`, `truncate`, `set_modified`, `space`) and `DriveHost` (`added`/`removed`, what the RDP adapter calls). No protocol dependency; `DriveError` maps to an errno on the FUSE side.
- **RDP adapter** (`crates/rdp/src/drive.rs`): `DriveBackend` implements `RdpdrServerBackend`; `RdpDrive` turns each operation into IRPs (`IRP_MJ_CREATE`, `READ`, `WRITE`, `QUERY_INFORMATION`, `SET_INFORMATION` for size and rename, `DIRECTORY_CONTROL`, `QUERY_VOLUME_INFORMATION`, `CLOSE`) and waits for the answer (30 s timeout). Reads are split into 256 KiB IRPs and looped until the requested block is complete: a short reply means end of file for the kernel. Deletions request `FILE_DELETE_ON_CLOSE` and `FileDispositionInformation` (a non-empty folder is refused beforehand). Directories are read one entry per request until `STATUS_NO_MORE_FILES`. `FlushBuffers` is not sent (each write is complete once the client answers).
- **Request/response correlation**: the IronRDP backend receives no request identifier. Patch in `third_party/ironrdp-rdpdr`: `ServerEvent::Rdpdr(Option<u64>, message)` carries a tag, `RdpdrServer::set_next_tag` stores it before `send_message`, and the `on_request_sent(completion_id, tag)` hook binds the tag to the completion id. Any number of requests may be in flight.
- **`PAKID_CORE_USER_LOGGEDON`**: the server sends it after the capability reply; without it Windows clients only announce their drives after an interactive logon and nothing gets mounted.
- **Never on the tokio loop**: blocking port calls run on a dedicated thread (`drive-host` for `added`/`removed`) and on the FUSE threads; disconnecting drops the backend, which fails pending waits and unmounts the drives.
- **Read-write FUSE** (`crates/drivefs`, crate `rdsktp-drivefs`, `fuser` 0.18, `fusermount3`): inode table with `forget` counters, 2 s listing cache used by `lookup`/`getattr`, 1 s attribute validity, four threads. A drive appears under `<directory>/<name>`; the name is sanitised by `directory_name` (`C:` becomes `C`, no separators, duplicates get a suffix).
- **Where**: `--drives-dir DIR`; default with `--session kwin`: `$XDG_RUNTIME_DIR/rdsktp-drives`; `off` disables it. Absent with `--test-mode` unless requested. With `--login`, the agent (running as the user) mounts: `$XDG_RUNTIME_DIR/rdsktp-drives`.
- **`--login` mode**: the server keeps the RDP connection and the agent mounts the FUSE. Agent protocol: `ToAgent::DriveAdded`/`DriveRemoved`/`DriveReply`, `FromAgent::DriveCall` (operation `DriveOp`, result `DriveResult`, 13 operations, tags 17 to 19 and 14). `AgentLink` runs each call on a thread (the link reader thread never blocks), refuses reads over 1 MiB, and the agent exposes `ProxyDrive` (a `RemoteDrive` whose every call is a round trip, 60 s timeout) to the same `DriveMounts`. Everything is unmounted when the connection ends.

## Tests
- Unit: port (`directory_name`, `is_valid_path`), agent protocol (round trip of every operation, lying counter refused), agent link with a fake drive (`drives_are_announced_to_the_agent_and_served_on_its_calls`), real FUSE with a `RemoteDrive` over `std::fs` (`crates/drivefs`), `ProxyDrive` / `Pending` / `Drives` in the agent.
- In-memory loopback (`bins/rdp-test-client/tests/drive_loopback.rs`): `RdpdrServer` + `DriveBackend` against a real IronRDP `Rdpdr` client backed by a folder (`drive_client.rs`): read, write, create, rename, delete, non-empty folder, accented names.
- End to end: `drives` scenario of the test client (`--drive-share DIR --drive-mount DIR`): files both ways (`--drive-mib`, 64 MiB ≈ 180 MiB/s read and 110 MiB/s write on the synthetic desktop), rename, `mkdir`/`rmdir`, delete, clean error on a missing file. `bash ci/drives-test.sh` runs it against `rdsktp --test-mode` and checks that nothing stays mounted. `ci/clients-test.sh` checks the files from the host while xfreerdp and Remmina (container) stay connected.

## Limits
- Only file drives are accepted here (printers are ADR 014; no serial ports or smart cards). Case follows Linux: two files differing only by case coexist on the desktop side, not on the Windows side.
- No locks, links or extended attributes; Unix permissions are synthetic (0755/0644, 0555/0444 for a read-only drive).
- A client disconnecting during a write leaves a truncated file; in-flight operations fail with `EIO`.

## Client peculiarities (observed)
- **Windows (mstsc)**: announces its drives after `PAKID_CORE_USER_LOGGEDON`; answers `STATUS_INVALID_PARAMETER` to a `Create` whose `SharedAccess` is `0xFFFFFFFF` (`SharedAccess::all()` of the crate, an open-bits type), so the server sends read | write | delete; answers `STATUS_INVALID_PARAMETER` for a missing path (mapped to "not found"); never answers `FlushBuffers`; applies the listing pattern inside the opened folder (`\*`).
- **FreeRDP (xfreerdp, Remmina)**: drive name announced as an ASCII C string (UTF-16 on Windows); resolves the listing pattern from the drive root (`\folder\*`); answers `STATUS_NOT_SUPPORTED` to `FlushBuffers`; deletes only on `FileDispositionInformation`; Remmina names the drive after the shared folder. The server therefore tries the pattern naming the folder, then `\*`.

## Results
- xfreerdp and Remmina (`bash ci/clients-test.sh`): files both ways, rename, folders, deletion checked from the host, all other checks unchanged.
- mstsc (`cargo xtask test --client`, Windows 11 VM): all checks pass, including `drives_roundtrip` / `drives_host` (3 MiB read and 2 MiB write through the mount, `mkdir`, delete, rename seen on both sides).
- IronRDP client (`bash ci/drives-test.sh`): 64 MiB each way.
- **`--login` relay (agent)**: validated in the Debian 13 VM with `ci/clients-login-test.sh` (xfreerdp and Remmina) and `ci/mstsc-login-test.sh` (mstsc), see ADR 014 for the results.
