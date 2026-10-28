//! Equipment, inventory and the item payload parts.
//!
//! Three layers, all native schema (no formulas, no policy):
//!
//! - [`Equipment`] (a fixed six-slot map of item blueprint ids) and
//!   [`Inventory`] (an ordered id list) are the persistent build state;
//! - [`Weapon`] and [`Armor`] are the typed payload parts an `Item_*`
//!   blueprint carries beside the [`crate::kinds::Item`] identity part;
//! - the helpers in this module apply the load/equip invariants (closed slot
//!   vocabulary, hands occupancy, ownership, pack capacity) so the console,
//!   the save path and any later pickup path share one implementation.
//!
//! Everything here is integer-valued and deterministic: iteration is a fixed
//! slot order or a `BTreeMap`, never a hash container. Tunable policy (armor
//! thresholds, the speed penalty) lives in the rules script; this module only
//! reports what is worn and what it weighs.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::ecs::{PartError, PartRegistry};
use crate::kinds::Item;
use crate::stats::AttributeName;
use crate::world::World;

/// `parts.Equipment`: slot name → equipped item blueprint id (`""` = empty).
pub const PART_EQUIPMENT: &str = "Equipment";
/// `parts.Inventory`: owned item blueprint ids in pickup order.
pub const PART_INVENTORY: &str = "Inventory";
/// `parts.Item`: the item identity part (registered by [`crate::kinds`]).
pub const PART_ITEM: &str = "Item";
/// `parts.ItemWeapon`: the melee weapon payload of an `Item_*` blueprint.
pub const PART_WEAPON: &str = "ItemWeapon";
/// `parts.ItemArmor`: the armor payload of an `Item_*` blueprint.
pub const PART_ARMOR: &str = "ItemArmor";

/// Pack capacity per point of Might (design: `10 × Might`).
pub const PACK_CAPACITY_PER_MIGHT: i64 = 10;

/// Load-time weight bound for one item (a slice-scale sanity cap).
pub const ITEM_WEIGHT_MAX: i64 = 100;

/// The closed item-tier vocabulary. Tiers are an axis for later affix work;
/// the gear layer only persists and reports them, so an unknown tier is a
/// load error.
pub const TIERS: [&str; 4] = ["common", "fine", "masterwork", "rare"];

/// The fixed equipment slot vocabulary. Slot names are schema, never data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EquipmentSlot {
    MainHand,
    OffHand,
    Head,
    Body,
    Arms,
    Legs,
}

impl EquipmentSlot {
    /// All six slots in declaration (deterministic) order.
    pub const ALL: [EquipmentSlot; 6] = [
        EquipmentSlot::MainHand,
        EquipmentSlot::OffHand,
        EquipmentSlot::Head,
        EquipmentSlot::Body,
        EquipmentSlot::Arms,
        EquipmentSlot::Legs,
    ];

    /// The armor slots whose weight counts toward the armor load.
    pub const ARMOR: [EquipmentSlot; 4] = [
        EquipmentSlot::Head,
        EquipmentSlot::Body,
        EquipmentSlot::Arms,
        EquipmentSlot::Legs,
    ];

    /// The canonical lowercase spelling of this slot.
    pub fn as_str(self) -> &'static str {
        match self {
            EquipmentSlot::MainHand => "main_hand",
            EquipmentSlot::OffHand => "off_hand",
            EquipmentSlot::Head => "head",
            EquipmentSlot::Body => "body",
            EquipmentSlot::Arms => "arms",
            EquipmentSlot::Legs => "legs",
        }
    }

    /// Parse a canonical slot name (lowercase, exact). Unknown names are
    /// `None` — callers fail closed rather than guessing.
    ///
    /// The fixed `from_str` name mirrors [`crate::stats::AttributeName`]; the
    /// std trait's method is deliberately shadowed.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(name: &str) -> Option<EquipmentSlot> {
        match name {
            "main_hand" => Some(EquipmentSlot::MainHand),
            "off_hand" => Some(EquipmentSlot::OffHand),
            "head" => Some(EquipmentSlot::Head),
            "body" => Some(EquipmentSlot::Body),
            "arms" => Some(EquipmentSlot::Arms),
            "legs" => Some(EquipmentSlot::Legs),
            _ => None,
        }
    }
}

