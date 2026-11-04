-- tessera-api: 1
-- The reference (expert) player policy: hold the line, strike the weakest
-- adjacent enemy.
--
-- Transcribed from the class prototype's knight_act, phase-1 subset: no
-- abilities exist yet, so the ability-gated branches are simply absent. When no
-- enemy stands adjacent the Knight WAITS -- it never advances on its own; only
-- the raiders approach, under their own policy. This is deliberate and is what
-- the approved bands were measured against.
engine.on("ActorTurnStart", engine.NORMAL, function(ev)
  local me = ev.actor
  if me ~= engine.player() then return true end
  if not engine.is_alive(me) then return true end

  -- Lowest-HP adjacent live enemy; ties resolve by entity id (the candidate
  -- scan is id-ordered and the comparison is strict).
  local best = nil
  local best_hp = nil
  for _, e in ipairs(engine.entities()) do
    if e ~= me then
      local d2 = engine.distance(me, e)
      local hp = engine.hp(e)
      if d2 ~= nil and d2 <= 2 and hp ~= nil and hp > 0 and engine.is_alive(e) then
        if best == nil or hp < best_hp then
          best = e
          best_hp = hp
        end
      end
    end
  end

  if best ~= nil then
    engine.attack(me, best)
  end
  return true
end)
