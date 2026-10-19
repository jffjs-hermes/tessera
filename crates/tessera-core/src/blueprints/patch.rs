//! Blueprint patch engine.
//!
//! Mods tweak existing blueprints via TOML patch files that apply
//! `replace` / `merge` / `remove` operations at dotted paths against the
//! raw `toml::Value` part payloads of a [`Blueprint`]. Part validation
//! (unknown part names, schema checks) is deliberately deferred to spawn
//! time; the engine only transforms raw values, so a patch may legitimately
//! produce a payload that spawn will later reject.
//!
//! A patch file is all-or-nothing: every op it declares is applied to a
//! throwaway copy of the target library, and nothing is committed unless
//! every op succeeds. Base blueprint content is therefore never mutated in
//! place — a failed file leaves the library exactly as it was.
//!
//! Determinism rules honored here (see `docs/determinism.md`): all paths are
//! stored in `BTreeMap` and ops are applied in sorted path order; the
//! `toml::value::Map` tables we traverse are BTreeMap-backed, so no unordered
//! iteration leaks in.

use std::collections::BTreeMap;

use serde::Deserialize;
use toml::Value;

use super::{Blueprint, BlueprintError, BlueprintLibrary};
use crate::vfs::Vfs;

/// Operations declared for one target blueprint in one patch file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchOps {
    /// Dotted path -> whole-value replacement (target must exist).
    #[serde(default)]
    pub replace: BTreeMap<String, Value>,
    /// Dotted path -> deep merge (intermediate tables must exist).
    #[serde(default)]
    pub merge: BTreeMap<String, Value>,
    /// Dotted paths to remove (target must exist).
    #[serde(default)]
    pub remove: Vec<String>,
}

/// A parsed patch file: the blueprint ids it targets and the ops per target.
#[derive(Debug, Clone)]
pub struct PatchFile {
    /// Source path of the file, for diagnostics.
    pub file: String,
    /// Blueprint id -> ops, in sorted id order.
    pub targets: BTreeMap<String, PatchOps>,
}

impl PatchFile {
    /// Parse a patch file's text into its target map. Unknown keys inside a
    /// target block, or a `remove` that is not a list of strings, are parse
    /// errors (tight schema, matching the blueprint loader).
    pub fn parse(file: &str, text: &str) -> Result<Self, BlueprintError> {
        let targets: BTreeMap<String, PatchOps> =
            toml::from_str(text).map_err(|source| BlueprintError::PatchParse {
                file: file.to_string(),
                source,
            })?;
        Ok(PatchFile {
            file: file.to_string(),
            targets,
        })
    }
}

impl BlueprintLibrary {
    /// Apply a whole patch file, all-or-nothing. Every target blueprint must
    /// already exist in the library (patching an unknown blueprint id is an
    /// error), and every op must succeed; otherwise the library is left
    /// unchanged. Blueprint ids and paths are processed in sorted order for
    /// determinism.
    pub fn apply_patch_file(&mut self, patch: &PatchFile) -> Result<(), BlueprintError> {
        // Work on a throwaway copy so a failure rolls back every op in the
        // file. Base content is only ever mutated here in memory, never on
        // disk, and only on overall success.
        let mut next = self.blueprints.clone();

        for id in patch.targets.keys() {
            if !next.contains_key(id) {
                return Err(BlueprintError::PatchTargetMissing {
                    file: patch.file.clone(),
                    id: id.clone(),
                });
            }
        }

        for (id, ops) in &patch.targets {
            let bp = next.get_mut(id).expect("all targets checked above");
            apply_ops(bp, &patch.file, id, ops)?;
        }

        self.blueprints = next;
        Ok(())
    }