/// `parts.Equipment`: the fixed slot map. An empty string means "nothing
/// equipped"; the struct carries every slot so a save is a complete build
/// snapshot and a patch can replace one slot without restating the rest.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Equipment {
    #[serde(default)]
    pub main_hand: String,
    #[serde(default)]
    pub off_hand: String,
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub arms: String,
    #[serde(default)]
    pub legs: String,
}

impl Equipment {
    /// Read one slot ( `""` when empty).
    pub fn slot(&self, slot: EquipmentSlot) -> &str {
        match slot {
            EquipmentSlot::MainHand => &self.main_hand,
            EquipmentSlot::OffHand => &self.off_hand,
            EquipmentSlot::Head => &self.head,
            EquipmentSlot::Body => &self.body,
            EquipmentSlot::Arms => &self.arms,
            EquipmentSlot::Legs => &self.legs,
        }
    }

    /// The mutable accessor for one slot.
    pub fn slot_mut(&mut self, slot: EquipmentSlot) -> &mut String {
        match slot {
            EquipmentSlot::MainHand => &mut self.main_hand,
            EquipmentSlot::OffHand => &mut self.off_hand,
            EquipmentSlot::Head => &mut self.head,
            EquipmentSlot::Body => &mut self.body,
            EquipmentSlot::Arms => &mut self.arms,
            EquipmentSlot::Legs => &mut self.legs,
        }
    }

    /// Write one slot (whole-value assignment).
    pub fn set(&mut self, slot: EquipmentSlot, item_id: impl Into<String>) {
        *self.slot_mut(slot) = item_id.into();
    }

    /// Empty one slot, returning the item id it held ( `""` if empty).
    pub fn take(&mut self, slot: EquipmentSlot) -> String {
        std::mem::take(self.slot_mut(slot))
    }

    /// Every `(slot, item id)` pair in fixed slot order, including empties.
    pub fn iter(&self) -> impl Iterator<Item = (EquipmentSlot, &str)> {
        EquipmentSlot::ALL
            .into_iter()
            .map(move |slot| (slot, self.slot(slot)))
    }

    /// True when no slot holds an item.
    pub fn is_empty(&self) -> bool {
        EquipmentSlot::ALL
            .into_iter()
            .all(|slot| self.slot(slot).is_empty())
    }
}

/// `parts.Inventory`: the owned item ids in pickup order. A list, not a set:
/// duplicates are legal (two identical daggers) and order is meaningful for
/// the console listing and the save snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    #[serde(default)]
    pub items: Vec<String>,
}

impl Inventory {
    /// Append one item id.
    pub fn push(&mut self, item_id: impl Into<String>) {
        self.items.push(item_id.into());
    }

    /// Remove and return the first occurrence of `item_id`, if any.
    pub fn take(&mut self, item_id: &str) -> Option<String> {
        let index = self.items.iter().position(|id| id == item_id)?;
        Some(self.items.remove(index))
    }

    /// Number of owned items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when the inventory holds nothing.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// `parts.ItemWeapon`: the melee payload of an item blueprint.
///
/// `stat` names the attribute that scales damage; `cost` is the attack's
/// energy cost in the scheduler's units. Damage-type resolution belongs to the
/// combat pipeline, so this part carries no damage-type field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "WeaponWire")]
pub struct Weapon {
    pub lo: i64,
    pub hi: i64,
    pub stat: String,
    pub hands: i64,
    pub reach: i64,
    pub cost: i64,
    #[serde(default)]
    pub accuracy_bonus: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WeaponWire {
    lo: i64,
    hi: i64,
    stat: String,
    hands: i64,
    reach: i64,
    cost: i64,
    #[serde(default)]
    accuracy_bonus: i64,
}

impl TryFrom<WeaponWire> for Weapon {
    type Error = String;

