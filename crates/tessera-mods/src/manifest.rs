//! `mod.toml` data model and strict validation.
//!
//! Parsing walks the raw `toml::Value` (matching the blueprint loader) so every
//! failure carries the mod source path and, once known, the mod id in its
//! [`ManifestError`]. This keeps diagnostics `file + id`-scoped like the rest
//! of the engine while refusing unknown keys at every level (the tight-schema
//! policy).
//!
//! The module only parses the manifest *text*; it never reads content files.
//! [`parse_manifest`] takes the `mod.toml` source directly, so validity of a
//! manifest is decoupled from disk/zip discovery (a later concern).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use semver::{Version, VersionReq};

use crate::error::ManifestError;

/// The implicit base mod's fixed id. It is always present and always loads
/// first, so it may not be declared as a dependency (see module docs).
pub const BASE_MOD_ID: &str = "base";

/// A validated mod id: `[a-z][a-z0-9_-]*`, max 64 chars.
///
/// Newtype so a manifest that parses is guaranteed to carry only ids that
/// satisfy the grammar rule, and so `BTreeMap`/`BTreeSet` keys are typed
/// rather than raw strings.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModId(String);

impl ModId {
    /// Validate the id grammar rules from the manifest schema:
    /// starts with a lowercase ascii letter, then `[a-z0-9_-]*`, max 64.
    /// Returns an error message suitable for embedding in a diagnostic.
    fn validate(s: &str) -> Result<(), String> {
        if s.is_empty() {
            return Err("id is empty".to_string());
        }
        if s.len() > 64 {
            return Err(format!(
                "id is {len} chars, the maximum is 64",
                len = s.chars().count()
            ));
        }
        let mut chars = s.chars();
        let first = chars.next().expect("non-empty checked above");
        if !first.is_ascii_lowercase() {
            return Err(format!(
                "id must start with a lowercase letter, got {first:?}"
            ));
        }
        if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
            return Err(format!(
                "id may only contain lowercase letters, digits, '_' and '-': {s:?}"
            ));
        }
        Ok(())
    }

    /// Wrap a `&str` that is already known to satisfy the grammar (e.g. one
    /// previously validated by [`ModId::validate`]).
    pub(crate) fn from_valid(s: &str) -> Self {
        ModId(s.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A mod dependency entry: a semver *requirement* plus whether the dependency
/// is optional. An `optional = true` dependency is skipped when the target mod
/// is absent but enforced (existence + version) when present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyReq {
    pub version: VersionReq,
    pub optional: bool,
}

/// Load-order *soft* hints. Edges are added only when the referenced mod is
/// present; a hint naming an absent mod is silently inert. Both sets are
/// sorted (`BTreeSet`) so ordering is deterministic regardless of how a mod
/// author listed the hints in TOML.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadOrder {
    pub after: BTreeSet<ModId>,
    pub before: BTreeSet<ModId>,
}

/// A fully parsed and validated `mod.toml`.
///
/// All collections are ordered (`BTreeMap`/`BTreeSet`), ready for the load
/// ordering code to consume without revalidation. `api`, id grammar, semver,
/// unknown-key and base-dependency checks have already passed by the time this
/// value exists.
#[derive(Clone, Debug, PartialEq)]
pub struct ModManifest {
    /// Source path of the manifest, for diagnostics (e.g. `mod.toml`).
    pub path: String,
    pub id: ModId,
    pub version: Version,
    /// The engine api level this mod targets; equal to
    /// [`tessera_core::TESSERA_API_VERSION`] (checked at parse time).
    pub api: u32,
    pub title: Option<String>,
    pub authors: Vec<String>,
    /// mod id -> requirement. `BTreeMap` (sorted) for determinism.
    pub dependencies: BTreeMap<ModId, DependencyReq>,
    pub load_order: LoadOrder,
}

/// Top-level keys accepted in `mod.toml`; anything else is a pinned error.
const TOP_KEYS: &[&str] = &[
    "id",
    "version",
    "api",
    "title",
    "authors",
    "dependencies",
    "load-order",
];

const STUB: &str = "<unknown id>";

fn invalid(path: &str, id: &str, msg: impl ToString) -> ManifestError {
    ManifestError::invalid(path, id, msg.to_string())
}

