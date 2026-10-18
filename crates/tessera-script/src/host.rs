//! The script host: owns the Luau VM(s), loads scripts, keeps the handler
//! registry, and runs the deterministic dispatch seam.
//!
//! # Value boundary
//!
//! Nothing from the ECS crosses into Luau as a borrow. When a handler runs the
//! host builds a **read snapshot** (plain `PartValue`s) the script may inspect
//! and a **pending-action list** that `engine.set_part`/`spawn`/`despawn`
//! append to; those are applied only after the handler returns, mirroring the
//! engine's post-`Move` commit pattern (§3.3). Engine API closures capture a
//! [`DispatchSlot`] and never hold a Rust borrow of the world across a Luau
//! call — nested mutable reentrancy is therefore impossible to express.
//!
//! # Budget carry
//!
//! A single shared instruction counter is reset only at the top of a dispatch,
//! so a handler that calls back into the VM through an engine API keeps
//! consuming the *same* budget (§3.2). Exceeding it yields
//! [`ScriptError::Budget`] — never a panic.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, MultiValue, Table, Value};
use tessera_core::script_part::PartValue;

use crate::error::ScriptError;
use crate::{QUARANTINE_THRESHOLD, sandbox, value};

/// Host-owned deterministic RNG stream backing `math.random` / `engine.rng`.
#[derive(Default)]
pub struct HostRng {
    state: u64,
}

impl HostRng {
    pub fn from_seed(seed: u64) -> Self {
        Self {
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        }
    }
    fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x as u32
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next_u32() as u64 % n
    }
    /// Derive an independent per-label stream (forked) so calling RNG in one
    /// label never perturbs another.
    pub fn fork(&mut self, label: &str) -> HostRng {
        let mut acc: u64 = 0x517c_c1b7_2722_0a95;
        for b in label.bytes() {
            acc = acc
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(b as u64);
        }
        Self::from_seed(self.state ^ acc)
    }
}

/// `math.random(a?, b?)` shim — deterministic integer results only.
pub(crate) fn rng_call(rng: &mut HostRng, args: MultiValue) -> Result<i64, ScriptError> {
    let vals: Vec<Value> = args.into_iter().collect();
    match vals.as_slice() {
        [] => Ok((rng.next_u32() & 1) as i64),
        [a] => {
            let a = coerce_int(a)?;
            Ok(1 + rng.below(a as u64) as i64)
        }
        [a, b] => {
            let a = coerce_int(a)?;
            let b = coerce_int(b)?;
            if b < a {
                return Err(ScriptError::Internal("math.random(a,b): b < a".into()));
            }
            let span = (b - a + 1) as u64;
            Ok(a + rng.below(span) as i64)
        }
        _ => Err(ScriptError::Internal(
            "math.random expects 0..2 args".into(),
        )),
    }
}

fn coerce_int(v: &Value) -> Result<i64, ScriptError> {
    match v {
        Value::Integer(i) => Ok(*i),
        Value::Number(f) => Ok(*f as i64),
        _ => Err(ScriptError::Internal("expected integer".into())),
    }
}

/// A queued mutation produced by a running handler, applied to the world only
/// after the handler returns.
#[derive(Debug, Clone)]
pub enum PendingAction {
    SetPart {
        entity: u64,
        name: String,
        fields: BTreeMap<String, PartValue>,
    },
    Spawn {
        blueprint: String,
    },
    Despawn {
        entity: u64,
    },
}

/// A read snapshot: entity id -> part name -> field name -> value.
pub type PartSnapshot = BTreeMap<u64, BTreeMap<String, BTreeMap<String, PartValue>>>;

/// Shared value-boundary slot: read snapshot and pending-action buffer for the
/// handler currently running. Accessors clone to avoid borrow/lifetime issues
/// at the closure boundary (the v1 scale is tiny).
#[derive(Default)]
pub struct DispatchSlot {
    inner: Mutex<PartSnapshot>,
    pending: Mutex<Vec<PendingAction>>,
}

