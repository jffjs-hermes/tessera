//! Virtual filesystem. All content loading goes through [`Vfs`], so mods can later
//! come from folders or zips and tests can use in-memory content.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};

use zip::ZipArchive;
use zip::result::ZipError;

/// Validate a content path is a safe relative path (no absolute prefix, no
/// Windows drive prefix, no `..` parent traversal) and normalize Windows
/// backslashes to `/`. Shared by [`DirVfs`] and [`ZipVfs`].
fn check_rel_path(path: &str) -> io::Result<String> {
    let norm = path.replace('\\', "/");
    if norm.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("path escapes vfs root: {path}"),
        ));
    }
    let b = norm.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/' {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("path escapes vfs root: {path}"),
        ));
    }
    if norm.split('/').any(|c| c == "..") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("path escapes vfs root: {path}"),
        ));
    }
    Ok(norm)
}

/// Read-only content source addressed by `/`-separated relative paths.
pub trait Vfs {
    fn read(&self, path: &str) -> io::Result<Vec<u8>>;

    fn read_to_string(&self, path: &str) -> io::Result<String> {
        String::from_utf8(self.read(path)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Sorted file paths (relative, `/`-separated) under `dir`, recursively.
    fn list(&self, dir: &str) -> io::Result<Vec<String>>;
}

/// Backed by a directory on disk. Paths may not escape the root.
pub struct DirVfs {
    root: PathBuf,
}

impl DirVfs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, path: &str) -> io::Result<PathBuf> {
        let norm = check_rel_path(path)?;
        Ok(self.root.join(Path::new(&norm)))
    }
}

impl Vfs for DirVfs {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.resolve(path)?)
    }

    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        fn walk(base: &Path, cur: &Path, out: &mut Vec<String>) -> io::Result<()> {
            for entry in std::fs::read_dir(cur)? {
                let path = entry?.path();
                if path.is_dir() {
                    walk(base, &path, out)?;
                } else if let Ok(rel) = path.strip_prefix(base) {
                    out.push(
                        rel.components()
                            .map(|c| c.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/"),
                    );
                }
            }
            Ok(())
        }
        let start = self.resolve(dir)?;
        let mut out = Vec::new();
        walk(&self.root, &start, &mut out)?;
        out.sort();
        Ok(out)
    }
}

/// In-memory content, for tests.
#[derive(Default)]
pub struct MemVfs {
    files: BTreeMap<String, Vec<u8>>,
}

impl MemVfs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, path: &str, data: impl Into<Vec<u8>>) -> Self {
        self.files.insert(path.to_string(), data.into());
        self
    }
}

impl Vfs for MemVfs {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.to_string()))
    }

    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        let prefix = if dir.is_empty() || dir == "." {
            String::new()
        } else {
            format!("{}/", dir.trim_end_matches('/'))
        };
        Ok(self
            .files
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect())
    }
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;

fn io_zip_err(e: ZipError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Backed by a zip archive over any `Read + Seek`. Paths are presented relative
/// to the detected mod root (the archive root when it holds `mod.toml`
/// directly, or the single top-level folder when the archive wraps a mod dir).
///
/// Entry names are normalized (`\` → `/`) and validated for path escape at
/// construction; `read` re-validates its argument. Duplicate entry names,
/// symlinks and non-regular entries are rejected at construction.
pub struct ZipVfs<R> {
    archive: RefCell<ZipArchive<R>>,
    /// normalized path relative to root -> archive entry index (regular files)
    files: BTreeMap<String, usize>,
}

impl<R: Read + Seek> ZipVfs<R> {
    pub fn new(reader: R) -> io::Result<Self> {
        let mut archive = ZipArchive::new(reader).map_err(io_zip_err)?;
        let len = archive.len();

        // Inspect every entry: validate the normalized name, reject duplicates
        // and non-regular/symlink entries. Collect normalized full names and
        // their directory/file kind for root detection.
        let mut entries: Vec<(String, bool, usize)> = Vec::with_capacity(len); // (norm, is_dir, idx)
        let mut norm_seen: BTreeSet<String> = BTreeSet::new();
        for i in 0..len {
            let f = archive.by_index(i).map_err(io_zip_err)?;
            let raw = f.name().to_string();
            let norm = check_rel_path(&raw)?;
            let is_dir = f.is_dir();
            if f.is_symlink() || (!is_dir && f.unix_mode().is_some_and(|m| m & S_IFMT != S_IFREG)) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported zip entry (not a regular file): {raw}"),
                ));
            }
            if !norm_seen.insert(norm.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate zip entry name: {raw}"),
                ));
            }
            entries.push((norm, is_dir, i));
        }

        // Root detection: direct root if `mod.toml` sits at the archive root,
        // otherwise a single top-level directory wraps the mod folder.
        let has_root_mod_toml = entries.iter().any(|(n, _, _)| n == "mod.toml");
        let root = if has_root_mod_toml {
            String::new()
        } else {
            let mut tops: BTreeSet<&str> = BTreeSet::new();
            for (n, _, _) in &entries {
                tops.insert(n.split('/').next().unwrap_or(""));
            }
            match tops.len() {
                1 => format!("{}/", tops.iter().next().unwrap()),
                _ => {
                    let listing = entries
                        .iter()
                        .map(|(n, _, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "ambiguous mod archive root (no top-level mod.toml, \
                             multiple top-level entries): {listing}"
                        ),
                    ));
                }
            }
        };

        // Map root-relative normalized paths to entry indices (regular files
        // only; directories are dropped, matching MemVfs/DirVfs list output).
        let mut files = BTreeMap::new();
        for (norm, is_dir, idx) in entries {
            if is_dir {
                continue;
            }
            let rel = if root.is_empty() {
                norm
            } else {
                norm.strip_prefix(&root).unwrap_or(&norm).to_string()
            };
            files.insert(rel, idx);
        }

        Ok(Self {
            archive: RefCell::new(archive),
            files,
        })
    }
}

