-- tessera-api: 1
-- Raider AI: chase the player, attack when adjacent, no corpse policy.
--
-- The real AI path (the ActorTurnStart turn hook) with a creature-local brain
-- part, so the demo goblin's respawn response never applies to a batch run.
-- Detection is straight-line squared distance <= detect^2, as on the demo
-- goblin; the walk is the engine's grid path, so the single-file queue through
-- a choke point is emergent from occupancy rather than scripted.

engine.define_part("RaiderBrain", {
  fields = {
    detect = { kind = "int", default = 40, min = 1, max = 100 },
  },
})

local ADJACENT2 = 2 -- 8-neighbourhood: squared distance of one step

engine.on("ActorTurnStart", engine.NORMAL, function(ev)
  local me = ev.actor
  local brain = engine.get_part(me, "RaiderBrain")
  if not brain then return true end
  if not engine.is_alive(me) then return true end

  local foe = engine.player()
  if not foe or not engine.is_alive(foe) then return true end

  local detect = brain.detect or 10
  local d2 = engine.distance(me, foe)
  if d2 == nil or d2 > detect * detect then return true end

  if d2 <= ADJACENT2 then
    engine.attack(me, foe)
    return true
  end
  local path = engine.path(me, foe)
  if path and path[1] then
    engine.step(me, path[1].x, path[1].y)
  end
  return true
end)

-- No corpse policy in phase 1 (no loot either): a dead raider leaves the map
-- so its tile frees up for the rest of the queue.
engine.on("Death", engine.NORMAL, function(ev)
  if engine.get_part(ev.target, "RaiderBrain") then
    engine.despawn(ev.target)
  end
  return true
end)