    fn try_from(w: WeaponWire) -> Result<Self, String> {
        if w.lo < 0 || w.hi < w.lo {
            return Err(format!(
                "weapon damage range must satisfy 0 <= lo <= hi, found {}..{}",
                w.lo, w.hi
            ));
        }
        if AttributeName::from_str(&w.stat).is_none() {
            return Err(format!("weapon stat '{}' is not a known attribute", w.stat));
        }
        if !(1..=2).contains(&w.hands) {
            return Err(format!("weapon hands must be 1 or 2, found {}", w.hands));
        }
        if !(1..=3).contains(&w.reach) {
            return Err(format!("weapon reach must be in [1, 3], found {}", w.reach));
        }
        if w.cost < 0 {
            return Err(format!("weapon cost must be >= 0, found {}", w.cost));
        }
        Ok(Weapon {
            lo: w.lo,
            hi: w.hi,
            stat: w.stat,
            hands: w.hands,
            reach: w.reach,
            cost: w.cost,
            accuracy_bonus: w.accuracy_bonus,
        })
    }
}

/// `parts.ItemArmor`: flat AC/block plus per-damage-type resistance and threshold
/// maps. Keys are damage-type names validated as lowercase identifiers; the
/// combat pipeline owns the authoritative type enum and consumes these maps.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(try_from = "ArmorWire")]
pub struct Armor {
    #[serde(default)]
    pub ac: i64,
    #[serde(default)]
    pub block: i64,
    #[serde(default)]
    pub resist: BTreeMap<String, i64>,
    #[serde(default)]
    pub threshold: BTreeMap<String, i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmorWire {
    #[serde(default)]
    ac: i64,
    #[serde(default)]
    block: i64,
    #[serde(default)]
    resist: BTreeMap<String, i64>,
    #[serde(default)]
    threshold: BTreeMap<String, i64>,
}

impl TryFrom<ArmorWire> for Armor {
    type Error = String;

    fn try_from(a: ArmorWire) -> Result<Self, String> {
        if a.ac < 0 {
            return Err(format!("armor ac must be >= 0, found {}", a.ac));
        }
        if !(0..=100).contains(&a.block) {
            return Err(format!(
                "armor block must be in [0, 100], found {}",
                a.block
            ));
        }
        for key in a.resist.keys().chain(a.threshold.keys()) {
            if key.is_empty()
                || !key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                return Err(format!(
                    "armor type key '{key}' must be a lowercase identifier"
                ));
            }
        }
        Ok(Armor {
            ac: a.ac,
            block: a.block,
            resist: a.resist,
            threshold: a.threshold,
        })
    }
}

/// Register the gear parts with a serde factory (the identity `Item` part is
/// already registered by [`crate::kinds`]).
pub fn register_all(registry: &mut PartRegistry) -> Result<(), PartError> {
    crate::kinds::register::<Equipment>(registry, PART_EQUIPMENT)?;
    crate::kinds::register::<Inventory>(registry, PART_INVENTORY)?;
    crate::kinds::register::<Weapon>(registry, PART_WEAPON)?;
    crate::kinds::register::<Armor>(registry, PART_ARMOR)?;
    Ok(())
}

/// The parsed payload of one `Item_*` blueprint (identity + optional weapon and
/// armor parts), or `None` when the id is unknown or malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemData {
    pub id: String,
    pub item: Item,
    pub weapon: Option<Weapon>,
    pub armor: Option<Armor>,
}

/// Read and parse an `Item_*` blueprint from the loaded library. Malformed
/// payloads are `None` here because the loader already failed closed on them;
/// an id that vanished post-load (a mod reload) is simply unknown.
pub fn blueprint_item(world: &World, id: &str) -> Option<ItemData> {
    let library = world.library().read().unwrap_or_else(|p| p.into_inner());
    let bp = library.get(id)?;
    let item: Item = bp.part(PART_ITEM)?.clone().try_into().ok()?;
    let weapon = bp
        .part(PART_WEAPON)
        .and_then(|v| v.clone().try_into::<Weapon>().ok());
    let armor = bp
        .part(PART_ARMOR)
        .and_then(|v| v.clone().try_into::<Armor>().ok());
    Some(ItemData {
        id: bp.id.clone(),
        item,
        weapon,
        armor,
    })
}

/// The flat item summary the script host hands to `engine.item(id)`: identity
/// plus the numeric payload the item policy script consumes. Nested
/// resistance/threshold maps are deliberately absent — they belong to the
/// combat resolver, not to the gear policy terms.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemInfo {
    pub name: String,
    pub slot: String,
    pub weight: i64,
    pub tier: String,
    pub price: i64,
    pub ac: i64,
    pub block: i64,
    pub hands: i64,
    pub cost: i64,
    pub lo: i64,
    pub hi: i64,
    pub accuracy_bonus: i64,
}