impl DispatchSlot {
    pub fn begin(&self, snapshot: PartSnapshot) {
        *self.inner.lock().expect("slot lock") = snapshot;
        self.pending.lock().expect("slot lock").clear();
    }
    pub fn finish(&self) -> Vec<PendingAction> {
        self.pending.lock().expect("slot lock").drain(..).collect()
    }
    pub fn pending_mut(&self) -> std::sync::MutexGuard<'_, Vec<PendingAction>> {
        self.pending.lock().expect("slot lock")
    }
    /// Owned clone of the snapshot (deterministic, small at v1 scale).
    pub fn snapshot(&self) -> PartSnapshot {
        self.inner.lock().expect("slot lock").clone()
    }
}

/// Shared handles installed into every engine closure (all `Send + Sync`).
#[derive(Clone)]
struct SharedHandles {
    registry: Arc<Mutex<tessera_core::ecs::PartRegistry>>,
    rng: Arc<Mutex<HostRng>>,
    slot: Arc<DispatchSlot>,
    on_slot: OnSlot,
    next_entity: Arc<AtomicU64>,
}

/// A pending `engine.on` registration: script, event, priority, handler.
type OnRegistration = (String, String, i8, Function);

/// Registration routing for `engine.on` that survives the `'static` closure
/// boundary: a small shared queue drained by the host after a script's top
/// level runs.
#[derive(Clone, Default)]
pub struct OnSlot {
    inner: Arc<Mutex<Vec<OnRegistration>>>,
}

impl OnSlot {
    pub fn register(&self, script: &str, event: &str, priority: i8, func: Function) {
        self.inner.lock().expect("on-slot lock").push((
            script.to_string(),
            event.to_string(),
            priority,
            func,
        ));
    }
    pub fn drain(&self) -> Vec<OnRegistration> {
        self.inner.lock().expect("on-slot lock").drain(..).collect()
    }
}

/// Read access the host needs from the world to build a handler snapshot. The
/// integration card implements this over `SimContext`/`World`; the runtime
/// crate supplies a headless in-memory store for tests.
pub trait WorldAccess {
    fn try_entity(&self, entity: u64) -> bool;
    /// Read one part of an entity as plain fields, or `None`.
    fn snapshot_part(&self, entity: u64, part: &str) -> Option<BTreeMap<String, PartValue>>;
    /// All part names of an entity to snapshot for a handler.
    fn entity_parts(&self, entity: u64) -> Vec<String>;
}

/// A registered script handler bound to one script + event.
#[derive(Clone)]
struct HandlerEntry {
    script: String,
    event: String,
    priority: i8,
    seq: u64,
    func: Function,
}

/// `ScriptHost` owns the Luau VM(s), the handler registry, and the part schema
/// store.
pub struct ScriptHost {
    lua: Option<Lua>,
    counter: Arc<AtomicU64>,
    shared: SharedHandles,
    handlers: BTreeMap<String, Vec<HandlerEntry>>,
    next_seq: u64,
    blueprints: BTreeMap<String, String>,
    quarantine: BTreeMap<(String, String), u32>,
    world: Box<dyn WorldAccess>,
}

impl ScriptHost {
    pub fn new(world: Box<dyn WorldAccess>) -> Self {
        Self {
            lua: None,
            counter: Arc::new(AtomicU64::new(crate::SCRIPT_INSTRUCTION_BUDGET)),
            shared: SharedHandles {
                registry: Arc::new(Mutex::new(tessera_core::ecs::PartRegistry::new())),
                rng: Arc::new(Mutex::new(HostRng::from_seed(0))),
                slot: Arc::new(DispatchSlot::default()),
                on_slot: OnSlot::default(),
                next_entity: Arc::new(AtomicU64::new(1)),
            },
            handlers: BTreeMap::new(),
            next_seq: 0,
            blueprints: BTreeMap::new(),
            quarantine: BTreeMap::new(),
            world,
        }
    }

    pub fn registry(&self) -> &Arc<Mutex<tessera_core::ecs::PartRegistry>> {
        &self.shared.registry
    }
    pub fn blueprint_of(&self, script: &str) -> Option<&str> {
        self.blueprints.get(script).map(|s| s.as_str())
    }

