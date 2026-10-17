//! Files of the clipboard scenarios: contents are generated from the offset, so a file of several
//! gigabytes needs no storage and every byte can be checked as it streams by.

use std::sync::Arc;

use oxydesk_core::error::ClipboardError;
use oxydesk_core::files::{FileEntry, FileList, FileSource};

/// Byte `pos` of the file `path`. The contents depend on the path, not on the index: a desktop that
/// lists a folder in its own order must still find the same bytes in each file.
pub fn pattern(path: &str, pos: u64) -> u8 {
    let seed = path
        .bytes()
        .fold(7u8, |a, b| a.wrapping_mul(31).wrapping_add(b));
    ((pos.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8) ^ seed
}

struct Generated {
    paths: Vec<String>,
    sizes: Vec<u64>,
}

impl FileSource for Generated {
    fn read(&self, index: u32, offset: u64, len: u32) -> Result<Vec<u8>, ClipboardError> {
        let size = *self
            .sizes
            .get(index as usize)
            .ok_or_else(|| ClipboardError::Backend("no such file".into()))?;
        let path = &self.paths[index as usize];
        let end = offset.saturating_add(u64::from(len)).min(size);
        Ok((offset.min(end)..end).map(|p| pattern(path, p)).collect())
    }
}

fn entry(path: &str, size: u64, is_dir: bool) -> FileEntry {
    FileEntry {
        path: path.into(),
        size,
        is_dir,
        modified: 1_700_000_000,
        read_only: false,
    }
}

/// A list of generated files: directories, an empty file, odd sizes, accents.
pub fn test_list() -> FileList {
    let entries = vec![
        entry("vide.txt", 0, false),
        entry("petit.bin", 1234, false),
        entry("dossier", 0, true),
        entry("dossier/sous", 0, true),
        entry("dossier/sous/é€😀.bin", 700_001, false),
        entry("bloc.bin", 512 * 1024, false),
        entry("bloc+1.bin", 512 * 1024 + 1, false),
    ];
    generated(entries)
}

fn generated(entries: Vec<FileEntry>) -> FileList {
    let paths = entries.iter().map(|e| e.path.clone()).collect();
    let sizes = entries.iter().map(|e| e.size).collect();
    FileList {
        entries,
        source: Arc::new(Generated { paths, sizes }),
    }
}

/// A list of one large file.
pub fn big_list(size: u64) -> FileList {
    generated(vec![entry("gros.bin", size, false)])
}

/// Checks that `list` holds the entries of `expected` (same paths, sizes and kinds, in any order: a
/// desktop lists a folder in its own order and has its own times and permissions) and that every byte
/// read through its source is the generated one (read in 1 MiB chunks, nothing kept). Returns the
/// number of bytes checked.
pub fn verify(list: &FileList, expected: &FileList) -> Result<u64, String> {
    let key = |l: &FileList| {
        let mut v: Vec<_> = l
            .entries
            .iter()
            .map(|e| (e.path.clone(), e.size, e.is_dir))
            .collect();
        v.sort();
        v
    };
    if key(list) != key(expected) {
        return Err(format!(
            "different file list ({} entries received, {} expected): {:?}",
            list.entries.len(),
            expected.entries.len(),
            key(list)
        ));
    }
    let mut total = 0u64;
    for (i, e) in list.entries.iter().enumerate() {
        if e.is_dir {
            continue;
        }
        let mut pos = 0u64;
        while pos < e.size {
            let want = (e.size - pos).min(1 << 20) as u32;
            let got = list
                .source
                .read(i as u32, pos, want)
                .map_err(|err| format!("{}: read at {pos}: {err}", e.path))?;
            if got.len() != want as usize {
                return Err(format!(
                    "{}: short read at {pos} ({} of {want} bytes)",
                    e.path,
                    got.len()
                ));
            }
            if let Some(bad) = got
                .iter()
                .enumerate()
                .position(|(k, b)| *b != pattern(&e.path, pos + k as u64))
            {
                return Err(format!("{}: wrong byte at {}", e.path, pos + bad as u64));
            }
            pos += u64::from(want);
        }
        total += e.size;
    }
    Ok(total)
}

/// Same list, one byte wrong in a file: a server that corrupts files (for the mock).
pub fn corrupt(list: &FileList) -> FileList {
    struct Flip(Arc<dyn FileSource>);
    impl FileSource for Flip {
        fn read(&self, i: u32, o: u64, l: u32) -> Result<Vec<u8>, ClipboardError> {
            let mut d = self.0.read(i, o, l)?;
            if let Some(b) = d.first_mut() {
                *b ^= 1;
            }
            Ok(d)
        }
    }
    FileList {
        entries: list.entries.clone(),
        source: Arc::new(Flip(list.source.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_verifies_against_itself_and_a_flipped_byte_is_caught() {
        let l = test_list();
        assert_eq!(
            verify(&l, &l).unwrap(),
            l.entries.iter().map(|e| e.size).sum::<u64>()
        );
        let e = verify(&corrupt(&l), &l).unwrap_err();
        assert!(e.contains("wrong byte"), "{e}");
        let big = big_list(3_000_000_000);
        assert_eq!(big.source.read(0, 2_999_999_999, 10).unwrap().len(), 1);
    }
}
