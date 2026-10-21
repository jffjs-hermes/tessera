//! Focused headless tests for the world-slice nav/read API and the turn-hook
//! surface exposed to scripts: every `engine.*` function registers and answers
//! with the expected plain values, the actions queue the expected
//! [`PendingAction`]s, `spawn_at` applies through the writer, and a hostile
//! repeated-path script still trips the existing budget/quarantine. Each
//! asserts behavior, not source text.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mlua::Value;
use tessera_core::events::Point;
use tessera_core::kinds::{Percent, TuningValue};
use tessera_core::script_part::PartValue;
use tessera_core::zone::Zone;
use tessera_script::{HostRng, PendingAction, ScriptError, ScriptHost, WorldAccess, WorldRead};

/// Entity id -> part name -> field name -> value (the flat nav world shape).
type PartStore = BTreeMap<u64, BTreeMap<String, BTreeMap<String, PartValue>>>;

/// A 5x5 all-floor zone with a wall ring on the outer border.
fn small_zone() -> Zone {
    let mut cells = vec!["terrain.floor".to_string(); 5 * 5];
    for y in 0..5i32 {
        for x in 0..5i32 {
            if x == 0 || y == 0 || x == 4 || y == 4 {
                cells[(y as usize) * 5 + (x as usize)] = "terrain.wall".to_string();
            }
        }
    }
    Zone::from_cells("test", 5, 5, Point::new(1, 1), cells)
}

/// A headless in-memory world backing both `WorldAccess` (writer) and
/// `WorldRead` (nav). Positions are stored as `Position` parts; the zone is a
/// fixed 5x5 grid with the player at (1,1) and a foe at (3,1).
#[derive(Clone)]
struct NavWorld {
    entities: Arc<Mutex<PartStore>>,
    zone: Zone,
    player: u64,
    player_turns: Arc<Mutex<u64>>,
    /// Tuning tables exposed to `engine.tuning` (merged values per table).
    tuning: BTreeMap<String, BTreeMap<String, TuningValue>>,
}

impl NavWorld {
    fn new() -> Self {
        let mut entities = BTreeMap::new();
        entities.insert(
            1,
            BTreeMap::from([(
                "Position".to_string(),
                BTreeMap::from([
                    ("x".to_string(), PartValue::Int(1)),
                    ("y".to_string(), PartValue::Int(1)),
                ]),
            )]),
        );
        entities.insert(
            2,
            BTreeMap::from([(
                "Position".to_string(),
                BTreeMap::from([
                    ("x".to_string(), PartValue::Int(3)),
                    ("y".to_string(), PartValue::Int(1)),
                ]),
            )]),
        );
        Self {
            entities: Arc::new(Mutex::new(entities)),
            zone: small_zone(),
            player: 1,
            player_turns: Arc::new(Mutex::new(0)),
            tuning: BTreeMap::new(),
        }
    }

    /// A nav world carrying one tuning table with all three value shapes
    /// (integer, percent-of-integer, integer-list).
    fn with_tuning() -> Self {
        let mut w = Self::new();
        w.tuning.insert(
            "Tuning_Combat".to_string(),
            BTreeMap::from([
                ("base_hit".to_string(), TuningValue::Int(50)),
                (
                    "crit_percent".to_string(),
                    TuningValue::Percent(Percent { pct: 150 }),
                ),
                (
                    "speed_table".to_string(),
                    TuningValue::IntList(vec![50, 90, 140]),
                ),
            ]),
        );
        w
    }

    fn snapshot(&self) -> PartStore {
        self.entities.lock().expect("lock").clone()
    }
}

