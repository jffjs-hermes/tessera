-- tessera-api: 1
-- Reference demo Zapper (§9.1): a goblin variant that, on each Attack it
-- lands, spends one Charge to zap the defender for an extra point of damage.
--
-- Runs *before* any entity spawns from the Zapper blueprint, registering the
-- Charge schema (declarative registration, not per-spawn). The Attack handler
-- is LATE, so it observes the defender's post-base-hit state and stacks the
-- zap on top of the native base damage.
engine.define_part("Charge", {
  fields = { charges = { kind = "int", default = 1, min = 0, max = 99 } },
})

local ATTACK_HOOK = engine.LATE -- runs after the native base hit, before clamp

engine.on("Attack", ATTACK_HOOK, function(ev)
  local charge = engine.get_part(ev.attacker, "Charge")
  if charge and charge.charges > 0 then
    engine.set_part(ev.attacker, "Charge", { charges = charge.charges - 1 })
    local dmg = engine.get_part(ev.defender, "Health")
    if dmg then
      dmg.hp = dmg.hp - 1                        -- via the whitelisted Health.hp path
      engine.set_part(ev.defender, "Health", dmg)
    end
    engine.log(("zapper zaps %d"):format(ev.defender))
  end
  return true
end)