//! Read-only virtual file system exposing the files copied on the remote side.
//!
//! The paste of an application reads real paths (`file:///run/user/…/oxydesk-files/3/photo.jpg`);
//! every read of such a file is turned into a range request on the list's `FileSource`. Nothing
//! is stored: a file of several gigabytes streams through, and a list that is never pasted
//! costs nothing.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    ReplyOpen, Request,
};
use oxydesk_core::files::{FileList, FileSource};

/// Lists kept mounted: a paste may still be reading an older one.
const KEEP_LISTS: usize = 4;
/// The tree of a list never changes: the kernel may cache it for a long time.
const TTL: Duration = Duration::from_secs(3600);
const ROOT: u64 = 1;

struct Node {
    parent: u64,
    is_dir: bool,
    size: u64,
    modified: i64,
    /// List and index of the entry (files only).
    file: Option<(Arc<dyn FileSource>, u32)>,
    children: BTreeMap<OsString, u64>,
}

#[derive(Default)]
struct Tree {
    nodes: HashMap<u64, Node>,
    next_ino: u64,
    next_list: u64,
    /// Inode of the directory of each published list, oldest first.
    lists: Vec<(u64, u64)>,
}

impl Tree {
    fn new() -> Self {
        let mut t = Tree {
            next_ino: ROOT + 1,
            next_list: 1,
            ..Default::default()
        };
        t.nodes.insert(ROOT, dir_node(ROOT, 0));
        t
    }

    fn add(&mut self, parent: u64, name: OsString, node: Node) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.nodes.insert(ino, node);
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children.insert(name, ino);
        }
        ino
    }

    fn remove_subtree(&mut self, ino: u64) {
        if let Some(node) = self.nodes.remove(&ino) {
            for child in node.children.into_values() {
                self.remove_subtree(child);
            }
        }
    }

    /// Adds the list as a new directory `/<id>`; returns the id and the inode of its root.
    fn publish(&mut self, list: &FileList) -> u64 {
        let id = self.next_list;
        self.next_list += 1;
        let top = self.add(ROOT, id.to_string().into(), dir_node(ROOT, 0));
        let mut by_path: HashMap<&str, u64> = HashMap::new();
        for (index, e) in list.entries.iter().enumerate() {
            let (dir, name) = match e.path.rsplit_once('/') {
                Some((d, n)) => (Some(d), n),
                None => (None, e.path.as_str()),
            };
            // A parent missing from the list (or a name clash) drops the entry: such a tree is
            // not one a file manager could paste anyway.
            let parent = match dir {
                None => top,
                Some(d) => match by_path.get(d) {
                    Some(p) => *p,
                    None => continue,
                },
            };
            if self.nodes[&parent].children.contains_key(OsStr::new(name)) {
                continue;
            }
            let node = Node {
                parent,
                is_dir: e.is_dir,
                size: e.size,
                modified: e.modified,
                file: (!e.is_dir).then(|| (list.source.clone(), index as u32)),
                children: BTreeMap::new(),
            };
            let ino = self.add(parent, OsString::from(name), node);
            by_path.insert(e.path.as_str(), ino);
        }
        self.lists.push((id, top));
        while self.lists.len() > KEEP_LISTS {
            let (old_id, old_top) = self.lists.remove(0);
            if let Some(root) = self.nodes.get_mut(&ROOT) {
                root.children.remove(OsStr::new(&old_id.to_string()));
            }
            self.remove_subtree(old_top);
        }
        id
    }
}

fn dir_node(parent: u64, modified: i64) -> Node {
    Node {
        parent,
        is_dir: true,
        size: 0,
        modified,
        file: None,
        children: BTreeMap::new(),
    }
}

