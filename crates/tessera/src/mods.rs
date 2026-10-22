//! Mod composition: discovery (directory + zip sources), deterministic load
//! order, and the blueprint patch pass that layers mod content on top of base.
//!
//! Wiring only — the loader algorithms live in `tessera-core` and `tessera-mods`
//! and are reused unchanged. This module:
//!
//! - discovers local directory and zip mod sources under a mods root;
//! - builds the [`ModRegistry`] (canonical Kahn load order, base first);
//! - exposes a composite [`Overlay`] `Vfs` that presents base + every mod's
//!   content dirs (`blueprints/`, `scripts/`, `tiles/`) as one namespace, so a
//!   single `BlueprintLibrary::load` resolves inheritance across base and mods
//!   (a mod blueprint may inherit a base blueprint) and later mods override
//!   earlier ones / base id-by-id;
//! - applies each mod's `patches/*.toml` in load order through the patch
//!   engine, so the shipped sample's Goblin patch (tile) lands.
//!
//! Determinism: all collections here are `BTreeMap`/`BTreeSet`/`Vec` (no
//! `HashMap`/`HashSet`), discovery order never influences the registry order,
//! and the overlay's `list` output is sorted. These are the same rules the
//! rest of the engine follows; this crate's `clippy.toml` enforces them.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use tessera_core::blueprints::{BlueprintLibrary, PatchFile};
use tessera_core::vfs::{Vfs, ZipVfs};
use tessera_core::world::World;
use tessera_mods::{ModRegistry, ModSource, discover_dir};

/// Discover every mod under `root`: immediate subdirectories carrying a
/// `mod.toml` (via [`discover_dir`]) plus `*.zip` archives opened through
/// [`ZipVfs`]. Both flow through the same registry API.
pub fn discover_sources(root: &Path) -> io::Result<Vec<ModSource>> {
    let mut sources = discover_dir(root)?;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "zip") {
            let file = std::fs::File::open(&path)?;
            let vfs = ZipVfs::new(file)?;
            sources.push(ModSource::from_vfs(
                path.display().to_string(),
                Box::new(vfs),
            ));
        }
    }
    Ok(sources)
}

/// A composite view over the base pack vfs plus every mod's vfs in load order.
/// Implements [`Vfs`] so the existing blueprint / tile / script loaders consume
/// the whole set as one namespace: `read` resolves a path from the latest layer
/// that has it, `list` returns the sorted union.
///
/// `'a` ties the overlay to the lifetime of the [`ModRegistry`] it borrows
/// from (the registry owns each mod's [`ModSource`], whose `vfs` this reads);
/// `base` is a separate owned layer at the bottom.
pub struct Overlay<'a> {
    /// Base vfs first, then each mod's vfs in canonical load order.
    layers: Vec<Box<dyn Vfs + 'a>>,
}

impl<'a> Overlay<'a> {
    /// Build the overlay from the base pack vfs and a validated registry.
    /// Mods load in [`ModRegistry::order`] (base first, then mods), so a later
    /// mod overrides an earlier one / base for the same content path.
    pub fn new(base: Box<dyn Vfs>, registry: &'a ModRegistry) -> Self {
        let mut layers: Vec<Box<dyn Vfs + 'a>> = vec![base];
        for id in registry.order.iter().skip(1) {
            if let Some(loaded) = registry.mods.get(id) {
                let vfs: &'a dyn Vfs = loaded.source.vfs.as_ref();
                layers.push(Box::new(Borrowed(vfs)));
            }
        }
        Self { layers }
    }
}

/// A [`Vfs`] shim borrowing an already-owned mod vfs so the overlay can hold a
/// reference without moving it out of the registry.
struct Borrowed<'a>(&'a dyn Vfs);

impl Vfs for Borrowed<'_> {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.0.read(path)
    }
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        self.0.read_to_string(path)
    }
    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        self.0.list(dir)
    }
}

impl Vfs for Overlay<'_> {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        // Latest layer wins: iterate from the top (last mod) down to base.
        let mut not_found = None;
        for layer in self.layers.iter().rev() {
            match layer.read(path) {
                Ok(b) => return Ok(b),
                Err(e) if e.kind() == io::ErrorKind::NotFound => not_found = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(not_found.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, path)))
    }

    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        // Sorted union; a path present in several layers is listed once (read
        // resolves to the latest).
        let mut set: BTreeSet<String> = BTreeSet::new();
        for layer in &self.layers {
            if let Ok(paths) = layer.list(dir) {
                set.extend(paths);
            }
        }
        Ok(set.into_iter().collect())
    }
}

