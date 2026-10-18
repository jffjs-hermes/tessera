//! Script runtime error taxonomy and non-panicking propagation.
//!
//! Every failure a script can encounter is a [`ScriptError`]; nothing in this
//! crate ever unwinds the caller with a panic because of script content. All
//! `mlua` errors are mapped here at the boundary (see [`From<mlua::Error>`]).

/// A runtime content error surfaced by the script host. Scripts are
/// semi-trusted mod content: errors are *runtime bugs* logged and counted, and
/// the quarantine policy (three consecutive failures per `(script, event)`)
/// decides what stops — never a panic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptError {
    #[error("script '{script}' failed to load: {msg}")]
    Load { script: String, msg: String },
    #[error("script '{script}' misused the API at load time: {msg}")]
    Register { script: String, msg: String },
    #[error("handler for '{event}' (script '{script}', entity {entity}) errored: {msg}")]
    Handler {
        script: String,
        event: String,
        entity: u64,
        msg: String,
    },
    #[error(
        "script '{script}' exceeded the instruction budget during {phase} ({used} instructions)"
    )]
    Budget {
        script: String,
        phase: String,
        used: u64,
    },
    #[error("script '{script}' exceeded the memory limit")]
    Memory { script: String },
    #[error("part '{name}' on entity {entity} is immutable from scripts")]
    ImmutablePart { entity: u64, name: String },
    #[error("unknown part '{name}'")]
    UnknownPart { name: String },
    #[error("unknown entity {entity}")]
    UnknownEntity { entity: u64 },
    #[error("unknown event '{event}'")]
    UnknownEvent { event: String },
    #[error("internal scripting error: {0}")]
    Internal(String),
}

impl From<mlua::Error> for ScriptError {
    fn from(err: mlua::Error) -> Self {
        match err {
            mlua::Error::SyntaxError { message, .. } => {
                ScriptError::Internal(format!("lua syntax: {message}"))
            }
            mlua::Error::RuntimeError(msg) => ScriptError::Internal(msg),
            mlua::Error::MemoryError(_) => ScriptError::Internal("lua out of memory".into()),
            mlua::Error::SafetyError(msg) => ScriptError::Internal(format!("lua safety: {msg}")),
            other => ScriptError::Internal(other.to_string()),
        }
    }
}

impl ScriptError {
    /// Rough phase classification used when a budget/memory error fires and we
    /// do not yet know whether it was at load or at handler time.
    pub fn phase(&self) -> &str {
        match self {
            ScriptError::Load { .. } => "load",
            ScriptError::Register { .. } => "register",
            ScriptError::Handler { .. } => "handler",
            ScriptError::Budget { phase, .. } => phase,
            ScriptError::Memory { .. } => "handler",
            ScriptError::ImmutablePart { .. } => "handler",
            ScriptError::UnknownPart { .. } => "handler",
            ScriptError::UnknownEntity { .. } => "handler",
            ScriptError::UnknownEvent { .. } => "register",
            ScriptError::Internal(_) => "handler",
        }
    }

    /// True when a failed dispatch of this kind should be treated as the
    /// script's own fault (eligible for quarantine counting). Structural
    /// errors like `Register`/`UnknownEvent` are load-time defects and are
    /// reported, not quarantined.
    pub fn is_quarantinable(&self) -> bool {
        matches!(
            self,
            ScriptError::Handler { .. }
                | ScriptError::Budget { .. }
                | ScriptError::Memory { .. }
                | ScriptError::ImmutablePart { .. }
        )
    }
}