/// Read the script-facing summary of one item blueprint.
pub fn item_info(world: &World, id: &str) -> Option<ItemInfo> {
    let data = blueprint_item(world, id)?;
    let weapon = data.weapon;
    let armor = data.armor;
    Some(ItemInfo {
        name: data.item.name,
        slot: data.item.slot,
        weight: data.item.weight,
        tier: data.item.tier,
        price: data.item.price,
        ac: armor.as_ref().map(|a| a.ac).unwrap_or(0),
        block: armor.as_ref().map(|a| a.block).unwrap_or(0),
        hands: weapon.as_ref().map(|w| w.hands).unwrap_or(0),
        cost: weapon.as_ref().map(|w| w.cost).unwrap_or(0),
        lo: weapon.as_ref().map(|w| w.lo).unwrap_or(0),
        hi: weapon.as_ref().map(|w| w.hi).unwrap_or(0),
        accuracy_bonus: weapon.as_ref().map(|w| w.accuracy_bonus).unwrap_or(0),
    })
}

/// The weight of one equipped/owned item id (0 for an unknown id — the equip
/// path rejects unknown ids first, so this only ever sees valid ones).
pub fn item_weight(world: &World, id: &str) -> i64 {
    if id.is_empty() {
        return 0;
    }
    blueprint_item(world, id)
        .map(|d| d.item.weight)
        .unwrap_or(0)
}

/// The armor load: the sum of worn armor-slot weights plus the off-hand item's
/// weight (a shield is worn, a second blade is not armor-cased). The speed
/// penalty itself is script policy over this number.
pub fn worn_weight(equipment: &Equipment, world: &World) -> i64 {
    let mut total = 0;
    for slot in EquipmentSlot::ARMOR {
        total += item_weight(world, equipment.slot(slot));
    }
    total += item_weight(world, equipment.slot(EquipmentSlot::OffHand));
    total
}

/// The weight of every item carried in the inventory.
pub fn inventory_weight(inventory: &Inventory, world: &World) -> i64 {
    inventory
        .items
        .iter()
        .map(|id| item_weight(world, id))
        .sum()
}

/// The pack capacity for one Might value (design: `10 × Might`).
pub fn pack_capacity(might: i64) -> i64 {
    might.max(0) * PACK_CAPACITY_PER_MIGHT
}

/// A gear operation failure. Every variant is a user-visible command error —
/// the console surfaces the text, never a silent no-op.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EquipError {
    #[error("unknown item '{id}'")]
    UnknownItem { id: String },
    #[error("'{id}' is not in the inventory")]
    NotInInventory { id: String },
    #[error("unknown equipment slot '{slot}'")]
    UnknownSlot { slot: String },
    #[error("slot '{slot}' is occupied")]
    SlotOccupied { slot: String },
    #[error("'{id}' belongs in slot '{expected}', not '{found}'")]
    WrongSlot {
        id: String,
        expected: String,
        found: String,
    },
    #[error("hands busy: a two-handed weapon leaves the off hand empty")]
    HandsBusy,
    #[error("slot '{slot}' is empty")]
    NotEquipped { slot: String },
    #[error("over capacity: {load} > {capacity}")]
    OverCapacity { load: i64, capacity: i64 },
}

