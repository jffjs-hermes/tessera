-- tessera-api: 1
-- The naive player policy: attack if adjacent, otherwise walk at the nearest
-- enemy. No HP awareness, no positioning — the comparison tier reported next to
-- the expert band, never asserted against it.
engine.on("ActorTurnStart", engine.NORMAL, function(ev)
  local me = ev.actor
  if me ~= engine.player() then return true end
  if not engine.is_alive(me) then return true end

  local nearest = nil
  local nearest_d = nil
  for _, e in ipairs(engine.entities()) do
    if e ~= me then
      local d2 = engine.distance(me, e)
      local hp = engine.hp(e)
      if d2 ~= nil and hp ~= nil and hp > 0 and engine.is_alive(e) then
        if nearest == nil or d2 < nearest_d then
          nearest = e
          nearest_d = d2
        end
      end
    end
  end

  if nearest == nil then return true end
  if nearest_d ~= nil and nearest_d <= 2 then
    engine.attack(me, nearest)
    return true
  end
  local path = engine.path(me, nearest)
  if path and path[1] then
    engine.step(me, path[1].x, path[1].y)
  end
  return true
end)
