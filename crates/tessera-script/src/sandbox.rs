//! Sandbox setup and deterministic environment hardening.
//!
//! Builds a per-script Luau VM with:
//! - safe stdlib + `Lua::sandbox(true)` (libs/metatables/globals read-only,
//!   safeenv on, `collectgarbage` restricted to `count`),
//! - `os.time / os.date / os.clock / os.difftime` nil'd (no wall-clock),
//! - `math.random` replaced by a host-owned, forked deterministic RNG stream
//!   and `math.randomseed` removed,
//! - an instruction-budget interrupt (aborts infinite loops with an error,
//!   never a panic),
//! - a memory cap producing [`mlua::Error::MemoryError`] on exceed.
//!
//! Order matters: the environment-stripping steps (os time, math.random) must
//! run *before* `Lua::sandbox(true)`, because sandboxing freezes libraries and
//! globals read-only. The interrupt and memory cap are installed after.

use std::sync::Arc;

use mlua::{Error, Lua, Result as LuaResult, VmState};

use crate::host::{HostRng, rng_call};
use crate::{SCRIPT_INSTRUCTION_BUDGET, SCRIPT_MEMORY_LIMIT};

/// Shared instruction counter for the interrupt callback. Counts *down* from
/// the budget; hitting zero aborts the running chunk with an error.
#[derive(Debug, Default)]
pub struct BudgetCounter(Arc<std::sync::atomic::AtomicU64>);

impl BudgetCounter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn reset(&self) {
        self.0.store(
            SCRIPT_INSTRUCTION_BUDGET,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    pub fn remaining(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Create a new, sandboxed Luau VM for one script, with the deterministic RNG
/// stream, memory limit and instruction-budget interrupt installed.
pub fn new_sandbox(
    counter: Arc<std::sync::atomic::AtomicU64>,
    rng: Arc<std::sync::Mutex<HostRng>>,
) -> LuaResult<Lua> {
    let lua = Lua::new();
    // Strip wall-clock and swap in the deterministic RNG *before* the sandbox
    // freezes everything read-only.
    strip_wall_clock(&lua)?;
    configure_math(&lua, rng)?;
    lua.sandbox(true)?;
    lua.set_memory_limit(SCRIPT_MEMORY_LIMIT)?;
    let counter = Arc::clone(&counter);
    lua.set_interrupt(move |_| {
        // Count down toward zero and stop there: the counter never underflows,
        // so `remaining()` stays a meaningful "instructions left" and the
        // reported `Budget.used` (budget - remaining) is real, not a wraparound.
        // (single-threaded VM, so the CAS loop below never contends).
        let hit_zero = counter
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |v| if v == 0 { None } else { Some(v - 1) },
            )
            .is_err();
        if hit_zero {
            return Err(Error::RuntimeError(
                "tessera: instruction budget exceeded".into(),
            ));
        }
        Ok(VmState::Continue)
    });
    Ok(lua)
}

/// Nil out the nondeterministic `os` time accessors before the sandbox freeze.
fn strip_wall_clock(lua: &Lua) -> LuaResult<()> {
    let globals = lua.globals();
    if let Ok(os) = globals.get::<mlua::Table>("os") {
        for name in ["time", "date", "clock", "difftime"] {
            os.set(name, mlua::Value::Nil)?;
        }
    }
    Ok(())
}

/// Replace `math.random` with a deterministic host-owned RNG and remove
/// `math.randomseed`. Must run before `Lua::sandbox(true)` (read-only freeze).
pub fn configure_math(lua: &Lua, stream: Arc<std::sync::Mutex<HostRng>>) -> LuaResult<()> {
    let globals = lua.globals();
    if let Ok(math) = globals.get::<mlua::Table>("math") {
        let stream = Arc::clone(&stream);
        let random = lua.create_function(move |_lua, args: mlua::MultiValue| {
            let mut s = stream
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("rng lock".into()))?;
            rng_call(&mut s, args).map_err(|e| mlua::Error::RuntimeError(e.to_string()))
        })?;
        math.set("random", random)?;
        math.set("randomseed", mlua::Value::Nil)?;
    }
    Ok(())
}

/// Extract the instruction budget error message marker. Returns `true` when an
/// [`mlua::Error::RuntimeError`] is the budget sentinel this crate installs.
pub fn is_budget_error(err: &Error) -> bool {
    matches!(err, Error::RuntimeError(m) if m.starts_with("tessera: instruction budget exceeded"))
}

/// Read the vendored Luau version string as reported by the VM `_VERSION`
/// global (e.g. `"Luau 740"`). Recorded in build/CI output for
/// reproducibility (spec §2.2, §8.2) so `luau0-src` drift is visible between
/// cache misses.
pub fn luau_version(lua: &Lua) -> String {
    lua.globals()
        .get::<String>("_VERSION")
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_luau_version_is_reportable() {
        let lua = Lua::new();
        let v = luau_version(&lua);
        assert!(
            v.starts_with("Luau "),
            "expected a 'Luau <version>' string, got {v:?}"
        );
    }
}