/// Give one item to the pack: checks capacity (`10 × Might`) and refuses
/// over-capacity pickups (no Burdened status exists in this increment).
pub fn give(
    inventory: &mut Inventory,
    might: i64,
    item_id: &str,
    world: &World,
) -> Result<(), EquipError> {
    let data = blueprint_item(world, item_id).ok_or_else(|| EquipError::UnknownItem {
        id: item_id.to_string(),
    })?;
    let capacity = pack_capacity(might);
    let load = inventory_weight(inventory, world) + data.item.weight;
    if load > capacity {
        return Err(EquipError::OverCapacity { load, capacity });
    }
    inventory.push(item_id);
    Ok(())
}

/// Move an owned item from the inventory into a slot.
///
/// Invariants enforced here (native, not script policy): the item must be
/// owned, its authored slot must exist, the target slot may be named but must
/// match the item's slot, the target slot must be empty, and a two-handed
/// main-hand weapon requires an empty off hand.
pub fn equip(
    equipment: &mut Equipment,
    inventory: &mut Inventory,
    item_id: &str,
    slot: Option<EquipmentSlot>,
    world: &World,
) -> Result<EquipmentSlot, EquipError> {
    let data = blueprint_item(world, item_id).ok_or_else(|| EquipError::UnknownItem {
        id: item_id.to_string(),
    })?;
    let item_slot =
        EquipmentSlot::from_str(&data.item.slot).ok_or_else(|| EquipError::UnknownSlot {
            slot: data.item.slot.clone(),
        })?;
    let target = slot.unwrap_or(item_slot);
    if target != item_slot {
        return Err(EquipError::WrongSlot {
            id: item_id.to_string(),
            expected: item_slot.as_str().to_string(),
            found: target.as_str().to_string(),
        });
    }
    if !equipment.slot(target).is_empty() {
        return Err(EquipError::SlotOccupied {
            slot: target.as_str().to_string(),
        });
    }
    let hands = data.weapon.as_ref().map(|w| w.hands).unwrap_or(0);
    match target {
        EquipmentSlot::MainHand if hands == 2 && !equipment.off_hand.is_empty() => {
            return Err(EquipError::HandsBusy);
        }
        EquipmentSlot::OffHand => {
            // The main hand already holds a two-handed weapon: no off hand.
            if let Some(main) = blueprint_item(world, &equipment.main_hand)
                && main.weapon.as_ref().is_some_and(|w| w.hands == 2)
            {
                return Err(EquipError::HandsBusy);
            }
        }
        _ => {}
    }
    inventory
        .take(item_id)
        .ok_or_else(|| EquipError::NotInInventory {
            id: item_id.to_string(),
        })?;
    equipment.set(target, item_id);
    Ok(target)
}

/// Move an equipped item back into the inventory.
pub fn unequip(
    equipment: &mut Equipment,
    inventory: &mut Inventory,
    slot: EquipmentSlot,
) -> Result<String, EquipError> {
    if equipment.slot(slot).is_empty() {
        return Err(EquipError::NotEquipped {
            slot: slot.as_str().to_string(),
        });
    }
    let item = equipment.take(slot);
    inventory.push(item.clone());
    Ok(item)
}

/// Parse a slot name for the console/script surface, returning the
/// user-facing error text on an unknown name.
pub fn slot_arg(name: &str) -> Result<EquipmentSlot, EquipError> {
    EquipmentSlot::from_str(name).ok_or_else(|| EquipError::UnknownSlot {
        slot: name.to_string(),
    })
}

/// The one-line equipment summary the inspector and console read
/// (`wield: Longsword  off: Round shield  load 33/28 (+10%)`).
pub fn load_summary(
    equipment: &Equipment,
    inventory: &Inventory,
    might: i64,
    threshold_per_might: i64,
    speed_per_weight: i64,
    world: &World,
) -> String {
    let name_of = |id: &str| {
        if id.is_empty() {
            "-".to_string()
        } else {
            blueprint_item(world, id)
                .map(|d| d.item.name)
                .unwrap_or_else(|| id.to_string())
        }
    };
    let worn = worn_weight(equipment, world);
    let threshold = threshold_per_might * might;
    let over = (worn - threshold).max(0);
    let penalty = speed_per_weight * over;
    format!(
        "wield: {}  off: {}  head: {}  body: {}  arms: {}  legs: {}  load {worn}/{threshold} (+{penalty}%)  pack {}/{}",
        name_of(equipment.slot(EquipmentSlot::MainHand)),
        name_of(equipment.slot(EquipmentSlot::OffHand)),
        name_of(equipment.slot(EquipmentSlot::Head)),
        name_of(equipment.slot(EquipmentSlot::Body)),
        name_of(equipment.slot(EquipmentSlot::Arms)),
        name_of(equipment.slot(EquipmentSlot::Legs)),
        inventory_weight(inventory, world),
        pack_capacity(might),
    )
}