    /// Load a script from source: validate version header, sandbox a shared VM
    /// (once), install `engine`, run the top level (declares parts + registers
    /// handlers). Fail-closed: any error returns [`ScriptError::Load`] and the
    /// script is not registered.
    pub fn load_script(
        &mut self,
        script: &str,
        blueprint: &str,
        source: &str,
    ) -> Result<(), ScriptError> {
        validate_version_header(script, source)?;

        if self.lua.is_none() {
            let lua = sandbox::new_sandbox(self.counter.clone(), self.shared.rng.clone()).map_err(
                |e| ScriptError::Load {
                    script: script.to_string(),
                    msg: format!("sandbox setup: {e}"),
                },
            )?;
            self.lua = Some(lua);
        }
        let lua = self.lua.as_ref().expect("lua initialized above");
        let engine = Self::build_engine(lua, self.shared.clone(), script, blueprint)?;

        // Run the top level in a per-script **sandboxed Thread** (§3.1): the
        // thread's proxied globals keep any global writes local to this script
        // and give it its own environment, so the `engine` table and any state
        // it creates cannot leak across scripts sharing the VM.
        self.counter
            .store(crate::SCRIPT_INSTRUCTION_BUDGET, Ordering::Relaxed);
        let script_owned = script.to_string();
        let source_owned = source.to_string();
        let engine_owned = engine.clone();
        let body = lua
            .create_function(move |lua2, ()| {
                lua2.globals().set("engine", engine_owned.clone())?;
                lua2.load(source_owned.as_bytes())
                    .set_name(&script_owned)
                    .exec()
            })
            .map_err(|e| ScriptError::Load {
                script: script.to_string(),
                msg: format!("create thread body: {e}"),
            })?;
        let thread = lua.create_thread(body).map_err(|e| ScriptError::Load {
            script: script.to_string(),
            msg: format!("create thread: {e}"),
        })?;
        thread.sandbox().map_err(|e| ScriptError::Load {
            script: script.to_string(),
            msg: format!("thread sandbox: {e}"),
        })?;
        thread.resume::<()>(()).map_err(|e| map_load(script, e))?;

        // Apply `on` registrations the top level queued.
        for (decl_script, event, priority, func) in self.shared.on_slot.drain() {
            if !self.known_events().contains(&event.as_str()) {
                return Err(ScriptError::Register {
                    script: script.to_string(),
                    msg: format!("unknown event '{event}'"),
                });
            }
            self.register_handler(&decl_script, &event, priority, func)?;
        }

        self.blueprints
            .insert(script.to_string(), blueprint.to_string());
        Ok(())
    }

