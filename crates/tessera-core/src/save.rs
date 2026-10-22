//! Versioned, compressed save codec (in-memory bytes API).
//!
//! Wire format (plaintext header *before* the zstd frame so `load` rejects an
//! unknown version without touching the decompressor):
//!
//! ```text
//! offset 0..4   magic  b"TESS"              (rejects wrong files before decompress)
//! offset 4..8   u32 LE format version        (SAVE_FORMAT_VERSION)
//! offset 8..12  u32 LE postcard payload length (pre-compression; sanity check)
//! offset 12..   zstd frame (level 3) of the postcard-encoded SaveData
//! ```
//!
//! See `docs/determinism.md`. Load defenses, in order: bad magic →
//! [`SaveError::BadMagic`]; unknown/unsupported version → [`SaveError::UnsupportedVersion`];
//! zstd frame error or postcard decode error → [`SaveError::Corrupt`]. Never panic on bad input.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ecs::{EntityBroker, Part, PartError, PartRegistry, PartTypeId};
use crate::effects::PART_DERIVED;
use crate::events::{Health, PART_HEALTH, PART_POSITION, Position};
use crate::rng::SimRng;
use crate::script_part::{PartValue, ScriptPart};
use crate::stats::{Attributes, Bonuses, Level, PART_ATTRIBUTES, PART_BONUSES, PART_LEVEL};
use crate::turns::{ActorEnergy, PART_ENERGY};
use crate::world::RenderTile;

/// Current save format version. Public so callers and the console can name it.
/// 1 → 2: the stats parts are the first new persistent parts, and the legacy
/// `Stats` part left the whitelist in the same increment (a v1 save would
/// fail closed on the removed part anyway), so the bump lands here.
pub const SAVE_FORMAT_VERSION: u32 = 2;

/// Four-byte magic that identifies a Tessera save file.
const MAGIC: &[u8; 4] = b"TESS";

/// Header size: magic (4) + version u32 (4) + postcard length u32 (4).
const HEADER_LEN: usize = 12;

/// zstd compression level (zstd's default).
const ZSTD_LEVEL: i32 = 3;

/// Errors produced by the save codec. All variants are fallible and non-panicking.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("bad magic: file is not a Tessera save")]
    BadMagic,
    #[error(
        "unsupported save format version {found} (supported: {supported}); \
         this file was written by a newer build"
    )]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("save data corrupt: {0}")]
    Corrupt(String),
    #[error("save write failed at {path}: {source}")]
    WriteFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("save read failed at {path}: {source}")]
    ReadFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("part '{name}' is unknown or not whitelisted for saving")]
    UnserializablePart { name: String },
    #[error("save belongs to zone '{saved}', but the loaded world is zone '{current}'")]
    ZoneMismatch { saved: String, current: String },
    #[error("part '{name}' failed to deserialize from the save: {msg}")]
    PartRestore { name: String, msg: String },
}

/// A serialized snapshot of simulation state.
///
/// The header's `version` field must equal [`SAVE_FORMAT_VERSION`]; `zone_id` is a
/// pack-specific check recorded but not enforced by the codec (the loader validates it).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SaveData {
    pub version: u32,
    pub zone_id: String,
    /// hecs entity ids via `Entity::to_bits`/`from_bits`.
    pub next_entity_bits: Vec<u64>,
    pub entities: Vec<SavedEntity>,
    pub player: Option<u64>,
    /// `Scheduler::ticks()`.
    pub clock: u64,
    /// scheduler state of record: (entity id, energy, speed).
    pub actors: Vec<(u64, i64, u32)>,
    /// ChaCha8 stream state; round-trips so a loaded world resumes exactly.
    pub rng: SimRng,
    pub player_turns: u64,
    pub facing: (i32, i32),
}

/// A single entity's parts, snapshotted as `(part name, value)` pairs.
///
/// The value is stored as its canonical TOML text. TOML was chosen over
/// `toml::Value` directly because postcard (the mandated wire format) cannot decode
/// `toml::Value` — it is self-describing and postcard refuses `deserialize_any`.
/// Parts already round-trip through `toml::Value` configs at blueprint-load time, so
/// carrying the serialized form here is symmetric with that path. Callers rebuild the
/// `toml::Value` with `toml::from_str`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedEntity {
    pub bits: u64,
    /// Optional engine-owned render identity (not a packageable PartMap part).
    #[serde(default)]
    pub render: Option<String>,
    /// `(part type name, toml::Value as canonical TOML text)`.
    pub parts: Vec<(String, String)>,
}

/// How one whitelisted part is snapshotted into and restored from a `toml::Value`.
pub struct SaveHook {
    pub snapshot: fn(&dyn Part) -> Result<toml::Value, PartError>,
    pub restore: fn(&toml::Value) -> Result<Box<dyn Part>, PartError>,
}

/// A snapshot from a live `dyn Part` into its canonical TOML value, or a
/// restore of that value back into a `Box<dyn Part>`. Parts are snapshotted by
/// whitelist only — `Box<dyn Part>` is type-erased, so generic serialization is
/// impossible by design. The engine registers hooks for the parts it knows how
/// to snapshot; any unknown part fails closed.
///
/// Deterministic: keys iterate in sorted (`BTreeMap`) part-name order.
#[derive(Default)]
pub struct SaveHooks {
    hooks: BTreeMap<PartTypeId, SaveHook>,
}

