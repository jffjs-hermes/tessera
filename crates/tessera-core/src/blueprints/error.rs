//! Load and resolution diagnostics. Every diagnostic names the file and, when
//! relevant, the blueprint id so a content author can locate the defect.

use std::fmt;

use thiserror::Error;

/// A single failure in a load/resolution pass, always `file`-scoped and, where
/// possible, `id`-scoped.
#[derive(Debug, Error)]
pub enum BlueprintError {
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
    #[error("invalid blueprint '{id}' ({path}): {msg}")]
    Invalid {
        path: String,
        id: String,
        msg: String,
    },
    #[error("unknown parent '{parent}' for '{id}' ({path})")]
    UnknownParent {
        path: String,
        id: String,
        parent: String,
    },
    #[error(
        "inheritance cycle: {chain} (defined at {path}:{id}, cycle entered at {entered_path}:{entered_id})"
    )]
    Cycle {
        path: String,
        id: String,
        chain: String,
        entered_path: String,
        entered_id: String,
    },
    #[error("part '{part}' of blueprint '{id}' ({path}) failed to deserialize: {msg}")]
    BadPart {
        path: String,
        id: String,
        part: String,
        msg: String,
    },
    #[error("no blueprint '{id}'")]
    NoId { id: String },
    #[error("blueprint '{id}' ({path}) has no part '{part}'")]
    NoPart {
        path: String,
        id: String,
        part: String,
    },
    #[error("parsing patch file {file}: {source}")]
    PatchParse {
        file: String,
        source: toml::de::Error,
    },
    #[error("patch file {file} targets unknown blueprint '{id}'")]
    PatchTargetMissing { file: String, id: String },
    #[error("replace op on '{path}' failed: no value to replace in blueprint '{id}' ({file})")]
    ReplaceTargetMissing {
        file: String,
        id: String,
        path: String,
    },
    #[error(
        "merge op on '{path}' failed: intermediate target missing in blueprint '{id}' ({file})"
    )]
    MergeTargetMissing {
        file: String,
        id: String,
        path: String,
    },
    #[error("remove op on '{path}' failed: no value to remove in blueprint '{id}' ({file})")]
    RemoveTargetMissing {
        file: String,
        id: String,
        path: String,
    },
    #[error("invalid patch path '{path}' in blueprint '{id}' ({file}): {msg}")]
    InvalidPatchPath {
        file: String,
        id: String,
        path: String,
        msg: String,
    },
}

/// A failed load pass carrying every `file + id` diagnostic collected so a typo
/// doesn't hide other errors. The whole batch is rejected on any error
/// (fail-closed hot reload keeps the previous library on such a failure).
#[derive(Debug)]
pub struct LoadFail {
    pub diagnostics: Vec<BlueprintError>,
}

impl LoadFail {
    pub fn new(diagnostics: Vec<BlueprintError>) -> Self {
        LoadFail { diagnostics }
    }

    pub fn single(e: BlueprintError) -> Self {
        LoadFail {
            diagnostics: vec![e],
        }
    }
}

impl fmt::Display for LoadFail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.diagnostics.as_slice() {
            [] => write!(f, "blueprint load failed"),
            [single] => write!(f, "{single}"),
            many => {
                writeln!(f, "{} blueprint diagnostics:", many.len())?;
                for d in many {
                    writeln!(f, "  - {d}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for LoadFail {}