    fn build_engine(
        lua: &Lua,
        shared: SharedHandles,
        script: &str,
        blueprint: &str,
    ) -> Result<Table, ScriptError> {
        let t = lua.create_table()?;
        let api_version_val = crate::TESSERA_API_VERSION as i64;
        t.set(
            "api_version",
            lua.create_function(move |_, ()| Ok(api_version_val))?,
        )?;

        let blueprint = blueprint.to_string();
        t.set(
            "blueprint_name",
            lua.create_function(move |_, ()| Ok(blueprint.clone()))?,
        )?;

        // define_part -> core PartRegistry (schema integration).
        let registry = shared.registry.clone();
        let reg_script = script.to_string();
        t.set(
            "define_part",
            lua.create_function(move |lua2, (name, schema): (String, Table)| {
                let ty = value::schema_table_to_type(lua2, &name, &schema)
                    .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                let mut reg = registry.lock().map_err(|_| lua_err("registry lock"))?;
                reg.register_script_part(ty)
                    .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                let _ = &reg_script;
                Ok(())
            })?,
        )?;

        // on -> queue a registration for the host to apply after top level.
        let on_slot = shared.on_slot.clone();
        let on_script = script.to_string();
        t.set(
            "on",
            lua.create_function(
                move |_lua2, (event, priority, func): (String, i8, Function)| {
                    on_slot.register(&on_script, &event, priority, func);
                    Ok(())
                },
            )?,
        )?;

        // get_part reads the dispatch snapshot (read-only).
        let slot = shared.slot.clone();
        t.set(
            "get_part",
            lua.create_function(move |lua2, (entity, name): (i64, String)| {
                let snapshot = slot.snapshot();
                let value: Value = match snapshot.get(&(entity as u64)).and_then(|p| p.get(&name)) {
                    Some(fields) => {
                        let table = value::field_table_to_lua(lua2, fields)
                            .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                        Value::Table(table)
                    }
                    None => Value::Nil,
                };
                Ok(value)
            })?,
        )?;

        let slot = shared.slot.clone();
        t.set(
            "try_entity",
            lua.create_function(move |_lua2, entity: i64| {
                Ok(slot.snapshot().contains_key(&(entity as u64)))
            })?,
        )?;

        // pos -> read-only Position accessor from the snapshot ({x, y} or nil).
        let slot = shared.slot.clone();
        t.set(
            "pos",
            lua.create_function(move |lua2, entity: i64| {
                let value: Value = match slot
                    .snapshot()
                    .get(&(entity as u64))
                    .and_then(|parts| parts.get("Position"))
                {
                    Some(fields) => {
                        let x = match fields.get("x") {
                            Some(PartValue::Int(v)) => *v,
                            _ => 0,
                        };
                        let y = match fields.get("y") {
                            Some(PartValue::Int(v)) => *v,
                            _ => 0,
                        };
                        let tbl = lua2
                            .create_table()
                            .map_err(|e: mlua::Error| mlua::Error::RuntimeError(e.to_string()))?;
                        tbl.set("x", x)
                            .map_err(|e: mlua::Error| mlua::Error::RuntimeError(e.to_string()))?;
                        tbl.set("y", y)
                            .map_err(|e: mlua::Error| mlua::Error::RuntimeError(e.to_string()))?;
                        Value::Table(tbl)
                    }
                    None => Value::Nil,
                };
                Ok(value)
            })?,
        )?;

        // set_part -> pending action (applied after handler returns).
        let slot = shared.slot.clone();
        t.set(
            "set_part",
            lua.create_function(move |lua2, (entity, name, tbl): (i64, String, Table)| {
                let mut fields = BTreeMap::new();
                for pair in tbl.pairs::<String, Value>() {
                    let (k, v) = pair?;
                    let pv = value::lua_to_part_value(&v)
                        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                    fields.insert(k, pv);
                }
                let _ = lua2;
                slot.pending_mut().push(PendingAction::SetPart {
                    entity: entity as u64,
                    name,
                    fields,
                });
                Ok(())
            })?,
        )?;

        // log -> tracing::info! under "script" target.
        t.set(
            "log",
            lua.create_function(|_, msg: String| {
                tracing::info!(target: "script", "{msg}");
                Ok(())
            })?,
        )?;

        // rng(label) -> forked deterministic stream returning ints.
        let rng = shared.rng.clone();
        t.set(
            "rng",
            lua.create_function(move |lua2, label: String| {
                let mut g = rng.lock().map_err(|_| lua_err("rng lock"))?;
                let mut stream = g.fork(&label);
                let f = lua2.create_function_mut(move |_lua3, args: MultiValue| {
                    rng_call(&mut stream, args)
                        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
                })?;
                Ok(f)
            })?,
        )?;

        // spawn/despawn -> pending actions with a deterministic host-side id.
        let slot = shared.slot.clone();
        let next_id = shared.next_entity.clone();
        t.set(
            "spawn",
            lua.create_function(move |_lua2, blueprint: String| {
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                slot.pending_mut().push(PendingAction::Spawn {
                    blueprint: blueprint.clone(),
                });
                Ok(id as i64)
            })?,
        )?;
        let slot = shared.slot.clone();
        t.set(
            "despawn",
            lua.create_function(move |_lua2, entity: i64| {
                slot.pending_mut().push(PendingAction::Despawn {
                    entity: entity as u64,
                });
                Ok(())
            })?,
        )?;

        t.raw_set("EARLY", -64)?;
        t.raw_set("NORMAL", 0)?;
        t.raw_set("LATE", 64)?;
        Ok(t)
    }