impl SaveHooks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a snapshot/restore hook for a part type.
    pub fn register(
        &mut self,
        id: impl Into<PartTypeId>,
        snapshot: fn(&dyn Part) -> Result<toml::Value, PartError>,
        restore: fn(&toml::Value) -> Result<Box<dyn Part>, PartError>,
    ) {
        self.hooks.insert(id.into(), SaveHook { snapshot, restore });
    }

    /// Snapshot a live part into its canonical TOML value. Unknown parts fail
    /// closed with [`SaveError::UnserializablePart`]; machinery never guesses.
    pub fn snapshot(&self, id: &PartTypeId, part: &dyn Part) -> Result<toml::Value, SaveError> {
        let hook = self
            .hooks
            .get(id)
            .ok_or_else(|| SaveError::UnserializablePart {
                name: id.as_str().to_string(),
            })?;
        (hook.snapshot)(part).map_err(|e| SaveError::PartRestore {
            name: id.as_str().to_string(),
            msg: e.to_string(),
        })
    }

    /// Restore a part from its canonical TOML value. An unknown name (a save
    /// from a build with more parts than this loader) fails closed — never
    /// silently truncated.
    pub fn restore(
        &self,
        id: &PartTypeId,
        value: &toml::Value,
    ) -> Result<Box<dyn Part>, SaveError> {
        let hook = self
            .hooks
            .get(id)
            .ok_or_else(|| SaveError::UnserializablePart {
                name: id.as_str().to_string(),
            })?;
        (hook.restore)(value).map_err(|e| SaveError::PartRestore {
            name: id.as_str().to_string(),
            msg: e.to_string(),
        })
    }
}

/// Build the whitelist hook registry used by the demo: the engine-native parts
/// `Position`, `Health`, `ActorEnergy`, `Attributes`, `Level`, `Bonuses`, plus
/// every script part registered in `registry`. Returns `(hooks, script_names)`.
pub fn build_save_hooks(registry: &PartRegistry) -> (SaveHooks, BTreeSet<String>) {
    let mut hooks = SaveHooks::new();
    let mut script_names = BTreeSet::new();

    hooks.register(
        PART_POSITION,
        |part| {
            let p = part
                .as_any()
                .downcast_ref::<Position>()
                .ok_or_else(|| not_a(PART_POSITION, "Position"))?;
            Ok(toml::Value::Table(toml::map::Map::from_iter([
                ("x".into(), toml::Value::Integer(p.x as i64)),
                ("y".into(), toml::Value::Integer(p.y as i64)),
            ])))
        },
        |v| {
            let (x, y) = table_coords(v)?;
            Ok(Box::new(Position { x, y }))
        },
    );

    hooks.register(
        PART_HEALTH,
        |part| {
            let p = part
                .as_any()
                .downcast_ref::<Health>()
                .ok_or_else(|| not_a(PART_HEALTH, "Health"))?;
            Ok(toml::Value::Table(toml::map::Map::from_iter([(
                "hp".into(),
                toml::Value::Integer(p.hp as i64),
            )])))
        },
        |v| {
            Ok(Box::new(Health {
                hp: table_i32(v, "hp")?,
            }))
        },
    );

    hooks.register(
        PART_ENERGY,
        |part| {
            let p = part
                .as_any()
                .downcast_ref::<ActorEnergy>()
                .ok_or_else(|| not_a(PART_ENERGY, "ActorEnergy"))?;
            Ok(toml::Value::Table(toml::map::Map::from_iter([
                ("speed".into(), toml::Value::Integer(p.speed as i64)),
                ("energy".into(), toml::Value::Integer(p.energy)),
            ])))
        },
        |v| {
            Ok(Box::new(ActorEnergy {
                speed: table_u32(v, "speed")?,
                energy: table_i64(v, "energy")?,
            }))
        },
    );

    hooks.register(
        PART_ATTRIBUTES,
        |part| {
            let p = part
                .as_any()
                .downcast_ref::<Attributes>()
                .ok_or_else(|| not_a(PART_ATTRIBUTES, "Attributes"))?;
            Ok(toml::Value::Table(toml::map::Map::from_iter([
                ("might".into(), toml::Value::Integer(p.might)),
                ("agility".into(), toml::Value::Integer(p.agility)),
                ("toughness".into(), toml::Value::Integer(p.toughness)),
                ("will".into(), toml::Value::Integer(p.will)),
                ("intellect".into(), toml::Value::Integer(p.intellect)),
            ])))
        },
        |v| typed_restore::<Attributes>(PART_ATTRIBUTES, v),
    );

    hooks.register(
        PART_LEVEL,
        |part| {
            let p = part
                .as_any()
                .downcast_ref::<Level>()
                .ok_or_else(|| not_a(PART_LEVEL, "Level"))?;
            Ok(toml::Value::Table(toml::map::Map::from_iter([(
                "level".into(),
                toml::Value::Integer(p.level),
            )])))
        },
        |v| typed_restore::<Level>(PART_LEVEL, v),
    );

    hooks.register(
        PART_BONUSES,
        |part| {
            let p = part
                .as_any()
                .downcast_ref::<Bonuses>()
                .ok_or_else(|| not_a(PART_BONUSES, "Bonuses"))?;
            Ok(toml::Value::Table(toml::map::Map::from_iter([
                ("accuracy".into(), toml::Value::Integer(p.accuracy)),
                ("dodge".into(), toml::Value::Integer(p.dodge)),
                ("ac".into(), toml::Value::Integer(p.ac)),
                ("damage_flat".into(), toml::Value::Integer(p.damage_flat)),
                ("block".into(), toml::Value::Integer(p.block)),
            ])))
        },
        |v| typed_restore::<Bonuses>(PART_BONUSES, v),
    );

    // Script parts: snapshot generically through the closed PartValue map, and
    // restore by name so an unknown script name on load fails closed.
    for name in registry.iter_storage() {
        if registry.script_schema(name.0).is_some() {
            hooks.register(name.0.clone(), script_snapshot, script_restore);
            script_names.insert(name.0.as_str().to_string());
        }
    }

    (hooks, script_names)
}

fn not_a(part: &str, ty: &str) -> PartError {
    PartError::BuildFailed {
        name: part.to_string(),
        msg: format!("snapshot expected a {ty}, found another part type"),
    }
}

/// Restore a typed native part from its canonical TOML value through the same
/// typed deserialization the blueprint loader uses. Restore thereby enforces
/// every load-time invariant of the part (`deny_unknown_fields`, bounds) —
/// a malformed or out-of-range payload fails closed like any other hook.
fn typed_restore<T>(name: &str, v: &toml::Value) -> Result<Box<dyn Part>, PartError>
where
    T: serde::de::DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
{
    let t: T = v
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| PartError::BuildFailed {
            name: name.to_string(),
            msg: e.to_string(),
        })?;
    Ok(Box::new(t))
}

