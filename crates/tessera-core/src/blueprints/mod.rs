//! Blueprint loader and library.
//!
//! Scope: load `blueprints/*.toml`
//! through the [`Vfs`] in sorted path order, validate the strict schema, then
//! resolve single-parent inheritance deterministically into a
//! [`BlueprintLibrary`]. Blueprint text is kept as `toml::Value` per part until
//! spawning, so a part registered later can still parse the same file.
//!
//! Determinism rules honored here (see `docs/determinism.md`): all collections
//! are `BTreeMap` keyed by id/part name (sorted iteration, no `HashMap`), and
//! resolution walks each chain parent→child in sorted id order.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use toml::Value;

use crate::ecs::{Part, PartRegistry, PartTypeId};
use crate::kinds::KIND_PARTS;
use crate::vfs::Vfs;

mod error;
mod patch;

pub use error::BlueprintError;
pub use error::LoadFail;
pub use patch::PatchFile;
pub use patch::PatchOps;

/// A blueprint definition as collected from a single file, before inheritance.
#[derive(Debug, Clone)]
struct RawBlueprint {
    file: String,
    inherits: Option<String>,
    parts: BTreeMap<String, Value>,
    /// Script files that register schemas/handlers for this blueprint, in
    /// deterministic (lexicographic) order. Relative to the mod root.
    scripts: BTreeSet<String>,
}

impl RawBlueprint {
    /// Validate one blueprint table against the strict schema and the registry.
    fn parse(
        id: &str,
        file: &str,
        body: BTreeMap<String, Value>,
        registry: &PartRegistry,
    ) -> Result<RawBlueprint, BlueprintError> {
        let invalid = |msg: String| BlueprintError::Invalid {
            path: file.to_string(),
            id: id.to_string(),
            msg,
        };

        let mut inherits = None;
        let mut parts = BTreeMap::new();
        let mut scripts = BTreeSet::new();
        for (key, val) in body {
            match key.as_str() {
                "inherits" => match val {
                    Value::String(s) => inherits = Some(s),
                    other => {
                        return Err(invalid(format!(
                            "`inherits` must be a string, got {}",
                            type_name(&other)
                        )));
                    }
                },
                "parts" => match val {
                    Value::Table(t) => {
                        for (pname, pval) in t {
                            match pval {
                                Value::Table(_) => {
                                    if !registry.contains(&PartTypeId::new(pname.clone())) {
                                        return Err(invalid(format!(
                                            "unknown part '{pname}': no PartRegistry entry \
                                             (mod-defined parts must be registered by the pack \
                                             that ships them)"
                                        )));
                                    }
                                    parts.insert(pname, pval);
                                }
                                other => {
                                    return Err(invalid(format!(
                                        "part '{pname}' payload must be a table, got {}",
                                        type_name(&other)
                                    )));
                                }
                            }
                        }
                    }
                    other => {
                        return Err(invalid(format!(
                            "`parts` must be a table, got {}",
                            type_name(&other)
                        )));
                    }
                },
                "handlers" => {
                    return Err(invalid(
                        "`handlers` is reserved for event handlers, which are not \
                         yet supported; remove it from this blueprint"
                            .to_string(),
                    ));
                }
                "scripts" => match val {
                    Value::Array(items) => {
                        for item in items {
                            match item {
                                Value::String(s) => {
                                    scripts.insert(s);
                                }
                                other => {
                                    return Err(invalid(format!(
                                        "`scripts` entries must be strings, got {}",
                                        type_name(&other)
                                    )));
                                }
                            }
                        }
                    }
                    other => {
                        return Err(invalid(format!(
                            "`scripts` must be an array of strings, got {}",
                            type_name(&other)
                        )));
                    }
                },
                other => {
                    return Err(invalid(format!(
                        "unknown top-level key '{other}' (allowed: `inherits`, `parts`, `scripts`)"
                    )));
                }
            }
        }
        Ok(RawBlueprint {
            file: file.to_string(),
            inherits,
            parts,
            scripts,
        })
    }
}

/// A fully resolved blueprint: inheritance flattened into `parts`.
#[derive(Debug, Clone)]
pub struct Blueprint {
    pub id: String,
    /// Path of the file that defined this id (its final, winning definition).
    pub file: String,
    /// Immediate parent id (kept for diagnostics/introspection).
    pub inherits: Option<String>,
    /// Resolved parts keyed by part name (sorted, whole-part inheritance).
    pub parts: BTreeMap<String, Value>,
    /// Resolved script files (sorted); a child replaces the parent's whole set.
    pub scripts: BTreeSet<String>,
}

impl Blueprint {
    pub fn part(&self, name: &str) -> Option<&Value> {
        self.parts.get(name)
    }

    /// Sorted part names.
    pub fn part_names(&self) -> impl Iterator<Item = &String> {
        self.parts.keys()
    }