impl WorldAccess for NavWorld {
    fn try_entity(&self, entity: u64) -> bool {
        self.entities.lock().expect("lock").contains_key(&entity)
    }
    fn snapshot_part(&self, entity: u64, part: &str) -> Option<BTreeMap<String, PartValue>> {
        self.entities
            .lock()
            .expect("lock")
            .get(&entity)?
            .get(part)
            .cloned()
    }
    fn entity_parts(&self, entity: u64) -> Vec<String> {
        self.entities
            .lock()
            .expect("lock")
            .get(&entity)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }
    fn apply_set_part(
        &mut self,
        entity: u64,
        name: &str,
        fields: BTreeMap<String, PartValue>,
    ) -> Result<(), ScriptError> {
        self.entities
            .lock()
            .expect("lock")
            .entry(entity)
            .or_default()
            .insert(name.to_string(), fields);
        Ok(())
    }
    fn apply_spawn(&mut self, id: u64, _blueprint: &str) -> Result<(), ScriptError> {
        self.entities.lock().expect("lock").entry(id).or_default();
        Ok(())
    }
    fn apply_spawn_at(
        &mut self,
        id: u64,
        blueprint: &str,
        x: Option<i32>,
        y: Option<i32>,
    ) -> Result<(), ScriptError> {
        if blueprint != "Goblin" {
            return Err(ScriptError::Internal(format!(
                "unknown blueprint {blueprint}"
            )));
        }
        let mut guard = self.entities.lock().expect("lock");
        let map = guard.entry(id).or_default();
        if let (Some(x), Some(y)) = (x, y) {
            map.insert(
                "Position".to_string(),
                BTreeMap::from([
                    ("x".to_string(), PartValue::Int(x as i64)),
                    ("y".to_string(), PartValue::Int(y as i64)),
                ]),
            );
        }
        Ok(())
    }
    fn apply_despawn(&mut self, entity: u64) -> Result<(), ScriptError> {
        if self
            .entities
            .lock()
            .expect("lock")
            .remove(&entity)
            .is_none()
        {
            return Err(ScriptError::UnknownEntity { entity });
        }
        Ok(())
    }
}

impl WorldRead for NavWorld {
    fn map_size(&self) -> (u32, u32) {
        (self.zone.width as u32, self.zone.height as u32)
    }
    fn is_walkable(&self, x: i32, y: i32) -> bool {
        self.zone.is_walkable(x, y)
    }
    fn is_occupied(&self, x: i32, y: i32) -> bool {
        self.snapshot().iter().any(|(_, parts)| {
            parts
                .get("Position")
                .map(|p| {
                    matches!(p.get("x"), Some(PartValue::Int(xx)) if *xx == x as i64)
                        && matches!(p.get("y"), Some(PartValue::Int(yy)) if *yy == y as i64)
                })
                .unwrap_or(false)
        })
    }
    fn position(&self, entity: u64) -> Option<(i32, i32)> {
        let snap = self.snapshot();
        let parts = snap.get(&entity)?;
        let p = parts.get("Position")?;
        let x = match p.get("x") {
            Some(PartValue::Int(v)) => *v as i32,
            _ => return None,
        };
        let y = match p.get("y") {
            Some(PartValue::Int(v)) => *v as i32,
            _ => return None,
        };
        Some((x, y))
    }
    fn player(&self) -> Option<u64> {
        Some(self.player)
    }
    fn is_alive(&self, entity: u64) -> bool {
        self.try_entity(entity)
    }
    fn distance(&self, a: u64, b: u64) -> Option<u32> {
        use tessera_core::path;
        let (ax, ay) = self.position(a)?;
        let (bx, by) = self.position(b)?;
        Some(path::dist2(Point::new(ax, ay), Point::new(bx, by)))
    }
    fn path(&self, from: u64, to: u64) -> Option<Vec<(i32, i32)>> {
        use tessera_core::path;
        let (fx, fy) = self.position(from)?;
        let (tx, ty) = self.position(to)?;
        path::path(&self.zone, Point::new(fx, fy), Point::new(tx, ty), 4096)
            .map(|pts| pts.into_iter().map(|p| (p.x, p.y)).collect())
    }
    fn nearest_walkable(&self, x: i32, y: i32) -> Option<(i32, i32)> {
        use tessera_core::path;
        let occ = |cx: i32, cy: i32| self.is_occupied(cx, cy);
        path::nearest_walkable_empty(&self.zone, &occ, x, y).map(|p| (p.x, p.y))
    }
    fn random_walkable_tile(&self, rng: &mut HostRng) -> Option<(i32, i32)> {
        let mut cells = Vec::new();
        for y in 0..self.zone.height as i32 {
            for x in 0..self.zone.width as i32 {
                if self.zone.is_walkable(x, y) && !self.is_occupied(x, y) {
                    cells.push((x, y));
                }
            }
        }
        if cells.is_empty() {
            return None;
        }
        Some(cells[rng.below(cells.len() as u64) as usize])
    }
    fn player_turns(&self) -> u64 {
        *self.player_turns.lock().expect("lock")
    }
    fn zone_start(&self) -> Option<(i32, i32)> {
        Some((self.zone.start.x, self.zone.start.y))
    }
    fn world_state(&self) -> Option<u64> {
        None
    }
    fn tuning_table(&self, table: &str) -> Option<BTreeMap<String, TuningValue>> {
        self.tuning.get(table).cloned()
    }
}