/// Snapshot any `ScriptPart` into a TOML table of its closed `PartValue` fields.
fn script_snapshot(part: &dyn Part) -> Result<toml::Value, PartError> {
    let sp = part
        .as_any()
        .downcast_ref::<ScriptPart>()
        .ok_or_else(|| not_a("ScriptPart", "ScriptPart"))?;
    let fields =
        sp.0.iter()
            .map(|(k, v)| (k.clone(), part_value_to_toml(v)))
            .collect();
    Ok(toml::Value::Table(fields))
}

/// Restore a `ScriptPart` from a TOML table of field values.
fn script_restore(v: &toml::Value) -> Result<Box<dyn Part>, PartError> {
    let table = v.as_table().ok_or_else(|| PartError::BuildFailed {
        name: "ScriptPart".into(),
        msg: "expected a TOML table".into(),
    })?;
    let mut fields = BTreeMap::new();
    for (k, val) in table {
        fields.insert(k.clone(), toml_to_part_value(val)?);
    }
    Ok(Box::new(ScriptPart(fields)))
}

fn part_value_to_toml(v: &PartValue) -> toml::Value {
    match v {
        PartValue::Int(n) => toml::Value::Integer(*n),
        PartValue::Bool(b) => toml::Value::Boolean(*b),
        PartValue::Str(s) => toml::Value::String(s.clone()),
        PartValue::List(items) => {
            toml::Value::Array(items.iter().map(part_value_to_toml).collect())
        }
    }
}

fn toml_to_part_value(v: &toml::Value) -> Result<PartValue, PartError> {
    let bad = || PartError::BuildFailed {
        name: "ScriptPart".into(),
        msg: format!("unsupported value kind in script part snapshot: {v}"),
    };
    match v {
        toml::Value::Integer(n) => Ok(PartValue::Int(*n)),
        toml::Value::Boolean(b) => Ok(PartValue::Bool(*b)),
        toml::Value::String(s) => Ok(PartValue::Str(s.clone())),
        toml::Value::Array(items) => {
            let out: Result<Vec<_>, _> = items.iter().map(toml_to_part_value).collect();
            Ok(PartValue::List(out?))
        }
        _ => Err(bad()),
    }
}

fn table_i32(v: &toml::Value, field: &str) -> Result<i32, PartError> {
    table_i64(v, field).map(|n| n as i32)
}

fn table_i64(v: &toml::Value, field: &str) -> Result<i64, PartError> {
    let table = v.as_table().ok_or_else(|| table_err(field, "a table"))?;
    table
        .get(field)
        .and_then(|f| f.as_integer())
        .ok_or_else(|| table_err(field, "an integer"))
}

fn table_u32(v: &toml::Value, field: &str) -> Result<u32, PartError> {
    table_i64(v, field).map(|n| n as u32)
}

fn table_coords(v: &toml::Value) -> Result<(i32, i32), PartError> {
    Ok((table_i32(v, "x")?, table_i32(v, "y")?))
}

fn table_err(field: &str, expected: &str) -> PartError {
    PartError::BuildFailed {
        name: "SaveHooks".into(),
        msg: format!("field '{field}' must be {expected}"),
    }
}

/// The deterministic entity snapshot of a broker: every live entity's id and
/// whitelisted parts, in entity-id then part-name order.
pub struct Snapshot {
    /// hecs entity bits, in deterministic (entity-id) order.
    pub next_entity_bits: Vec<u64>,
    pub entities: Vec<SavedEntity>,
}

/// Parts the snapshot silently excludes rather than failing on. `Derived` is
/// Rebuilt-class: it is genuinely cache, not state, so a present cache is
/// skipped (an absent/stale `Derived` on disk would otherwise make every save
/// of a stat-ful entity fail with `UnserializablePart`).
pub const NON_PERSISTENT_PARTS: &[&str] = &[PART_DERIVED];

/// Snapshot every live entity in `broker` into [`SavedEntity`]s via `hooks`,
/// in deterministic entity-id order. Any part not whitelisted fails closed
/// with [`SaveError::UnserializablePart`] (the name so the gap is obvious),
/// except the explicit skip-list [`NON_PERSISTENT_PARTS`], which is silently
/// excluded.
pub fn snapshot_entities(broker: &EntityBroker, hooks: &SaveHooks) -> Result<Snapshot, SaveError> {
    let mut next_entity_bits = Vec::new();
    let mut entities = Vec::new();
    for entity in broker.entities() {
        let bits = entity.to_bits().get();
        next_entity_bits.push(bits);
        let Some(map) = broker.part_map(entity) else {
            continue;
        };
        let mut parts = Vec::new();
        for (id, part) in map.iter() {
            if NON_PERSISTENT_PARTS.contains(&id.as_str()) {
                continue;
            }
            let value = hooks.snapshot(id, part)?;
            parts.push((id.as_str().to_string(), toml_value_to_text(&value)?));
        }
        let render = broker
            .world()
            .get::<&RenderTile>(entity)
            .ok()
            .map(|tile| tile.tile.clone());
        entities.push(SavedEntity {
            bits,
            render,
            parts,
        });
    }
    Ok(Snapshot {
        next_entity_bits,
        entities,
    })
}

