//! Virtual filesystem. All content loading goes through [`Vfs`], so mods can later
//! come from folders or zips and tests can use in-memory content.

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};

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
        let p = Path::new(path);
        if p.components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("path escapes vfs root: {path}"),
            ));
        }
        Ok(self.root.join(p))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_vfs_read_and_list() {
        let v = MemVfs::new()
            .with("a/x.toml", "1")
            .with("a/y.toml", "2")
            .with("b/z", "3");
        assert_eq!(v.read_to_string("a/x.toml").unwrap(), "1");
        assert_eq!(v.list("a").unwrap(), vec!["a/x.toml", "a/y.toml"]);
        assert!(v.read("nope").is_err());
    }

    #[test]
    fn dir_vfs_rejects_escape_and_lists_sorted() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("t")).unwrap();
        std::fs::write(d.path().join("t/b.txt"), "b").unwrap();
        std::fs::write(d.path().join("t/a.txt"), "a").unwrap();
        let v = DirVfs::new(d.path());
        assert_eq!(v.list("t").unwrap(), vec!["t/a.txt", "t/b.txt"]);
        assert_eq!(v.read_to_string("t/a.txt").unwrap(), "a");
        assert_eq!(
            v.read("../etc/passwd").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
