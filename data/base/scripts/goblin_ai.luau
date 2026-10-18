-- tessera-api: 1
-- Goblin AI + death/respawn policy (goblin only).
--
-- Stateless per turn (the ActorTurnStart hook): straight-line distance-10
-- chase, adjacent attack, and seeded wander via engine.wander. The script owns
-- the goblin's Death response: a dead goblin respawns after 5 player turns at a
-- random walkable-empty tile. The respawn clock lives in a `RespawnSystem`
-- script part on the world-state singleton (`engine.world_state()`), which the
-- binary creates at load. The player's death/respawn is NOT handled here — it
-- lives in the Player-owned `scripts/player_respawn.luau`.
--
-- `parts.GoblinBrain = { detect = 10 }` (on the Goblin blueprint) carries the
-- tuning: detection is straight-line squared distance <= detect^2 (100).

engine.define_part("GoblinBrain", {
  fields = {
    detect = { kind = "int", default = 10, min = 1, max = 100 },
  },
})

engine.define_part("RespawnSystem", {
  fields = {
    due = { kind = "int", default = -1, min = -1, max = 1000000000 },
    blueprint = { kind = "string", default = "" },
  },
})

local DETECT2 = 100 -- straight-line distance <= 10 (squared)
local ADJACENT2 = 2 -- Chebyshev distance 1 (8-neighborhood, squared)
local RESPAWN_DELAY = 5 -- player turns
local GOBLIN_BP = "Goblin"

local function schedule_goblin_respawn()
  local ws = engine.world_state()
  if not ws then return end
  local clock = engine.get_part(ws, "RespawnSystem") or { due = -1, blueprint = "" }
  clock.due = engine.player_turns() + RESPAWN_DELAY
  clock.blueprint = GOBLIN_BP
  engine.set_part(ws, "RespawnSystem", clock)
  engine.log(("goblin death: respawn due at player turn %d"):format(clock.due))
end

-- Checked on the player's turn (ActorTurnStart fires every player turn): once
-- the player-turn clock reaches `due`, respawn the queued blueprint at a random
-- walkable-empty tile. A full map (no tile) leaves the clock pending and retries
-- next player turn.
local function check_goblin_respawn()
  local ws = engine.world_state()
  if not ws then return end
  local clock = engine.get_part(ws, "RespawnSystem")
  if not clock or clock.due < 0 or engine.player_turns() < clock.due then return end
  local tile = engine.random_walkable_tile()
  if not tile then
    engine.log("goblin respawn: no walkable empty tile, retry next turn")
    return
  end
  engine.spawn_at(clock.blueprint, tile.x, tile.y)
  engine.set_part(ws, "RespawnSystem", { due = -1, blueprint = "" })
  engine.log(("goblin respawned at %d,%d"):format(tile.x, tile.y))
end

engine.on("ActorTurnStart", engine.NORMAL, function(ev)
  local me = ev.actor

  -- The respawn clock is checked from the player's turn (fires every player turn).
  if me == engine.player() then
    check_goblin_respawn()
    return true
  end

  -- Only a goblin (carrying the GoblinBrain part) acts on its own turn.
  if not engine.get_part(me, "GoblinBrain") then return true end
  if not engine.is_alive(me) then return true end

  local foe = engine.player()
  if not foe or not engine.is_alive(foe) then return true end

  local d2 = engine.distance(me, foe)
  if d2 and d2 <= DETECT2 then
    if d2 <= ADJACENT2 then
      engine.attack(me, foe)
    else
      local path = engine.path(me, foe)
      if path and path[1] then
        engine.step(me, path[1].x, path[1].y)
      end
    end
  else
    engine.wander(me)
  end
  return true
end)

engine.on("Death", engine.NORMAL, function(ev)
  local target = ev.target
  if engine.get_part(target, "GoblinBrain") then
    engine.despawn(target)
    schedule_goblin_respawn()
    return true
  end
  -- The player's death is NOT handled here; it belongs to the Player-owned
  -- `scripts/player_respawn.luau`.
  return true
end)