/// Build a host over a fresh [`NavWorld`] with the given reader installed.
fn nav_host() -> (ScriptHost, Arc<NavWorld>) {
    let nav = Arc::new(NavWorld::new());
    let reader: Arc<dyn WorldRead> = nav.clone();
    let host = ScriptHost::with_reader(Box::new((*nav).clone()), reader);
    (host, nav)
}

/// Assert that every read-only nav function is installed and returns the
/// expected plain value against the fixed 5x5 layout.
#[test]
fn nav_read_api_registers_and_answers() {
    let (mut h, _nav) = nav_host();
    h.load_script(
        "nav",
        "Bp",
        r#"-- tessera-api: 1
             engine.on("Move", engine.NORMAL, function(ev)
               local size = engine.map_size()
               assert(size.width == 5 and size.height == 5, "map_size")
               assert(engine.is_walkable(1, 1), "is_walkable floor")
               assert(not engine.is_walkable(0, 0), "is_walkable wall")
               assert(engine.is_occupied(3, 1), "is_occupied foe at (3,1)")
               assert(not engine.is_occupied(2, 1), "is_occupied empty")
               local p = engine.position(ev.target)
               assert(p and p.x == 1 and p.y == 1, "position")
               assert(engine.player() == 1, "player")
               assert(engine.is_alive(2), "is_alive")
               local pd = engine.position(2)
               assert(pd.x == 3 and pd.y == 1, "foe position")
               assert(engine.distance(ev.target, 2) == 4, "distance dist2 (2,0)")
               local path = engine.path(ev.target, 2)
               assert(path and #path >= 1, "path exists")
               local nw = engine.nearest_walkable(0, 0)
               assert(nw, "nearest_walkable from wall")
               assert(engine.is_walkable(nw.x, nw.y), "nearest tile walkable")
               assert(not engine.is_occupied(nw.x, nw.y), "nearest tile empty")
               local rw = engine.random_walkable_tile()
               assert(rw, "random_walkable_tile")
               assert(engine.is_walkable(rw.x, rw.y), "random tile walkable")
               assert(engine.player_turns() == 0, "player_turns initial")
               engine.log("nav-ok")
               return true
             end)"#,
    )
    .expect("loads");
    let e = 1;
    let res = h.dispatch("Move", e, &[]);
    assert_eq!(res.error_count(), 0, "{res:?}");
    assert!(!res.any_veto(), "{res:?}");
    let log = h.drain_log();
    assert!(log.contains(&"nav-ok".to_string()), "{log:?}");
}

/// Every action function queues the expected [`PendingAction`]; `spawn_at`
/// additionally applies through the writer (with position and scheduler id).
#[test]
fn nav_action_api_queues_pending_and_applies_spawn_at() {
    let (mut h, _nav) = nav_host();
    h.load_script(
        "act",
        "Bp",
        r#"-- tessera-api: 1
             engine.on("Move", engine.NORMAL, function(ev)
               engine.step(ev.target, 2, 1)
               engine.attack(ev.target, 2)
               engine.wander(2)
               local id = engine.spawn_at("Goblin", 2, 2)
               assert(id > 0, "spawn_at returns positive id")
               engine.log(("spawned %d"):format(id))
               return true
             end)"#,
    )
    .expect("loads");
    let res = h.dispatch("Move", 1, &[]);
    assert_eq!(res.error_count(), 0, "handler passes: {res:?}");
    assert!(res.apply_errors.is_empty(), "{:?}", res.apply_errors);

    // Order and shape of the queued pending actions.
    assert!(
        matches!(
            &res.pending_actions[0],
            PendingAction::Step { entity, x, y } if *entity == 1 && *x == 2 && *y == 1
        ),
        "{:?}",
        res.pending_actions
    );
    assert!(
        matches!(
            &res.pending_actions[1],
            PendingAction::Attack { attacker, defender }
                if *attacker == 1 && *defender == 2
        ),
        "{:?}",
        res.pending_actions
    );
    assert!(
        matches!(&res.pending_actions[2], PendingAction::Wander { entity } if *entity == 2),
        "{:?}",
        res.pending_actions
    );
    assert!(
        matches!(
            &res.pending_actions[3],
            PendingAction::SpawnAt { id, blueprint, x, y }
                if *blueprint == "Goblin" && *x == Some(2) && *y == Some(2)
        ),
        "{:?}",
        res.pending_actions
    );

    // spawn_at was applied by the writer: a new entity exists at (2,2).
    let spawned = match &res.pending_actions[3] {
        PendingAction::SpawnAt { id, .. } => *id,
        _ => unreachable!(),
    };
    assert!(h.world().try_entity(spawned), "spawn_at applied");
    assert_eq!(
        h.world().snapshot_part(spawned, "Position").unwrap()["x"],
        PartValue::Int(2)
    );
}

/// `spawn_at` with a blueprint the writer rejects surfaces an apply error (not
/// a panic) and the rest of the batch still applies.
#[test]
fn spawn_at_unknown_blueprint_surfaces_apply_error() {
    let (mut h, _nav) = nav_host();
    h.load_script(
        "bad-spawn",
        "Bp",
        r#"-- tessera-api: 1
             engine.on("Move", engine.NORMAL, function(ev)
               local id = engine.spawn_at("NoSuch", 2, 2)
               engine.log(("id %d"):format(id))
               return true
             end)"#,
    )
    .expect("loads");
    let res = h.dispatch("Move", 1, &[]);
    assert_eq!(res.error_count(), 0, "handler itself passes: {res:?}");
    assert_eq!(
        res.apply_errors.len(),
        1,
        "one apply error for unknown blueprint: {:?}",
        res.apply_errors
    );
    assert!(
        matches!(&res.apply_errors[0], ScriptError::Internal(msg) if msg.contains("NoSuch")),
        "{:?}",
        res.apply_errors[0]
    );
}

/// A hostile handler that keeps calling `engine.path` in a loop still trips
/// the instruction budget (quarantinable), not a panic — repeated native calls
/// cannot be used to exhaust the VM.
#[test]
fn repeated_path_calls_still_trip_budget() {
    let (mut h, _nav) = nav_host();
    h.load_script(
        "hot",
        "Bp",
        r#"-- tessera-api: 1
             engine.on("Move", engine.NORMAL, function(ev)
               while true do
                 engine.path(1, 2)
               end
               return true
             end)"#,
    )
    .expect("loads");
    let res = h.dispatch("Move", 1, &[]);
    let errored_budget = res.outcomes.iter().any(|o| {
        matches!(
            o,
            tessera_script::HandlerOutcome::Errored {
                error: ScriptError::Budget { .. },
            }
        )
    });
    assert!(
        errored_budget,
        "expected a Budget error from repeated path calls, got {res:?}"
    );
}

/// `engine.on("Death", ...)` registers (the reserved event is in the known
/// set), and `engine.on("ActorTurnStart", ...)` registers too — scripts can
/// hook either.
#[test]
fn death_and_actor_turn_start_are_registrable() {
    let (mut h, nav) = nav_host();
    h.load_script(
        "hooks",
        "Bp",
        r#"-- tessera-api: 1
             engine.on("Death", engine.NORMAL, function(ev)
               engine.log(("death %d"):format(ev.target))
               return true
             end)
             engine.on("ActorTurnStart", engine.NORMAL, function(ev)
               engine.log(("turn %d"):format(ev.actor))
               return true
             end)"#,
    )
    .expect("loads");
    assert_eq!(h.handler_count("Death"), 1);
    assert_eq!(h.handler_count("ActorTurnStart"), 1);

    // Dispatching a Death to entity 2 runs the handler.
    let res = h.dispatch("Death", 2, &[]);
    assert_eq!(res.error_count(), 0, "{res:?}");
    let log = h.drain_log();
    assert!(log.iter().any(|l| l == "death 2"), "{log:?}");

    // Dispatching ActorTurnStart to entity 1 runs the handler. The `actor`
    // param is supplied by the caller (in the sim it's the acting entity).
    let res = h.dispatch("ActorTurnStart", 1, &[("actor", Value::Integer(1_i64))]);
    assert_eq!(res.error_count(), 0, "{res:?}");
    let log = h.drain_log();
    assert!(log.iter().any(|l| l == "turn 1"), "{log:?}");
    let _ = nav;
}

/// `engine.tuning` reads a named table with all three typed shapes: integers,
/// percent-of-integer tables and integer-list arrays. The read is a snapshot:
/// the same call site sees authored values until a native override lands.
#[test]
fn tuning_binding_reads_int_pct_and_list() {
    let nav = Arc::new(NavWorld::with_tuning());
    let reader: Arc<dyn WorldRead> = nav.clone();
    let mut h = ScriptHost::with_reader(Box::new((*nav).clone()), reader);
    h.load_script(
        "tuning-read",
        "Bp",
        r#"-- tessera-api: 1
           engine.on("Move", engine.NORMAL, function(ev)
             local t = engine.tuning("Tuning_Combat")
             assert(t.base_hit == 50, "int value")
             assert(t.crit_percent.pct == 150, "percent value")
             assert(t.speed_table[1] == 50 and t.speed_table[3] == 140, "list values")
             engine.log("tuning-ok")
             return true
           end)"#,
    )
    .expect("loads");
    let res = h.dispatch("Move", 1, &[]);
    assert_eq!(res.error_count(), 0, "{res:?}");
    let log = h.drain_log();
    assert!(log.contains(&"tuning-ok".to_string()), "{log:?}");
}

/// `engine.tuning` fails closed on a missing table and on a missing key
/// (fail-closed reads: the error names the table and key).
#[test]
fn tuning_binding_missing_table_and_key_raise_script_errors() {
    // A missing key raises inside the handler, naming the key and table.
    let nav = Arc::new(NavWorld::with_tuning());
    let reader: Arc<dyn WorldRead> = nav.clone();
    let mut h = ScriptHost::with_reader(Box::new((*nav).clone()), reader);
    h.load_script(
        "tuning-missing-key",
        "Bp",
        r#"-- tessera-api: 1
           engine.on("Move", engine.NORMAL, function(ev)
             local t = engine.tuning("Tuning_Combat")
             local _ = t.missing_key
             return true
           end)"#,
    )
    .expect("loads");
    let res = h.dispatch("Move", 1, &[]);
    assert_eq!(res.error_count(), 1, "{res:?}");
    let msg_has_key = res.outcomes.iter().any(|o| {
        matches!(
            o,
            tessera_script::HandlerOutcome::Errored {
                error: ScriptError::Handler { msg, .. },
            } if msg.contains("missing_key") && msg.contains("Tuning_Combat")
        )
    });
    assert!(msg_has_key, "{res:?}");

    // A missing table raises inside the handler, naming the table.
    let nav = Arc::new(NavWorld::with_tuning());
    let reader: Arc<dyn WorldRead> = nav.clone();
    let mut h2 = ScriptHost::with_reader(Box::new((*nav).clone()), reader);
    h2.load_script(
        "tuning-missing-table",
        "Bp",
        r#"-- tessera-api: 1
           engine.on("Move", engine.NORMAL, function(ev)
             local t = engine.tuning("Tuning_Nope")
             return true
           end)"#,
    )
    .expect("loads");
    let res = h2.dispatch("Move", 1, &[]);
    assert_eq!(res.error_count(), 1, "{res:?}");
    let msg_has_table = res.outcomes.iter().any(|o| {
        matches!(
            o,
            tessera_script::HandlerOutcome::Errored {
                error: ScriptError::Handler { msg, .. },
            } if msg.contains("unknown tuning table 'Tuning_Nope'")
        )
    });
    assert!(msg_has_table, "{res:?}");
}

/// The tuning binding is read-only: any write attempt raises a script error
/// (scripts must not mutate authored or overridden tuning).
#[test]
fn tuning_binding_rejects_writes() {
    let nav = Arc::new(NavWorld::with_tuning());
    let reader: Arc<dyn WorldRead> = nav.clone();
    let mut h = ScriptHost::with_reader(Box::new((*nav).clone()), reader);
    h.load_script(
        "tuning-write",
        "Bp",
        r#"-- tessera-api: 1
           engine.on("Move", engine.NORMAL, function(ev)
             local t = engine.tuning("Tuning_Combat")
             t.base_hit = 60
             return true
           end)"#,
    )
    .expect("loads");
    let res = h.dispatch("Move", 1, &[]);
    assert_eq!(res.error_count(), 1, "{res:?}");
    let msg_readonly = res.outcomes.iter().any(|o| {
        matches!(
            o,
            tessera_script::HandlerOutcome::Errored {
                error: ScriptError::Handler { msg, .. },
            } if msg.contains("read-only")
        )
    });
    assert!(msg_readonly, "{res:?}");
}
