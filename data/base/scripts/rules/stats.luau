-- tessera-api: 1
-- rules/stats.luau — the five S1 derived stats (s1-stats §4.2), verbatim
-- from Specs/Combat Formulas.md and calibrated against BOTH prototypes
-- (Specs/prototype_formula_sim.py and Specs/prototype_class_sim.py: the
-- shared hp_formula, K["acc_per_agi"]=2, clamp(100+2*(A-10),50,140), and the
-- post-status max(sp,20) floor).
--
-- Integer-only: the ONLY division operator is Luau floor division `//`
-- (floor also for negatives), exactly as the formula doc specifies. No
-- floats anywhere.
--
-- Registered once per engine as the RecomputeDerived handler (the engine
-- looks up the well-known script path rules/stats.luau at sim install; the
-- first pack providing it wins, M5 semantics). The handler is called after
-- every declared dirtying event; it computes the five FORMULA BASES from the
-- settled snapshot (Attributes/Level/Bonuses + Tuning_Combat) and writes
-- them via engine.set_part — the native recompute seam then applies live
-- status modifiers, the invariant clamps and the Health.hp clamp before
-- control returns to the scheduler (§4.5, §5.2). The script never touches
-- Health and never reads the cache. Tuning is read per invocation (the
-- table is built when blueprints load, after this script's top level), so a
-- tuning reload flows into the next recompute without re-registration.

-- Attribute modifier, floor for negatives (the formula doc's shared
-- convention; consumed by damage resolution in S2, defined here verbatim).
local function mod(x)
  return (x - 10) // 2
end

engine.on("RecomputeDerived", engine.NORMAL, function(ev)
  local T = engine.tuning("Tuning_Combat")

  local function max_hp(toughness, level)
    return T.hp_base + T.hp_per_toughness * toughness
         + (level - 1) * (T.hp_level_base + toughness // T.hp_toughness_divisor)
  end

  local function accuracy(level, agility, bonuses)
    return T.level_bonus * level
         + T.acc_per_agility * (agility - 10)
         + (bonuses.accuracy or 0)
  end

  local function dodge(level, agility, bonuses)
    return T.level_bonus * level
         + T.acc_per_agility * (agility - 10)
         + (bonuses.dodge or 0)
  end

  local function ac(bonuses)
    return bonuses.ac or 0
  end

  local function defense(dodge_v, ac_v)
    return dodge_v + math.min(ac_v * T.ac_factor, T.ac_cap)
  end

  local function speed_base(agility)
    local raw = 100 + T.speed_slope * (agility - 10)
    if raw < T.speed_min then return T.speed_min end
    if raw > T.speed_max then return T.speed_max end
    return raw
  end

  local attrs = engine.get_part(ev.target, "Attributes")
  if attrs == nil then
    -- A legacy entity without Attributes recomputes nothing; the native
    -- seam keeps the absent cache (detectable) and legacy energy speed.
    return
  end
  local level = engine.get_part(ev.target, "Level")
  local bonuses = engine.get_part(ev.target, "Bonuses") or {}

  local lvl = level.level
  local agi = attrs.agility
  local tou = attrs.toughness
  local d = dodge(lvl, agi, bonuses)
  local ac_v = ac(bonuses)

  engine.set_part(ev.target, "Derived", {
    max_hp = max_hp(tou, lvl),
    accuracy = accuracy(lvl, agi, bonuses),
    dodge = d,
    ac = ac_v,
    defense = defense(d, ac_v),
    speed = speed_base(agi),
  })

  -- The combat cache: block chance plus per-type resistance and threshold.
  -- Written in the same handler as `Derived` so one recompute fills both
  -- caches. The values are the INNATE sources (the authored Bonuses part):
  -- equipment contributions are the gear layer's terms and are added there,
  -- never written back into Bonuses, so nothing is counted twice.
  --
  -- Block: the shield's block base plus +1 per Might above 10 (the design
  -- table's `15 + (Might - 10)`); a negative total floors at 0.
  local block = (bonuses.block or 0) + (attrs.might - 10)
  if block < 0 then block = 0 end

  local combat = { block = block }
  local res = bonuses.resist or {}
  local thr = bonuses.threshold or {}
  -- Iterated in the native vocabulary's declaration order so the written
  -- field set is deterministic.
  local TYPES = { "slash", "pierce", "blunt", "fire", "cold",
                  "lightning", "poison", "holy", "shadow" }
  for _, t in ipairs(TYPES) do
    local r = res[t]
    if r ~= nil then
      if r < T.res_floor then r = T.res_floor end
      if r > T.res_ceiling then r = T.res_ceiling end
      combat["resist_" .. t] = r
    end
    local v = thr[t]
    if v ~= nil then combat["threshold_" .. t] = v end
  end

  engine.set_part(ev.target, "Combat", combat)
end)