fn as_table<'a>(
    path: &str,
    context: &str,
    v: &'a toml::Value,
) -> Result<&'a toml::map::Map<String, toml::Value>, ManifestError> {
    v.as_table()
        .ok_or_else(|| invalid(path, STUB, format!("{context} must be a table")))
}

fn as_str<'a>(
    path: &str,
    id: &str,
    key: &str,
    v: &'a toml::Value,
) -> Result<&'a str, ManifestError> {
    v.as_str().ok_or_else(|| {
        invalid(
            path,
            id,
            format!("`{key}` must be a string, got {}", type_name(v)),
        )
    })
}

fn type_name(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

/// Parse and strictly validate a `mod.toml` manifest from its text.
///
/// The `path` is a *diagnostic* label (the mod source path); no file is read
/// here. `api` is checked against [`tessera_core::TESSERA_API_VERSION`] before
/// anything else, matching the rule that an api mismatch rejects the mod
/// before any content file is read.
pub fn parse_manifest(path: &str, text: &str) -> Result<ModManifest, ManifestError> {
    let value: toml::Value = toml::from_str(text).map_err(|source| ManifestError::Parse {
        path: path.to_string(),
        source,
    })?;
    let table = as_table(path, "manifest", &value)?;

    // --- deny unknown top-level keys, producing a full-list diagnostic.
    // Only the known keys are legal; everything else is a hard error.
    for key in table.keys() {
        if !TOP_KEYS.contains(&key.as_str()) {
            return Err(invalid(
                path,
                STUB,
                format!(
                    "unknown top-level key `{key}`; expected one of {}",
                    TOP_KEYS.join(", ")
                ),
            ));
        }
    }

    // --- required scalar fields, with targeted diagnostics.
    let id_str = match table.get("id") {
        Some(v) => as_str(path, STUB, "id", v)?.to_string(),
        None => return Err(invalid(path, STUB, "missing required field `id`")),
    };
    let id = match ModId::validate(&id_str) {
        Ok(()) => ModId::from_valid(&id_str),
        Err(msg) => return Err(invalid(path, &id_str, msg)),
    };
    if id_str == BASE_MOD_ID {
        return Err(invalid(
            path,
            &id_str,
            "`base` is the implicit base mod id and may not be declared by a mod",
        ));
    }

    let version_str = match table.get("version") {
        Some(v) => as_str(path, &id_str, "version", v)?.to_string(),
        None => return Err(invalid(path, &id_str, "missing required field `version`")),
    };
    let version = Version::parse(&version_str).map_err(|e| {
        invalid(
            path,
            &id_str,
            format!("`version` is not a valid semver (X.Y.Z with optional -pre+build): {e}"),
        )
    })?;

    let api = match table.get("api") {
        Some(v) => match v.as_integer() {
            Some(n) => n,
            None => {
                return Err(invalid(
                    path,
                    &id_str,
                    format!("`api` must be an integer, got {}", type_name(v)),
                ));
            }
        },
        None => return Err(invalid(path, &id_str, "missing required field `api`")),
    };
    if api != tessera_core::TESSERA_API_VERSION as i64 {
        return Err(invalid(
            path,
            &id_str,
            format!(
                "`api` mismatch: manifest targets api {api}, engine is api {}",
                tessera_core::TESSERA_API_VERSION
            ),
        ));
    }

    // --- optional display fields.
    let title = match table.get("title") {
        None => None,
        Some(v) => Some(as_str(path, &id_str, "title", v)?.to_string()),
    };
    if let Some(t) = &title
        && t.chars().count() > 80
    {
        return Err(invalid(
            path,
            &id_str,
            format!("`title` is {} chars, the maximum is 80", t.chars().count()),
        ));
    }

    let authors = match table.get("authors") {
        None => Vec::new(),
        Some(v) => {
            let arr = v.as_array().ok_or_else(|| {
                invalid(
                    path,
                    &id_str,
                    format!(
                        "`authors` must be an array of strings, got {}",
                        type_name(v)
                    ),
                )
            })?;
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                let s = item.as_str().ok_or_else(|| {
                    invalid(
                        path,
                        &id_str,
                        format!("`authors` entries must be strings, got {}", type_name(item)),
                    )
                })?;
                out.push(s.to_string());
            }
            out
        }
    };

    // --- dependencies: table of id -> requirement (string or {version, optional}).
    let dependencies = match table.get("dependencies") {
        None => BTreeMap::new(),
        Some(v) => parse_dependencies(path, &id_str, v)?,
    };

    // --- load-order: table with optional `after` / `before` string arrays.
    let load_order = match table.get("load-order") {
        None => LoadOrder::default(),
        Some(v) => parse_load_order(path, &id_str, v)?,
    };

    Ok(ModManifest {
        path: path.to_string(),
        id,
        version,
        api: api as u32,
        title,
        authors,
        dependencies,
        load_order,
    })
}