    /// Known handler event names for `engine.on` validation (the canonical
    /// core events shipped by the event bus).
    pub fn known_events(&self) -> &'static [&'static str] {
        &["Move", "Attack", "TakeDamage", "ActorTurnStart"]
    }

    pub fn register_handler(
        &mut self,
        script: &str,
        event: &str,
        priority: i8,
        func: Function,
    ) -> Result<(), ScriptError> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let list = self.handlers.entry(event.to_string()).or_default();
        list.push(HandlerEntry {
            script: script.to_string(),
            event: event.to_string(),
            priority,
            seq,
            func,
        });
        list.sort_by_key(|e| (e.priority, e.seq));
        Ok(())
    }

    pub fn unregister_script_event(&mut self, script: &str, event: &str) {
        if let Some(list) = self.handlers.get_mut(event) {
            list.retain(|e| !(e.script == script && e.event == event));
        }
    }

    pub fn handler_count(&self, event: &str) -> usize {
        self.handlers.get(event).map(|l| l.len()).unwrap_or(0)
    }

    pub fn quarantine_count(&self, script: &str, event: &str) -> u32 {
        self.quarantine
            .get(&(script.to_string(), event.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Dispatch one event to all scripted handlers in deterministic order.
    /// Budget is reset once; errors are mapped (never panicked) and drive
    /// quarantine. Returns per-handler outcomes + quarantined `(script,event)`.
    ///
    /// The read snapshot covers the primary `entity` plus every entity id that
    /// appears as an integer value in `params` (e.g. `attacker`/`defender`),
    /// so a handler can call `engine.get_part(ev.other_entity, ...)`.
    pub fn dispatch(
        &mut self,
        event: &str,
        entity: u64,
        params: &[(&str, Value)],
    ) -> DispatchResult {
        let handlers = self.handlers.get(event).cloned().unwrap_or_default();

        // Collect every entity id to snapshot: the primary plus any integer
        // values in the event params.
        let mut ids: Vec<u64> = vec![entity];
        for (_, v) in params {
            match v {
                Value::Integer(n) if *n >= 0 => ids.push(*n as u64),
                Value::Number(f) if *f >= 0.0 && (*f as i64) as f64 == *f => {
                    ids.push(*f as i64 as u64)
                }
                _ => {}
            }
        }
        ids.sort_unstable();
        ids.dedup();

        let mut snapshot: PartSnapshot = BTreeMap::new();
        for id in ids {
            if !self.world.try_entity(id) {
                continue;
            }
            let mut parts = BTreeMap::new();
            for name in self.world.entity_parts(id) {
                if let Some(fields) = self.world.snapshot_part(id, &name) {
                    parts.insert(name, fields);
                }
            }
            snapshot.insert(id, parts);
        }

        // Reset shared budget once per dispatch (carries across nested calls).
        self.counter
            .store(crate::SCRIPT_INSTRUCTION_BUDGET, Ordering::Relaxed);
        self.shared.slot.begin(snapshot);

        let mut quarantined: Vec<(String, String)> = Vec::new();
        let mut outcomes: Vec<HandlerOutcome> = Vec::new();

        for entry in &handlers {
            let outcome = self.run_handler(event, entity, params, entry);
            match outcome {
                HandlerOutcome::Errored { error } if error.is_quarantinable() => {
                    let key = (entry.script.clone(), event.to_string());
                    let count = self.quarantine.entry(key.clone()).or_insert(0);
                    *count += 1;
                    if *count >= QUARANTINE_THRESHOLD {
                        self.unregister_script_event(&entry.script, event);
                        self.quarantine.remove(&key);
                        quarantined.push((entry.script.clone(), event.to_string()));
                        outcomes.push(HandlerOutcome::Quarantined {
                            script: entry.script.clone(),
                            event: event.to_string(),
                        });
                    } else {
                        outcomes.push(HandlerOutcome::Errored { error });
                    }
                }
                other => {
                    // A success (or load/register error) resets the streak.
                    self.quarantine
                        .remove(&(entry.script.clone(), event.to_string()));
                    outcomes.push(other);
                }
            }
        }

        self.shared.slot.finish();
        DispatchResult {
            outcomes,
            quarantined_this_pass: quarantined,
        }
    }

    fn run_handler(
        &mut self,
        event: &str,
        entity: u64,
        params: &[(&str, Value)],
        entry: &HandlerEntry,
    ) -> HandlerOutcome {
        // Budget is reset once at dispatch top; nested engine calls within this
        // handler keep consuming the same counter.
        let lua = self.lua.as_ref().expect("vm initialized at load");
        let params_table = lua.create_table().expect("create params table");
        params_table
            .set("target", entity as i64)
            .expect("set target");
        for (k, v) in params {
            params_table.set(*k, v.clone()).expect("set param");
        }

        match entry.func.call::<MultiValue>(params_table) {
            Ok(vals) => {
                let vetoed = vals
                    .into_iter()
                    .next()
                    .map(|v| matches!(v, Value::Boolean(false)))
                    .unwrap_or(false);
                if vetoed {
                    HandlerOutcome::Vetoed
                } else {
                    HandlerOutcome::Passed
                }
            }
            Err(e) => {
                let error = map_handler_error(&entry.script, event, entity, e);
                HandlerOutcome::Errored { error }
            }
        }
    }
}

/// Outcome of running one scripted handler for an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOutcome {
    Passed,
    Vetoed,
    Errored { error: ScriptError },
    Quarantined { script: String, event: String },
}

impl HandlerOutcome {
    pub fn is_failure(&self) -> bool {
        matches!(self, HandlerOutcome::Errored { .. })
    }
}

/// Aggregated result of dispatching one event.
#[derive(Debug, Clone, Default)]
pub struct DispatchResult {
    pub outcomes: Vec<HandlerOutcome>,
    pub quarantined_this_pass: Vec<(String, String)>,
}

impl DispatchResult {
    pub fn any_veto(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| matches!(o, HandlerOutcome::Vetoed))
    }
    pub fn error_count(&self) -> usize {
        self.outcomes.iter().filter(|o| o.is_failure()).count()
    }
}