fn attr(ino: u64, n: &Node, uid: u32, gid: u32) -> FileAttr {
    let time = UNIX_EPOCH + Duration::from_secs(n.modified.max(0) as u64);
    FileAttr {
        ino: INodeNo(ino),
        size: n.size,
        blocks: n.size.div_ceil(512),
        atime: time,
        mtime: time,
        ctime: time,
        crtime: time,
        kind: if n.is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        },
        // Read-only whatever the origin says: the files cannot be written back.
        perm: if n.is_dir { 0o555 } else { 0o444 },
        nlink: if n.is_dir { 2 } else { 1 },
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

struct Fs {
    tree: Arc<Mutex<Tree>>,
    uid: u32,
    gid: u32,
}

impl Filesystem for Fs {
    fn init(&mut self, _: &Request, config: &mut fuser::KernelConfig) -> io::Result<()> {
        let _ = config.set_max_readahead(1024 * 1024);
        Ok(())
    }

    fn lookup(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let tree = self.tree.lock().expect("tree");
        let found = tree
            .nodes
            .get(&parent.0)
            .and_then(|p| p.children.get(name))
            .and_then(|i| tree.nodes.get(i).map(|n| (*i, n)));
        match found {
            Some((ino, n)) => reply.entry(&TTL, &attr(ino, n, self.uid, self.gid), Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _: &Request, ino: INodeNo, _: Option<FileHandle>, reply: ReplyAttr) {
        let tree = self.tree.lock().expect("tree");
        match tree.nodes.get(&ino.0) {
            Some(n) => reply.attr(&TTL, &attr(ino.0, n, self.uid, self.gid)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _: &Request, ino: INodeNo, _: OpenFlags, reply: ReplyOpen) {
        let tree = self.tree.lock().expect("tree");
        match tree.nodes.get(&ino.0) {
            Some(n) if !n.is_dir => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
            Some(_) => reply.error(Errno::EISDIR),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _: &Request,
        ino: INodeNo,
        _: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let target = {
            let tree = self.tree.lock().expect("tree");
            tree.nodes.get(&ino.0).map(|n| (n.file.clone(), n.size))
        };
        match target {
            Some((Some((source, index)), file_size)) => {
                if offset >= file_size {
                    return reply.data(&[]);
                }
                let len = u64::from(size).min(file_size - offset) as u32;
                match source.read(index, offset, len) {
                    Ok(data) => reply.data(&data),
                    Err(e) => {
                        tracing::warn!("reading a remote file: {e}");
                        reply.error(Errno::EIO)
                    }
                }
            }
            Some(_) => reply.error(Errno::EISDIR),
            None => reply.error(Errno::ENOENT),
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
        let tree = self.tree.lock().expect("tree");
        let Some(dir) = tree.nodes.get(&ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        if !dir.is_dir {
            return reply.error(Errno::ENOTDIR);
        }
        let mut entries: Vec<(u64, FileType, &OsStr)> = vec![
            (ino.0, FileType::Directory, OsStr::new(".")),
            (dir.parent, FileType::Directory, OsStr::new("..")),
        ];
        for (name, child) in &dir.children {
            let kind = match tree.nodes.get(child) {
                Some(n) if n.is_dir => FileType::Directory,
                _ => FileType::RegularFile,
            };
            entries.push((*child, kind, name));
        }
        for (i, (child, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

/// The mounted file system and the lists it publishes.
pub struct RemoteFiles {
    dir: PathBuf,
    tree: Arc<Mutex<Tree>>,
    _session: BackgroundSession,
}

impl RemoteFiles {
    /// Mounts the file system on `dir` (created if needed; a stale mount of a previous process
    /// is released first).
    pub fn mount(dir: &Path) -> io::Result<Self> {
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(dir)
            .stderr(std::process::Stdio::null())
            .status();
        std::fs::create_dir_all(dir)?;
        let tree = Arc::new(Mutex::new(Tree::new()));
        let fs = Fs {
            tree: tree.clone(),
            uid: unsafe { libc_uid() },
            gid: unsafe { libc_gid() },
        };
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::RO,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::NoAtime,
            MountOption::FSName("oxydesk-clipboard".into()),
        ];
        config.n_threads = Some(4);
        config.clone_fd = true;
        let session = fuser::spawn_mount(fs, dir, &config)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            tree,
            _session: session,
        })
    }

    /// Publishes a list; returns the paths of its top-level entries.
    pub fn publish(&self, list: &FileList) -> Vec<PathBuf> {
        let id = self.tree.lock().expect("tree").publish(list);
        list.entries
            .iter()
            .filter(|e| e.is_root())
            .map(|e| self.dir.join(id.to_string()).join(&e.path))
            .collect()
    }
}

unsafe fn libc_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

unsafe fn libc_gid() -> u32 {
    unsafe extern "C" {
        fn getegid() -> u32;
    }
    unsafe { getegid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_core::error::ClipboardError;
    use oxydesk_core::files::FileEntry;

    struct Pattern;
    impl FileSource for Pattern {
        fn read(&self, index: u32, offset: u64, len: u32) -> Result<Vec<u8>, ClipboardError> {
            Ok((0..len)
                .map(|i| (offset + u64::from(i) + u64::from(index)) as u8)
                .collect())
        }
    }

    fn e(path: &str, size: u64, is_dir: bool) -> FileEntry {
        FileEntry {
            path: path.into(),
            size,
            is_dir,
            modified: 0,
            read_only: false,
        }
    }

    /// Reads through a real mount (skipped where FUSE is not usable).
    #[test]
    fn a_mounted_list_streams_ranges_of_a_huge_file() {
        use std::io::{Read, Seek, SeekFrom};
        let dir = std::env::temp_dir().join(format!("oxydesk-fuse-{}", std::process::id()));
        let fs = match RemoteFiles::mount(&dir) {
            Ok(fs) => fs,
            Err(e) => return eprintln!("FUSE unavailable, test skipped: {e}"),
        };
        let huge = 5 * 1024 * 1024 * 1024 + 7; // beyond 32 bits
        let list = FileList {
            entries: vec![
                e("d", 0, true),
                e("d/small", 10, false),
                e("huge", huge, false),
            ],
            source: Arc::new(Pattern),
        };
        let roots = fs.publish(&list);
        assert_eq!(roots.len(), 2);
        assert_eq!(std::fs::metadata(&roots[1]).unwrap().len(), huge);
        assert_eq!(
            std::fs::read(roots[0].join("small")).unwrap(),
            (1..11).collect::<Vec<u8>>()
        );
        let mut f = std::fs::File::open(&roots[1]).unwrap();
        f.seek(SeekFrom::Start(huge - 100)).unwrap();
        let mut tail = Vec::new();
        f.read_to_end(&mut tail).unwrap();
        assert_eq!(tail.len(), 100);
        assert_eq!(tail[0], ((huge - 100) + 2) as u8); // index 2 of the list
        assert_eq!(
            std::fs::read_dir(roots[0].parent().unwrap())
                .unwrap()
                .count(),
            2
        );
        assert!(std::fs::write(roots[0].join("small"), b"x").is_err()); // read-only
        drop(fs);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn publishes_a_tree_and_drops_the_oldest_lists() {
        let mut t = Tree::new();
        let list = FileList {
            entries: vec![
                e("d", 0, true),
                e("d/a.txt", 5, false),
                e("d/sub", 0, true),
                e("orphan/x", 1, false), // parent missing: dropped
                e("top", 3, false),
            ],
            source: Arc::new(Pattern),
        };
        let id = t.publish(&list);
        let top = t.nodes[&ROOT].children[OsStr::new(&id.to_string())];
        let names: Vec<_> = t.nodes[&top].children.keys().cloned().collect();
        assert_eq!(names, [OsString::from("d"), OsString::from("top")]);
        let d = t.nodes[&top].children[OsStr::new("d")];
        assert_eq!(t.nodes[&d].children.len(), 2);
        assert!(!t.nodes.values().any(|n| n.size == 1));
        for _ in 0..KEEP_LISTS {
            t.publish(&list);
        }
        assert_eq!(t.lists.len(), KEEP_LISTS);
        assert!(!t.nodes[&ROOT]
            .children
            .contains_key(OsStr::new(&id.to_string())));
        assert!(!t.nodes.contains_key(&top));
    }
}
