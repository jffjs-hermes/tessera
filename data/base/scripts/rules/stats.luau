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

  -- Gear terms (items and equipment): every contribution is read from the
  -- equipped item blueprints through engine.item; nothing is written into
  -- `Bonuses`, so an equipped item is never double-counted. `worn` is the
  -- armor load (armor slots + off-hand, the design's Items and Gear rule);
  -- the overload penalty is applied to the speed base before modifiers.
  local ARMOR_SLOTS = { "head", "body", "arms", "legs" }

  local function gear_totals(equipment, might)
    local totals = { ac = 0, accuracy = 0, worn = 0, over = 0 }
    if equipment == nil then return totals end
    local function add(id)
      if id == nil or id == "" then return end
      local item = engine.item(id)
      if item == nil then return end
      totals.ac = totals.ac + (item.ac or 0)
      totals.accuracy = totals.accuracy + (item.accuracy_bonus or 0)
      totals.worn = totals.worn + (item.weight or 0)
    end
    for _, slot in ipairs(ARMOR_SLOTS) do
      add(equipment[slot])
    end
    add(equipment.off_hand)
    totals.over = math.max(0, totals.worn - T.armor_threshold_per_might * might)
    return totals
  end

  local function speed_with_load(base, gear)
    local penalty = T.armor_speed_per_weight * gear.over
    if penalty <= 0 then return base end
    return base * (100 - penalty) // 100
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
  local gear = gear_totals(engine.get_part(ev.target, "Equipment"), attrs.might)
  local d = dodge(lvl, agi, bonuses)
  local ac_v = ac(bonuses) + gear.ac

  engine.set_part(ev.target, "Derived", {
    max_hp = max_hp(tou, lvl),
    accuracy = accuracy(lvl, agi, bonuses) + gear.accuracy,
    dodge = d,
    ac = ac_v,
    defense = defense(d, ac_v),
    speed = speed_with_load(speed_base(agi), gear),
  })
end)