    /// List the sorted `.toml` patch files under `patches/` in `vfs`. Callers
    /// apply them in this order so that sequential patch files run in sorted
    /// (deterministic) path order, independent of platform enumeration.
    pub fn sorted_patch_paths(vfs: &dyn Vfs) -> Result<Vec<String>, BlueprintError> {
        let mut files = vfs.list("patches").map_err(|source| BlueprintError::Io {
            path: "patches".into(),
            source,
        })?;
        files.retain(|p| p.ends_with(".toml"));
        files.sort();
        Ok(files)
    }
}

/// Apply one target's ops to a single blueprint in deterministic order:
/// replaces (sorted path), then merges (sorted path), then removes (sorted).
fn apply_ops(
    bp: &mut Blueprint,
    file: &str,
    id: &str,
    ops: &PatchOps,
) -> Result<(), BlueprintError> {
    for (path, value) in &ops.replace {
        run_replace(bp, file, id, path, value)?;
    }
    for (path, value) in &ops.merge {
        run_merge(bp, file, id, path, value)?;
    }
    let mut removes = ops.remove.clone();
    removes.sort();
    for path in &removes {
        run_remove(bp, file, id, path)?;
    }
    Ok(())
}

/// Build a uniform root table `{ parts: { ... } }` from `bp.parts` so every
/// dotted path (including `parts.<Name>`) traverses plain `toml::Value`
/// tables, then fold the result back into `bp.parts`.
fn fold_parts<F>(bp: &mut Blueprint, f: F) -> Result<(), BlueprintError>
where
    F: FnOnce(&mut Value) -> Result<(), BlueprintError>,
{
    let mut parts = toml::map::Map::new();
    for (name, payload) in &bp.parts {
        parts.insert(name.clone(), payload.clone());
    }
    let mut root = toml::map::Map::new();
    root.insert("parts".to_string(), Value::Table(parts));
    let mut root = Value::Table(root);

    f(&mut root)?;

    let parts = root
        .as_table()
        .expect("we built the root table")
        .get("parts")
        .and_then(Value::as_table)
        .expect("we built the parts table");
    bp.parts.clear();
    for (name, payload) in parts {
        bp.parts.insert(name.clone(), payload.clone());
    }
    Ok(())
}

/// Validate a dotted path: it must be at least `parts.<Name>` so the patch
/// always targets a part payload.
fn path_comps<'a>(file: &str, id: &str, path: &'a str) -> Result<Vec<&'a str>, BlueprintError> {
    let comps: Vec<&str> = path.split('.').collect();
    if comps.len() < 2 || comps[0] != "parts" {
        return Err(BlueprintError::InvalidPatchPath {
            file: file.to_string(),
            id: id.to_string(),
            path: path.to_string(),
            msg: "patch paths must begin with `parts.` and name a part".to_string(),
        });
    }
    Ok(comps)
}

/// Where a navigation step landed, so each op can report its own error.
enum Nav {
    /// Intermediate component present but not a table.
    NotTable,
    /// Intermediate component absent (the patch would have to create it).
    Missing,
    /// Reached the parent table; the final key's presence was checked by the
    /// caller on `comps[last]`.
    Reached,
}

/// Walk `root` down the intermediate components of `comps` (all but the last),
/// returning the parent table holding the final key. Each intermediate
/// component must be a present table.
fn descend<'a>(
    root: &'a mut Value,
    comps: &[&str],
) -> Result<&'a mut toml::map::Map<String, Value>, Nav> {
    let last = comps.len().saturating_sub(1);
    let mut cur = root.as_table_mut().ok_or(Nav::NotTable)?;
    for (i, c) in comps.iter().enumerate() {
        if i == last {
            return Ok(cur);
        }
        match cur.get_mut(*c) {
            Some(Value::Table(t)) => cur = t,
            Some(_) => return Err(Nav::NotTable),
            None => return Err(Nav::Missing),
        }
    }
    Err(Nav::Reached)
}

