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

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::rng::SimRng;

/// Current save format version. Public so callers and the console can name it.
pub const SAVE_FORMAT_VERSION: u32 = 1;

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
    /// `(part type name, toml::Value as canonical TOML text)`.
    pub parts: Vec<(String, String)>,
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
                Err(e) if attempt + 1 < ATTEMPTS => {
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
    if let Ok(dir) = File::open(parent) {
        if let Err(e) = dir.sync_all() {
            tracing::warn!(path = %parent.display(), error = %e, "could not fsync save directory");
        }
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
    if path.exists() {
        if let Err(e) = rename_replacing(path, &bak) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not rotate previous save to .bak; the save itself is still atomic"
            );
        }
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
}
