# ADR 012: file copy-paste (CLIPRDR), large files included

## Context
The clipboard exchanged text and images. Files must travel both ways, including beyond 1 GiB, without the server or the agent having to store them or hold them in memory.

## Decision
- **Nothing is copied in advance.** A file list (`FileList` of the `core` crate: `FileEntry` items + `FileSource::read(index, offset, len)`) travels, and content is read **by ranges** when an application pastes. On the RDP side this is the normal CLIPRDR mechanism (`FileGroupDescriptorW` + `FileContentsRequest` as `RANGE`, `CB_HUGE_FILE_SUPPORT_ENABLED` for positions beyond 2 GiB, `CAN_LOCK_CLIPDATA` locks).
- **RDP → desktop**: `RemoteSource` (`crates/rdp/src/clipfiles.rs`) asks the client for 512 KiB blocks and keeps the last one. The Wayland adapter (`crates/clipboard/src/fuse.rs`) mounts a **read-only** FUSE file system (`fuser`, `fusermount3`; package `fuse3`) whose files are read on demand through the list; the Wayland selection is a `text/uri-list` (with `x-special/gnome-copied-files` and `application/x-kde-cutselection`) pointing into it. A file manager therefore copies as a stream from the client.
- **Desktop → RDP**: the adapter reads the `text/uri-list` selection of a file manager (`local_files.rs`), walks folders, and `LocalSource` reads ranges from disk. The `cliprdr-files` thread answers `FileContentsRequest` (size or range) so that disk reads never block the connection.
- **`--login` mode**: the agent holds the Wayland clipboard, the server holds the RDP connection. Lists go through `FilesSet` / `Files`, and ranges through `FileRead` / `FileData` in both directions (`FileExchange` of the `core` crate, one list identifier per copy; the last two lists stay readable so that a read in progress survives a new copy).
- **Safeguards**: files announced only if `STREAM_FILECLIP_ENABLED` is negotiated; names validated (`FileEntry::is_valid`: relative, no `..`, ≤ 259 UTF-16 units) on both sides; a read of a replaced list fails immediately (generation) instead of going to a list the peer has forgotten; 60 s timeout per range; whatever is in flight fails on disconnection.
- **Patch in `third_party/ironrdp-server`**: an error returned by the clipboard channel (a range refused by the library's validation, for example) no longer closes the connection; it is logged.

## Tests
- Unit: descriptor <-> entry conversion, block reads (`clipfiles`), `FileExchange`, agent messages, agent link (`files_flow_both_ways_by_range`), FUSE (reads at positions beyond 4 GiB without storage), URI list, real agent (`bins/oxydesk-agent/tests/files.rs`).
- Test client: `clipboard_files` (7 entries: empty file, odd sizes, nested folders, accented name, exact block and block+1) and `clipboard_files_large` (`--large-file-mib N`, a file generated on the fly, checked byte by byte as a stream, without storage). The file makes the round trip client → server → desktop → server → client.
- mstsc (Windows 11 VM): file clipboard checks pass, including a 128 MiB file in `--login` mode (`ci/mstsc-login-test.sh`).

## Limits
- Read-only on the Linux side: files pasted into the desktop cannot be modified at the source.
- Symbolic links to files are followed; those to folders are ignored (loops); special files (pipes, sockets, devices) too.
- A move (cut) is treated as a copy.
- Not tried by hand: xfreerdp and Remmina with real files (`FileGroupDescriptorW` follows the specification; clients differ mostly on folders and locks).
