//! Deterministic mod load order: dependency validation and a canonical
//! topological sort over the discovered mod set.
//!
//! The output [`ModRegistry`] is a pure function of the mod set and its
//! manifests: the load order is computed by Kahn's algorithm over a
//! `BTreeMap<ModId, BTreeSet<ModId>>` edge set with a lexicographic tie-break,
//! so two machines with the same mods produce the same order regardless of
//! discovery or platform enumeration order. No `HashMap`, no `HashSet`
//! (enforced by this crate's `clippy.toml`).
//!
//! All diagnostics are collected across the pass and returned together, then
//! the load is rejected wholesale (fail-closed).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::discovery::ModSource;
use crate::error::ManifestError;
use crate::manifest::{BASE_MOD_ID, ModId, ModManifest, parse_manifest};

/// A single load-order failure. Every variant carries the mod ids and source
/// paths a mod author needs to locate the defect (the engine's `file + id`
/// diagnostic convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOrderError {
    /// A cycle in the (hard or active soft) dependency graph. `path` is the
    /// full cycle, e.g. `[a, b, c]` rendering as `a -> b -> c -> a`;
    /// `soft_edges` lists the edges in the cycle that come from
    /// `load-order.after` / `load-order.before` hints, with the hint source
    /// (`"after"` or `"before"`), so a soft-edge cycle is diagnosed as such.
    Cycle {
        path: Vec<ModId>,
        soft_edges: Vec<(ModId, ModId, &'static str)>,
    },
    /// A required (non-optional) dependency is absent from the mod set.
    MissingDependency { mod_id: ModId, dep: ModId },
    /// A present dependency's version does not satisfy the declared
    /// requirement. `found` is the dependency's actual version string.
    VersionConflict {
        mod_id: ModId,
        dep: ModId,
        required: String,
        found: Option<String>,
    },
    /// Two sources claim the same mod id; both source paths are reported.
    DuplicateId {
        id: ModId,
        first_path: String,
        second_path: String,
    },
    /// A mod source's manifest could not be read or parsed.
    InvalidManifest { path: String, msg: String },
}

impl fmt::Display for LoadOrderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadOrderError::Cycle { path, soft_edges } => {
                write!(f, "mod cycle: ")?;
                for (i, id) in path.iter().enumerate() {
                    if i > 0 {
                        write!(f, " -> ")?;
                    }
                    write!(f, "{id}")?;
                }
                if let Some(first) = path.first() {
                    write!(f, " -> {first}")?;
                }
                for (a, b, kind) in soft_edges {
                    write!(f, " (soft edge {a} -> {b} from load-order.{kind})")?;
                }
                Ok(())
            }
            LoadOrderError::MissingDependency { mod_id, dep } => {
                write!(f, "mod '{mod_id}' depends on missing mod '{dep}'")
            }
            LoadOrderError::VersionConflict {
                mod_id,
                dep,
                required,
                found,
            } => {
                write!(f, "mod '{mod_id}' requires '{dep}' {required}")?;
                match found {
                    Some(v) => write!(f, ", found '{v}'"),
                    None => write!(f, ", not found"),
                }
            }
            LoadOrderError::DuplicateId {
                id,
                first_path,
                second_path,
            } => write!(
                f,
                "duplicate mod id '{id}': both '{first_path}' and '{second_path}' claim it"
            ),
            LoadOrderError::InvalidManifest { path, msg } => {
                write!(f, "invalid manifest '{path}': {msg}")
            }
        }
    }
}

impl std::error::Error for LoadOrderError {}

/// A single mod loaded into the registry: its manifest plus the source it
/// came from (kept so downstream content loading can read through the same
/// `Vfs`).
#[derive(Debug)]
pub struct LoadedMod {
    pub manifest: ModManifest,
    pub source: ModSource,
}

/// The validated, deterministically ordered mod set. `order` lists every mod
/// id that must load, `base` first, then each discovered mod in canonical
/// topological order (base is placed first by construction).
#[derive(Debug)]
pub struct ModRegistry {
    /// Deterministic load order; always begins with [`BASE_MOD_ID`].
    pub order: Vec<ModId>,
    /// id -> loaded mod, for every discovered (non-base) mod.
    pub mods: BTreeMap<ModId, LoadedMod>,
}

