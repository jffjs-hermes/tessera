-- tessera-api: 1
-- Goblin AI + death/respawn policy.
--
-- Stateless per turn (the ActorTurnStart hook): straight-line distance-10
-- chase, adjacent attack, and seeded wander via engine.wander. The script owns
-- both Death responses: a dead player respawns at the zone start tile (or the
-- nearest free tile), and a dead goblin respawns after 5 player turns at a
-- random walkable-empty tile. The respawn clock lives in a `RespawnSystem`
-- script part on the world-state singleton (`engine.world_state()`), which the
-- binary creates at load.
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
local PLAYER_BP = "Player"
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

local function respawn_player()
  local start = engine.zone_start()
  if not start then return end
  local x, y = start.x, start.y
  if engine.is_occupied(x, y) then
    local near = engine.nearest_walkable(x, y)
    if not near then return end
    x, y = near.x, near.y
  end
  engine.spawn_at(PLAYER_BP, x, y)
  engine.log(("player respawned at %d,%d"):format(x, y))
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
  if target == engine.player() then
    engine.despawn(target)
    respawn_player()
    return true
  end
  if engine.get_part(target, "GoblinBrain") then
    engine.despawn(target)
    schedule_goblin_respawn()
    return true
  end
  return true
end)