    /// Sorted script paths.
    pub fn script_paths(&self) -> impl Iterator<Item = &String> {
        self.scripts.iter()
    }
}

/// An immutable, deterministic library of resolved blueprints.
#[derive(Default, Debug)]
pub struct BlueprintLibrary {
    blueprints: BTreeMap<String, Blueprint>,
}

impl BlueprintLibrary {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &str) -> Option<&Blueprint> {
        self.blueprints.get(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.blueprints.contains_key(id)
    }

    /// Sorted blueprint ids.
    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.blueprints.keys()
    }

    /// Load every `blueprints/*.toml` in the vfs in sorted path order: collect
    /// definitions, validate the strict schema against `registry`, then resolve
    /// inheritance parent→child. Diagnostics accumulate across the whole pass and
    /// the entire batch is rejected if anything is malformed (fail-closed, so hot
    /// reload can keep the previous library on any error).
    pub fn load(vfs: &dyn Vfs, registry: &PartRegistry) -> Result<Self, LoadFail> {
        let mut diags = Vec::new();

        let files = match vfs.list("blueprints") {
            Ok(f) => f,
            Err(source) => {
                return Err(LoadFail::single(BlueprintError::Io {
                    path: "blueprints".into(),
                    source,
                }));
            }
        };

        // Collection pass: parse each file, validate each blueprint. Last
        // definition of an id wins (later files override earlier ones).
        let mut collected: BTreeMap<String, RawBlueprint> = BTreeMap::new();
        for path in files.iter().filter(|p| p.ends_with(".toml")) {
            let text = match vfs.read_to_string(path) {
                Ok(t) => t,
                Err(source) => {
                    diags.push(BlueprintError::Io {
                        path: path.clone(),
                        source,
                    });
                    continue;
                }
            };
            let table: BTreeMap<String, BTreeMap<String, Value>> = match toml::from_str(&text) {
                Ok(t) => t,
                Err(source) => {
                    diags.push(BlueprintError::Parse {
                        path: path.clone(),
                        source,
                    });
                    continue;
                }
            };
            for (id, body) in table {
                match RawBlueprint::parse(&id, path, body, registry) {
                    Ok(raw) => {
                        collected.insert(id, raw);
                    }
                    Err(e) => diags.push(e),
                }
            }
        }
        if !diags.is_empty() {
            return Err(LoadFail::new(diags));
        }

        // Resolution pass: walk each chain parent→child in sorted id order.
        let ids: Vec<String> = collected.keys().cloned().collect();
        let mut resolved: BTreeMap<String, Blueprint> = BTreeMap::new();
        let mut on_stack: Vec<String> = Vec::new();
        for id in &ids {
            Self::resolve(id, &collected, &mut resolved, &mut on_stack, &mut diags);
        }
        if !diags.is_empty() {
            return Err(LoadFail::new(diags));
        }

        // Naming lint: a blueprint that
        // carries a kind part whose id prefix does not match it is a load
        // error. Runs on the fully resolved set so *inherited* kind parts
        // count toward the check, and errors only on contradiction — never on
        // style (a kind-prefixed blueprint without `inherits` from the
        // abstract root is allowed; the abstract roots themselves are exempt).
        Self::lint_kind_naming(&resolved, &mut diags);
        if !diags.is_empty() {
            return Err(LoadFail::new(diags));
        }

        Ok(BlueprintLibrary {
            blueprints: resolved,
        })
    }

    /// Validate the kind-prefix / kind-part pairing of every resolved
    /// blueprint, in sorted id order (deterministic diagnostics). Two rules,
    /// both fail-closed:
    ///
    /// 1. An id with a kind prefix (`Ability_`, `Skill_`, …) must carry the
    ///    matching kind part (`parts.Ability`, …) — either its own payload or
    ///    inherited.
    /// 2. A blueprint carrying a kind part must have the matching id prefix —
    ///    unless its id is the kind's bare abstract root (`[Ability]`, …),
    ///    which is where base ships the kind's full defaults.
    ///
    /// The lint never looks at payload contents and never touches inheritance
    /// or patch behavior; it only rejects contradictory id/part names.
    fn lint_kind_naming(resolved: &BTreeMap<String, Blueprint>, diags: &mut Vec<BlueprintError>) {
        for bp in resolved.values() {
            for (prefix, part) in KIND_PARTS {
                // Rule 1: prefix present, kind part missing.
                if bp.id.starts_with(prefix) && !bp.parts.contains_key(*part) {
                    diags.push(BlueprintError::Invalid {
                        path: bp.file.clone(),
                        id: bp.id.clone(),
                        msg: format!(
                            "id uses the '{prefix}' kind prefix, so it must carry \
                             parts.{part}; add `parts.{part} = {{ ... }}` (it may be \
                             inherited) or rename the id"
                        ),
                    });
                }
                // Rule 2: kind part present, id prefix missing (abstract root
                // ids — `[Ability]` etc. — are the deliberate exception).
                if bp.parts.contains_key(*part) && bp.id != *part && !bp.id.starts_with(prefix) {
                    diags.push(BlueprintError::Invalid {
                        path: bp.file.clone(),
                        id: bp.id.clone(),
                        msg: format!(
                            "carries parts.{part}, which requires an id with the \
                             '{prefix}' prefix (got '{}'); rename the id or remove \
                             the kind part",
                            bp.id
                        ),
                    });
                }
            }
        }
    }

    /// Depth-first resolution of one id. `on_stack` tracks the current chain for
    /// cycle detection; `resolved` memoizes completed blueprints.
    fn resolve(
        id: &str,
        collected: &BTreeMap<String, RawBlueprint>,
        resolved: &mut BTreeMap<String, Blueprint>,
        on_stack: &mut Vec<String>,
        diags: &mut Vec<BlueprintError>,
    ) {
        if resolved.contains_key(id) {
            return;
        }
        if let Some(entered_idx) = on_stack.iter().position(|x| x == id) {
            // id repeats on the current chain => cycle. The chain runs from the
            // first occurrence back around to it.
            let entered = &on_stack[entered_idx];
            let chain = on_stack[entered_idx..]
                .iter()
                .map(|s| s.as_str())
                .chain(std::iter::once(entered.as_str()))
                .collect::<Vec<_>>()
                .join(" -> ");
            diags.push(BlueprintError::Cycle {
                id: id.to_string(),
                path: collected[id].file.clone(),
                chain,
                entered_id: entered.clone(),
                entered_path: collected[entered].file.clone(),
            });
            return;
        }

        let raw = &collected[id];
        on_stack.push(id.to_string());

        let mut parts = BTreeMap::new();
        if let Some(parent) = &raw.inherits {
            if !collected.contains_key(parent) {
                diags.push(BlueprintError::UnknownParent {
                    path: raw.file.clone(),
                    id: id.to_string(),
                    parent: parent.clone(),
                });
                on_stack.pop();
                return;
            }
            Self::resolve(parent, collected, resolved, on_stack, diags);
            if let Some(parent_bp) = resolved.get(parent) {
                parts.extend(parent_bp.parts.clone());
            }
        }
        // Whole-part replacement: a child part with the same name replaces the
        // parent's (no deep merge).
        parts.extend(raw.parts.clone());

        // Scripts are resolved as a whole set per blueprint: a child that
        // declares any scripts replaces the parent's entire set (no union).
        let scripts = if raw.scripts.is_empty() {
            match &raw.inherits {
                Some(parent) => resolved
                    .get(parent)
                    .map(|p| p.scripts.clone())
                    .unwrap_or_default(),
                None => BTreeSet::new(),
            }
        } else {
            raw.scripts.clone()
        };

        resolved.insert(
            id.to_string(),
            Blueprint {
                id: id.to_string(),
                file: raw.file.clone(),
                inherits: raw.inherits.clone(),
                parts,
                scripts,
            },
        );
        on_stack.pop();
    }

    /// The typed deserialization seam: deserialize the stored `toml::Value` for
    /// `part` of blueprint `id` into its registered Rust type. Fails with a
    /// `file + id` diagnostic when the payload is malformed or the part is
    /// unknown to the registry.
    pub fn part_typed(
        &self,
        registry: &PartRegistry,
        id: &str,
        part_name: &str,
    ) -> Result<Box<dyn Part>, BlueprintError> {
        let bp = self
            .blueprints
            .get(id)
            .ok_or_else(|| BlueprintError::NoId { id: id.to_string() })?;
        let value = bp
            .parts
            .get(part_name)
            .ok_or_else(|| BlueprintError::NoPart {
                path: bp.file.clone(),
                id: id.to_string(),
                part: part_name.to_string(),
            })?;
        registry
            .build(&PartTypeId::new(part_name), value)
            .map_err(|e| BlueprintError::BadPart {
                path: bp.file.clone(),
                id: id.to_string(),
                part: part_name.to_string(),
                msg: e.to_string(),
            })
    }
}