fn parse_dependencies(
    path: &str,
    mod_id: &str,
    v: &toml::Value,
) -> Result<BTreeMap<ModId, DependencyReq>, ManifestError> {
    let table = as_table(path, "`dependencies`", v)?;
    let mut out = BTreeMap::new();
    for (dep_id, value) in table {
        // Base cannot be named as a dependency: it is implicit and always first.
        if dep_id == BASE_MOD_ID {
            return Err(invalid(
                path,
                mod_id,
                format!(
                    "declaring `{BASE_MOD_ID}` as a dependency is illegal: base is always \
                     present and always loads first"
                ),
            ));
        }
        if let Err(msg) = ModId::validate(dep_id) {
            return Err(invalid(
                path,
                mod_id,
                format!("dependency id {dep_id:?} is invalid: {msg}"),
            ));
        }
        let req = match value {
            // `dep = "0.2"` shorthand: a requirement, optional = false.
            toml::Value::String(s) => DependencyReq {
                version: parse_req(path, mod_id, dep_id, s)?,
                optional: false,
            },
            // `dep = { version = "0.2", optional = true }` table form.
            toml::Value::Table(t) => {
                // Deny unknown keys in the dependency table.
                for key in t.keys() {
                    if key != "version" && key != "optional" {
                        return Err(invalid(
                            path,
                            mod_id,
                            format!(
                                "unknown key `{key}` in dependency `{dep_id}`; \
                                 expected `version` and/or `optional`"
                            ),
                        ));
                    }
                }
                match t.get("version") {
                    Some(sv) => {
                        let req =
                            as_str(path, mod_id, &format!("dependencies.{dep_id}.version"), sv)
                                .map_err(|e| prepend_dep(e, dep_id))?;
                        let req = parse_req(path, mod_id, dep_id, req)?;
                        let optional = match t.get("optional") {
                            None => false,
                            Some(ov) => ov.as_bool().ok_or_else(|| {
                                invalid(
                                    path,
                                    mod_id,
                                    format!(
                                        "`optional` in dependency `{dep_id}` must be a boolean, \
                                         got {}",
                                        type_name(ov)
                                    ),
                                )
                            })?,
                        };
                        DependencyReq {
                            version: req,
                            optional,
                        }
                    }
                    None => {
                        return Err(invalid(
                            path,
                            mod_id,
                            format!(
                                "dependency `{dep_id}` table is missing the required `version` \
                                 field"
                            ),
                        ));
                    }
                }
            }
            other => {
                return Err(invalid(
                    path,
                    mod_id,
                    format!(
                        "dependency `{dep_id}` must be a string or a table, got {}",
                        type_name(other)
                    ),
                ));
            }
        };
        out.insert(ModId::from_valid(dep_id), req);
    }
    Ok(out)
}

fn parse_req(
    path: &str,
    mod_id: &str,
    dep_id: &str,
    raw: &str,
) -> Result<VersionReq, ManifestError> {
    VersionReq::parse(raw).map_err(|e| {
        invalid(
            path,
            mod_id,
            format!("dependency `{dep_id}` has invalid semver requirement {raw:?}: {e}"),
        )
    })
}

