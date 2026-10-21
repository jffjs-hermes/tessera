//! Post-load tuning: typed tables collected from the loaded blueprint
//! library plus a validated override overlay.
//!
//! A tuning table is a blueprint carrying `parts.Tuning` (typed in
//! [`crate::kinds::Tuning`]). [`TuningStore::from_library`] collects every
//! such table from a resolved library, so inheritance and patch behavior are
//! exactly the loader's own. Overrides are *not* content: they are applied
//! through [`TuningStore::apply_override`] as a final validation-checked
//! overlay — an override naming an unknown table/key is an error, an override
//! with a wrong value type is an error, and applying an override never
//! mutates the authored tables. The applied-override list is recorded in
//! application order so a run's numbers are reproducible from its own output.
//!
//! Determinism (see `docs/determinism.md`): every container here is a
//! `BTreeMap` (sorted key iteration) or a `Vec` in application order. There is
//! no `HashMap`/`HashSet` anywhere in this module.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::blueprints::{BlueprintError, BlueprintLibrary};
use crate::ecs::PartRegistry;
use crate::kinds::{Tuning, TuningValue};

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// Authored tuning tables plus the validated override overlay. Built once
/// from a loaded library; `apply_override` mutates only the overlay and the
/// applied record, never the authored tables.
#[derive(Debug, Default)]
pub struct TuningStore {
    /// Authored values, table id -> key -> value. Sorted (BTreeMap).
    tables: BTreeMap<String, BTreeMap<String, TuningValue>>,
    /// Applied overrides in application order (run-output record).
    applied: Vec<TuningOverride>,
    /// The overlay: (table id, key) -> override value.
    overlay: BTreeMap<(String, String), TuningValue>,
}

/// One applied override, recorded in application order so run output can
/// reproduce the exact numbers used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuningOverride {
    pub table: String,
    pub key: String,
    pub value: TuningValue,
}

/// A failure in override application. Every case is fail-closed: an override
/// that names an unknown table/key or carries a wrong value type is rejected
/// before it can affect simulation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TuningError {
    #[error("unknown tuning table '{0}'")]
    UnknownTable(String),
    #[error("unknown tuning key '{key}' in tuning table '{table}'")]
    UnknownKey { table: String, key: String },
    #[error("override for '{table}.{key}' has wrong type: authored is {authored}, got {got}")]
    WrongType {
        table: String,
        key: String,
        authored: &'static str,
        got: String,
    },
}

impl TuningStore {
    /// Collect every tuning table from a resolved blueprint library: each
    /// blueprint carrying a `Tuning` part is deserialized through the same
    /// `part_typed` seam used elsewhere, so a table with a wrong-typed or
    /// unknown-field payload is a load error (`file + id` diagnostic) before
    /// any override — or any simulation — can run.
    pub fn from_library(
        library: &BlueprintLibrary,
        registry: &PartRegistry,
    ) -> Result<Self, BlueprintError> {
        let mut tables = BTreeMap::new();
        for id in library.ids() {
            let Some(bp) = library.get(id.as_str()) else {
                continue;
            };
            if !bp.part_names().any(|p| p.as_str() == "Tuning") {
                continue;
            }
            let part = library.part_typed(registry, id.as_str(), "Tuning")?;
            let tuning = part
                .as_ref()
                .as_any()
                .downcast_ref::<Tuning>()
                .ok_or_else(|| BlueprintError::BadPart {
                    path: bp.file.clone(),
                    id: id.to_string(),
                    part: "Tuning".into(),
                    msg: "Tuning factory produced a non-Tuning value".to_string(),
                })?;
            tables.insert(id.to_string(), tuning.values.clone());
        }
        Ok(TuningStore {
            tables,
            applied: Vec::new(),
            overlay: BTreeMap::new(),
        })
    }

    /// Sorted table ids (the blueprints carrying `parts.Tuning`).
    pub fn tables(&self) -> impl Iterator<Item = &String> {
        self.tables.keys()
    }

    /// The authored value of one key (before any override), or `None`.
    pub fn authored(&self, table: &str, key: &str) -> Option<&TuningValue> {
        self.tables.get(table)?.get(key)
    }

    /// Whether a table exists (authored).
    pub fn contains_table(&self, table: &str) -> bool {
        self.tables.contains_key(table)
    }

    /// Merged values of one table (authored + overlay), sorted by key, or
    /// `None` when no such table is loaded. Owned copy — the value boundary
    /// never borrows the store into a caller.
    pub fn values(&self, table: &str) -> Option<BTreeMap<String, TuningValue>> {
        let authored = self.tables.get(table)?;
        let mut merged = authored.clone();
        for ((t, k), v) in &self.overlay {
            if t == table {
                merged.insert(k.clone(), v.clone());
            }
        }
        Some(merged)
    }