/// Human-readable name of a `toml::Value` variant, for diagnostics.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "a string",
        Value::Integer(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Boolean(_) => "a boolean",
        Value::Datetime(_) => "a datetime",
        Value::Array(_) => "an array",
        Value::Table(_) => "a table",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::{PartError, StorageClass};
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
        #[serde(default)]
        strength: u8,
        #[serde(default)]
        agility: u8,
    }

    /// Register `T` under `name` as a `Dynamic` part using a serde factory.
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

    /// A `PartFactory` deserializing `toml::Value` into `T` (captures nothing,
    /// so it is a usable `fn` pointer).
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

    fn creatures() -> MemVfs {
        MemVfs::new().with(
            "blueprints/creatures.toml",
            r##"
[Actor]
parts = {}

[Humanoid]
inherits = "Actor"
parts.Render = { tile = "creature.humanoid" }
parts.Stats = { strength = 10, agility = 10 }

[Goblin]
inherits = "Humanoid"
parts.Render = { tile = "creature.goblin" }
parts.Stats = { strength = 18, agility = 6 }
"##,
        )
    }

    #[test]
    fn inheritance_chain_flattens_parent_to_child() {
        let lib = BlueprintLibrary::load(&creatures(), &registry()).unwrap();

        // Actor defines nothing.
        let actor = lib.get("Actor").unwrap();
        assert!(actor.part("Render").is_none());
        assert!(actor.part("Stats").is_none());
        assert!(classes(&lib).len() == 3, "only flat / no phantom ids");

        // Humanoid gets its own parts.
        let h = lib.part_typed(&registry(), "Humanoid", "Stats").unwrap();
        assert_eq!(
            h.as_ref().as_any().downcast_ref::<StatsPart>(),
            Some(&StatsPart {
                strength: 10,
                agility: 10
            })
        );

        // Goblin inherits Humanoid's parts and overrides both.
        let g_render = lib.part_typed(&registry(), "Goblin", "Render").unwrap();
        assert_eq!(
            g_render.as_ref().as_any().downcast_ref::<RenderPart>(),
            Some(&RenderPart {
                tile: "creature.goblin".into()
            })
        );
        let g_stats = lib.part_typed(&registry(), "Goblin", "Stats").unwrap();
        assert_eq!(
            g_stats.as_ref().as_any().downcast_ref::<StatsPart>(),
            Some(&StatsPart {
                strength: 18,
                agility: 6
            })
        );
    }

    fn classes(lib: &BlueprintLibrary) -> Vec<String> {
        lib.ids().cloned().collect()
    }

    #[test]
    fn override_is_whole_part_not_deep_merge() {
        let r = registry();
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            r##"
[Base]
parts.Render = { tile = "base", fg = "#ffffff" }

[Child]
inherits = "Base"
parts.Render = { tile = "child" }
"##,
        );
        let lib = BlueprintLibrary::load(&v, &r).unwrap();
        let child = lib.get("Child").unwrap();
        // Whole-part replacement: Child's Render is exactly its own table, so the
        // parent's `fg` field must NOT leak in.
        let table = child.part("Render").unwrap().as_table().unwrap();
        assert_eq!(table.len(), 1, "parent fields must not leak in: {table:?}");
        assert!(table.contains_key("tile"));
    }

    #[test]
    fn later_file_redefines_id_last_wins() {
        let r = registry();
        let v = MemVfs::new()
            .with("blueprints/a.toml", "[Goblin]\nparts.Render = { tile = \"old\" }")
            .with(
                "blueprints/b.toml",
                "[Goblin]\ninherits = \"Humanoid\"\nparts.Stats = { strength = 5 }",
            )
            .with(
                "blueprints/c.toml",
                "[Humanoid]\nparts.Render = { tile = \"hum\" }\nparts.Stats = { strength = 1, agility = 2 }",
            );
        let lib = BlueprintLibrary::load(&v, &r).unwrap();
        let goblin = lib.get("Goblin").unwrap();
        // b.toml won: Goblin inherits Humanoid and declares only Stats. Its own
        // Render from a.toml is gone; it now inherits Humanoid's Render ("hum").
        assert_eq!(goblin.inherits.as_deref(), Some("Humanoid"));
        let render = goblin.part("Render").unwrap().as_table().unwrap();
        assert_eq!(render.get("tile").and_then(|v| v.as_str()), Some("hum"));
        let s = lib.part_typed(&r, "Goblin", "Stats").unwrap();
        assert_eq!(
            s.as_ref().as_any().downcast_ref::<StatsPart>(),
            Some(&StatsPart {
                strength: 5,
                agility: 0
            })
        );
    }

    #[test]
    fn ids_are_sorted_deterministically() {
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            "[Zed]\n\n[Alpha]\nparts.Render = { tile = \"a\" }",
        );
        let lib = BlueprintLibrary::load(&v, &registry()).unwrap();
        assert_eq!(classes(&lib), vec!["Alpha".to_string(), "Zed".to_string()]);
    }

    #[test]
    fn unknown_parent_reports_file_and_id() {
        let v = MemVfs::new().with("blueprints/t.toml", "[Goblin]\ninherits = \"Humanoid\"");
        let err = BlueprintLibrary::load(&v, &registry()).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("unknown parent 'Humanoid' for 'Goblin'"),
            "{text}"
        );
        assert!(text.contains("blueprints/t.toml"), "{text}");
    }

    #[test]
    fn cycle_reports_chain_and_both_ends() {
        let v = MemVfs::new()
            .with("blueprints/a.toml", "[B]\ninherits = \"A\"")
            .with("blueprints/b.toml", "[C]\ninherits = \"B\"")
            .with("blueprints/c.toml", "[A]\ninherits = \"C\"");
        let err = BlueprintLibrary::load(&v, &registry()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("inheritance cycle"), "{text}");
        assert!(text.contains("A -> C -> B -> A"), "{text}");
        assert!(text.contains("blueprints/c.toml:A"), "{text}");
    }

    #[test]
    fn bad_part_payload_reports_file_and_id() {
        let r = registry();
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            "[B]\nparts.Stats = { strength = \"banana\" }",
        );
        // Load succeeds (payload text is kept raw); the seam catches the bad part.
        let lib = BlueprintLibrary::load(&v, &r).unwrap();
        let err = match lib.part_typed(&r, "B", "Stats") {
            Ok(_) => panic!("bad payload must error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("strength"), "{err}");
        assert!(err.contains("'B'"), "{err}");
        assert!(err.contains("blueprints/t.toml"), "{err}");
    }

    #[test]
    fn unknown_part_name_is_load_error() {
        let v = MemVfs::new().with("blueprints/t.toml", "[B]\nparts.Throwable = { x = 1 }");
        let err = BlueprintLibrary::load(&v, &registry()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("unknown part 'Throwable'"), "{text}");
        assert!(text.contains("blueprints/t.toml"), "{text}");
    }

    #[test]
    fn reserved_keys_and_schema_are_strict() {
        let r = registry();
        // handlers reserved
        let v = MemVfs::new().with("blueprints/t.toml", "[B]\nhandlers = { OnHit = 1 }");
        let text = BlueprintLibrary::load(&v, &r).unwrap_err().to_string();
        assert!(text.contains("`handlers` is reserved"), "{text}");
        // unknown top-level key
        let v = MemVfs::new().with("blueprints/t.toml", "[B]\npatch = {}");
        let text = BlueprintLibrary::load(&v, &r).unwrap_err().to_string();
        assert!(text.contains("unknown top-level key 'patch'"), "{text}");
        // inherits must be a string
        let v = MemVfs::new().with("blueprints/t.toml", "[B]\ninherits = 42");
        let text = BlueprintLibrary::load(&v, &r).unwrap_err().to_string();
        assert!(text.contains("`inherits` must be a string"), "{text}");
        // part payload must be a table
        let v = MemVfs::new().with("blueprints/t.toml", "[B]\nparts.Stats = 7");
        let text = BlueprintLibrary::load(&v, &r).unwrap_err().to_string();
        assert!(text.contains("payload must be a table"), "{text}");
    }

    #[test]
    fn whole_batch_rejected_and_errors_accumulate() {
        let v = MemVfs::new()
            .with("blueprints/bad1.toml", "[A]\nunknown = 1")
            .with("blueprints/bad2.toml", "[B]\nparts.Zzz = { x = 1 }")
            .with(
                "blueprints/good.toml",
                "[OK]\nparts.Render = { tile = \"x\" }",
            );
        let err = BlueprintLibrary::load(&v, &registry()).unwrap_err();
        assert_eq!(err.diagnostics.len(), 2, "both bad files collected: {err}");
        let text = err.to_string();
        assert!(text.contains("unknown top-level key 'unknown'"), "{text}");
        assert!(text.contains("unknown part 'Zzz'"), "{text}");
    }

    #[test]
    fn resolution_collects_all_unknown_parents() {
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            "[A]\ninherits = \"X\"\n\n[B]\ninherits = \"Y\"",
        );
        let err = BlueprintLibrary::load(&v, &registry()).unwrap_err();
        assert_eq!(
            err.diagnostics.len(),
            2,
            "both unknown parents collected: {err}"
        );
    }

    #[test]
    fn scripts_key_carries_sorted_paths() {
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            "[B]\nscripts = [\"scripts/zap.luau\", \"scripts/aaa.luau\"]",
        );
        let lib = BlueprintLibrary::load(&v, &registry()).unwrap();
        let bp = lib.get("B").unwrap();
        // Deterministic: stored sorted, independent of declaration order.
        assert_eq!(
            bp.script_paths().cloned().collect::<Vec<_>>(),
            vec![
                "scripts/aaa.luau".to_string(),
                "scripts/zap.luau".to_string()
            ]
        );
    }

    #[test]
    fn scripts_key_rejects_non_string_entries() {
        let v = MemVfs::new().with("blueprints/t.toml", "[B]\nscripts = [\"a.luau\", 42]");
        let text = BlueprintLibrary::load(&v, &registry())
            .unwrap_err()
            .to_string();
        assert!(text.contains("`scripts` entries must be strings"), "{text}");
        // whole value not an array
        let v = MemVfs::new().with("blueprints/t.toml", "[B]\nscripts = \"a.luau\"");
        let text = BlueprintLibrary::load(&v, &registry())
            .unwrap_err()
            .to_string();
        assert!(text.contains("`scripts` must be an array"), "{text}");
    }

    #[test]
    fn scripts_inherit_as_whole_set() {
        // Child with no scripts inherits the parent's set; child with scripts
        // replaces it (no union).
        let v = MemVfs::new()
            .with(
                "blueprints/base.toml",
                "[Actor]\nscripts = [\"scripts/shared.luau\"]\n\
                     parts.Render = { tile = \"a\" }",
            )
            .with(
                "blueprints/goblin.toml",
                "[Goblin]\ninherits = \"Actor\"\n\
                     scripts = [\"scripts/goblin.luau\"]",
            );
        let lib = BlueprintLibrary::load(&v, &registry()).unwrap();
        assert_eq!(
            lib.get("Goblin")
                .unwrap()
                .script_paths()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["scripts/goblin.luau".to_string()],
            "child replaces parent's whole set"
        );
        // Inherit-only child keeps parent's set.
        let v = MemVfs::new()
            .with(
                "blueprints/base.toml",
                "[Actor]\nscripts = [\"scripts/shared.luau\"]\n\
                     parts.Render = { tile = \"a\" }",
            )
            .with("blueprints/goblin.toml", "[Goblin]\ninherits = \"Actor\"");
        let lib = BlueprintLibrary::load(&v, &registry()).unwrap();
        assert_eq!(
            lib.get("Goblin")
                .unwrap()
                .script_paths()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["scripts/shared.luau".to_string()],
            "child inherits parent's set when it declares none"
        );
    }

    // --- Kind parts: registration through the loader and naming lint ---

    /// A registry carrying just the six content kind parts (kinds::register_all).
    fn kind_registry() -> PartRegistry {
        let mut r = PartRegistry::new();
        crate::kinds::register_all(&mut r).unwrap();
        r
    }

    /// The smallest data-only blueprint set exercising all six kinds, with
    /// ids that satisfy the naming lint (no scripts, no combat).
    fn content() -> MemVfs {
        MemVfs::new().with(
            "blueprints/kinds.toml",
            r##"
[Ability_ShieldBash]
parts.Ability = { name = "Shield Bash", kind = "active", target = "adjacent",
                  tags = ["melee", "stagger"] }

[Item_Rock]
parts.Item = { name = "Rock", slot = "misc", weight = 1, price = 0 }

[Quest_FirstBlood]
parts.Quest = { name = "First Blood", stages = [ { trigger = "on_kill" } ] }

[Skill_Blade]
parts.Skill = { name = "Blade", family = "weapon" }

[Status_Staggered]
parts.Status = { name = "Staggered", duration = 100, stacking = "refresh",
                 tags = ["harmful"] }

[Tuning_Combat]
parts.Tuning = { base_hit = 50, crit_pct = { pct = 150 }, speed_table = [50, 90, 140] }
"##,
        )
    }

    #[test]
    fn six_kind_parts_load_and_read_back_typed() {
        use crate::kinds::{
            Ability, AbilityKind, AbilityTarget, Item, Quest, Skill, Stacking, Status, Tuning,
            TuningValue,
        };

        let r = kind_registry();
        let lib = BlueprintLibrary::load(&content(), &r).unwrap();
        // Deterministic library ordering: ids sorted across kinds.
        assert_eq!(
            lib.ids().cloned().collect::<Vec<_>>(),
            vec![
                "Ability_ShieldBash".to_string(),
                "Item_Rock".to_string(),
                "Quest_FirstBlood".to_string(),
                "Skill_Blade".to_string(),
                "Status_Staggered".to_string(),
                "Tuning_Combat".to_string(),
            ]
        );

        // Each kind part deserializes through the registry into its typed
        // payload container.
        let a = lib.part_typed(&r, "Ability_ShieldBash", "Ability").unwrap();
        assert_eq!(
            a.as_ref().as_any().downcast_ref::<Ability>(),
            Some(&Ability {
                name: "Shield Bash".into(),
                kind: AbilityKind::Active,
                target: AbilityTarget::Adjacent,
                tags: vec!["melee".into(), "stagger".into()],
            })
        );

        let i = lib.part_typed(&r, "Item_Rock", "Item").unwrap();
        assert_eq!(
            i.as_ref().as_any().downcast_ref::<Item>(),
            Some(&Item {
                name: "Rock".into(),
                slot: "misc".into(),
                weight: 1,
                price: 0,
                tier: "common".into(),
            })
        );

        let q = lib.part_typed(&r, "Quest_FirstBlood", "Quest").unwrap();
        assert_eq!(
            q.as_ref().as_any().downcast_ref::<Quest>(),
            Some(&Quest {
                name: "First Blood".into(),
                stages: vec![crate::kinds::QuestStage {
                    trigger: "on_kill".into()
                }],
            })
        );

        let s = lib.part_typed(&r, "Skill_Blade", "Skill").unwrap();
        assert_eq!(
            s.as_ref().as_any().downcast_ref::<Skill>(),
            Some(&Skill {
                name: "Blade".into(),
                family: "weapon".into(),
                hidden: false,
                rank_cost: vec![],
                grants: vec![],
            })
        );

        let st = lib.part_typed(&r, "Status_Staggered", "Status").unwrap();
        assert_eq!(
            st.as_ref().as_any().downcast_ref::<Status>(),
            Some(&Status {
                name: "Staggered".into(),
                duration: 100,
                stacking: Stacking::Refresh,
                tags: vec!["harmful".into()],
            })
        );

        let t = lib.part_typed(&r, "Tuning_Combat", "Tuning").unwrap();
        let tuning = t.as_ref().as_any().downcast_ref::<Tuning>().unwrap();
        assert_eq!(tuning.values.get("base_hit"), Some(&TuningValue::Int(50)));
        assert_eq!(
            tuning.values.get("crit_pct"),
            Some(&TuningValue::Percent(crate::kinds::Percent { pct: 150 }))
        );
        assert_eq!(
            tuning.values.get("speed_table"),
            Some(&TuningValue::IntList(vec![50, 90, 140]))
        );
    }

    #[test]
    fn kind_part_requires_matching_id_prefix() {
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            r##"
[Goblin]
parts.Ability = { name = "X", kind = "active", target = "adjacent" }
"##,
        );
        let err = BlueprintLibrary::load(&v, &kind_registry()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("parts.Ability"), "{text}");
        assert!(text.contains("'Ability_'"), "{text}");
        assert!(text.contains("'Goblin'"), "{text}");
        assert!(text.contains("blueprints/t.toml"), "{text}");
    }

    #[test]
    fn kind_id_prefix_requires_kind_part() {
        // Load succeeds only with the kind part present: an `Ability_` id with
        // no parts.Ability (own or inherited) is a load error.
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            r##"
[Ability_Solo]

[Tuning_Combat]
parts.Tuning = { base_hit = 1 }
"##,
        );
        let err = BlueprintLibrary::load(&v, &kind_registry()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("'Ability_'"), "{text}");
        assert!(text.contains("'Ability_Solo'"), "{text}");
        assert!(text.contains("parts.Ability"), "{text}");
        assert!(
            !text.contains("'Tuning_Combat'"),
            "Tuning_Combat carries parts.Tuning: {text}"
        );
    }

    #[test]
    fn mismatched_kind_pair_reports_both_contradictions() {
        // `Ability_` id carrying parts.Skill contradicts the convention in
        // both directions; both diagnostics are collected in one load pass.
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            r##"
[Ability_ShieldBash]
parts.Skill = { name = "Blade", family = "weapon" }
"##,
        );
        let err = BlueprintLibrary::load(&v, &kind_registry()).unwrap_err();
        assert_eq!(err.diagnostics.len(), 2, "both contradictions: {err}");
        let text = err.to_string();
        assert!(text.contains("parts.Ability"), "{text}");
        assert!(text.contains("parts.Skill"), "{text}");
        assert!(text.contains("'Ability_ShieldBash'"), "{text}");
    }

    #[test]
    fn abstract_root_ids_are_exempt_from_the_lint() {
        // Base ships the bare abstract roots; they may (or may not) carry
        // their own kind part with full defaults and must load either way.
        let v = MemVfs::new().with(
            "blueprints/roots.toml",
            r##"
[Ability]
parts.Ability = { name = "Ability", kind = "active", target = "user" }

[Item]

[Skill]
parts.Skill = { name = "Skill", family = "general" }

[Tuning]
parts.Tuning = { }
"##,
        );
        let lib = BlueprintLibrary::load(&v, &kind_registry()).unwrap();
        assert_eq!(
            lib.ids().cloned().collect::<Vec<_>>(),
            vec![
                "Ability".to_string(),
                "Item".to_string(),
                "Skill".to_string(),
                "Tuning".to_string()
            ]
        );
    }

    #[test]
    fn inherited_kind_part_satisfies_the_lint() {
        // The lint runs on the resolved set, so a child whose kind part comes
        // from inheritance passes without restating it...
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            r##"
[Ability]
parts.Ability = { name = "Ability", kind = "active", target = "user" }

[Ability_ShieldBash]
inherits = "Ability"
"##,
        );
        let r = kind_registry();
        let lib = BlueprintLibrary::load(&v, &r).unwrap();
        let a = lib.part_typed(&r, "Ability_ShieldBash", "Ability").unwrap();
        assert_eq!(
            a.as_ref()
                .as_any()
                .downcast_ref::<crate::kinds::Ability>()
                .unwrap()
                .name,
            "Ability",
            "inherited kind part resolves whole-part from the root"
        );

        // ...and a child restating the full payload replaces the whole part
        // (whole-part replacement: no field-level merge leaks parent fields).
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            r##"
[Ability]
parts.Ability = { name = "Ability", kind = "active", target = "user", tags = ["base"] }