fn run_replace(
    bp: &mut Blueprint,
    file: &str,
    id: &str,
    path: &str,
    value: &Value,
) -> Result<(), BlueprintError> {
    let comps = path_comps(file, id, path)?;
    fold_parts(bp, |root| {
        let parent = descend(root, &comps).map_err(|_| missing(file, id, path, "replace"))?;
        let last = comps[comps.len() - 1];
        if !parent.contains_key(last) {
            return Err(missing(file, id, path, "replace"));
        }
        parent.insert(last.to_string(), value.clone());
        Ok(())
    })
}

fn run_merge(
    bp: &mut Blueprint,
    file: &str,
    id: &str,
    path: &str,
    value: &Value,
) -> Result<(), BlueprintError> {
    let comps = path_comps(file, id, path)?;
    fold_parts(bp, |root| {
        let parent = descend(root, &comps).map_err(|_| missing(file, id, path, "merge"))?;
        let last = comps[comps.len() - 1];
        match parent.get_mut(last) {
            Some(Value::Table(target)) if matches!(value, Value::Table(_)) => {
                deep_merge(target, value.as_table().expect("checked above"));
            }
            Some(target) => *target = value.clone(),
            None => {
                parent.insert(last.to_string(), value.clone());
            }
        }
        Ok(())
    })
}

fn run_remove(bp: &mut Blueprint, file: &str, id: &str, path: &str) -> Result<(), BlueprintError> {
    let comps = path_comps(file, id, path)?;
    fold_parts(bp, |root| {
        let parent = descend(root, &comps).map_err(|_| missing(file, id, path, "remove"))?;
        let last = comps[comps.len() - 1];
        if !parent.contains_key(last) {
            return Err(missing(file, id, path, "remove"));
        }
        parent.remove(last);
        Ok(())
    })
}

fn missing(file: &str, id: &str, path: &str, op: &str) -> BlueprintError {
    match op {
        "replace" => BlueprintError::ReplaceTargetMissing {
            file: file.to_string(),
            id: id.to_string(),
            path: path.to_string(),
        },
        "merge" => BlueprintError::MergeTargetMissing {
            file: file.to_string(),
            id: id.to_string(),
            path: path.to_string(),
        },
        _ => BlueprintError::RemoveTargetMissing {
            file: file.to_string(),
            id: id.to_string(),
            path: path.to_string(),
        },
    }
}