/// Parse a `parts.ItemWeapon`/`parts.ItemArmor`/`parts.Equipment` payload from a TOML
/// table, the same `toml::Value -> T` seam the loader uses.
pub fn parse<'de, T>(value: &'de toml::Value) -> Result<T, String>
where
    T: Deserialize<'de>,
{
    T::deserialize(value.clone()).map_err(|e: toml::de::Error| e.to_string())
}

/// Plain-toml snapshot of an [`Equipment`] part (used by the save hooks).
pub fn equipment_to_toml(e: &Equipment) -> toml::Value {
    let mut map = toml::map::Map::new();
    for slot in EquipmentSlot::ALL {
        map.insert(
            slot.as_str().to_string(),
            toml::Value::String(e.slot(slot).to_string()),
        );
    }
    toml::Value::Table(map)
}

/// Plain-toml snapshot of an [`Inventory`] part (used by the save hooks).
pub fn inventory_to_toml(i: &Inventory) -> toml::Value {
    let mut map = toml::map::Map::new();
    map.insert(
        "items".to_string(),
        toml::Value::Array(
            i.items
                .iter()
                .map(|id| toml::Value::String(id.clone()))
                .collect(),
        ),
    );
    toml::Value::Table(map)
}

/// Restore an [`Equipment`] from its save payload, failing closed on unknown
/// slot keys or non-string values.
pub fn equipment_from_toml(value: &toml::Value) -> Result<Equipment, String> {
    let equipment: Equipment = parse(value)?;
    for slot in EquipmentSlot::ALL {
        let id = equipment.slot(slot);
        if !id.is_empty() && id.trim() != id {
            return Err(format!(
                "equipment slot '{}' holds a malformed item id '{id}'",
                slot.as_str()
            ));
        }
    }
    Ok(equipment)
}

