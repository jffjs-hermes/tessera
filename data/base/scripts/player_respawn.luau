-- tessera-api: 1
-- Player death/respawn policy.
--
-- Stateless `Death` handler owned by the **Player** blueprint (wired via the
-- `scripts = ["scripts/player_respawn.luau"]` reference on `[Player]` in
-- `data/base/blueprints/creatures.toml`). When the player dies, despawn the
-- entity and respawn a fresh `Player` at the zone start tile (or the nearest
-- free walkable tile when the start is occupied). Behavior matches M6 spec
-- §7.1 exactly; the goblin's script does not touch the player.

engine.on("Death", engine.NORMAL, function(ev)
  local target = ev.target
  if target ~= engine.player() then return true end

  engine.despawn(target)

  local start = engine.zone_start()
  if not start then return true end
  local x, y = start.x, start.y
  if engine.is_occupied(x, y) then
    local near = engine.nearest_walkable(x, y)
    if not near then return true end
    x, y = near.x, near.y
  end
  engine.spawn_at("Player", x, y)
  engine.log(("player respawned at %d,%d"):format(x, y))
  return true
end)