/// Recursive table merge: for each patch key, if both the existing value and
/// the patch value are tables, recurse; otherwise the patch value replaces the
/// existing value (scalars and lists replace; a missing key is inserted).
fn deep_merge(target: &mut toml::map::Map<String, Value>, patch: &toml::map::Map<String, Value>) {
    for (key, value) in patch {
        match target.get_mut(key) {
            Some(Value::Table(existing)) if matches!(value, Value::Table(_)) => {
                deep_merge(existing, value.as_table().expect("checked above"));
            }
            Some(existing) => *existing = value.clone(),
            None => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blueprints::BlueprintLibrary;
    use crate::ecs::{Part, PartError, PartRegistry, PartTypeId, StorageClass};
    use crate::vfs::MemVfs;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq, Clone, Default)]
    #[serde(deny_unknown_fields)]
    struct RenderPart {
        tile: String,
    }

    #[derive(Debug, Deserialize, PartialEq, Clone, Default)]
    #[serde(deny_unknown_fields)]
    struct StatsPart {
        strength: u8,
        agility: u8,
    }

    fn register<T>(r: &mut PartRegistry, name: &str) -> Result<(), PartError>
    where
        T: serde::de::DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
    {
        r.register(
            PartTypeId::new(name),
            StorageClass::Dynamic,
            build_factory::<T>,
        )
    }

    fn build_factory<T>(value: &Value) -> Result<Box<dyn Part>, PartError>
    where
        T: serde::de::DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
    {
        let t: T =
            value
                .clone()
                .try_into()
                .map_err(|e: toml::de::Error| PartError::BuildFailed {
                    name: std::any::type_name::<T>().to_string(),
                    msg: e.to_string(),
                })?;
        Ok(Box::new(t))
    }

    fn registry() -> PartRegistry {
        let mut r = PartRegistry::new();
        register::<RenderPart>(&mut r, "Render").unwrap();
        register::<StatsPart>(&mut r, "Stats").unwrap();
        r
    }

    const CREATURES: &str = r##"
[Goblin]
parts.Render = { tile = "creature.goblin", fg = "#00aa00", flags = ["wander", "hostile"] }
parts.Stats = { strength = 18, agility = 6 }

[Humanoid]
parts.Render = { tile = "creature.humanoid" }
parts.Stats = { strength = 10, agility = 10 }
"##;

    fn load() -> BlueprintLibrary {
        let v = MemVfs::new().with("blueprints/creatures.toml", CREATURES);
        BlueprintLibrary::load(&v, &registry()).unwrap()
    }

    fn apply(lib: &mut BlueprintLibrary, text: &str) -> Result<(), BlueprintError> {
        let pf = PatchFile::parse("patches/t.toml", text).unwrap();
        lib.apply_patch_file(&pf)
    }

    fn part<'a>(lib: &'a BlueprintLibrary, id: &str, name: &str) -> &'a Value {
        lib.get(id).unwrap().part(name).unwrap()
    }

    #[test]
    fn replace_handles_scalar_table_and_list_at_dotted_paths() {
        let mut lib = load();
        apply(
            &mut lib,
            r##"
["Goblin"]
replace = { "parts.Render.tile" = "creature.goblin.blue", "parts.Render.flags" = ["angry"], "parts.Stats" = { strength = 99 } }
"##,
        )
        .unwrap();
        let render = part(&lib, "Goblin", "Render");
        let render_t = render.as_table().unwrap();
        assert_eq!(render_t["tile"].as_str(), Some("creature.goblin.blue"));
        // scalar replace on a dotted path leaves siblings intact
        assert_eq!(render_t["fg"].as_str(), Some("#00aa00"));
        // list replace replaces the whole list
        let flags = render_t["flags"].as_array().unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].as_str(), Some("angry"));
        // whole-table replace on a part name replaces it entirely
        let stats = part(&lib, "Goblin", "Stats").as_table().unwrap();
        assert_eq!(
            stats.len(),
            1,
            "whole-table replace drops agility: {stats:?}"
        );
        assert_eq!(stats["strength"].as_integer(), Some(99));
    }

    #[test]
    fn merge_recurses_into_tables_and_replaces_scalars_and_lists() {
        let mut lib = load();
        // Dotted scalar tweak: strength 18 -> 22, siblings preserved.
        apply(
            &mut lib,
            "[\"Goblin\"]\nmerge = { \"parts.Stats.strength\" = 22 }",
        )
        .unwrap();
        let stats = part(&lib, "Goblin", "Stats").as_table().unwrap();
        assert_eq!(stats["strength"].as_integer(), Some(22));
        assert_eq!(
            stats["agility"].as_integer(),
            Some(6),
            "merge must not restate or remove siblings"
        );

        // Recursive table merge at the part level: tile replaced, other keys kept.
        apply(&mut lib, "[\"Goblin\"]\nmerge = { \"parts.Render\" = { tile = \"creature.goblin.angry\", tint = \"red\" } }")
            .unwrap();
        let render = part(&lib, "Goblin", "Render").as_table().unwrap();
        assert_eq!(render["tile"].as_str(), Some("creature.goblin.angry"));
        assert_eq!(
            render["fg"].as_str(),
            Some("#00aa00"),
            "deep merge keeps untouched keys"
        );
        assert_eq!(
            render["tint"].as_str(),
            Some("red"),
            "deep merge adds new keys"
        );
        assert_eq!(
            render["flags"].as_array().unwrap().len(),
            2,
            "list under a merged table is untouched"
        );
    }

    #[test]
    fn merge_table_over_scalar_target_is_a_replace_not_an_error() {
        let mut lib = load();
        apply(
            &mut lib,
            "[\"Goblin\"]\nmerge = { \"parts.Stats.strength\" = { foo = 1 } }",
        )
        .unwrap();
        let stats = part(&lib, "Goblin", "Stats").as_table().unwrap();
        assert!(
            stats["strength"].is_table(),
            "table over scalar target is a replace, not an error"
        );
    }

    #[test]
    fn replace_missing_target_is_an_error() {
        let mut lib = load();
        let err = apply(
            &mut lib,
            "[\"Goblin\"]\nreplace = { \"parts.Render.bogus\" = \"x\" }",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("replace"), "{err}");
        assert!(err.contains("parts.Render.bogus"), "{err}");
        // intermediate missing also fails as the same op error
        let err = apply(
            &mut lib,
            "[\"Goblin\"]\nreplace = { \"parts.Missing.x\" = \"x\" }",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("replace"), "{err}");
    }

    #[test]
    fn merge_missing_intermediate_is_an_error() {
        let mut lib = load();
        // intermediate table absent
        let err = apply(&mut lib, "[\"Goblin\"]\nmerge = { \"parts.Bogus.x\" = 1 }")
            .unwrap_err()
            .to_string();
        assert!(err.contains("merge"), "{err}");
        // intermediate present but not a table
        let err = apply(
            &mut lib,
            "[\"Goblin\"]\nmerge = { \"parts.Stats.agility.deep\" = 1 }",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("merge"), "{err}");
    }

    #[test]
    fn remove_missing_target_is_an_error() {
        let mut lib = load();
        let err = apply(&mut lib, "[\"Goblin\"]\nremove = [\"parts.Render.bogus\"]")
            .unwrap_err()
            .to_string();
        assert!(err.contains("remove"), "{err}");
        assert!(err.contains("parts.Render.bogus"), "{err}");
    }

    #[test]
    fn remove_whole_part_is_allowed() {
        let mut lib = load();
        apply(&mut lib, "[\"Goblin\"]\nremove = [\"parts.Stats\"]").unwrap();
        assert!(
            lib.get("Goblin").unwrap().part("Stats").is_none(),
            "removing a whole part is allowed"
        );
        assert!(lib.get("Goblin").unwrap().part("Render").is_some());
    }

    #[test]
    fn one_failed_op_rolls_back_every_prior_op_in_that_file() {
        let mut lib = load();
        // Replace on Goblin succeeds first, then a bogus merge fails.
        let err = apply(
            &mut lib,
            r##"
["Goblin"]
replace = { "parts.Render.tile" = "patched" }
merge = { "parts.Bogus.x" = 1 }
"##,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("merge"), "{err}");
        let render = part(&lib, "Goblin", "Render").as_table().unwrap();
        assert_eq!(
            render["tile"].as_str(),
            Some("creature.goblin"),
            "prior replace in the same file must be rolled back"
        );
    }

    #[test]
    fn file_success_rolls_back_across_blueprints_but_only_commits_on_success() {
        // A file targeting two blueprints where the second op fails must not
        // commit the first blueprint's change either (all-or-nothing per file).
        let mut lib = load();
        let before = part(&lib, "Goblin", "Render").clone();
        let err = apply(
            &mut lib,
            r##"
["Goblin"]
replace = { "parts.Render.tile" = "goblin-patched" }
["Humanoid"]
merge = { "parts.Bogus.x" = 1 }
"##,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("merge"), "{err}");
        assert_eq!(
            part(&lib, "Goblin", "Render"),
            &before,
            "no target may be modified when any op in the file fails"
        );
    }

    #[test]
    fn apply_commits_only_the_patched_blueprint_and_leaves_others_unchanged() {
        let mut lib = load();
        let humanoid_before = blueprint_parts(&lib, "Humanoid");
        apply(
            &mut lib,
            "[\"Goblin\"]\nreplace = { \"parts.Render.tile\" = \"patched\" }",
        )
        .unwrap();
        assert_eq!(
            blueprint_parts(&lib, "Humanoid"),
            humanoid_before,
            "unrelated blueprint must not change on a successful patch"
        );
        assert_eq!(
            part(&lib, "Goblin", "Render").as_table().unwrap()["tile"].as_str(),
            Some("patched")
        );
    }

    #[test]
    fn sequential_patch_files_run_in_sorted_path_order() {
        let v = MemVfs::new()
            .with("blueprints/creatures.toml", CREATURES)
            .with(
                "patches/b.toml",
                "[\"Goblin\"]\nreplace = { \"parts.Stats.strength\" = 2 }",
            )
            .with(
                "patches/a.toml",
                "[\"Goblin\"]\nreplace = { \"parts.Stats.strength\" = 1 }",
            );
        let mut lib = BlueprintLibrary::load(&v, &registry()).unwrap();
        let paths = BlueprintLibrary::sorted_patch_paths(&v).unwrap();
        assert_eq!(
            paths,
            vec!["patches/a.toml".to_string(), "patches/b.toml".to_string()]
        );
        for p in &paths {
            let text = v.read_to_string(p).unwrap();
            let pf = PatchFile::parse(p, &text).unwrap();
            lib.apply_patch_file(&pf).unwrap();
        }
        // a.toml runs first, b.toml last wins => strength is 2, not 1.
        let stats = part(&lib, "Goblin", "Stats").as_table().unwrap();
        assert_eq!(
            stats["strength"].as_integer(),
            Some(2),
            "later sorted file wins"
        );
    }

    #[test]
    fn malformed_part_payload_is_not_rejected_by_patch_and_fails_at_spawn() {
        let r = registry();
        let v = MemVfs::new().with("blueprints/creatures.toml", CREATURES);
        let mut lib = BlueprintLibrary::load(&v, &r).unwrap();
        // strength is an integer in base; patched to a string — no schema check
        // at patch time (deferred to spawn).
        apply(
            &mut lib,
            "[\"Goblin\"]\nmerge = { \"parts.Stats.strength\" = \"banana\" }",
        )
        .unwrap();
        let raw = part(&lib, "Goblin", "Stats").as_table().unwrap()["strength"].clone();
        assert!(
            raw.is_str(),
            "malformed payload survives patch application {raw:?}"
        );
        // Spawn-time validation rejects it, same class as a hand-written bad blueprint.
        let err = match lib.part_typed(&r, "Goblin", "Stats") {
            Ok(_) => panic!("malformed payload must fail at spawn"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("strength"), "{err}");
        assert!(err.contains("'Goblin'"), "{err}");
    }

    #[test]
    fn patching_unknown_blueprint_id_fails() {
        let mut lib = load();
        let err = apply(
            &mut lib,
            "[\"Orc\"]\nreplace = { \"parts.Render.tile\" = \"x\" }",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown blueprint 'Orc'"), "{err}");
    }

    #[test]
    fn non_parts_path_and_unknown_keys_are_rejected() {
        let mut lib = load();
        let err = apply(&mut lib, "[\"Goblin\"]\nreplace = { \"inherits\" = \"X\" }")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must begin with `parts.`"), "{err}");
        // unknown key inside a target block is a strict parse error
        let err = PatchFile::parse("patches/t.toml", "[\"Goblin\"]\nfoo = 1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("parsing patch file"), "{err}");
        // remove must be a list of strings
        let err = PatchFile::parse("patches/t.toml", "[\"Goblin\"]\nremove = \"parts.Render\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("parsing patch file"), "{err}");
    }

    fn blueprint_parts(lib: &BlueprintLibrary, id: &str) -> BTreeMap<String, Value> {
        lib.get(id).unwrap().parts.clone()
    }
}
