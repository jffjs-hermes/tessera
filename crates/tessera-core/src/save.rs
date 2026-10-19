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