/// Restore an [`Inventory`] from its save payload.
pub fn inventory_from_toml(value: &toml::Value) -> Result<Inventory, String> {
    parse(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_toml<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, toml::de::Error> {
        let anchored = format!("payload = {text}");
        let value: toml::Value = toml::from_str(&anchored)?;
        value
            .get("payload")
            .expect("anchored payload")
            .clone()
            .try_into()
    }

    #[test]
    fn equipment_defaults_empty_and_round_trips_toml() {
        let e = Equipment::default();
        assert!(e.is_empty());
        let value = equipment_to_toml(&e);
        let back = equipment_from_toml(&value).unwrap();
        assert_eq!(back, e);

        let e: Equipment =
            parse_toml(r#"{ main_hand = "Item_Longsword", off_hand = "", head = "", body = "Item_MailHauberk", arms = "", legs = "" }"#)
                .unwrap();
        assert_eq!(e.main_hand, "Item_Longsword");
        assert_eq!(e.body, "Item_MailHauberk");
        assert!(!e.is_empty());
        assert_eq!(equipment_from_toml(&equipment_to_toml(&e)).unwrap(), e);

        // A partial table loads (absent slots are empty), an unknown slot key
        // fails closed.
        let e: Equipment = parse_toml(r#"{ main_hand = "Item_Longsword" }"#).unwrap();
        assert_eq!(e.off_hand, "");
        assert!(parse_toml::<Equipment>(r#"{ tail = "Item_Longsword" }"#).is_err());
    }

    #[test]
    fn slot_vocabulary_round_trips_and_rejects_unknowns() {
        for slot in EquipmentSlot::ALL {
            assert_eq!(EquipmentSlot::from_str(slot.as_str()), Some(slot));
        }
        assert_eq!(EquipmentSlot::from_str("ring"), None);
        assert_eq!(EquipmentSlot::from_str("MainHand"), None);
        assert_eq!(EquipmentSlot::ARMOR.len(), 4);
    }

    #[test]
    fn equipment_slot_accessors_are_whole_value() {
        let mut e = Equipment::default();
        e.set(EquipmentSlot::MainHand, "Item_Longsword");
        assert_eq!(e.slot(EquipmentSlot::MainHand), "Item_Longsword");
        assert_eq!(e.take(EquipmentSlot::MainHand), "Item_Longsword");
        assert_eq!(e.take(EquipmentSlot::MainHand), "");
        assert!(e.is_empty());
    }

    #[test]
    fn inventory_is_ordered_and_removes_first_occurrence() {
        let mut inv = Inventory::default();
        inv.push("Item_Dagger");
        inv.push("Item_Longsword");
        inv.push("Item_Dagger");
        assert_eq!(inv.len(), 3);
        assert_eq!(inv.take("Item_Dagger").as_deref(), Some("Item_Dagger"));
        assert_eq!(inv.items, vec!["Item_Longsword", "Item_Dagger"]);
        assert_eq!(inv.take("Item_Axe"), None);
    }

    #[test]
    fn weapon_and_armor_validate_their_payloads() {
        let w: Weapon =
            parse_toml(r#"{ lo = 4, hi = 8, stat = "might", hands = 1, reach = 1, cost = 100 }"#)
                .unwrap();
        assert_eq!((w.lo, w.hi, w.hands, w.cost), (4, 8, 1, 100));
        assert_eq!(w.accuracy_bonus, 0);

        for bad in [
            r#"{ lo = 8, hi = 4, stat = "might", hands = 1, reach = 1, cost = 100 }"#,
            r#"{ lo = 4, hi = 8, stat = "strength", hands = 1, reach = 1, cost = 100 }"#,
            r#"{ lo = 4, hi = 8, stat = "might", hands = 3, reach = 1, cost = 100 }"#,
            r#"{ lo = 4, hi = 8, stat = "might", hands = 1, reach = 0, cost = 100 }"#,
            r#"{ lo = 4, hi = 8, stat = "might", hands = 1, reach = 1, cost = -1 }"#,
        ] {
            assert!(parse_toml::<Weapon>(bad).is_err(), "{bad}");
        }

        let a: Armor = parse_toml(
            r#"{ ac = 4, block = 0, resist = { slash = 30, pierce = 20 }, threshold = {} }"#,
        )
        .unwrap();
        assert_eq!(a.ac, 4);
        assert_eq!(a.resist["slash"], 30);
        assert!(parse_toml::<Armor>(r#"{ ac = -1 }"#).is_err());
        assert!(parse_toml::<Armor>(r#"{ ac = 1, block = 101 }"#).is_err());
        assert!(parse_toml::<Armor>(r#"{ ac = 1, resist = { Slash = 10 } }"#).is_err());
    }

    #[test]
    fn gear_parts_register_as_dynamic() {
        let mut registry = PartRegistry::new();
        register_all(&mut registry).unwrap();
        let names: Vec<&str> = registry.iter_storage().map(|(id, _)| id.as_str()).collect();
        // The item payload parts are deliberately `Item*`-named: `Weapon` is
        // the combat pipeline's resolved wielded profile, not a blueprint
        // payload, so the two never collide in the shared registry.
        assert_eq!(
            names,
            vec!["Equipment", "Inventory", "ItemArmor", "ItemWeapon"]
        );
        for name in names {
            assert_eq!(
                registry.storage(&crate::ecs::PartTypeId::new(name)),
                Some(crate::ecs::StorageClass::Dynamic),
                "{name}"
            );
        }
        assert!(matches!(
            register_all(&mut registry),
            Err(PartError::DuplicatePartType(id)) if id.as_str() == "Equipment"
        ));
    }

    #[test]
    fn pack_capacity_scales_with_might() {
        assert_eq!(pack_capacity(14), 140);
        assert_eq!(pack_capacity(0), 0);
        assert_eq!(pack_capacity(-5), 0);
    }
}
