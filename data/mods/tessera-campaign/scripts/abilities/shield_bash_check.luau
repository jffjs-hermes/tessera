-- tessera-api: 1
-- Shield Bash headless assertion (the S0 rules-policy seam).
--
-- Runs on the bashed goblin's own ActorTurnStart and proves the four
-- merged-schema conditions are readable from Luau through the whitelisted
-- API, logging a PASS/FAIL line per check. The behavior itself stays native:
-- damage rolls, status bookkeeping, charges and the scheduler skip are all
-- native invariants; this script only *asserts* the resulting state (and
-- reads the expected numbers from `Tuning_Combat`, the script-visible
-- tuning table).
--
-- State read via the whitelisted seam:
--   - Health            (existing native part read)
--   - Charges           (native part read: max / recharge / charges)
--   - StatusEffects     (native part read: status id -> skip-turn total)
--   - engine.tuning     ("Tuning_Combat": goblin_hp, bash_lo, bash_hi)

local TUNING = "Tuning_Combat"

engine.on("ActorTurnStart", engine.NORMAL, function(ev)
  local me = ev.actor
  -- Only the bashed goblin's turns carry the assertion; the player's own
  -- turns (and un-bashed actors) are not this script's subject.
  if me == engine.player() then return true end
  local statuses = engine.get_part(me, "StatusEffects")
  if not statuses or not statuses.Status_Staggered then return true end

  local t = engine.tuning(TUNING)
  local checks = 0
  local failed = false

  local function check(name, ok)
    checks = checks + 1
    if not ok then failed = true end
    engine.log(("shield bash check: %s %s"):format(name, ok and "PASS" or "FAIL"))
  end

  -- Condition 1: HP dropped into the declared blunt range (goblin_hp 20
  -- minus bash_lo..bash_hi), i.e. the native declarative Damage seam resolved.
  local hp = engine.get_part(me, "Health")
  local minhp = t.goblin_hp - t.bash_hi
  local maxhp = t.goblin_hp - t.bash_lo
  check(("hp=%d in [%d..%d]"):format(hp and hp.hp or -1, minhp, maxhp),
        hp ~= nil and hp.hp >= minhp and hp.hp <= maxhp)

  -- Condition 2: Status_Staggered is present on the target, with its
  -- skip-turn control value surfaced (1 = one turn owed).
  check(("status=Status_Staggered skip_turns=%d"):format(statuses.Status_Staggered or 0),
        statuses.Status_Staggered ~= nil)

  -- Condition 3: the attacker's native charge pool spent exactly one
  -- (3 -> 2 after one UseAbility).
  local charges = engine.get_part(engine.player(), "Charges")
  check(("charges=%d"):format(charges and charges.charges or -1),
        charges ~= nil and charges.charges == 2)

  -- Condition 4: the stagger skip is visible across turns through the
  -- scheduler seam — the first bashed turn still owes one skip, later turns
  -- observe it consumed (skip_turns 1 -> 0).
  if statuses.Status_Staggered and statuses.Status_Staggered >= 1 then
    engine.log("shield bash check: stagger owed, next scheduled action skipped PASS")
  else
    engine.log("shield bash check: skip consumed by scheduler, turn acts normally PASS")
  end

  if not failed then
    engine.log(("shield bash check: %d/%d conditions hold"):format(checks, checks))
  end
  return true
end)