fn parse_load_order(path: &str, mod_id: &str, v: &toml::Value) -> Result<LoadOrder, ManifestError> {
    let table = as_table(path, "`load-order`", v)?;
    for key in table.keys() {
        if key != "after" && key != "before" {
            return Err(invalid(
                path,
                mod_id,
                format!("unknown key `{key}` in `load-order`; expected `after` and/or `before`"),
            ));
        }
    }
    let mut out = LoadOrder::default();
    for list_key in ["after", "before"] {
        let Some(arr) = table.get(list_key) else {
            continue;
        };
        let arr = arr.as_array().ok_or_else(|| {
            invalid(
                path,
                mod_id,
                format!(
                    "`load-order.{list_key}` must be an array of mod ids, got {}",
                    type_name(arr)
                ),
            )
        })?;
        for item in arr {
            let id = item.as_str().ok_or_else(|| {
                invalid(
                    path,
                    mod_id,
                    format!(
                        "`load-order.{list_key}` entries must be strings, got {}",
                        type_name(item)
                    ),
                )
            })?;
            if let Err(msg) = ModId::validate(id) {
                return Err(invalid(
                    path,
                    mod_id,
                    format!("load-order id {id:?} is invalid: {msg}"),
                ));
            }
            let mid = ModId::from_valid(id);
            if list_key == "after" {
                out.after.insert(mid);
            } else {
                out.before.insert(mid);
            }
        }
    }
    Ok(out)
}