[Ability_ShieldBash]
inherits = "Ability"
parts.Ability = { name = "Shield Bash", kind = "active", target = "adjacent" }
"##,
        );
        let r = kind_registry();
        let lib = BlueprintLibrary::load(&v, &r).unwrap();
        let a = lib.part_typed(&r, "Ability_ShieldBash", "Ability").unwrap();
        let ability = a
            .as_ref()
            .as_any()
            .downcast_ref::<crate::kinds::Ability>()
            .unwrap();
        assert_eq!(ability.name, "Shield Bash");
        assert!(
            ability.tags.is_empty(),
            "whole-part replacement: {ability:?}"
        );
    }

    #[test]
    fn kind_payload_errors_report_file_id_and_field() {
        let r = kind_registry();
        // Unknown field inside a kind payload.
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            r##"
[Ability_ShieldBash]
parts.Ability = { name = "Shield Bash", kind = "active", target = "adjacent", zap = 1 }
"##,
        );
        let lib = BlueprintLibrary::load(&v, &r).unwrap();
        let err = match lib.part_typed(&r, "Ability_ShieldBash", "Ability") {
            Ok(_) => panic!("unknown field must fail typed deserialization"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("zap"), "{err}");
        assert!(err.contains("'Ability_ShieldBash'"), "{err}");
        assert!(err.contains("blueprints/t.toml"), "{err}");

        // Bad enum value inside a kind payload.
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            r##"
[Status_Staggered]
parts.Status = { name = "Staggered", duration = 100, stacking = "expire" }
"##,
        );
        let lib = BlueprintLibrary::load(&v, &r).unwrap();
        let err = match lib.part_typed(&r, "Status_Staggered", "Status") {
            Ok(_) => panic!("bad stacking value must fail typed deserialization"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("expire"), "{err}");
        assert!(err.contains("refresh"), "{err}");
        assert!(err.contains("'Status_Staggered'"), "{err}");
    }

    #[test]
    fn lint_diagnostics_are_deterministic_in_sorted_id_order() {
        // Both ids violate rule 1; declared in reverse order, the errors must
        // still surface sorted by id (BTreeMap ids + sorted resolution).
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            r##"
[Skill_ZZZ]

[Ability_AAA]
"##,
        );
        let err = BlueprintLibrary::load(&v, &kind_registry()).unwrap_err();
        assert_eq!(err.diagnostics.len(), 2, "{err}");
        let text = err.to_string();
        let ability_pos = text
            .find("'Ability_AAA'")
            .expect("first diag sorted: {text}");
        let skill_pos = text
            .find("'Skill_ZZZ'")
            .expect("second diag sorted: {text}");
        assert!(ability_pos < skill_pos, "{text}");
    }

    #[test]
    fn kind_parts_patch_with_no_special_cases() {
        // Patch ops act on raw payloads before typed deserialization, so
        // kind parts patch exactly like any other part.
        let r = kind_registry();
        let mut lib = BlueprintLibrary::load(&content(), &r).unwrap();

        // merge a scalar into a tuning table
        let pf = PatchFile::parse(
            "patches/t.toml",
            r##"
["Tuning_Combat"]
merge = { "parts.Tuning.base_hit" = 60 }
"##,
        )
        .unwrap();
        lib.apply_patch_file(&pf).unwrap();
        let t = lib.part_typed(&r, "Tuning_Combat", "Tuning").unwrap();
        assert_eq!(
            t.as_ref()
                .as_any()
                .downcast_ref::<crate::kinds::Tuning>()
                .unwrap()
                .values
                .get("base_hit"),
            Some(&crate::kinds::TuningValue::Int(60)),
            "patched value wins; siblings (crit_pct, speed_table) untouched"
        );

        // remove a whole kind part
        let pf = PatchFile::parse(
            "patches/t.toml",
            r##"
["Item_Rock"]
remove = ["parts.Item"]
"##,
        )
        .unwrap();
        lib.apply_patch_file(&pf).unwrap();
        assert!(lib.get("Item_Rock").unwrap().part("Item").is_none());
    }
}