fn lua_err(msg: &str) -> mlua::Error {
    mlua::Error::RuntimeError(msg.to_string())
}

fn map_load(script: &str, e: mlua::Error) -> ScriptError {
    if sandbox::is_budget_error(&e) {
        ScriptError::Budget {
            script: script.to_string(),
            phase: "load".into(),
            used: 0,
        }
    } else if matches!(e, mlua::Error::MemoryError(_)) {
        ScriptError::Memory {
            script: script.to_string(),
        }
    } else {
        ScriptError::Load {
            script: script.to_string(),
            msg: e.to_string(),
        }
    }
}

fn map_handler_error(script: &str, event: &str, entity: u64, e: mlua::Error) -> ScriptError {
    if sandbox::is_budget_error(&e) {
        ScriptError::Budget {
            script: script.to_string(),
            phase: format!("handler:{event}"),
            used: 0,
        }
    } else if matches!(e, mlua::Error::MemoryError(_)) {
        ScriptError::Memory {
            script: script.to_string(),
        }
    } else {
        ScriptError::Handler {
            script: script.to_string(),
            event: event.to_string(),
            entity,
            msg: e.to_string(),
        }
    }
}

/// Validate the `-- tessera-api: N` version header before any execution.
fn validate_version_header(script: &str, source: &str) -> Result<(), ScriptError> {
    let header_line = source
        .lines()
        .find(|l| l.trim_start().starts_with("--"))
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    if !header_line.starts_with("-- tessera-api:") {
        return Err(ScriptError::Load {
            script: script.to_string(),
            msg: "missing '-- tessera-api: N' version header".into(),
        });
    }
    let version: u32 = header_line
        .split(':')
        .nth(1)
        .map(|s| s.trim().parse().unwrap_or(0))
        .unwrap_or(0);
    if version == crate::TESSERA_API_VERSION {
        Ok(())
    } else {
        Err(ScriptError::Load {
            script: script.to_string(),
            msg: format!(
                "unsupported api version {version} (expected {})",
                crate::TESSERA_API_VERSION
            ),
        })
    }
}