fn prepend_dep(e: ManifestError, dep_id: &str) -> ManifestError {
    match e {
        ManifestError::Invalid { path, id, msg } => ManifestError::Invalid {
            path,
            id,
            msg: format!("in dependency `{dep_id}`: {msg}"),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_core::TESSERA_API_VERSION;

    const PATH: &str = "some/mod.toml";

    fn valid_minimal() -> &'static str {
        concat!(
            "id      = \"more-monsters\"\n",
            "version = \"0.3.1\"\n",
            "api     = 1\n",
        )
    }

    fn full() -> &'static str {
        concat!(
            "id      = \"more-monsters\"\n",
            "version = \"0.3.1\"\n",
            "api     = 1\n",
            "title   = \"More Monsters\"\n",
            "authors = [\"jeff\", \"someone-else\"]\n",
            "\n",
            "[dependencies]\n",
            "qud-forks = { version = \"0.2\", optional = true }\n",
            "\n",
            "[load-order]\n",
            "after  = [\"other-mod\"]\n",
            "before = []\n",
        )
    }

    fn err_str(r: Result<ModManifest, ManifestError>) -> String {
        match r {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn minimal_manifest_parses() {
        let m = parse_manifest(PATH, valid_minimal()).unwrap();
        assert_eq!(m.id.as_str(), "more-monsters");
        assert_eq!(m.version.to_string(), "0.3.1");
        assert_eq!(m.api, TESSERA_API_VERSION);
        assert_eq!(m.title, None);
        assert_eq!(m.authors, Vec::<String>::new());
        assert!(m.dependencies.is_empty());
        assert!(m.load_order.after.is_empty());
        assert!(m.load_order.before.is_empty());
    }

    #[test]
    fn full_manifest_parses() {
        let m = parse_manifest(PATH, full()).unwrap();
        assert_eq!(m.title.as_deref(), Some("More Monsters"));
        assert_eq!(m.authors, vec!["jeff", "someone-else"]);
        let qud = m.dependencies.get(&ModId::from_valid("qud-forks")).unwrap();
        assert_eq!(qud.version, VersionReq::parse("^0.2").unwrap());
        assert!(qud.optional);
        assert!(m.load_order.after.contains(&ModId::from_valid("other-mod")));
        assert!(m.load_order.before.is_empty());
    }

    #[test]
    fn string_dependency_shorthand_is_not_optional() {
        let txt = concat!(
            "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n",
            "[dependencies]\nbase-req = \"0.3\"\n"
        );
        let m = parse_manifest(PATH, txt).unwrap();
        let d = m.dependencies.get(&ModId::from_valid("base-req")).unwrap();
        assert!(!d.optional);
        assert_eq!(d.version, VersionReq::parse("^0.3").unwrap());
    }

    #[test]
    fn missing_required_field_fails() {
        // No id.
        let e = err_str(parse_manifest(PATH, "version = \"0.1.0\"\napi = 1\n"));
        assert!(e.contains("path") || e.contains(PATH), "{e}");
        assert!(e.contains("missing required field `id`"), "{e}");

        // No version.
        let e = err_str(parse_manifest(PATH, "id = \"a\"\napi = 1\n"));
        assert!(e.contains("missing required field `version`"), "{e}");

        // No api.
        let e = err_str(parse_manifest(PATH, "id = \"a\"\nversion = \"0.1.0\"\n"));
        assert!(e.contains("missing required field `api`"), "{e}");
    }

    #[test]
    fn empty_manifest_fails() {
        let e = err_str(parse_manifest(PATH, ""));
        assert!(e.contains("missing required field `id`"), "{e}");
    }

    #[test]
    fn empty_id_and_empty_version_fail() {
        let e = err_str(parse_manifest(
            PATH,
            "id = \"\"\nversion = \"0.1.0\"\napi = 1\n",
        ));
        assert!(e.contains("id is empty"), "{e}");

        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"\"\napi = 1\n",
        ));
        assert!(
            e.contains("not a valid semver") || e.contains("empty"),
            "{e}"
        );
    }

    #[test]
    fn invalid_id_fails_with_diagnostic() {
        // Leading uppercase (must start lowercase).
        let e = err_str(parse_manifest(
            PATH,
            "id = \"MoreMonsters\"\nversion = \"0.1.0\"\napi = 1\n",
        ));
        assert!(e.contains("must start with a lowercase letter"), "{e}");
        // Invalid char.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"more monsters\"\nversion = \"0.1.0\"\napi = 1\n",
        ));
        assert!(e.contains("only contain lowercase"), "{e}");
        // Too long (65 chars).
        let long = "a".repeat(65);
        let e = err_str(parse_manifest(
            PATH,
            &format!("id = \"{long}\"\nversion = \"0.1.0\"\napi = 1\n"),
        ));
        assert!(e.contains("maximum is 64"), "{e}");
        // All carry the path.
        assert!(e.contains(PATH), "{e}");
    }

    #[test]
    fn invalid_semver_fails() {
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"not-semver\"\napi = 1\n",
        ));
        assert!(e.contains("not a valid semver"), "{e}");
        // Two-part versions are rejected by strict semver (needs X.Y.Z).
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"1.0\"\napi = 1\n",
        ));
        assert!(e.contains("not a valid semver"), "{e}");
    }

    #[test]
    fn api_mismatch_and_non_integer_fail_before_content() {
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 99\n",
        ));
        assert!(e.contains("api") && e.contains("mismatch"), "{e}");

        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = \"one\"\n",
        ));
        assert!(e.contains("must be an integer"), "{e}");
    }

    #[test]
    fn unknown_keys_fail_at_every_level() {
        // Top level.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\nbogus = 1\n",
        ));
        assert!(e.contains("unknown top-level key `bogus`"), "{e}");

        // Dependency table.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\n[dependencies]\nx = { version = \"0.1\", nope = 2 }\n",
        ));
        assert!(e.contains("unknown key `nope` in dependency `x`"), "{e}");

        // load-order table.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\n[load-order]\nnonsense = []\n",
        ));
        assert!(e.contains("unknown key `nonsense` in `load-order`"), "{e}");
    }

    #[test]
    fn malformed_dependency_forms_fail() {
        // Non-string non-table dependency value.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\n[dependencies]\nx = 5\n",
        ));
        assert!(e.contains("must be a string or a table"), "{e}");

        // Table missing version.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\n[dependencies]\nx = { optional = true }\n",
        ));
        assert!(e.contains("missing the required `version`"), "{e}");

        // Invalid requirement.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\n[dependencies]\nx = { version = \"not-a-req\" }\n",
        ));
        assert!(e.contains("invalid semver requirement"), "{e}");

        // Invalid optional type.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\n[dependencies]\nx = { version = \"0.1\", optional = \"yes\" }\n",
        ));
        assert!(e.contains("must be a boolean"), "{e}");
    }

    #[test]
    fn malformed_load_order_fails() {
        // after is not an array.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\n[load-order]\nafter = \"x\"\n",
        ));
        assert!(e.contains("must be an array"), "{e}");

        // Entry not a string.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\n[load-order]\nafter = [1]\n",
        ));
        assert!(e.contains("entries must be strings"), "{e}");

        // Invalid id in after.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\n[load-order]\nafter = [\"bad id\"]\n",
        ));
        assert!(e.contains("load-order id"), "{e}");
    }

    #[test]
    fn declaring_base_as_dependency_is_illegal() {
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\n[dependencies]\nbase = \"0.1.0\"\n",
        ));
        assert!(
            e.contains("base is always present") || e.contains("illegal"),
            "{e}"
        );
        assert!(e.contains(PATH), "{e}");
    }

    #[test]
    fn declaring_base_as_own_id_is_illegal() {
        // Base is the implicit built-in mod id; a mod may not claim it.
        let e = err_str(parse_manifest(
            PATH,
            "id = \"base\"\nversion = \"0.1.0\"\napi = 1\n",
        ));
        assert!(e.contains("`base` is the implicit base mod id"), "{e}");
    }

    #[test]
    fn optional_dependency_preserves_absent_flag() {
        // A table with optional=true is recorded distinctly from a shorthand
        // string (optional=false). absent-target semantics survive round-trip.
        let txt = concat!(
            "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n",
            "[dependencies]\n",
            "hard = \"0.1\"\n",
            "soft = { version = \"0.1\", optional = true }\n",
        );
        let m = parse_manifest(PATH, txt).unwrap();
        assert!(!m.dependencies[&ModId::from_valid("hard")].optional);
        assert!(m.dependencies[&ModId::from_valid("soft")].optional);
    }

    #[test]
    fn parse_does_not_touch_content() {
        // parse_manifest only takes the manifest text + a diagnostic path; no
        // filesystem or content discovery happens. A path that points nowhere
        // still parses fine, proving content is never read.
        let m = parse_manifest("/does/not/exist/mod.toml", valid_minimal()).unwrap();
        assert_eq!(m.id.as_str(), "more-monsters");
    }

    #[test]
    fn collections_are_ordered() {
        let txt = concat!(
            "id = \"a\"\nversion = \"1.0.0\"\napi = 1\n",
            "[dependencies]\n",
            "zeta = \"0.1\"\n",
            "alpha = { version = \"0.1\", optional = true }\n",
            "[load-order]\n",
            "after = [\"mike\", \"bravo\"]\n",
        );
        let m = parse_manifest(PATH, txt).unwrap();
        // BTreeMap keys iterate sorted.
        let dep_ids: Vec<&str> = m.dependencies.keys().map(|k| k.as_str()).collect();
        assert_eq!(dep_ids, vec!["alpha", "zeta"]);
        // BTreeSet entries iterate sorted (bravo before mike).
        let after: Vec<&str> = m.load_order.after.iter().map(|k| k.as_str()).collect();
        assert_eq!(after, vec!["bravo", "mike"]);
    }

    #[test]
    fn title_over_80_chars_fails() {
        let long_title = "t".repeat(81);
        let e = err_str(parse_manifest(
            PATH,
            &format!("id = \"a\"\nversion = \"0.1.0\"\napi = 1\ntitle = \"{long_title}\"\n"),
        ));
        assert!(e.contains("maximum is 80"), "{e}");
    }

    #[test]
    fn title_and_authors_wrong_types_fail() {
        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\ntitle = 5\n",
        ));
        assert!(e.contains("`title` must be a string"), "{e}");

        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\nauthors = \"jeff\"\n",
        ));
        assert!(e.contains("`authors` must be an array"), "{e}");

        let e = err_str(parse_manifest(
            PATH,
            "id = \"a\"\nversion = \"0.1.0\"\napi = 1\nauthors = [5]\n",
        ));
        assert!(e.contains("entries must be strings"), "{e}");
    }

    #[test]
    fn diagnostics_carry_path_and_id() {
        let e = err_str(parse_manifest(
            "custom/path.toml",
            "id = \"a\"\nversion = \"0.1.0\"\napi = 2\n",
        ));
        assert!(e.contains("custom/path.toml"), "{e}");
        // The id is captured once it's known.
        assert!(e.contains("'a'") || e.contains("for 'a'"), "{e}");
    }
}