/// Build a fresh broker whose entities are restored at their exact saved ids
/// (via `spawn_at`, preserving deterministic entity-id ordering) with every
/// saved part restored through `hooks`. Fail-closed: any unknown part name
/// returns [`SaveError::UnserializablePart`] rather than truncating silently.
pub fn restore_broker(data: &SaveData, hooks: &SaveHooks) -> Result<EntityBroker, SaveError> {
    let mut broker = EntityBroker::new();
    for saved in &data.entities {
        let entity = broker.spawn_at(saved.bits);
        if let Some(tile) = &saved.render {
            broker
                .world_mut()
                .insert_one(entity, RenderTile { tile: tile.clone() })
                .map_err(|e| SaveError::PartRestore {
                    name: "Render".into(),
                    msg: format!("failed to restore render tile: {e}"),
                })?;
        }
        for (name, text) in &saved.parts {
            let value =
                toml::from_str::<toml::Value>(text).map_err(|e| SaveError::PartRestore {
                    name: name.clone(),
                    msg: format!("invalid TOML snapshot: {e}"),
                })?;
            let part = hooks.restore(&PartTypeId::new(name.clone()), &value)?;
            broker
                .insert_part(entity, name.clone(), part)
                .map_err(|e| SaveError::PartRestore {
                    name: name.clone(),
                    msg: e.to_string(),
                })?;
        }
    }
    Ok(broker)
}

/// Serialize a `toml::Value` to the canonical TOML text stored in a save's
/// `(name, text)` part list. Values must be tables (part snapshots are config
/// style, matching the blueprint-load-time shape); non-table roots are not
/// valid TOML documents.
fn toml_value_to_text(value: &toml::Value) -> Result<String, SaveError> {
    if !value.is_table() {
        return Err(SaveError::PartRestore {
            name: "<part>".into(),
            msg: "part snapshot must be a TOML table (config-style)".into(),
        });
    }
    toml::to_string(value).map_err(|e| SaveError::PartRestore {
        name: "<part>".into(),
        msg: format!("toml serialization failed: {e}"),
    })
}