impl ModRegistry {
    /// Build the registry from discovered sources.
    ///
    /// Reads and parses each source's `mod.toml` through the `Vfs` interface,
    /// then validates hard/optional dependencies and active soft
    /// `load-order` edges, and finally computes the canonical order. All
    /// errors are collected and returned together; any error rejects the
    /// whole load.
    ///
    /// `sources` may arrive in any order (discovery is unordered by design);
    /// the resulting order is identical regardless.
    pub fn build(sources: Vec<ModSource>) -> Result<ModRegistry, Vec<LoadOrderError>> {
        // ---- 1. Parse every manifest; collect invalid manifests + dup ids.
        // Source order is irrelevant here: manifests are keyed by ModId in a
        // BTreeMap, and duplicate detection is by id, not position.
        let mut errors: Vec<LoadOrderError> = Vec::new();
        let mut mods: BTreeMap<ModId, LoadedMod> = BTreeMap::new();

        for source in sources {
            let text = match source.vfs.read_to_string("mod.toml") {
                Ok(t) => t,
                Err(e) => {
                    errors.push(LoadOrderError::InvalidManifest {
                        path: source.path.clone(),
                        msg: format!("reading mod.toml: {e}"),
                    });
                    continue;
                }
            };
            let manifest = match parse_manifest(&source.path, &text) {
                Ok(m) => m,
                Err(ManifestError::Invalid { path, id: _, msg }) => {
                    errors.push(LoadOrderError::InvalidManifest { path, msg });
                    continue;
                }
                Err(ManifestError::Io { path, source }) => {
                    errors.push(LoadOrderError::InvalidManifest {
                        path,
                        msg: source.to_string(),
                    });
                    continue;
                }
                Err(ManifestError::Parse { path, source }) => {
                    errors.push(LoadOrderError::InvalidManifest {
                        path,
                        msg: source.to_string(),
                    });
                    continue;
                }
            };

            // Duplicate id: keep the first (by iteration order of `sources`),
            // record the second path for the diagnostic. Both paths are
            // reported regardless of which is "first".
            if let Some(prev) = mods.get(&manifest.id) {
                errors.push(LoadOrderError::DuplicateId {
                    id: manifest.id.clone(),
                    first_path: prev.manifest.path.clone(),
                    second_path: manifest.path.clone(),
                });
                continue;
            }
            mods.insert(manifest.id.clone(), LoadedMod { manifest, source });
        }

        // ---- 2. Build edges and check dependency presence/version.
        // Hard edges from every present hard dependency; active soft edges
        // from load-order.after / inverted load-order.before, added only when
        // the referenced mod is present (absent hints are silently inert).
        // `edges[from]` = set of mods that must load *after* `from`.
        let mut edges: BTreeMap<ModId, BTreeSet<ModId>> = BTreeMap::new();
        let mut soft_edges: BTreeMap<(ModId, ModId), &'static str> = BTreeMap::new();

        for (id, loaded) in &mods {
            let manifest = &loaded.manifest;
            // Hard + optional dependencies.
            for (dep, req) in &manifest.dependencies {
                match mods.get(dep) {
                    Some(dep_loaded) => {
                        if !req.version.matches(&dep_loaded.manifest.version) {
                            errors.push(LoadOrderError::VersionConflict {
                                mod_id: id.clone(),
                                dep: dep.clone(),
                                required: req.version.to_string(),
                                found: Some(dep_loaded.manifest.version.to_string()),
                            });
                        } else {
                            edges.entry(dep.clone()).or_default().insert(id.clone());
                        }
                    }
                    None => {
                        // Optional dependency absent -> skipped silently.
                        if !req.optional {
                            errors.push(LoadOrderError::MissingDependency {
                                mod_id: id.clone(),
                                dep: dep.clone(),
                            });
                        }
                    }
                }
            }
            // Soft after: `id` loads after `other` when `other` is present.
            for other in &manifest.load_order.after {
                if mods.contains_key(other) {
                    edges.entry(other.clone()).or_default().insert(id.clone());
                    soft_edges.insert((other.clone(), id.clone()), "after");
                }
            }
            // Soft before: `id` loads before `other` when `other` is present.
            for other in &manifest.load_order.before {
                if mods.contains_key(other) {
                    edges.entry(id.clone()).or_default().insert(other.clone());
                    soft_edges.insert((id.clone(), other.clone()), "before");
                }
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        // ---- 3. Kahn's topological sort with lexicographic tie-break.
        let order = match topo_sort(&mods, &edges, &soft_edges) {
            Ok(order) => order,
            Err(e) => return Err(vec![e]),
        };

        // ---- 4. Base first by construction.
        let mut full_order = Vec::with_capacity(order.len() + 1);
        full_order.push(ModId::from_valid(BASE_MOD_ID));
        full_order.extend(order);

        Ok(ModRegistry {
            order: full_order,
            mods,
        })
    }
}

/// Kahn's algorithm over a `BTreeMap` edge set, repeatedly taking the
/// lexicographically smallest ready mod id. Returns the canonical order of
/// the discovered mods, or a [`LoadOrderError::Cycle`] with the full cycle
/// path when the graph is cyclic.
fn topo_sort(
    mods: &BTreeMap<ModId, LoadedMod>,
    edges: &BTreeMap<ModId, BTreeSet<ModId>>,
    soft_edges: &BTreeMap<(ModId, ModId), &'static str>,
) -> Result<Vec<ModId>, LoadOrderError> {
    // In-degree: number of present prerequisites.
    let mut in_degree: BTreeMap<ModId, usize> = BTreeMap::new();
    for id in mods.keys() {
        in_degree.insert(id.clone(), 0);
    }
    for (from, tos) in edges {
        if !mods.contains_key(from) {
            continue;
        }
        for to in tos {
            if mods.contains_key(to) {
                *in_degree.get_mut(to).expect("key present") += 1;
            }
        }
    }

    // Ready set, sorted so pop_first gives the lexicographically smallest id.
    let mut ready: BTreeSet<ModId> = in_degree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| id.clone())
        .collect();

    let mut order = Vec::with_capacity(mods.len());
    while let Some(id) = ready.pop_first() {
        order.push(id.clone());
        if let Some(tos) = edges.get(&id) {
            for to in tos {
                let d = in_degree.get_mut(to).expect("key present");
                *d -= 1;
                if *d == 0 {
                    ready.insert(to.clone());
                }
            }
        }
    }

    if order.len() < mods.len() {
        return Err(cycle_error(mods, edges, soft_edges, &order));
    }
    Ok(order)
}

/// Build a full-cycle diagnostic from the set of mods Kahn could not place.
///
/// The unplaced mods form a subgraph where every node has an in-edge from
/// another unplaced node (a node with in-degree 0 would have been placed), so
/// at least one cycle exists. To report one deterministically and with only
/// real edges, we first peel away nodes with no successor inside the unplaced
/// subgraph (reverse topological pruning): cycle nodes are never peeled, and
/// everything that remains has at least one successor in the remaining set.
/// We then walk from the lexicographically smallest remaining id, always
/// stepping to the lexicographically smallest remaining successor, until a
/// node repeats — the segment between repeats is a genuine cycle. Soft edges
/// on the cycle are named in the message.
fn cycle_error(
    mods: &BTreeMap<ModId, LoadedMod>,
    edges: &BTreeMap<ModId, BTreeSet<ModId>>,
    soft_edges: &BTreeMap<(ModId, ModId), &'static str>,
    placed: &[ModId],
) -> LoadOrderError {
    let placed: BTreeSet<ModId> = placed.iter().cloned().collect();
    let mut remaining: BTreeSet<ModId> = mods
        .keys()
        .filter(|k| !placed.contains(k))
        .cloned()
        .collect();

    // Peel nodes with no successor inside `remaining`. A node whose every
    // successor is placed (or that has none) cannot be on a cycle; removing
    // it may expose more such nodes.
    loop {
        let sinks: Vec<ModId> = remaining
            .iter()
            .filter(|n| {
                edges
                    .get(*n)
                    .is_none_or(|tos| !tos.iter().any(|t| remaining.contains(t)))
            })
            .cloned()
            .collect();
        if sinks.is_empty() {
            break;
        }
        for s in sinks {
            remaining.remove(&s);
        }
    }
    assert!(
        !remaining.is_empty(),
        "Kahn stalled but the unplaced subgraph had no cycle"
    );

    // Walk: every remaining node has a successor in `remaining`, so following
    // the smallest successor from the smallest remaining id must revisit a
    // node; the visited segment between the repeats is the cycle.
    let start = remaining.iter().next().expect("non-empty").clone();
    let mut path: Vec<ModId> = Vec::new();
    let mut seen: BTreeMap<ModId, usize> = BTreeMap::new();
    let mut cur = start;
    loop {
        if let Some(&idx) = seen.get(&cur) {
            path = path.split_off(idx);
            break;
        }
        seen.insert(cur.clone(), path.len());
        path.push(cur.clone());
        cur = edges
            .get(&cur)
            .and_then(|tos| tos.iter().filter(|t| remaining.contains(t)).min())
            .expect("every remaining node has a remaining successor")
            .clone();
    }

    // Name the soft edges on the cycle, with the hint source ("after"/"before").
    let soft: Vec<(ModId, ModId, &'static str)> = path
        .iter()
        .zip(path.iter().skip(1).chain(path.first()))
        .filter_map(|(a, b)| {
            let key = ((*a).clone(), (*b).clone());
            soft_edges
                .get(&key)
                .map(|kind| ((*a).clone(), (*b).clone(), *kind))
        })
        .collect();

    LoadOrderError::Cycle {
        path,
        soft_edges: soft,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover_dir;
    use tessera_core::vfs::MemVfs;

    fn source(path: &str, toml: &str) -> ModSource {
        ModSource::from_vfs(path, Box::new(MemVfs::new().with("mod.toml", toml)))
    }

    /// Order string with base stripped, for readability.
    fn mod_order(reg: &ModRegistry) -> Vec<String> {
        reg.order
            .iter()
            .skip(1) // base
            .map(|i| i.as_str().to_string())
            .collect()
    }

    fn assert_err(r: Result<ModRegistry, Vec<LoadOrderError>>) -> Vec<LoadOrderError> {
        match r {
            Ok(_) => panic!("expected load error, got Ok"),
            Err(e) => e,
        }
    }

    #[test]
    fn base_is_first_and_isolated_mods_sort_lexicographically() {
        let reg = ModRegistry::build(vec![
            source("p/zeta", "id = \"zeta\"\nversion = \"1.0.0\"\napi = 1\n"),
            source("p/alpha", "id = \"alpha\"\nversion = \"1.0.0\"\napi = 1\n"),
            source("p/mike", "id = \"mike\"\nversion = \"1.0.0\"\napi = 1\n"),
        ])
        .unwrap();
        assert_eq!(reg.order[0].as_str(), BASE_MOD_ID);
        assert_eq!(mod_order(&reg), vec!["alpha", "mike", "zeta"]);
    }

    #[test]
    fn shuffled_discovery_yields_identical_order() {
        // Rebuild sources per permutation (ModSource owns its Vfs and is not
        // Clone), then assert the resulting order is identical regardless of
        // how the same mod set is presented to the registry.
        fn make_a() -> ModSource {
            source(
                "a",
                "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nb = \"1.0.0\"\n",
            )
        }
        fn make_b() -> ModSource {
            source(
                "b",
                "id = \"b\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nc = \"1.0.0\"\n",
            )
        }
        fn make_c() -> ModSource {
            source("c", "id = \"c\"\nversion = \"1.0.0\"\napi = 1\n")
        }

        // Same mod set, three different discovery orders.
        let r1 = ModRegistry::build(vec![make_a(), make_b(), make_c()]).unwrap();
        let r2 = ModRegistry::build(vec![make_c(), make_a(), make_b()]).unwrap();
        let r3 = ModRegistry::build(vec![make_b(), make_c(), make_a()]).unwrap();
        assert_eq!(mod_order(&r1), mod_order(&r2));
        assert_eq!(mod_order(&r2), mod_order(&r3));
        // c (no deps) is placed first among the mods; then b; then a.
        assert_eq!(mod_order(&r1), vec!["c", "b", "a"]);
    }

    #[test]
    fn lexicographic_tie_break_is_deterministic() {
        // Independent mods with no edges: order is purely lexical.
        let reg = ModRegistry::build(vec![
            source("p/zz", "id = \"zz\"\nversion = \"1.0.0\"\napi = 1\n"),
            source("p/aa", "id = \"aa\"\nversion = \"1.0.0\"\napi = 1\n"),
        ])
        .unwrap();
        assert_eq!(mod_order(&reg), vec!["aa", "zz"]);
    }

    #[test]
    fn hard_missing_dependency_is_an_error() {
        let errs = assert_err(ModRegistry::build(vec![source(
            "p/one",
            "id = \"one\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nmissing = \"1.0.0\"\n",
        )]));
        assert_eq!(errs.len(), 1);
        match &errs[0] {
            LoadOrderError::MissingDependency { mod_id, dep } => {
                assert_eq!(mod_id.as_str(), "one");
                assert_eq!(dep.as_str(), "missing");
            }
            other => panic!("expected MissingDependency, got {other}"),
        }
    }

    #[test]
    fn hard_version_conflict_is_an_error() {
        let errs = assert_err(ModRegistry::build(vec![
            source("p/lib", "id = \"lib\"\nversion = \"2.0.0\"\napi = 1\n"),
            source(
                "p/app",
                "id = \"app\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nlib = \"1.0.0\"\n",
            ),
        ]));
        match &errs[0] {
            LoadOrderError::VersionConflict {
                mod_id,
                dep,
                required,
                found,
            } => {
                assert_eq!(mod_id.as_str(), "app");
                assert_eq!(dep.as_str(), "lib");
                assert!(required.contains("1.0.0"), "{required}");
                assert_eq!(found.as_deref(), Some("2.0.0"));
            }
            other => panic!("expected VersionConflict, got {other}"),
        }
    }

    #[test]
    fn version_matching_caret_and_tilde_requirements_pass() {
        // "0.2" => ^0.2.0 matches 0.2.7; "~1.5" matches 1.5.x.
        let reg = ModRegistry::build(vec![
            source("p/lib", "id = \"lib\"\nversion = \"0.2.7\"\napi = 1\n"),
            source(
                "p/app",
                "id = \"app\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nlib = \"0.2\"\n",
            ),
        ])
        .unwrap();
        assert_eq!(mod_order(&reg), vec!["lib", "app"]);
    }

    #[test]
    fn optional_absent_dependency_is_skipped() {
        let reg = ModRegistry::build(vec![source(
            "p/one",
            "id = \"one\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nghost = { version = \"1.0.0\", optional = true }\n",
        )])
        .unwrap();
        assert_eq!(mod_order(&reg), vec!["one"]);
    }

    #[test]
    fn optional_present_dependency_is_enforced() {
        // Present optional dep: must satisfy version and orders the mod.
        let reg = ModRegistry::build(vec![
            source("p/lib", "id = \"lib\"\nversion = \"1.0.0\"\napi = 1\n"),
            source(
                "p/app",
                "id = \"app\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nlib = { version = \"1.0.0\", optional = true }\n",
            ),
        ])
        .unwrap();
        assert_eq!(mod_order(&reg), vec!["lib", "app"]);

        // Present optional dep with wrong version: version conflict.
        let errs = assert_err(ModRegistry::build(vec![
            source("p/lib", "id = \"lib\"\nversion = \"2.0.0\"\napi = 1\n"),
            source(
                "p/app",
                "id = \"app\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nlib = { version = \"1.0.0\", optional = true }\n",
            ),
        ]));
        assert!(matches!(errs[0], LoadOrderError::VersionConflict { .. }));
    }

    #[test]
    fn absent_optional_hint_is_inert() {
        // load-order.after naming an absent optional dependency: silently
        // inert, no error, no edge.
        let reg = ModRegistry::build(vec![source(
            "p/one",
            "id = \"one\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nghost = { version = \"1.0.0\", optional = true }\n[load-order]\nafter = [\"ghost\"]\n",
        )])
        .unwrap();
        assert_eq!(mod_order(&reg), vec!["one"]);
    }

    #[test]
    fn absent_hard_hint_is_inert_too() {
        // Even a load-order hint naming an absent mod that is NOT a dependency
        // is silently inert (owner decision: hint to absent optional dep is
        // inert; soft edges are active only when both mods present).
        let reg = ModRegistry::build(vec![source(
            "p/one",
            "id = \"one\"\nversion = \"1.0.0\"\napi = 1\n[load-order]\nafter = [\"ghost\"]\n",
        )])
        .unwrap();
        assert_eq!(mod_order(&reg), vec!["one"]);
    }

    #[test]
    fn active_soft_after_edge_orders_mods() {
        let reg = ModRegistry::build(vec![
            source("p/a", "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n"),
            source(
                "p/b",
                "id = \"b\"\nversion = \"1.0.0\"\napi = 1\n[load-order]\nafter = [\"a\"]\n",
            ),
        ])
        .unwrap();
        // b after a even though there is no hard dependency.
        assert_eq!(mod_order(&reg), vec!["a", "b"]);
    }

    #[test]
    fn active_soft_before_edge_orders_mods() {
        let reg = ModRegistry::build(vec![
            source(
                "p/a",
                "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n[load-order]\nbefore = [\"b\"]\n",
            ),
            source("p/b", "id = \"b\"\nversion = \"1.0.0\"\napi = 1\n"),
        ])
        .unwrap();
        // a before b via the before hint (edge a -> b).
        assert_eq!(mod_order(&reg), vec!["a", "b"]);
    }

    #[test]
    fn soft_after_cycle_is_diagnosed_with_soft_edge_named() {
        let errs = assert_err(ModRegistry::build(vec![
            source(
                "p/a",
                "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n[load-order]\nafter = [\"b\"]\n",
            ),
            source(
                "p/b",
                "id = \"b\"\nversion = \"1.0.0\"\napi = 1\n[load-order]\nafter = [\"a\"]\n",
            ),
        ]));
        assert_eq!(errs.len(), 1);
        match &errs[0] {
            LoadOrderError::Cycle { path, soft_edges } => {
                let rendered = path
                    .iter()
                    .map(|i| i.as_str())
                    .collect::<Vec<_>>()
                    .join(" -> ");
                assert!(
                    rendered == "a -> b" || rendered == "b -> a",
                    "full cycle path, got {rendered}"
                );
                // Both edges are soft; at least one soft edge is named.
                assert_eq!(soft_edges.len(), 2, "{:?}", soft_edges);
                // The hint source is named in the message.
                assert!(
                    soft_edges.iter().all(|(_, _, k)| *k == "after"),
                    "{:?}",
                    soft_edges
                );
            }
            other => panic!("expected Cycle, got {other}"),
        }
        assert!(errs[0].to_string().contains("soft edge"), "{}", errs[0]);
        assert!(
            errs[0].to_string().contains("load-order.after"),
            "{}",
            errs[0]
        );
    }

    #[test]
    fn cycle_with_peeled_sink_reports_only_real_edges() {
        // A 2-cycle between `m` and `n`, plus a sink node `a` that only has an
        // in-edge (m -> a via a's load-order.after hint) and no outgoing
        // edges. `a` is unplaced but not on any cycle; the reported cycle must
        // be exactly the m<->n cycle, with no phantom edge through `a`.
        let errs = assert_err(ModRegistry::build(vec![
            source(
                "p/a",
                "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n[load-order]\nafter = [\"m\"]\n",
            ),
            source(
                "p/m",
                "id = \"m\"\nversion = \"1.0.0\"\napi = 1\n[load-order]\nafter = [\"n\"]\n",
            ),
            source(
                "p/n",
                "id = \"n\"\nversion = \"1.0.0\"\napi = 1\n[load-order]\nafter = [\"m\"]\n",
            ),
        ]));
        assert_eq!(errs.len(), 1);
        match &errs[0] {
            LoadOrderError::Cycle { path, soft_edges } => {
                assert_eq!(path.len(), 2, "cycle is just m<->n, got {:?}", path);
                let mut names: Vec<&str> = path.iter().map(|i| i.as_str()).collect();
                names.sort_unstable();
                assert_eq!(names, vec!["m", "n"]);
                // Both edges are real soft edges (m->n and n->m), both from
                // load-order.after hints.
                assert_eq!(soft_edges.len(), 2, "{:?}", soft_edges);
            }
            other => panic!("expected Cycle, got {other}"),
        }
        let s = errs[0].to_string();
        assert!(!s.contains("a ->"), "sink must not appear in cycle: {s}");
        assert!(s.contains("m -> n") || s.contains("n -> m"), "{s}");
    }

    #[test]
    fn hard_dependency_cycle_reports_full_path() {
        let errs = assert_err(ModRegistry::build(vec![
            source(
                "p/a",
                "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nb = \"1.0.0\"\n",
            ),
            source(
                "p/b",
                "id = \"b\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nc = \"1.0.0\"\n",
            ),
            source(
                "p/c",
                "id = \"c\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\na = \"1.0.0\"\n",
            ),
        ]));
        assert_eq!(errs.len(), 1);
        match &errs[0] {
            LoadOrderError::Cycle { path, soft_edges } => {
                // The deterministic walk reports the full cycle (3 distinct
                // nodes, exactly the ones in the cycle, in a valid ring
                // order). Verify structure, not a fixed textual rotation.
                assert_eq!(path.len(), 3);
                let mut names: Vec<&str> = path.iter().map(|i| i.as_str()).collect();
                names.sort_unstable();
                assert_eq!(names, vec!["a", "b", "c"]);
                assert!(soft_edges.is_empty(), "pure hard cycle has no soft edges");
                let s = errs[0].to_string();
                assert!(s.contains("->"), "{s}");
                // The message closes the loop back to the start.
                let start = path[0].as_str();
                assert!(s.ends_with(start), "cycle closes to start: {s}");
            }
            other => panic!("expected Cycle, got {other}"),
        }
    }

    #[test]
    fn mixed_hard_soft_cycle_reports_full_path_and_soft_edge() {
        // b has a hard dep on a; a has a soft after hint on b -> cycle.
        let errs = assert_err(ModRegistry::build(vec![
            source(
                "p/a",
                "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n[load-order]\nafter = [\"b\"]\n",
            ),
            source(
                "p/b",
                "id = \"b\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\na = \"1.0.0\"\n",
            ),
        ]));
        match &errs[0] {
            LoadOrderError::Cycle { path, soft_edges } => {
                assert_eq!(path.len(), 2);
                assert_eq!(soft_edges.len(), 1, "{:?}", soft_edges);
                let s = errs[0].to_string();
                assert!(s.contains("->"), "{s}");
                assert!(s.contains("soft edge"), "{s}");
            }
            other => panic!("expected Cycle, got {other}"),
        }
    }

    #[test]
    fn duplicate_id_reports_both_paths() {
        let errs = assert_err(ModRegistry::build(vec![
            source("first/dir", "id = \"dup\"\nversion = \"1.0.0\"\napi = 1\n"),
            source("second/dir", "id = \"dup\"\nversion = \"1.0.0\"\napi = 1\n"),
        ]));
        assert_eq!(errs.len(), 1);
        match &errs[0] {
            LoadOrderError::DuplicateId {
                id,
                first_path,
                second_path,
            } => {
                assert_eq!(id.as_str(), "dup");
                assert_eq!(first_path, "first/dir");
                assert_eq!(second_path, "second/dir");
            }
            other => panic!("expected DuplicateId, got {other}"),
        }
        assert!(errs[0].to_string().contains("first/dir"));
        assert!(errs[0].to_string().contains("second/dir"));
    }

    #[test]
    fn invalid_manifest_is_reported_with_path() {
        let errs = assert_err(ModRegistry::build(vec![source(
            "broken/mod",
            "id = \"a\"\nversion = \"not-a-version\"\napi = 1\n",
        )]));
        match &errs[0] {
            LoadOrderError::InvalidManifest { path, .. } => {
                assert_eq!(path, "broken/mod");
            }
            other => panic!("expected InvalidManifest, got {other}"),
        }
    }

    #[test]
    fn missing_manifest_file_is_reported() {
        let src = ModSource::from_vfs(
            "empty/dir",
            Box::new(MemVfs::new()), // no mod.toml
        );
        let errs = assert_err(ModRegistry::build(vec![src]));
        assert!(matches!(errs[0], LoadOrderError::InvalidManifest { .. }));
    }

    #[test]
    fn multiple_errors_collected_together() {
        let errs = assert_err(ModRegistry::build(vec![
            source(
                "p/one",
                "id = \"one\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nmissing1 = \"1.0.0\"\nmissing2 = \"1.0.0\"\n",
            ),
            source("p/two", "id = \"two\"\nversion = \"1.0.0\"\napi = 1\n"),
        ]));
        // Both missing deps collected in one pass.
        assert_eq!(errs.len(), 2);
        let deps: Vec<&str> = errs
            .iter()
            .map(|e| match e {
                LoadOrderError::MissingDependency { dep, .. } => dep.as_str(),
                other => panic!("unexpected {other}"),
            })
            .collect();
        assert_eq!(deps, vec!["missing1", "missing2"]);
    }

    #[test]
    fn base_declared_as_id_or_dep_is_rejected() {
        // The manifest layer rejects claiming id `base`; the registry reports
        // it as an invalid manifest.
        let errs = assert_err(ModRegistry::build(vec![source(
            "p/bad",
            "id = \"base\"\nversion = \"1.0.0\"\napi = 1\n",
        )]));
        assert!(matches!(errs[0], LoadOrderError::InvalidManifest { .. }));

        // Declaring base as a dependency is also rejected at parse time.
        let errs = assert_err(ModRegistry::build(vec![source(
            "p/bad",
            "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\nbase = \"0.1.0\"\n",
        )]));
        assert!(matches!(errs[0], LoadOrderError::InvalidManifest { .. }));
    }

    #[test]
    fn dir_and_zip_sources_feed_same_registry_api() {
        use std::collections::BTreeMap;

        // Same content as a directory on disk and as a "zip" (represented by
        // a Vfs over the same file set, as ZipVfs will present it).
        let tmp = tempfile::tempdir().unwrap();
        let mod_a_dir = tmp.path().join("a");
        let mod_b_dir = tmp.path().join("b");
        std::fs::create_dir_all(&mod_a_dir).unwrap();
        std::fs::create_dir_all(&mod_b_dir).unwrap();
        std::fs::write(
            mod_a_dir.join("mod.toml"),
            "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n",
        )
        .unwrap();
        std::fs::write(
            mod_b_dir.join("mod.toml"),
            "id = \"b\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\na = \"1.0.0\"\n",
        )
        .unwrap();

        // Directory sources: route through discover_dir so the discovery layer
        // (not just ModSource::directory) feeds the registry.
        let dir_sources = discover_dir(tmp.path()).unwrap();
        let dir_reg = ModRegistry::build(dir_sources).unwrap();

        // Zip-equivalent sources (MemVfs, the shape ZipVfs exposes).
        let files: BTreeMap<String, String> = BTreeMap::from([
            (
                "a/mod.toml".to_string(),
                "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n".to_string(),
            ),
            (
                "b/mod.toml".to_string(),
                "id = \"b\"\nversion = \"1.0.0\"\napi = 1\n[dependencies]\na = \"1.0.0\"\n"
                    .to_string(),
            ),
        ]);
        let zip_sources: Vec<ModSource> = files.into_iter().map(|(p, t)| source(&p, &t)).collect();
        let zip_reg = ModRegistry::build(zip_sources).unwrap();

        // Same registry: identical order and identical mod manifests.
        assert_eq!(mod_order(&dir_reg), mod_order(&zip_reg));
        assert_eq!(mod_order(&dir_reg), vec!["a", "b"]);
        assert_eq!(dir_reg.order, zip_reg.order);
        assert_eq!(
            dir_reg.mods.len(),
            zip_reg.mods.len(),
            "same mod set from dir and zip sources"
        );
    }

    #[test]
    fn registry_keeps_sources_for_content_loading() {
        // The registry retains each mod's Vfs so downstream content loading
        // (blueprints/scripts/patches) reads through the same source.
        let reg = ModRegistry::build(vec![source(
            "p/one",
            "id = \"one\"\nversion = \"1.0.0\"\napi = 1\n",
        )])
        .unwrap();
        let loaded = reg.mods.get(&ModId::from_valid("one")).unwrap();
        assert_eq!(loaded.manifest.path, "p/one");
    }
}
