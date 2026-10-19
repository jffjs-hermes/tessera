//! Manifest load diagnostics. Every diagnostic names the manifest path and,
//! when known, the mod id so a mod author can locate the defect (matching the
//! engine-wide `file + id` convention used by the blueprint loader).

use thiserror::Error;

/// A single failure parsing or validating one `mod.toml`.
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("invalid manifest for '{id}' ({path}): {msg}")]
    Invalid {
        path: String,
        id: String,
        msg: String,
    },
}

impl ManifestError {
    /// A validation error where the mod id is already known.
    pub fn invalid(path: impl Into<String>, id: impl Into<String>, msg: impl Into<String>) -> Self {
        ManifestError::Invalid {
            path: path.into(),
            id: id.into(),
            msg: msg.into(),
        }
    }

    /// A validation error for a manifest that failed before (or independent
    /// of) a valid id was established — the id field defaults to the raw
    /// `id` value seen, or an empty string when absent.
    pub fn invalid_pre_id(path: impl Into<String>, id: &str, msg: impl Into<String>) -> Self {
        ManifestError::Invalid {
            path: path.into(),
            id: id.to_string(),
            msg: msg.into(),
        }
    }
}