/// Encode a [`SaveData`] into the full save byte string (header + zstd frame).
pub fn save_to_bytes(data: &SaveData) -> Result<Vec<u8>, SaveError> {
    if data.version != SAVE_FORMAT_VERSION {
        return Err(SaveError::UnsupportedVersion {
            found: data.version,
            supported: SAVE_FORMAT_VERSION,
        });
    }
    let payload = postcard::to_stdvec(data)
        .map_err(|e| SaveError::Corrupt(format!("postcard serialize: {e}")))?;
    let compressed = zstd::stream::encode_all(payload.as_slice(), ZSTD_LEVEL)
        .map_err(|e| SaveError::Corrupt(format!("zstd encode: {e}")))?;

    let mut out = Vec::with_capacity(HEADER_LEN + compressed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&SAVE_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

/// Decode a full save byte string back into a [`SaveData`].
///
/// Defenses in order (never panics): header size, magic, version, the
/// pre-compression length check, then zstd and postcard errors → [`SaveError::Corrupt`].
pub fn load_from_bytes(bytes: &[u8]) -> Result<SaveData, SaveError> {
    if bytes.len() < HEADER_LEN {
        return Err(SaveError::Corrupt(format!(
            "truncated header: {} bytes (need at least {HEADER_LEN})",
            bytes.len()
        )));
    }
    if &bytes[0..4] != MAGIC {
        return Err(SaveError::BadMagic);
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().expect("4-byte slice"));
    if version != SAVE_FORMAT_VERSION {
        return Err(SaveError::UnsupportedVersion {
            found: version,
            supported: SAVE_FORMAT_VERSION,
        });
    }
    let declared_len = u32::from_le_bytes(bytes[8..12].try_into().expect("4-byte slice")) as usize;

    let compressed = &bytes[HEADER_LEN..];
    let payload = zstd::stream::decode_all(compressed)
        .map_err(|e| SaveError::Corrupt(format!("zstd decode: {e}")))?;
    if payload.len() != declared_len {
        return Err(SaveError::Corrupt(format!(
            "zstd payload length mismatch: declared {declared_len}, decoded {}",
            payload.len()
        )));
    }
    postcard::from_bytes(&payload).map_err(|e| SaveError::Corrupt(format!("postcard decode: {e}")))
}

/// The temp sibling of a save path (`path.tmp`), in the same directory so the
/// final rename is atomic (same volume).
fn tmp_path(path: &Path) -> PathBuf {
    path.with_extension("tmp")
}

/// The backup sibling of a save path (`path.bak`), produced by best-effort
/// rotation of the previous save before the new one lands.
fn bak_path(path: &Path) -> PathBuf {
    path.with_extension("bak")
}

/// Rename `from` onto `to`, replacing an existing `to`.
///
/// On Windows `std::fs::rename` uses `MoveFileExW` with replace-existing, but a
/// concurrent handle on the destination (antivirus/indexer scan) can make it
/// fail with a sharing violation, so the rename is retried up to 3 times with a
/// short pause between attempts. Unix needs no retry: a same-directory rename
/// is atomic and takes no lock on the destination.
fn rename_replacing(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        const ATTEMPTS: u32 = 3;
        const PAUSE: Duration = Duration::from_millis(50);
        let mut attempt = 0u32;
        loop {
            match fs::rename(from, to) {
                Ok(()) => return Ok(()),
                Err(_e) if attempt + 1 < ATTEMPTS => {
                    attempt += 1;
                    std::thread::sleep(PAUSE);
                }
                Err(e) => return Err(e),
            }
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}

/// Best-effort fsync of the directory containing `path`, the POSIX guarantee
/// that a completed rename survives power loss. Unix-only: there is no portable
/// Windows equivalent, and on Windows the NTFS journal provides the same
/// durability for the rename itself. Failures are logged, not returned — the
/// save data is already durable from the file `sync_all` in [`write_atomic`].
#[cfg(unix)]
fn sync_parent_dir(path: &Path) {
    let Some(parent) = path.parent() else { return };
    if parent.as_os_str().is_empty() {
        return;
    }
    if let Ok(dir) = File::open(parent)
        && let Err(e) = dir.sync_all()
    {
        tracing::warn!(path = %parent.display(), error = %e, "could not fsync save directory");
    }
}
/// Write `data` to `path` crash-safely: a process kill at any instant leaves
/// either the previous save or the new save intact and loadable, never a
/// half-written file.
///
/// Sequence (same-directory tmp → fsync → best-effort `.bak` rotation →
/// atomic rename → Unix parent-dir sync):
/// 1. write the full encoded save to `path.tmp` in the same directory, `flush`
///    and `sync_all` it;
/// 2. best-effort rotate any previous `path` to `path.bak` (failure only loses
///    the extra recovery copy — the tmp→final rename is still atomic);
/// 3. rename `path.tmp` over `path` (Windows: retried up to 3 times on a
///    sharing violation);
/// 4. Unix only: fsync the parent directory so the rename itself is durable.
///
/// On a final rename failure this returns [`SaveError::WriteFailed`] and leaves
/// the previous save and `.bak` intact; the leftover tmp is swept on next
/// startup. Encoding errors (e.g. an unsupported payload version) are returned
/// before any file is touched.
pub fn write_atomic(path: &Path, data: &SaveData) -> Result<(), SaveError> {
    let tmp = tmp_path(path);
    let bak = bak_path(path);

    let bytes = save_to_bytes(data)?;

    // 1. Write to the same-directory temp file (same volume ⇒ atomic rename).
    //    A stale tmp from a crashed previous run is truncated by `File::create`.
    {
        let mut file = File::create(&tmp).map_err(|e| SaveError::WriteFailed {
            path: tmp.clone(),
            source: e,
        })?;
        file.write_all(&bytes).map_err(|e| SaveError::WriteFailed {
            path: tmp.clone(),
            source: e,
        })?;
        file.flush().map_err(|e| SaveError::WriteFailed {
            path: tmp.clone(),
            source: e,
        })?;
        file.sync_all().map_err(|e| SaveError::WriteFailed {
            path: tmp.clone(),
            source: e,
        })?;
    }

    // 2. Best-effort rotation of the previous save to `.bak`.
    if path.exists()
        && let Err(e) = rename_replacing(path, &bak)
    {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "could not rotate previous save to .bak; the save itself is still atomic"
        );
    }

    // 3. Atomic tmp → final rename. If this fails after retries, the previous
    //    save (or its `.bak` if step 2 rotated) is left intact and the tmp
    //    remains for the next startup sweep.
    rename_replacing(&tmp, path).map_err(|e| SaveError::WriteFailed {
        path: path.to_path_buf(),
        source: e,
    })?;

    // 4. Unix parent-directory sync so the rename survives power loss.
    #[cfg(unix)]
    sync_parent_dir(path);

    Ok(())
}

/// Read and decode the save file at `path`.
///
/// There is no silent fallback: if the file at `path` is missing or fails to
/// decode, the error is returned as-is — a sibling `.bak` is never loaded
/// implicitly. A caller that wants the backup explicitly points at `path.bak`.
pub fn load(path: &Path) -> Result<SaveData, SaveError> {
    let bytes = fs::read(path).map_err(|e| SaveError::ReadFailed {
        path: path.to_path_buf(),
        source: e,
    })?;
    load_from_bytes(&bytes)
}

/// True if `name` matches the stale-temp pattern `*.tmp` / `*.tmp.*` left by a
/// crashed [`write_atomic`].
fn is_stale_tmp_name(name: &str) -> bool {
    name.ends_with(".tmp") || name.contains(".tmp.")
}

/// Remove stale temp files from `dir` — any regular file whose name matches the
/// `*.tmp` / `*.tmp.*` patterns. A `.bak` file (and everything else) is never
/// touched. Run this on startup before any load. Returns the number of files
/// removed; a missing directory is a no-op (`Ok(0)`).
pub fn sweep_stale_tmp(dir: &Path) -> std::io::Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0usize;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_stale_tmp_name(&name) {
            fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(name: &str, val: toml::Value) -> (String, String) {
        // Part snapshots are config-style TOML tables (as at blueprint-load time);
        // `toml::to_string` requires a table root.
        let table = toml::Value::Table(toml::map::Map::from_iter([("value".to_string(), val)]));
        (
            name.to_string(),
            toml::to_string(&table).expect("serialize"),
        )
    }

    fn sample_data() -> SaveData {
        SaveData {
            version: SAVE_FORMAT_VERSION,
            zone_id: "start".to_string(),
            next_entity_bits: vec![1, 2, 3],
            entities: vec![
                SavedEntity {
                    bits: 1,
                    render: None,
                    parts: vec![
                        part(
                            "Position",
                            toml::Value::Table(toml::map::Map::from_iter([
                                ("x".into(), toml::Value::Integer(10)),
                                ("y".into(), toml::Value::Integer(4)),
                            ])),
                        ),
                        part(
                            "Health",
                            toml::Value::Table(toml::map::Map::from_iter([(
                                "hp".into(),
                                toml::Value::Integer(7),
                            )])),
                        ),
                    ],
                },
                SavedEntity {
                    bits: 2,
                    render: None,
                    parts: vec![part("ActorEnergy", toml::Value::Integer(100))],
                },
            ],
            player: Some(1),
            clock: 42,
            actors: vec![(1, 0, 10), (2, -5, 8)],
            rng: SimRng::from_seed(1234),
            player_turns: 6,
            facing: (0, -1),
        }
    }

    #[test]
    fn populated_payload_round_trips() {
        let original = sample_data();
        let bytes = save_to_bytes(&original).expect("save");
        // Header: magic + version + declared payload length.
        assert_eq!(&bytes[0..4], b"TESS");
        assert!(bytes.len() > HEADER_LEN);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            SAVE_FORMAT_VERSION
        );
        assert!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()) > 0);

        let loaded = load_from_bytes(&bytes).expect("load");
        assert_eq!(loaded, original);
        assert_eq!(loaded.rng, original.rng);
        assert_eq!(loaded.entities, original.entities);

        // Each part's TOML text parses back to the original toml::Value as a document.
        let value_text = &loaded.entities[0].parts[0].1;
        let parsed: toml::Value = toml::from_str(value_text).expect("parse TOML");
        assert_eq!(
            parsed,
            toml::Value::Table(toml::map::Map::from_iter([(
                "value".into(),
                toml::Value::Table(toml::map::Map::from_iter([
                    ("x".into(), toml::Value::Integer(10)),
                    ("y".into(), toml::Value::Integer(4)),
                ]))
            )]))
        );
    }

    #[test]
    fn bad_magic_is_rejected_without_panic() {
        let mut bytes = save_to_bytes(&sample_data()).expect("save");
        bytes[0..4].copy_from_slice(b"XXXX");
        match load_from_bytes(&bytes) {
            Err(SaveError::BadMagic) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_version_is_rejected_without_panic() {
        let mut bytes = save_to_bytes(&sample_data()).expect("save");
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        match load_from_bytes(&bytes) {
            Err(SaveError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, 99);
                assert_eq!(supported, SAVE_FORMAT_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn truncated_bytes_are_corrupt_without_panic() {
        let bytes = save_to_bytes(&sample_data()).expect("save");
        for cut in [0usize, 4, 11, 12, bytes.len() - 3] {
            let truncated = &bytes[..cut];
            match load_from_bytes(truncated) {
                Err(SaveError::Corrupt(_)) => {}
                other => panic!("cut={cut}: expected Corrupt, got {other:?}"),
            }
            // The error must not panic; reaching here is the assertion.
        }
    }

    #[test]
    fn corrupt_frame_is_corrupt_without_panic() {
        let mut bytes = save_to_bytes(&sample_data()).expect("save");
        // Flip a byte inside the zstd frame (past the header).
        let frame = bytes.len() - 4;
        bytes[frame] ^= 0xFF;
        match load_from_bytes(&bytes) {
            Err(SaveError::Corrupt(_)) => {}
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn declared_length_mismatch_is_corrupt() {
        let mut bytes = save_to_bytes(&sample_data()).expect("save");
        // Claim a shorter payload than actually present -> length check trips.
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        match load_from_bytes(&bytes) {
            Err(SaveError::Corrupt(msg)) => {
                assert!(msg.contains("length"), "unhelpful message: {msg}");
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn save_rejects_wrong_version_in_payload() {
        let mut data = sample_data();
        data.version = 99;
        assert!(matches!(
            save_to_bytes(&data),
            Err(SaveError::UnsupportedVersion { .. })
        ));
    }

    // ---- snapshot hooks + broker snapshot/restore ----------------------

    /// A PartRegistry with the engine-native parts plus a script part schema,
    /// mirroring the shipped demo (Position/Health/ActorEnergy/Attributes/
    /// Level/Bonuses native; `Charge` a script part).
    fn demo_registry() -> PartRegistry {
        use crate::ecs::StorageClass;
        use crate::script_part::{PartValue, ScriptPartType};
        fn build<T>(v: &toml::Value) -> Result<Box<dyn Part>, PartError>
        where
            T: serde::de::DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
        {
            let t: T =
                v.clone()
                    .try_into()
                    .map_err(|e: toml::de::Error| PartError::BuildFailed {
                        name: std::any::type_name::<T>().to_string(),
                        msg: e.to_string(),
                    })?;
            Ok(Box::new(t))
        }
        let mut reg = PartRegistry::new();
        reg.register(PART_POSITION, StorageClass::Dynamic, build::<Position>)
            .unwrap();
        reg.register(PART_HEALTH, StorageClass::Dynamic, build::<Health>)
            .unwrap();
        reg.register(PART_ENERGY, StorageClass::Dynamic, build::<ActorEnergy>)
            .unwrap();
        reg.register(PART_ATTRIBUTES, StorageClass::Dynamic, build::<Attributes>)
            .unwrap();
        reg.register(PART_LEVEL, StorageClass::Dynamic, build::<Level>)
            .unwrap();
        reg.register(PART_BONUSES, StorageClass::Dynamic, build::<Bonuses>)
            .unwrap();
        let mut schema = BTreeMap::new();
        schema.insert(
            "charges".to_string(),
            crate::script_part::FieldDef::int()
                .default(PartValue::Int(1))
                .min(0)
                .max(99),
        );
        reg.register_script_part(ScriptPartType::new("Charge", schema))
            .unwrap();
        reg
    }

    fn demo_hooks() -> SaveHooks {
        build_save_hooks(&demo_registry()).0
    }

    #[test]
    fn whitelist_hooks_round_trip_native_parts() {
        let hooks = demo_hooks();
        let id_pos = PartTypeId::new(PART_POSITION);
        let pos: Box<dyn Part> = Box::new(Position { x: 10, y: 4 });
        let v = hooks.snapshot(&id_pos, pos.as_ref()).unwrap();
        let restored = hooks.restore(&id_pos, &v).unwrap();
        assert_eq!(
            restored.as_ref().as_any().downcast_ref::<Position>(),
            Some(&Position { x: 10, y: 4 })
        );

        let id_hp = PartTypeId::new(PART_HEALTH);
        let hp: Box<dyn Part> = Box::new(Health { hp: 7 });
        let v = hooks.snapshot(&id_hp, hp.as_ref()).unwrap();
        let restored = hooks.restore(&id_hp, &v).unwrap();
        assert_eq!(
            restored.as_ref().as_any().downcast_ref::<Health>(),
            Some(&Health { hp: 7 })
        );

        let id_energy = PartTypeId::new(PART_ENERGY);
        let e: Box<dyn Part> = Box::new(ActorEnergy {
            speed: 100,
            energy: -500,
        });
        let v = hooks.snapshot(&id_energy, e.as_ref()).unwrap();
        let restored = hooks.restore(&id_energy, &v).unwrap();
        assert_eq!(
            restored.as_ref().as_any().downcast_ref::<ActorEnergy>(),
            Some(&ActorEnergy {
                speed: 100,
                energy: -500
            })
        );
    }

    #[test]
    fn whitelist_hooks_round_trip_stat_parts_and_enforce_bounds() {
        use crate::stats::DerivedStat;
        let hooks = demo_hooks();

        // Attributes round-trip exactly.
        let id = PartTypeId::new(PART_ATTRIBUTES);
        let attrs: Box<dyn Part> = Box::new(Attributes {
            might: 14,
            agility: 9,
            toughness: 13,
            will: 8,
            intellect: 11,
        });
        let v = hooks.snapshot(&id, attrs.as_ref()).unwrap();
        assert_eq!(
            v.as_table()
                .unwrap()
                .get("might")
                .and_then(|f| f.as_integer()),
            Some(14),
            "snapshot is the canonical table shape"
        );
        let restored = hooks.restore(&id, &v).unwrap();
        assert_eq!(
            restored.as_ref().as_any().downcast_ref::<Attributes>(),
            Some(&Attributes {
                might: 14,
                agility: 9,
                toughness: 13,
                will: 8,
                intellect: 11
            })
        );

        // Restore enforces the load-time bounds: an out-of-range payload fails
        // closed rather than being clamped or accepted silently.
        let bad: toml::Value =
            toml::from_str("might = 14\nagility = 9\ntoughness = 99\nwill = 8\nintellect = 11\n")
                .unwrap();
        let err = match hooks.restore(&id, &bad) {
            Ok(_) => panic!("expected restore to fail closed on out-of-range toughness"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("toughness"),
            "bounds error names the field: {err}"
        );

        // Level round-trips and rejects out-of-range restores.
        let id = PartTypeId::new(PART_LEVEL);
        let lvl: Box<dyn Part> = Box::new(Level { level: 3 });
        let v = hooks.snapshot(&id, lvl.as_ref()).unwrap();
        let restored = hooks.restore(&id, &v).unwrap();
        assert_eq!(
            restored.as_ref().as_any().downcast_ref::<Level>(),
            Some(&Level { level: 3 })
        );
        let bad: toml::Value = toml::from_str("level = 0\n").unwrap();
        assert!(hooks.restore(&id, &bad).is_err());

        // Bonuses round-trips; missing fields restore to defaults (0).
        let id = PartTypeId::new(PART_BONUSES);
        let b: Box<dyn Part> = Box::new(Bonuses {
            accuracy: 3,
            dodge: 0,
            ac: 2,
            damage_flat: 0,
            block: 0,
        });
        let v = hooks.snapshot(&id, b.as_ref()).unwrap();
        let restored = hooks.restore(&id, &v).unwrap();
        assert_eq!(
            restored.as_ref().as_any().downcast_ref::<Bonuses>(),
            Some(&Bonuses {
                accuracy: 3,
                dodge: 0,
                ac: 2,
                damage_flat: 0,
                block: 0
            })
        );
        // The canonical snapshot spells all five keys; a partial payload is a
        // legal authored shape (zeros fill in) but the engine writes the full
        // table.
        let partial: toml::Value = toml::from_str("accuracy = 4\n").unwrap();
        let restored = hooks.restore(&id, &partial).unwrap();
        assert_eq!(
            restored.as_ref().as_any().downcast_ref::<Bonuses>(),
            Some(&Bonuses {
                accuracy: 4,
                dodge: 0,
                ac: 0,
                damage_flat: 0,
                block: 0
            })
        );

        // The stat vocabularies parse the names the fixtures carry.
        assert_eq!(DerivedStat::from_str("max_hp"), Some(DerivedStat::MaxHp));
        assert_eq!(
            DerivedStat::from_str("accuracy"),
            Some(DerivedStat::Accuracy)
        );
        assert_eq!(DerivedStat::from_str("hp"), None, "not a stat name");
    }

    #[test]
    fn legacy_stats_part_is_not_whitelisted_and_rejected() {
        // The removed part: a save carrying `Stats` must fail closed on restore
        // (the whitelist no longer knows the name), never silently truncate.
        let hooks = demo_hooks();
        let stale: toml::Value = toml::from_str("strength = 18\n").unwrap();
        match hooks.restore(&PartTypeId::new("Stats"), &stale) {
            Err(SaveError::UnserializablePart { name }) => assert_eq!(name, "Stats"),
            Ok(_) => panic!("expected UnserializablePart for legacy Stats, got Ok with a part",),
            Err(other) => panic!("expected UnserializablePart, got {other:?}"),
        }
        // And the demo registry no longer declares the name at all.
        let reg = demo_registry();
        assert_eq!(
            reg.storage(&PartTypeId::new("Stats")),
            None,
            "legacy Stats must not be a registered part"
        );
    }

    #[test]
    fn derived_cache_is_excluded_from_snapshots() {
        // An entity carrying a live `Derived` cache snapshots cleanly, with the
        // cache silently skipped (never UnserializablePart, never persisted).
        let hooks = demo_hooks();
        let mut broker = EntityBroker::new();
        let e = broker.spawn();
        broker
            .insert_part(
                e,
                PART_POSITION,
                Box::new(Position { x: 1, y: 1 }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .insert_part(
                e,
                PART_DERIVED,
                Box::new(crate::effects::Derived {
                    values: BTreeMap::from([("max_hp".to_string(), 36)]),
                }) as Box<dyn Part>,
            )
            .unwrap();

        let snapshot = snapshot_entities(&broker, &hooks).unwrap();
        let saved_parts = &snapshot.entities[0].parts;
        assert!(
            saved_parts.iter().any(|(n, _)| n == PART_POSITION),
            "position survives the snapshot"
        );
        assert!(
            !saved_parts.iter().any(|(n, _)| n == "Derived"),
            "Derived cache must be silently excluded, got {saved_parts:?}"
        );
        assert!(
            NON_PERSISTENT_PARTS.contains(&"Derived"),
            "skip-list is the explicit record of non-persistent parts"
        );
    }

    #[test]
    fn v1_save_is_rejected_as_unsupported() {
        // The first format bump: a format-1 save (pre-stats) loads as
        // UnsupportedVersion { found: 1, supported: 2 } — rejected, not
        // guessed at.
        let mut bytes = save_to_bytes(&sample_data()).expect("save");
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
        match load_from_bytes(&bytes) {
            Err(SaveError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, 1);
                assert_eq!(supported, SAVE_FORMAT_VERSION);
                assert_eq!(supported, 2, "the constant is the named bump");
            }
            other => panic!("expected UnsupportedVersion for v1, got {other:?}"),
        }
    }

    #[test]
    fn script_part_hook_round_trips_closed_fields() {
        let hooks = demo_hooks();
        let id = PartTypeId::new("Charge");
        let mut sp = ScriptPart::default();
        sp.insert("charges".into(), PartValue::Int(3));
        let boxed: Box<dyn Part> = Box::new(sp);
        let v = hooks.snapshot(&id, boxed.as_ref()).unwrap();
        let restored = hooks.restore(&id, &v).unwrap();
        let back = restored
            .as_ref()
            .as_any()
            .downcast_ref::<ScriptPart>()
            .unwrap();
        assert_eq!(back.get("charges"), Some(&PartValue::Int(3)));
        // Restore is by whitelisted name: an unregistered script name fails closed.
        let stale = PartTypeId::new("GoneScript");
        assert!(matches!(
            hooks.restore(&stale, &v),
            Err(SaveError::UnserializablePart { .. })
        ));
    }

    #[test]
    fn unknown_part_fails_closed_on_snapshot() {
        let hooks = demo_hooks();
        // A part type with no registered hook: the whitelist has no entry.
        // (Any `'static + Send + Sync` type is a `Part` via the blanket impl.)
        let unwhitelisted: Box<dyn Part> = Box::new(37u32);
        let id = PartTypeId::new("Unwhitelisted");
        match hooks.snapshot(&id, unwhitelisted.as_ref()) {
            Err(SaveError::UnserializablePart { name }) => assert_eq!(name, "Unwhitelisted"),
            other => panic!("expected UnserializablePart, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_and_restore_broker_preserves_ids_and_parts() {
        let hooks = demo_hooks();
        let mut broker = EntityBroker::new();
        let player = broker.spawn();
        broker
            .insert_part(
                player,
                PART_POSITION,
                Box::new(Position { x: 2, y: 1 }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .insert_part(
                player,
                PART_HEALTH,
                Box::new(Health { hp: 5 }) as Box<dyn Part>,
            )
            .unwrap();
        let goblin = broker.spawn();
        broker
            .insert_part(
                goblin,
                PART_POSITION,
                Box::new(Position { x: 5, y: 5 }) as Box<dyn Part>,
            )
            .unwrap();
        let mut sp = ScriptPart::default();
        sp.insert("charges".into(), PartValue::Int(2));
        broker.insert_part(goblin, "Charge", Box::new(sp)).unwrap();

        // Snapshot and rebuild a fresh broker from the snapshot.
        let snapshot = snapshot_entities(&broker, &hooks).unwrap();
        assert_eq!(snapshot.entities.len(), 2, "both live entities captured");
        assert_eq!(
            snapshot.next_entity_bits,
            vec![player.to_bits().get(), goblin.to_bits().get()],
            "entity ids captured in deterministic order"
        );

        let data = SaveData {
            version: SAVE_FORMAT_VERSION,
            zone_id: "test".into(),
            next_entity_bits: snapshot.next_entity_bits.clone(),
            entities: snapshot.entities,
            player: Some(player.to_bits().get()),
            clock: 100,
            actors: vec![(player.to_bits().get(), 5, 100)],
            rng: SimRng::from_seed(1),
            player_turns: 0,
            facing: (1, 0),
        };
        let restored = restore_broker(&data, &hooks).unwrap();

        // Deterministic entity-id ordering preserved: same ids in same order.
        let ids: Vec<u64> = restored
            .entities()
            .into_iter()
            .map(|e| e.to_bits().get())
            .collect();
        assert_eq!(
            ids,
            vec![player.to_bits().get(), goblin.to_bits().get()],
            "ids restored at exact saved bits, in saved order"
        );
        // Parts restored exactly.
        assert_eq!(
            restored
                .typed_part::<Position>(restored.entities()[0], &PartTypeId::new(PART_POSITION)),
            Some(&Position { x: 2, y: 1 })
        );
        assert_eq!(
            restored.typed_part::<Health>(restored.entities()[0], &PartTypeId::new(PART_HEALTH)),
            Some(&Health { hp: 5 })
        );
        let sp = restored
            .typed_part::<ScriptPart>(restored.entities()[1], &PartTypeId::new("Charge"))
            .unwrap();
        assert_eq!(sp.get("charges"), Some(&PartValue::Int(2)));
    }

    #[test]
    fn restore_broker_unknown_part_fails_closed() {
        let hooks = demo_hooks();
        let mut broker = EntityBroker::new();
        let e = broker.spawn();
        broker
            .insert_part(e, PART_POSITION, Box::new(Position { x: 0, y: 0 }))
            .unwrap();
        let snapshot = snapshot_entities(&broker, &hooks).unwrap();

        // Simulate a save written by a build with an extra, unknown part.
        let mut data = SaveData {
            version: SAVE_FORMAT_VERSION,
            zone_id: "test".into(),
            next_entity_bits: snapshot.next_entity_bits.clone(),
            entities: snapshot.entities,
            player: Some(e.to_bits().get()),
            clock: 0,
            actors: Vec::new(),
            rng: SimRng::from_seed(1),
            player_turns: 0,
            facing: (1, 0),
        };
        data.entities[0].parts.push((
            "FuturePart".to_string(),
            toml::to_string(&toml::Value::Table(toml::map::Map::from_iter([(
                "value".into(),
                toml::Value::Integer(1),
            )])))
            .unwrap(),
        ));
        match restore_broker(&data, &hooks) {
            Err(SaveError::UnserializablePart { name }) => assert_eq!(name, "FuturePart"),
            Ok(b) => panic!(
                "expected UnserializablePart, got Ok with {} entity(s)",
                b.entities().len()
            ),
            Err(other) => panic!("expected UnserializablePart, got {other:?}"),
        }
    }
}