    /// Apply one override triple with full validation: the table and key must
    /// exist in the authored content, and the value must deserialize into a
    /// `TuningValue` *and* keep the authored value's shape (integer,
    /// percent-of-integer, integer-list). On success the override is recorded
    /// in application order; authored tables are never touched.
    pub fn apply_override(
        &mut self,
        table: &str,
        key: &str,
        value: &toml::Value,
    ) -> Result<(), TuningError> {
        let authored = self
            .tables
            .get(table)
            .ok_or_else(|| TuningError::UnknownTable(table.to_string()))?
            .get(key)
            .ok_or_else(|| TuningError::UnknownKey {
                table: table.to_string(),
                key: key.to_string(),
            })?;

        let parsed: TuningValue =
            value
                .clone()
                .try_into::<TuningValue>()
                .map_err(|e: toml::de::Error| TuningError::WrongType {
                    table: table.to_string(),
                    key: key.to_string(),
                    authored: variant_name(authored),
                    got: format!("{e}"),
                })?;

        if variant_name(&parsed) != variant_name(authored) {
            return Err(TuningError::WrongType {
                table: table.to_string(),
                key: key.to_string(),
                authored: variant_name(authored),
                got: variant_name(&parsed).to_string(),
            });
        }

        self.overlay
            .insert((table.to_string(), key.to_string()), parsed.clone());
        self.applied.push(TuningOverride {
            table: table.to_string(),
            key: key.to_string(),
            value: parsed,
        });
        Ok(())
    }

    /// The applied overrides in application order (run-output record). Empty
    /// when nothing was overridden.
    pub fn applied_overrides(&self) -> &[TuningOverride] {
        &self.applied
    }
}