impl<R: Read + Seek> Vfs for ZipVfs<R> {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        let norm = check_rel_path(path)?;
        let idx = self
            .files
            .get(&norm)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.to_string()))?;
        let mut archive = self.archive.borrow_mut();
        let mut f = archive.by_index(*idx).map_err(io_zip_err)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        let norm = check_rel_path(dir)?;
        let prefix = if norm.is_empty() || norm == "." {
            String::new()
        } else {
            format!("{}/", norm.trim_end_matches('/'))
        };
        Ok(self
            .files
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::io::Write;
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    /// Minimal CRC-32 (IEEE) used to hand-craft raw zip archives for cases the
    /// `ZipWriter` refuses to build (e.g. literal duplicate entry names).
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// Build a raw (STORED, no zip64, no extra fields) zip archive containing
    /// exactly the given `(name, bytes)` entries, *duplicates allowed*. Used to
    /// exercise duplicate detection that a compliant writer never produces.
    fn raw_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        fn u16le(v: u16) -> [u8; 2] {
            v.to_le_bytes()
        }
        fn u32le(v: u32) -> [u8; 4] {
            v.to_le_bytes()
        }

        let mut out = Vec::new();
        let mut lho_offsets = Vec::new(); // local header offsets per entry
        let mut central = Vec::new();
        for (name, data) in entries {
            let name = name.as_bytes();
            let crc = crc32(data);
            lho_offsets.push(out.len() as u32);
            // Local file header.
            out.extend_from_slice(&u32le(0x0403_4b50));
            out.extend_from_slice(&u16le(20)); // version needed
            out.extend_from_slice(&u16le(0)); // flags
            out.extend_from_slice(&u16le(0)); // stored
            out.extend_from_slice(&u16le(0)); // mod time
            out.extend_from_slice(&u16le(0x21)); // mod date = 1980-01-01
            out.extend_from_slice(&u32le(crc));
            out.extend_from_slice(&u32le(data.len() as u32));
            out.extend_from_slice(&u32le(data.len() as u32));
            out.extend_from_slice(&u16le(name.len() as u16));
            out.extend_from_slice(&u16le(0)); // extra length
            out.extend_from_slice(name);
            out.extend_from_slice(data);

            // Central directory header for this entry.
            let mut ce = Vec::new();
            ce.extend_from_slice(&u32le(0x0201_4b50));
            ce.extend_from_slice(&u16le(0x031E)); // version made by (unix)
            ce.extend_from_slice(&u16le(20)); // version needed
            ce.extend_from_slice(&u16le(0)); // flags
            ce.extend_from_slice(&u16le(0)); // stored
            ce.extend_from_slice(&u16le(0)); // mod time
            ce.extend_from_slice(&u16le(0x21)); // mod date
            ce.extend_from_slice(&u32le(crc));
            ce.extend_from_slice(&u32le(data.len() as u32));
            ce.extend_from_slice(&u32le(data.len() as u32));
            ce.extend_from_slice(&u16le(name.len() as u16));
            ce.extend_from_slice(&u16le(0)); // extra length
            ce.extend_from_slice(&u16le(0)); // comment length
            ce.extend_from_slice(&u16le(0)); // disk start
            ce.extend_from_slice(&u16le(0)); // internal attrs
            ce.extend_from_slice(&u32le(0o100644)); // external attrs (regular file)
            ce.extend_from_slice(&u32le(*lho_offsets.last().unwrap()));
            ce.extend_from_slice(name);
            central.extend_from_slice(&ce);
        }
        let central_start = out.len() as u32;
        out.extend_from_slice(&central);
        let central_size = central.len() as u32;
        // End of central directory.
        out.extend_from_slice(&u32le(0x0605_4b50));
        out.extend_from_slice(&u16le(0)); // disk number
        out.extend_from_slice(&u16le(0)); // central dir disk
        out.extend_from_slice(&u16le(entries.len() as u16)); // entries on disk
        out.extend_from_slice(&u16le(entries.len() as u16)); // total entries
        out.extend_from_slice(&u32le(central_size));
        out.extend_from_slice(&u32le(central_start));
        out.extend_from_slice(&u16le(0)); // comment length
        out
    }

    /// Build a zip archive over the given `(name, bytes)` file entries,
    /// optionally compressing with deflate. Directory entries are written
    /// first so the archive is structurally valid.
    fn make_zip(entries: &[(&str, &[u8])], deflate: bool) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default().compression_method(if deflate {
                CompressionMethod::Deflated
            } else {
                CompressionMethod::Stored
            });
            for (name, data) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    fn zip_vfs(entries: &[(&str, &[u8])], deflate: bool) -> ZipVfs<Cursor<Vec<u8>>> {
        ZipVfs::new(Cursor::new(make_zip(entries, deflate))).unwrap()
    }

    #[test]
    fn zip_direct_root_read_list_matches_mem_vfs() {
        let entries = [
            ("mod.toml", &b"id = \"sample\""[..]),
            ("blueprints/b.toml", &b"b"[..]),
            ("blueprints/a.toml", &b"a"[..]),
            ("patches/p.toml", &b"p"[..]),
        ];
        let z = zip_vfs(&entries, false);
        // Equivaleent MemVfs over the same relative paths.
        let mut mem = MemVfs::new();
        for (name, data) in &entries {
            mem = mem.with(name, *data);
        }
        assert_eq!(z.list("").unwrap(), mem.list("").unwrap());
        assert_eq!(
            z.list("blueprints").unwrap(),
            mem.list("blueprints").unwrap()
        );
        assert_eq!(z.read_to_string("mod.toml").unwrap(), "id = \"sample\"");
        assert_eq!(z.read_to_string("blueprints/a.toml").unwrap(), "a");
        assert_eq!(z.read_to_string("blueprints/b.toml").unwrap(), "b");
        assert_eq!(z.read_to_string("patches/p.toml").unwrap(), "p");
    }

    #[test]
    fn zip_wrapped_root_strips_single_top_dir() {
        // Common "download from GitHub" shape: a single top-level folder wraps
        // the mod content.
        let entries = [
            ("sample-mod/mod.toml", &b"id = \"sample\""[..]),
            ("sample-mod/blueprints/a.toml", &b"a"[..]),
            ("sample-mod/patches/x.toml", &b"x"[..]),
        ];
        let z = zip_vfs(&entries, false);
        // Equivalent MemVfs over the root-stripped relative paths.
        let mem = MemVfs::new()
            .with("mod.toml", "id = \"sample\"")
            .with("blueprints/a.toml", "a")
            .with("patches/x.toml", "x");
        assert_eq!(
            z.list("").unwrap(),
            vec!["blueprints/a.toml", "mod.toml", "patches/x.toml"]
        );
        assert_eq!(z.list("").unwrap(), mem.list("").unwrap());
        assert_eq!(z.read_to_string("mod.toml").unwrap(), "id = \"sample\"");
        assert_eq!(z.read_to_string("blueprints/a.toml").unwrap(), "a");
    }

    #[test]
    fn zip_list_is_sorted_and_slash_normalized() {
        // Backslashes in entry names are normalized to `/`.
        let entries = [
            ("mod.toml", &b"m"[..]),
            (r"dir\b.txt", &b"b"[..]),
            (r"dir\a.txt", &b"a"[..]),
        ];
        let z = zip_vfs(&entries, false);
        let list = z.list("").unwrap();
        assert_eq!(list, vec![r"dir/a.txt", r"dir/b.txt", "mod.toml"]);
        assert!(list.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn zip_rejects_duplicate_entry_names() {
        let raw = raw_zip(&[("mod.toml", b"a"), ("mod.toml", b"b")]);
        assert!(ZipVfs::new(Cursor::new(raw)).is_err());
    }

    #[test]
    fn raw_zip_builder_produces_readable_archive() {
        // Sanity: the hand-crafted raw_zip must itself be a valid archive, so
        // the duplicate-rejection test above is exercising dedup, not corruption.
        let raw = raw_zip(&[("mod.toml", b"hi"), ("dir/f.txt", b"f")]);
        let mut arc = zip::ZipArchive::new(Cursor::new(&raw)).unwrap();
        assert_eq!(arc.len(), 2);
        let mut s = String::new();
        arc.by_name("mod.toml")
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "hi");
    }

    #[test]
    fn zip_rejects_duplicate_after_backslash_normalization() {
        // "a/b" and "a\\b" normalize to the same name -> duplicate.
        let raw = make_zip(&[("a/b", b"x"), (r"a\b", b"y")], false);
        assert!(ZipVfs::new(Cursor::new(raw)).is_err());
    }

    #[test]
    fn zip_rejects_absolute_path_entries() {
        for bad in ["/x", r"\x", "C:/x", "C:\\x"] {
            let raw = make_zip(&[(bad, b"x")], false);
            assert!(
                ZipVfs::new(Cursor::new(raw)).is_err(),
                "expected rejection for entry {bad:?}"
            );
        }
    }

    #[test]
    fn zip_rejects_dotdot_traversal() {
        for bad in [r"..\x", "dir/../x", "dir/..", "../x"] {
            let raw = make_zip(&[(bad, b"x")], false);
            assert!(
                ZipVfs::new(Cursor::new(raw)).is_err(),
                "expected rejection for entry {bad:?}"
            );
        }
    }

    #[test]
    fn zip_rejects_symlink_and_non_regular_entries() {
        // A symlink entry is rejected at construction.
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            w.add_symlink("mod.toml", "../target", SimpleFileOptions::default())
                .unwrap();
            w.finish().unwrap();
        }
        assert!(ZipVfs::new(Cursor::new(buf.into_inner())).is_err());
    }

    #[test]
    fn zip_rejects_ambiguous_and_missing_root() {
        // Two top-level directories, no mod.toml -> ambiguous.
        let raw = make_zip(&[("a/x", b"x"), ("b/y", b"y")], false);
        assert!(ZipVfs::new(Cursor::new(raw)).is_err());
        // No mod.toml at all, no single top-level dir (empty) -> ambiguous.
        let raw = make_zip(&[], false);
        assert!(ZipVfs::new(Cursor::new(raw)).is_err());
    }

    #[test]
    fn zip_read_rechecks_paths() {
        let z = zip_vfs(&[("mod.toml", b"m")], false);
        for bad in [
            "/etc/passwd",
            r"\etc\passwd",
            "C:/x",
            "C:\\x",
            "../x",
            "a/../x",
        ] {
            let err = z.read(bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "for {bad:?}");
        }
    }

    #[test]
    fn zip_deflate_archive_round_trips() {
        let entries = [
            ("mod.toml", &b"id = \"sample\"\n"[..]),
            ("blueprints/b.toml", &b"content b"[..]),
            ("patches/x.toml", &b"content x"[..]),
        ];
        let z = zip_vfs(&entries, true);
        for (name, data) in &entries {
            assert_eq!(z.read(name).unwrap(), *data, "mismatch for {name}");
        }
        assert_eq!(z.list("").unwrap().len(), entries.len());
    }

    #[test]
    fn zip_dir_entries_are_skipped_in_list() {
        // Explicit directory entries (trailing slash) should not surface as files.
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            w.add_directory("mod.toml/x/", SimpleFileOptions::default())
                .unwrap();
            let opts = SimpleFileOptions::default();
            w.start_file("mod.toml/x/f.txt", opts).unwrap();
            w.write_all(b"f").unwrap();
            w.finish().unwrap();
        }
        let z = ZipVfs::new(Cursor::new(buf.into_inner())).unwrap();
        assert_eq!(z.list("").unwrap(), vec!["x/f.txt"]);
        assert_eq!(z.read_to_string("x/f.txt").unwrap(), "f");
    }
}
