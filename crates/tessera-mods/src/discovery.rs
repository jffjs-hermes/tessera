//! Mod source discovery: locating mod directories (and, once the ZipVfs
//! boundary exists, zip archives) and presenting each as a [`ModSource`] — a
//! `Vfs` rooted at the mod's root plus a diagnostic path label.
//!
//! Discovery is deliberately source-agnostic: every source is consumed by the
//! same registry API ([`crate::load_order::ModRegistry::build`]) whether it is
//! backed by a directory on disk ([`tessera_core::vfs::DirVfs`]), a zip
//! archive (a [`tessera_core::vfs::ZipVfs`] instance built by the VFS layer),
//! or an in-memory tree ([`tessera_core::vfs::MemVfs`], tests). This keeps the
//! load-order computation a pure function of the mod set and its manifests,
//! independent of how the sources were obtained (determinism rule 3).
//!
//! Directory enumeration here is sorted, so discovery never depends on
//! filesystem ordering.

use std::io;
use std::path::{Path, PathBuf};

use tessera_core::vfs::{DirVfs, Vfs};

/// A discovered mod source: a `Vfs` rooted at the mod's root (its `mod.toml`
/// reads at `"mod.toml"`, content under `blueprints/`, `scripts/`, `patches/`,
/// `tiles/`) plus a diagnostic path label.
pub struct ModSource {
    /// Diagnostic path label, e.g. `data/mods/foo` or `data/mods/foo.zip`.
    pub path: String,
    /// The mod's content, addressable by `/`-separated relative paths.
    pub vfs: Box<dyn Vfs>,
}

impl ModSource {
    /// Wrap an on-disk mod directory as a source.
    pub fn directory(path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        let label = p.display().to_string();
        Self {
            path: label,
            vfs: Box::new(DirVfs::new(p)),
        }
    }

    /// Wrap an already-open source — e.g. a zip archive opened through
    /// `ZipVfs` by the caller. This is the seam that lets directory and zip
    /// sources feed the same registry API.
    pub fn from_vfs(path: impl Into<String>, vfs: Box<dyn Vfs>) -> Self {
        Self {
            path: path.into(),
            vfs,
        }
    }
}

impl std::fmt::Debug for ModSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModSource")
            .field("path", &self.path)
            .finish()
    }
}

/// Scan `root` for mod sources: every immediate subdirectory that contains a
/// `mod.toml` becomes one [`ModSource`]. Entries are visited in sorted path
/// order so the returned set is independent of filesystem enumeration order.
///
/// This is the directory half of discovery. Zip archives are opened by the
/// caller (the VFS layer's `ZipVfs`) and handed to
/// [`ModRegistry::build`](crate::load_order::ModRegistry::build) through
/// [`ModSource::from_vfs`], so both source kinds flow through the same
/// registry.
pub fn discover_dir(root: &Path) -> io::Result<Vec<ModSource>> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)?
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<_>>()?;
    dirs.sort();
    let mut out = Vec::new();
    for dir in dirs {
        if dir.is_dir() && dir.join("mod.toml").is_file() {
            out.push(ModSource::directory(dir));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_dir_finds_mod_directories_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        // Write mod dirs in an order that differs from sorted order, plus a
        // non-mod dir and a stray file.
        std::fs::create_dir(tmp.path().join("zeta")).unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir(tmp.path().join("not-a-mod")).unwrap();
        std::fs::write(
            tmp.path().join("alpha/mod.toml"),
            "id = \"alpha\"\nversion = \"1.0.0\"\napi = 1\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("zeta/mod.toml"),
            "id = \"zeta\"\nversion = \"1.0.0\"\napi = 1\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("README.md"), "not a mod").unwrap();

        let sources = discover_dir(tmp.path()).unwrap();
        let paths: Vec<String> = sources.iter().map(|s| s.path.clone()).collect();
        // alpha before zeta regardless of creation order; non-mod entries skipped.
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("alpha"));
        assert!(paths[1].ends_with("zeta"));
    }
}