/// Stable name of a tuning value's shape, for diagnostics.
fn variant_name(v: &TuningValue) -> &'static str {
    match v {
        TuningValue::Int(_) => "integer",
        TuningValue::Percent(_) => "percent-of-integer",
        TuningValue::IntList(_) => "integer-list",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::kinds::register_all;
    use crate::vfs::MemVfs;

    fn registry() -> PartRegistry {
        let mut r = PartRegistry::new();
        register_all(&mut r).unwrap();
        r
    }

    fn store(text: &str) -> Result<TuningStore, BlueprintError> {
        let vfs = MemVfs::new().with("blueprints/tuning.toml", text);
        let lib = BlueprintLibrary::load(&vfs, &registry()).unwrap();
        TuningStore::from_library(&lib, &registry())
    }

    /// Parse a raw TOML scalar/inline value (anchored under a dummy key, the
    /// same way authored payload values reach the store).
    fn v(text: &str) -> toml::Value {
        let anchored = format!("payload = {text}");
        let doc: toml::Value = toml::from_str(&anchored).unwrap();
        doc.get("payload").unwrap().clone()
    }

    const COMBAT: &str = r##"
[Tuning_Combat]
parts.Tuning = { base_hit = 50, ac_factor = 2, crit_percent = { pct = 150 },
                 speed_table = [50, 90, 140] }
"##;

    #[test]
    fn collects_every_tuning_table_sorted() {
        let s = store(COMBAT).unwrap();
        let names: Vec<String> = s.tables().map(|t| t.to_string()).collect();
        assert_eq!(names, vec!["Tuning_Combat"]);
        let values = s.values("Tuning_Combat").unwrap();
        // Sorted by key regardless of authored order.
        let keys: Vec<String> = values.keys().map(|k| k.to_string()).collect();
        assert_eq!(
            keys,
            vec!["ac_factor", "base_hit", "crit_percent", "speed_table"]
        );
        assert_eq!(values.get("base_hit"), Some(&TuningValue::Int(50)));
        assert_eq!(
            values.get("crit_percent"),
            Some(&TuningValue::Percent(crate::kinds::Percent { pct: 150 }))
        );
        // Non-tuning blueprints are not tuning tables.
        assert!(!s.contains_table("Tuning_Combat_Other"));
        assert!(s.values("Tuning_Combat_Other").is_none());
    }

    #[test]
    fn malformed_authored_payload_is_a_load_error() {
        // Wrong-typed authored value: fails at collection, i.e. before any
        // override or simulation can run.
        let err = store("[Tuning_Combat]\nparts.Tuning = { base_hit = 50.5 }\n").unwrap_err();
        assert!(err.to_string().contains("Tuning_Combat"), "{err}");
        assert!(err.to_string().contains("Tuning"), "{err}");
        // Unknown part payload shape.
        let err = store("[Tuning_Combat]\nparts.Tuning = { base_hit = { n = 1 } }\n").unwrap_err();
        assert!(err.to_string().contains("Tuning_Combat"), "{err}");
    }

    #[test]
    fn override_success_updates_reads_and_records() {
        let mut s = store(COMBAT).unwrap();
        s.apply_override("Tuning_Combat", "base_hit", &v("60"))
            .unwrap();
        s.apply_override("Tuning_Combat", "crit_percent", &v("{ pct = 175 }"))
            .unwrap();
        s.apply_override("Tuning_Combat", "speed_table", &v("[1, 2, 3]"))
            .unwrap();

        let values = s.values("Tuning_Combat").unwrap();
        assert_eq!(values.get("base_hit"), Some(&TuningValue::Int(60)));
        assert_eq!(
            values.get("crit_percent"),
            Some(&TuningValue::Percent(crate::kinds::Percent { pct: 175 }))
        );
        assert_eq!(
            values.get("speed_table"),
            Some(&TuningValue::IntList(vec![1, 2, 3]))
        );
        // Non-overridden keys keep authored values.
        assert_eq!(values.get("ac_factor"), Some(&TuningValue::Int(2)));

        // Recorded in application order, with the exact applied values.
        let applied = s.applied_overrides();
        assert_eq!(applied.len(), 3);
        assert_eq!(applied[0].key, "base_hit");
        assert_eq!(applied[0].value, TuningValue::Int(60));
        assert_eq!(applied[2].value, TuningValue::IntList(vec![1, 2, 3]));
    }

    #[test]
    fn override_unknown_table_and_key_fail_closed() {
        let mut s = store(COMBAT).unwrap();
        let err = s
            .apply_override("Tuning_Nope", "base_hit", &v("1"))
            .unwrap_err();
        assert!(
            matches!(&err, TuningError::UnknownTable(t) if t.as_str() == "Tuning_Nope"),
            "{err:?}"
        );
        let err = s
            .apply_override("Tuning_Combat", "bogus", &v("1"))
            .unwrap_err();
        assert!(
            matches!(
                &err,
                TuningError::UnknownKey { table, key, .. }
                    if table.as_str() == "Tuning_Combat" && key.as_str() == "bogus"
            ),
            "{err:?}"
        );
    }

    #[test]
    fn override_wrong_type_fails_closed() {
        let mut s = store(COMBAT).unwrap();
        // Float is not a valid tuning value at all.
        let err = s
            .apply_override("Tuning_Combat", "base_hit", &v("60.5"))
            .unwrap_err();
        assert!(
            matches!(&err, TuningError::WrongType { authored, .. } if *authored == "integer"),
            "{err:?}"
        );
        // Shape mismatch: integer override on a percent key.
        let err = s
            .apply_override("Tuning_Combat", "crit_percent", &v("175"))
            .unwrap_err();
        assert!(
            matches!(
                &err,
                TuningError::WrongType { authored, got, .. }
                    if *authored == "percent-of-integer" && got.as_str() == "integer"
            ),
            "{err:?}"
        );
        // Shape mismatch: list override on an integer key.
        let err = s
            .apply_override("Tuning_Combat", "base_hit", &v("[1]"))
            .unwrap_err();
        assert!(
            matches!(&err, TuningError::WrongType { authored, .. } if *authored == "integer"),
            "{err:?}"
        );
        // Nothing was recorded by the failed overrides.
        assert!(s.applied_overrides().is_empty());
    }

    #[test]
    fn overrides_never_mutate_authored_content() {
        let mut s = store(COMBAT).unwrap();
        s.apply_override("Tuning_Combat", "base_hit", &v("60"))
            .unwrap();
        // The authored map is untouched.
        assert_eq!(
            s.authored("Tuning_Combat", "base_hit"),
            Some(&TuningValue::Int(50))
        );
        // A store rebuilt from the same library sees authored values again.
        let vfs = MemVfs::new().with("blueprints/tuning.toml", COMBAT);
        let lib = BlueprintLibrary::load(&vfs, &registry()).unwrap();
        let fresh = TuningStore::from_library(&lib, &registry()).unwrap();
        assert_eq!(
            fresh.authored("Tuning_Combat", "base_hit"),
            Some(&TuningValue::Int(50))
        );
        assert!(fresh.applied_overrides().is_empty());
    }

    #[test]
    fn values_iterate_in_sorted_key_order() {
        let s = store(
            r##"
[Tuning_Combat]
parts.Tuning = { zeta = 1, alpha = 2, Mango = 3, banana = 4 }
"##,
        )
        .unwrap();
        let keys: Vec<String> = s
            .values("Tuning_Combat")
            .unwrap()
            .keys()
            .map(|k| k.to_string())
            .collect();
        // Byte order: uppercase sorts before lowercase.
        assert_eq!(keys, vec!["Mango", "alpha", "banana", "zeta"]);
        let tbls: Vec<String> = s.tables().map(|t| t.to_string()).collect();
        assert_eq!(tbls, vec!["Tuning_Combat"]);
    }
}
