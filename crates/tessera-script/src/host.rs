//! The script host: owns the Luau VM(s), loads scripts, keeps the handler
//! registry, and runs the deterministic dispatch seam.
//!
//! # Value boundary
//!
//! Nothing from the ECS crosses into Luau as a borrow. When a handler runs the
//! host builds a **read snapshot** (plain `PartValue`s) the script may inspect
//! and a **pending-action list** that `engine.set_part`/`spawn`/`despawn`
//! append to; those are applied only after the handler returns, mirroring the
//! engine's post-`Move` commit pattern. Engine API closures capture a
//! [`DispatchSlot`] and never hold a Rust borrow of the world across a Luau
//! call — nested mutable reentrancy is therefore impossible to express.
//!
//! # Budget carry
//!
//! A single shared instruction counter is reset only at the top of a dispatch,
//! so a handler that calls back into the VM through an engine API keeps
//! consuming the *same* budget. Exceeding it yields
//! [`ScriptError::Budget`] — never a panic.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, MultiValue, Table, Value};
use tessera_core::kinds::TuningValue;
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
    pub fn below(&mut self, n: u64) -> u64 {
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingAction {
    SetPart {
        entity: u64,
        name: String,
        fields: BTreeMap<String, PartValue>,
    },
    Spawn {
        /// The host-assigned entity id already returned to the script; the
        /// world must create the entity under exactly this id.
        id: u64,
        blueprint: String,
    },
    Despawn {
        entity: u64,
    },
    /// A scripted step request: move `entity` to the cell `(x, y)` as that
    /// actor's turn action. Applied by the simulation's turn loop (this is a
    /// *turn action*, not a write to a part), so [`WorldAccess`] does not
    /// implement it — the host surfaces it in [`DispatchResult::pending_actions`]
    /// and the caller (the sim) resolves it as a `Move`.
    Step {
        entity: u64,
        x: i32,
        y: i32,
    },
    /// A scripted attack request: `attacker` hits `defender`. Also a turn
    /// action applied by the sim's turn loop (dispatched as an `Attack`).
    Attack {
        attacker: u64,
        defender: u64,
    },
    /// A scripted wander request: `entity` takes one seeded random step toward
    /// a walkable-empty neighbour. Applied by the sim's turn loop.
    Wander {
        entity: u64,
    },
    /// Spawn `blueprint` at `(x, y)` (or a seeded random walkable empty cell
    /// when `x`/`y` are `None`) under a host-assigned id, registering with the
    /// scheduler if the blueprint carries `ActorEnergy`. Applied through
    /// [`WorldAccess::apply_spawn_at`].
    SpawnAt {
        id: u64,
        blueprint: String,
        x: Option<i32>,
        y: Option<i32>,
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
    /// Deterministic capture buffer for `engine.log(...)`: each message is
    /// appended in call order. Callers drain it after a dispatch
    /// so script log evidence lands in the sim's observable event log (see
    /// [`ScriptHost::drain_log`]).
    log_sink: Arc<Mutex<Vec<String>>>,
    /// Shared read-only spatial query handle for the world-slice nav API
    /// closures.
    world_read: Arc<dyn WorldRead>,
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

/// Read access the host needs from the world to build a handler snapshot.
/// Implementers provide this over their `SimContext`/`World`; the runtime
/// crate supplies a headless in-memory store for tests.
///
/// The trait is also the **apply seam**: after a handler batch returns,
/// the host validates each [`PendingAction`] (immutability whitelist +
/// write-schema) and commits valid ones through the mutable methods below.
/// Implementers provide these over their `World` (spawn via
/// `spawn_blueprint`, part writes via its part store, despawn via the world).
pub trait WorldAccess {
    fn try_entity(&self, entity: u64) -> bool;
    /// Read one part of an entity as plain fields, or `None`.
    fn snapshot_part(&self, entity: u64, part: &str) -> Option<BTreeMap<String, PartValue>>;
    /// All part names of an entity to snapshot for a handler.
    fn entity_parts(&self, entity: u64) -> Vec<String>;

    /// Commit a validated `set_part` write (already checked against the schema
    /// or the native whitelist by the host) to the world.
    fn apply_set_part(
        &mut self,
        entity: u64,
        name: &str,
        fields: BTreeMap<String, PartValue>,
    ) -> Result<(), ScriptError>;
    /// Spawn an entity for the given (host-assigned) id and blueprint.
    fn apply_spawn(&mut self, id: u64, blueprint: &str) -> Result<(), ScriptError>;
    /// Despawn (remove) an entity.
    fn apply_despawn(&mut self, entity: u64) -> Result<(), ScriptError>;
    /// Spawn a blueprint at a cell (or a seeded random walkable empty cell when
    /// `x`/`y` are `None`) under a host-assigned id, registering the new
    /// entity with the scheduler if it carries `ActorEnergy`. This is the real
    /// implementation behind the earlier stubbed spawn path and `engine.spawn_at`.
    fn apply_spawn_at(
        &mut self,
        id: u64,
        blueprint: &str,
        x: Option<i32>,
        y: Option<i32>,
    ) -> Result<(), ScriptError>;
}

/// Read-only spatial/entity queries the script host exposes to `engine.*`
/// closures. Implemented by the simulation over its live shared state and
/// shared with the closures via [`SharedHandles`], so a handler can answer the
/// map/nav reads without spending script instructions iterating cells
/// (the "no map loops in scripts" rule — one native call per decision).
///
/// Everything here is a pure read returning owned, plain values; no borrow of
/// the world crosses into Luau (the value boundary in the module doc).
pub trait WorldRead: Send + Sync + 'static {
    /// Zone dimensions `(width, height)`.
    fn map_size(&self) -> (u32, u32);
    /// Terrain walkability of a cell (out of bounds = not walkable).
    fn is_walkable(&self, x: i32, y: i32) -> bool;
    /// Whether an actor currently stands on the cell (occupancy query).
    fn is_occupied(&self, x: i32, y: i32) -> bool;
    /// The grid position of an entity, if it has one.
    fn position(&self, entity: u64) -> Option<(i32, i32)>;
    /// The simulated "player" entity (the demo's `[Player]` spawn), if any.
    fn player(&self) -> Option<u64>;
    /// Whether an entity is live (not despawned).
    fn is_alive(&self, entity: u64) -> bool;
    /// Integer squared straight-line distance `dist2(a, b)`.
    fn distance(&self, a: u64, b: u64) -> Option<u32>;
    /// One deterministic native A* path from `a` to `b` (excluding `from`,
    /// including `to`), or `None` when unreachable/budget-exceeded. The path is
    /// returned as a list of `(x, y)` cells; scripts step one cell per turn.
    fn path(&self, from: u64, to: u64) -> Option<Vec<(i32, i32)>>;
    /// Nearest walkable-empty cell to `(x, y)` via a deterministic BFS ring.
    fn nearest_walkable(&self, x: i32, y: i32) -> Option<(i32, i32)>;
    /// A walkable-empty cell chosen from the row-major list via
    /// `rng.below(len)` — one native, deterministic draw, or `None` when no
    /// cell qualifies. The caller passes the host RNG fork so the draw is
    /// deterministic and does not cross a value boundary.
    fn random_walkable_tile(&self, rng: &mut HostRng) -> Option<(i32, i32)>;
    /// The simulation's player-turn clock (increments once per submitted player
    /// action, cancelled or not).
    fn player_turns(&self) -> u64;
    /// The zone's `start` tile `(x, y)` (the player respawn anchor; `None`
    /// before a zone is set). Read-only, deterministic.
    fn zone_start(&self) -> Option<(i32, i32)>;
    /// The world-state singleton's script-facing entity id, if one has been
    /// installed (the respawn-scheduling anchor for script policies). `None`
    /// when the load wiring has not created one.
    fn world_state(&self) -> Option<u64>;
    /// Read-only tuning: the merged (authored + applied-override) values of
    /// one tuning table, sorted by key, or `None` when no such table is
    /// loaded. Fail-closed: scripts may not mutate tuning — the host wraps
    /// the returned values in a read-only binding.
    fn tuning_table(&self, table: &str) -> Option<BTreeMap<String, TuningValue>>;
}

/// A default [`WorldRead`] used when a host is built without a reader (tests /
/// minimal worlds): every map query is a negative/empty answer, so scripts
/// that touch the nav API fail closed rather than panicking.
#[derive(Clone, Default)]
pub struct DefaultWorldRead;

impl WorldRead for DefaultWorldRead {
    fn map_size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn is_walkable(&self, _x: i32, _y: i32) -> bool {
        false
    }
    fn is_occupied(&self, _x: i32, _y: i32) -> bool {
        true
    }
    fn position(&self, _entity: u64) -> Option<(i32, i32)> {
        None
    }
    fn player(&self) -> Option<u64> {
        None
    }
    fn is_alive(&self, _entity: u64) -> bool {
        false
    }
    fn distance(&self, _a: u64, _b: u64) -> Option<u32> {
        None
    }
    fn path(&self, _from: u64, _to: u64) -> Option<Vec<(i32, i32)>> {
        None
    }
    fn nearest_walkable(&self, _x: i32, _y: i32) -> Option<(i32, i32)> {
        None
    }
    fn random_walkable_tile(&self, _rng: &mut HostRng) -> Option<(i32, i32)> {
        None
    }
    fn player_turns(&self) -> u64 {
        0
    }
    fn zone_start(&self) -> Option<(i32, i32)> {
        None
    }
    fn world_state(&self) -> Option<u64> {
        None
    }
    fn tuning_table(&self, _table: &str) -> Option<BTreeMap<String, TuningValue>> {
        None
    }
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
    /// Native parts a script may write, as `part name -> writable field names`.
    /// The v1 whitelist is `Health -> { hp }` for the Zapper demo; extend
    /// via [`Self::allow_native_write`] for other fields/parts.
    writable_native: BTreeMap<String, BTreeSet<String>>,
}

impl ScriptHost {
    pub fn new(world: Box<dyn WorldAccess>) -> Self {
        Self::with_reader_base(
            world,
            Arc::new(DefaultWorldRead),
            crate::FIRST_SCRIPT_ENTITY,
        )
    }

    /// Build a host with the given [`WorldAccess`] (writer) and [`WorldRead`]
    /// (shared spatial query) handle. The reader is what the world-slice nav
    /// API closures answer from; the writer is the apply seam the host uses for
    /// `set_part`/`spawn_at`/etc. A host built with [`Self::new`] gets a
    /// fail-closed [`DefaultWorldRead`].
    pub fn with_reader(world: Box<dyn WorldAccess>, world_read: Arc<dyn WorldRead>) -> Self {
        Self::with_reader_base(world, world_read, crate::FIRST_SCRIPT_ENTITY)
    }

    /// Like [`Self::with_reader`] but with an explicit first script-assigned
    /// entity id. The base must encode a nonzero hecs generation (high 32 bits)
    /// so [`WorldAccess::apply_spawn_at`] can reconstruct the entity via
    /// `Entity::from_bits`; the simulation's own entities use generation 0, so
    /// a high base keeps script ids distinct from live ones.
    pub fn with_reader_base(
        world: Box<dyn WorldAccess>,
        world_read: Arc<dyn WorldRead>,
        first_script_entity: u64,
    ) -> Self {
        let mut writable_native = BTreeMap::new();
        writable_native.insert("Health".to_string(), BTreeSet::from(["hp".to_string()]));
        Self {
            lua: None,
            counter: Arc::new(AtomicU64::new(crate::SCRIPT_INSTRUCTION_BUDGET)),
            shared: SharedHandles {
                registry: Arc::new(Mutex::new(tessera_core::ecs::PartRegistry::new())),
                rng: Arc::new(Mutex::new(HostRng::from_seed(0))),
                slot: Arc::new(DispatchSlot::default()),
                on_slot: OnSlot::default(),
                next_entity: Arc::new(AtomicU64::new(first_script_entity)),
                log_sink: Arc::new(Mutex::new(Vec::new())),
                world_read,
            },
            handlers: BTreeMap::new(),
            next_seq: 0,
            blueprints: BTreeMap::new(),
            quarantine: BTreeMap::new(),
            world,
            writable_native,
        }
    }

    /// Whitelist an additional native field a script may write (e.g. to let
    /// a mod give scripts access to another native part's field). Deterministic
    /// (BTreeMap/BTreeSet).
    pub fn allow_native_write(&mut self, part: &str, field: &str) {
        self.writable_native
            .entry(part.to_string())
            .or_default()
            .insert(field.to_string());
    }

    pub fn registry(&self) -> &Arc<Mutex<tessera_core::ecs::PartRegistry>> {
        &self.shared.registry
    }
    pub fn blueprint_of(&self, script: &str) -> Option<&str> {
        self.blueprints.get(script).map(|s| s.as_str())
    }
    /// Drain (and clear) every `engine.log` message captured so far, in call
    /// order. Callers run this after each scripted dispatch so
    /// the messages appear in the sim's observable event log; draining keeps
    /// the buffer bounded and deterministic across dispatches.
    pub fn drain_log(&self) -> Vec<String> {
        self.shared
            .log_sink
            .lock()
            .expect("log sink lock")
            .drain(..)
            .collect()
    }
    /// Read-only view of the world for inspection (used by tests and
    /// callers to confirm applied mutations).
    pub fn world(&self) -> &dyn WorldAccess {
        self.world.as_ref()
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

        // Run the top level in a per-script **sandboxed Thread**: the
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
        thread
            .resume::<()>(())
            .map_err(|e| map_load(script, e, &self.counter))?;

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

        // log -> tracing::info! under "script" target, and captured into the
        // deterministic log sink (drained by callers after a
        // dispatch so `engine.log` evidence appears in the sim event log).
        let sink = shared.log_sink.clone();
        t.set(
            "log",
            lua.create_function(move |_, msg: String| {
                tracing::info!(target: "script", "{msg}");
                sink.lock().expect("log sink lock").push(msg);
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
                    id,
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

        // ---- world-slice nav / world API (reads over the shared reader; actions as
        // pending turn actions or spawn_at applied through the writer) ----

        let world_read = shared.world_read.clone();
        t.set(
            "map_size",
            lua.create_function(move |lua2, ()| {
                let (w, h) = world_read.map_size();
                let tbl = lua2
                    .create_table()
                    .map_err(|e: mlua::Error| mlua::Error::RuntimeError(e.to_string()))?;
                tbl.set("width", w).map_err(mlua_lua_err)?;
                tbl.set("height", h).map_err(mlua_lua_err)?;
                Ok(tbl)
            })?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "is_walkable",
            lua.create_function(move |_, (x, y): (i64, i64)| {
                Ok(world_read.is_walkable(x as i32, y as i32))
            })?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "is_occupied",
            lua.create_function(move |_, (x, y): (i64, i64)| {
                Ok(world_read.is_occupied(x as i32, y as i32))
            })?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "position",
            lua.create_function(move |lua2, entity: i64| {
                point_to_lua(lua2, world_read.position(entity as u64))
            })?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "player",
            lua.create_function(move |_, ()| Ok(world_read.player().map(|e| e as i64)))?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "is_alive",
            lua.create_function(move |_, entity: i64| Ok(world_read.is_alive(entity as u64)))?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "distance",
            lua.create_function(move |_, (a, b): (i64, i64)| {
                Ok(world_read.distance(a as u64, b as u64).map(|d| d as i64))
            })?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "path",
            lua.create_function(move |lua2, (from, to): (i64, i64)| {
                let Some(path) = world_read.path(from as u64, to as u64) else {
                    return Ok(Value::Nil);
                };
                let tbl = lua2
                    .create_table()
                    .map_err(|e: mlua::Error| mlua::Error::RuntimeError(e.to_string()))?;
                for (idx, (x, y)) in path.iter().enumerate() {
                    let pt = lua2
                        .create_table()
                        .map_err(|e: mlua::Error| mlua::Error::RuntimeError(e.to_string()))?;
                    pt.set("x", *x)
                        .map_err(|e: mlua::Error| mlua::Error::RuntimeError(e.to_string()))?;
                    pt.set("y", *y)
                        .map_err(|e: mlua::Error| mlua::Error::RuntimeError(e.to_string()))?;
                    tbl.set(idx + 1, pt).map_err(mlua_lua_err)?;
                }
                Ok(Value::Table(tbl))
            })?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "nearest_walkable",
            lua.create_function(move |lua2, (x, y): (i64, i64)| {
                point_to_lua(lua2, world_read.nearest_walkable(x as i32, y as i32))
            })?,
        )?;

        let world_read = shared.world_read.clone();
        let rng = shared.rng.clone();
        t.set(
            "random_walkable_tile",
            lua.create_function(move |lua2, ()| {
                let mut g = rng.lock().map_err(|_| lua_err("rng lock"))?;
                let mut stream = g.fork("random_walkable_tile");
                point_to_lua(lua2, world_read.random_walkable_tile(&mut stream))
            })?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "player_turns",
            lua.create_function(move |_, ()| Ok(world_read.player_turns()))?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "zone_start",
            lua.create_function(move |lua2, ()| point_to_lua(lua2, world_read.zone_start()))?,
        )?;

        let world_read = shared.world_read.clone();
        t.set(
            "world_state",
            lua.create_function(move |_, ()| Ok(world_read.world_state().map(|e| e as i64)))?,
        )?;

        // tuning -> read-only view of one typed tuning table. Returns a Lua
        // proxy table; a missing table or missing key raises a script error
        // (fail-closed, matching how the rest of the API treats absent
        // keys). The proxy is deliberately empty: values live in a hidden
        // source table captured by the metatable, so `__index` reads through
        // to it (raising on unknown keys) and `__newindex` raises for every
        // write — scripts can read tuning but never mutate it.
        let world_read = shared.world_read.clone();
        t.set(
            "tuning",
            lua.create_function(move |lua2, table: String| {
                let values = world_read.tuning_table(&table).ok_or_else(|| {
                    mlua::Error::RuntimeError(format!("unknown tuning table '{table}'"))
                })?;
                let src = lua2.create_table()?;
                for (k, v) in values {
                    let lv = match v {
                        TuningValue::Int(n) => Value::Integer(n),
                        TuningValue::Percent(p) => {
                            let pct = lua2.create_table()?;
                            pct.set("pct", p.pct)?;
                            Value::Table(pct)
                        }
                        TuningValue::IntList(items) => {
                            let list = lua2.create_table()?;
                            for (i, n) in items.iter().enumerate() {
                                list.set(i + 1, *n)?;
                            }
                            Value::Table(list)
                        }
                    };
                    src.set(k, lv)?;
                }
                let proxy = lua2.create_table()?;
                let meta = lua2.create_table()?;
                let err_table = table.clone();
                let src_meta = src.clone();
                meta.set(
                    "__index",
                    lua2.create_function(move |_lua3, (proxy, k): (Table, String)| {
                        let _ = proxy;
                        match src_meta.get::<Value>(k.as_str()) {
                            Ok(v) if v != Value::Nil => Ok(v),
                            _ => Err(mlua::Error::RuntimeError(format!(
                                "unknown tuning key '{k}' in tuning table '{err_table}'"
                            ))),
                        }
                    })?,
                )?;
                let err_table = table.clone();
                meta.set(
                    "__newindex",
                    lua2.create_function(move |_, (_t, k, _v): (Table, String, Value)| {
                        Err::<(), _>(mlua::Error::RuntimeError(format!(
                            "tuning table '{err_table}.{k}' is read-only \
                             (apply overrides natively)"
                        )))
                    })?,
                )?;
                proxy.set_metatable(Some(meta))?;
                Ok(proxy)
            })?,
        )?;

        // Actions queue a pending turn action / spawn-at; the sim resolves
        // Step/Attack/Wander in its own turn loop (they are turn actions, not
        // part writes) and the host applies SpawnAt through the writer.
        let slot = shared.slot.clone();
        t.set(
            "step",
            lua.create_function(move |_, (entity, x, y): (i64, i64, i64)| {
                slot.pending_mut().push(PendingAction::Step {
                    entity: entity as u64,
                    x: x as i32,
                    y: y as i32,
                });
                Ok(())
            })?,
        )?;

        let slot = shared.slot.clone();
        t.set(
            "attack",
            lua.create_function(move |_, (attacker, defender): (i64, i64)| {
                slot.pending_mut().push(PendingAction::Attack {
                    attacker: attacker as u64,
                    defender: defender as u64,
                });
                Ok(())
            })?,
        )?;

        let slot = shared.slot.clone();
        t.set(
            "wander",
            lua.create_function(move |_, entity: i64| {
                slot.pending_mut().push(PendingAction::Wander {
                    entity: entity as u64,
                });
                Ok(())
            })?,
        )?;

        let slot = shared.slot.clone();
        let next_id = shared.next_entity.clone();
        t.set(
            "spawn_at",
            lua.create_function(
                move |_lua2, (blueprint, x, y): (String, Option<i64>, Option<i64>)| {
                    let id = next_id.fetch_add(1, Ordering::Relaxed);
                    slot.pending_mut().push(PendingAction::SpawnAt {
                        id,
                        blueprint: blueprint.clone(),
                        x: x.map(|v| v as i32),
                        y: y.map(|v| v as i32),
                    });
                    Ok(id as i64)
                },
            )?,
        )?;

        t.raw_set("EARLY", -64)?;
        t.raw_set("NORMAL", 0)?;
        t.raw_set("LATE", 64)?;
        Ok(t)
    }

    /// Known handler event names for `engine.on` validation (the canonical
    /// core events shipped by the event bus).
    pub fn known_events(&self) -> &'static [&'static str] {
        &["Move", "Attack", "TakeDamage", "ActorTurnStart", "Death"]
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

        let pending_actions = self.shared.slot.finish();
        let apply_errors = self.apply_pending(&pending_actions);
        DispatchResult {
            outcomes,
            quarantined_this_pass: quarantined,
            pending_actions,
            apply_errors,
        }
    }

    /// Apply a batch of [`PendingAction`]s produced by a dispatch's handlers:
    /// validate each `set_part` (scripted write-schema or native
    /// whitelist) and commit valid actions to the world through the
    /// [`WorldAccess`] apply methods. A failing action is skipped and its
    /// error returned, the remaining valid actions still apply — one bad
    /// mutation never blocks the others, and nothing panics. `dispatch` calls
    /// this automatically after the handler batch; it is public so
    /// callers can also drive it directly if they need to inspect
    /// actions before applying.
    pub fn apply_pending(&mut self, actions: &[PendingAction]) -> Vec<ScriptError> {
        let mut errors = Vec::new();
        for action in actions {
            let result = match action {
                PendingAction::SetPart {
                    entity,
                    name,
                    fields,
                } => self.apply_set_part(*entity, name, fields),
                PendingAction::Spawn { id, blueprint } => self.world.apply_spawn(*id, blueprint),
                PendingAction::Despawn { entity } => self.world.apply_despawn(*entity),
                PendingAction::SpawnAt {
                    id,
                    blueprint,
                    x,
                    y,
                } => self.world.apply_spawn_at(*id, blueprint, *x, *y),
                // Step/Attack/Wander are *turn actions* the sim resolves in its
                // own turn loop, not part writes the host applies. They stay in
                // the returned `pending_actions` for the caller to consume, so
                // the host deliberately does not attempt to apply them.
                PendingAction::Step { .. }
                | PendingAction::Attack { .. }
                | PendingAction::Wander { .. } => {
                    continue;
                }
            };
            if let Err(e) = result {
                errors.push(e);
            }
        }
        errors
    }

    /// Validate and commit one `set_part` mutation: unknown/missing
    /// entities error; a *scripted* part is mutable but the write is checked
    /// against its declared schema; a *native* part is mutable only for the
    /// whitelisted fields in [`Self`]`::writable_native`, otherwise
    /// [`ScriptError::ImmutablePart`].
    fn apply_set_part(
        &mut self,
        entity: u64,
        name: &str,
        fields: &BTreeMap<String, PartValue>,
    ) -> Result<(), ScriptError> {
        if !self.world.try_entity(entity) {
            return Err(ScriptError::UnknownEntity { entity });
        }

        let scripted = {
            let reg = self
                .shared
                .registry
                .lock()
                .map_err(|_| ScriptError::Internal("registry lock".into()))?;
            reg.script_schema(&tessera_core::ecs::PartTypeId::new(name))
                .is_some()
        };

        if scripted {
            let schema = {
                let reg = self
                    .shared
                    .registry
                    .lock()
                    .map_err(|_| ScriptError::Internal("registry lock".into()))?;
                reg.script_schema(&tessera_core::ecs::PartTypeId::new(name))
                    .expect("scripted checked above")
                    .clone()
            };
            value::validate_scripted_write(&schema, fields)?;
        } else {
            let writable = self.writable_native.get(name).cloned().unwrap_or_default();
            if fields.keys().any(|f| !writable.contains(f.as_str())) {
                return Err(ScriptError::ImmutablePart {
                    entity,
                    name: name.to_string(),
                });
            }
        }

        self.world.apply_set_part(entity, name, fields.clone())
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
                let error = map_handler_error(&entry.script, event, entity, e, &self.counter);
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
    /// The mutations handlers queued during this dispatch, in dispatch order.
    /// The host has already validated and applied valid ones to the
    /// world; this is the caller's handoff view.
    pub pending_actions: Vec<PendingAction>,
    /// Validation/commit failures from applying `pending_actions` (e.g.
    /// `ImmutablePart`, `UnknownEntity`, write-schema violations). Never
    /// panics the host.
    pub apply_errors: Vec<ScriptError>,
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

/// Map an [`mlua::Error`] through a `RuntimeError` (used by helper closures
/// whose inner calls already return `mlua::Error`, keeping the closure body
/// terse).
fn mlua_lua_err(e: mlua::Error) -> mlua::Error {
    mlua::Error::RuntimeError(e.to_string())
}

/// Convert an optional `(x, y)` cell into a Lua `{x=.., y=..}` table, or `nil`.
fn point_to_lua(lua: &Lua, point: Option<(i32, i32)>) -> Result<Value, mlua::Error> {
    let Some((x, y)) = point else {
        return Ok(Value::Nil);
    };
    let tbl = lua.create_table()?;
    tbl.set("x", x)?;
    tbl.set("y", y)?;
    Ok(Value::Table(tbl))
}

/// Instructions consumed by the current dispatch/load, for `Budget.used`.
/// The shared counter counts *down* from `SCRIPT_INSTRUCTION_BUDGET`; used is
/// what remains of the budget.
fn used_instructions(counter: &AtomicU64) -> u64 {
    crate::SCRIPT_INSTRUCTION_BUDGET.saturating_sub(counter.load(Ordering::Relaxed))
}

fn map_load(script: &str, e: mlua::Error, counter: &AtomicU64) -> ScriptError {
    if sandbox::is_budget_error(&e) {
        ScriptError::Budget {
            script: script.to_string(),
            phase: "load".into(),
            used: used_instructions(counter),
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

fn map_handler_error(
    script: &str,
    event: &str,
    entity: u64,
    e: mlua::Error,
    counter: &AtomicU64,
) -> ScriptError {
    if sandbox::is_budget_error(&e) {
        ScriptError::Budget {
            script: script.to_string(),
            phase: format!("handler:{event}"),
            used: used_instructions(counter),
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