/// Apply every mod's `patches/*.toml` in load order to the world's blueprint
/// library. Each mod's patch files run in sorted path order; mods apply their
/// whole patch set after their own blueprints and in registry order, so a patch
/// in a later mod sees the results of an earlier one. Base content is never
/// mutated on disk (all-or-nothing per file, in memory, via the patch engine).
pub fn apply_mod_patches(world: &World, registry: &ModRegistry) -> Result<(), String> {
    for id in registry.order.iter().skip(1) {
        let Some(loaded) = registry.mods.get(id) else {
            continue;
        };
        let vfs = loaded.source.vfs.as_ref();
        let paths = BlueprintLibrary::sorted_patch_paths(vfs)
            .map_err(|e| format!("mod '{}': listing patches: {e}", id.as_str()))?;
        for p in paths {
            let text = vfs
                .read_to_string(&p)
                .map_err(|e| format!("mod '{}': reading {p}: {e}", id.as_str()))?;
            // The mod's own patch file applies against the taget blueprints
            // already in the library (base + all earlier mods).
            let patch = PatchFile::parse(&format!("{}:{p}", loaded.source.path), &text)
                .map_err(|e| format!("mod '{}': parsing {p}: {e}", id.as_str()))?;
            let mut library = world
                .library()
                .write()
                .map_err(|_| "blueprint library lock poisoned".to_string())?;
            library
                .apply_patch_file(&patch)
                .map_err(|e| format!("mod '{}': applying {p}: {e}", id.as_str()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_core::vfs::MemVfs;

    fn source(path: &str, toml: &str, content: &[(&str, &str)]) -> ModSource {
        let mut v = MemVfs::new().with("mod.toml", toml);
        for &(p, c) in content {
            v = v.with(p, c);
        }
        ModSource::from_vfs(path, Box::new(v))
    }

    /// `beta` hard-depends on `alpha`, so the canonical order is base, alpha,
    /// beta regardless of how the sources are presented.
    fn registry() -> ModRegistry {
        let sources = vec![
            source(
                "alpha",
                "id = \"alpha\"\nversion = \"1.0.0\"\napi = 1\n",
                &[("blueprints/extra.toml", "[ExtraAlpha]\nparts = {}")],
            ),
            source(
                "beta",
                "id = \"beta\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nalpha = \"1.0.0\"\n",
                &[("blueprints/extra.toml", "[ExtraBeta]\nparts = {}")],
            ),
        ];
        ModRegistry::build(sources).expect("registry")
    }

    fn mem_source(path: &str, toml: &str, content: &[(&str, &str)]) -> (String, MemVfs) {
        let mut v = MemVfs::new().with("mod.toml", toml);
        for &(p, c) in content {
            v = v.with(p, c);
        }
        (path.to_string(), v)
    }

    #[test]
    fn registry_order_is_base_first_and_deterministic() {
        let reg = registry();
        let ids: Vec<&str> = reg.order.iter().map(|m| m.as_str()).collect();
        assert_eq!(ids, vec!["base", "alpha", "beta"]);
    }

    #[test]
    fn overlay_reads_latest_layer_that_has_a_path() {
        // alpha and beta both ship blueprints/extra.toml with different ids, so
        // the overlay must resolve read to the later mod (beta).
        let (a_path, av) = mem_source(
            "alpha",
            "id = \"alpha\"\nversion = \"1.0.0\"\napi = 1\n",
            &[("blueprints/extra.toml", "[ExtraAlpha]\nparts = {}")],
        );
        let (b_path, bv) = mem_source(
            "beta",
            "id = \"beta\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nalpha = \"1.0.0\"\n",
            &[("blueprints/extra.toml", "[ExtraBeta]\nparts = {}")],
        );
        let sources = vec![
            ModSource::from_vfs(a_path, Box::new(av)),
            ModSource::from_vfs(b_path, Box::new(bv)),
        ];
        let reg = ModRegistry::build(sources).expect("registry");

        let base = MemVfs::new().with("blueprints/base.toml", "[BaseA]\nparts = {}");
        let overlay = Overlay::new(Box::new(base), &reg);
        // beta's blueprint wins for the shared path (latest layer).
        let text = overlay.read_to_string("blueprints/extra.toml").unwrap();
        assert!(
            text.contains("ExtraBeta"),
            "later mod must override: {text}"
        );
        // base file still resolves.
        let text = overlay.read_to_string("blueprints/base.toml").unwrap();
        assert!(text.contains("BaseA"));
        // missing path -> NotFound.
        assert_eq!(
            overlay.read("blueprints/missing.toml").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn overlay_lists_sorted_union() {
        let reg = registry();
        let base = MemVfs::new().with("blueprints/base.toml", "[BaseA]\nparts = {}");
        let overlay = Overlay::new(Box::new(base), &reg);
        let mut files = overlay.list("blueprints").unwrap();
        files.sort_unstable();
        assert_eq!(
            files,
            vec![
                "blueprints/base.toml".to_string(),
                "blueprints/extra.toml".to_string(),
            ],
            "union across base + both mods, listed once, sorted"
        );
    }
}
