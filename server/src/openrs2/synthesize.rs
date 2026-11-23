use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use sha2::{Digest, Sha256};
use tracing::info;

use crate::engine::normalize::normalize_display_name;
use crate::engine::{
    CatalogItem, CatalogSourceInfo, CategoryMeta, FieldMeta, FieldValue,
};
use crate::openrs2::RawRs3CacheData;
use crate::openrs2::item_def::ItemDef;

/// Param IDs dynamically resolved by name from Archive 67 Group 37.
struct ParamIds {
    nonbankable: Option<u32>,
    quickchat_dont_list: Option<u32>,
    ge_category: Option<u32>,
    destroy_on_drop: Option<u32>,
    food_heal: Option<u32>,
    food_heal_percent: Option<u32>,
    no_alchemy: Option<u32>,
    potion_type: Option<u32>,
    potion_struct: Option<u32>,
    melee_weapon: Option<u32>,
    ranged_weapon: Option<u32>,
    magic_weapon: Option<u32>,
    necromancy_weapon: Option<u32>,
    player_armour: Option<u32>,
    invent_augmented_version: Option<u32>,
    invent_nonstandard_aug: Option<u32>,
    invent_base_object: Option<u32>,
    invent_contraption_type: Option<u32>,
    invent_item_tier_override: Option<u32>,
    invent_charge_drain: Option<u32>,
    degradeable_item: Option<u32>,
    degrade_max: Option<u32>,
    degrade_recharge: Option<u32>,
    barrows_degraded: Option<u32>,
    diango_return_fee: Option<u32>,
    diango_filters: Vec<u32>,
    lore_storage_space: Option<u32>,
    wear_stat: [Option<u32>; 6],
    wear_level: [Option<u32>; 6],
}

impl ParamIds {
    fn from_table(param_names: &BTreeMap<u32, String>) -> Self {
        let mut by_name: HashMap<String, u32> = HashMap::with_capacity(param_names.len());
        for (&id, name) in param_names {
            by_name.insert(name.to_ascii_uppercase(), id);
        }
        let get = |k: &str| by_name.get(k).copied();
        let diango_filters = [
            "DIANGO_FILTER_HOLIDAY",
            "DIANGO_FILTER_PROMO",
            "DIANGO_FILTER_SKILL",
            "DIANGO_FILTER_COSMETIC",
            "DIANGO_FILTER_WEAPON",
            "DIANGO_FILTER_ARMOUR",
        ]
        .iter()
        .filter_map(|k| get(k))
        .collect();

        Self {
            nonbankable: get("NONBANKABLE"),
            quickchat_dont_list: get("QUICKCHAT_DONT_LIST"),
            ge_category: get("GE_CATEGORY"),
            destroy_on_drop: get("DESTROYONDROP"),
            food_heal: get("FOOD_HEAL"),
            food_heal_percent: get("FOODHEALPERCENT"),
            no_alchemy: get("NO_ALCHEMY"),
            potion_type: get("POTION_TYPE"),
            potion_struct: get("POTION_STRUCT"),
            melee_weapon: get("COMBATV2_MELEE_WEAPON"),
            ranged_weapon: get("COMBATV2_RANGED_WEAPON"),
            magic_weapon: get("COMBATV2_MAGIC_WEAPON"),
            necromancy_weapon: get("COMBATV2_NECROMANCY_WEAPON"),
            player_armour: get("COMBATV2_PLAYER_ARMOUR"),
            invent_augmented_version: get("INVENT_AUGMENTED_VERSION"),
            invent_nonstandard_aug: get("INVENT_NONSTANDARD_AUGMENTED_VERSION"),
            invent_base_object: get("INVENT_BASE_OBJECT"),
            invent_contraption_type: get("INVENT_CONTRAPTION_TYPE"),
            invent_item_tier_override: get("INVENT_ITEM_TIER_OVERRIDE"),
            invent_charge_drain: get("INVENT_CHARGE_DRAIN_RATE"),
            degradeable_item: get("DEGRADEABLE_ITEM"),
            degrade_max: get("ITEM_DEGRADE_MAXDEGRADE"),
            degrade_recharge: get("ITEM_DEGRADE_RECHARGECOST"),
            barrows_degraded: get("BARROWS_DEGRADEDSTATE"),
            diango_return_fee: get("DIANGO_RETURN_FEE"),
            diango_filters,
            lore_storage_space: get("LORE_STORAGE_SPACE"),
            wear_stat: [
                get("WEAR_REQUIRES_STAT1"),
                get("WEAR_REQUIRES_STAT2"),
                get("WEAR_REQUIRES_STAT3"),
                get("WEAR_REQUIRES_STAT4"),
                get("WEAR_REQUIRES_STAT5"),
                get("WEAR_REQUIRES_STAT6"),
            ],
            wear_level: [
                get("WEAR_REQUIRES_STAT_LEVEL1"),
                get("WEAR_REQUIRES_STAT_LEVEL2"),
                get("WEAR_REQUIRES_STAT_LEVEL3"),
                get("WEAR_REQUIRES_STAT_LEVEL4"),
                get("WEAR_REQUIRES_STAT_LEVEL5"),
                get("WEAR_REQUIRES_STAT_LEVEL6"),
            ],
        }
    }
}

/// Converts an Archive 67 Obj name into the canonical Lua `config.Obj` key
/// (uppercase, with leading `_` when starting with an ASCII digit).
pub fn canonical_config_name(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    if upper.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
        format!("_{upper}")
    } else {
        upper
    }
}

/// Synthesizes full RuneQL catalog items directly from OpenRS2 RS3 cache data,
/// optionally merging curated RuneQL attributes from `overlay_dir`.
pub fn synthesize_catalog(
    raw: &RawRs3CacheData,
    overlay_dir: Option<&Path>,
) -> Result<(Vec<CatalogItem>, String, CatalogSourceInfo)> {
    let pids = ParamIds::from_table(&raw.param_names);

    let overlay: HashMap<String, BTreeMap<String, FieldValue>> = match overlay_dir {
        Some(dir) if dir.exists() => {
            let loaded = load_packed_attributes_overlay(dir)?;
            info!(
                overlay_records = loaded.len(),
                dir = %dir.display(),
                "Loaded curated RuneQL attribute overlay"
            );
            loaded
        }
        _ => HashMap::new(),
    };

    let mut items = Vec::with_capacity(raw.items.len());
    let mut overlay_matched = 0usize;
    let mut hasher = Sha256::new();
    hasher.update(format!("openrs2:{}:{}:", raw.meta.id, raw.archive19_revision).as_bytes());

    for (&id, def) in &raw.items {
        let Some(raw_config) = raw.obj_config_names.get(&id) else {
            continue;
        };
        let config_name = canonical_config_name(raw_config);
        if config_name.is_empty() {
            continue;
        }

        // Resolve display name (skip items without any usable display name).
        let display_name = match def.name.as_deref() {
            Some(n) if !n.trim().is_empty() && !n.eq_ignore_ascii_case("null") => n.to_string(),
            _ => continue,
        };
        let normalized_name = normalize_display_name(&display_name);
        if normalized_name.is_empty() {
            continue;
        }

        // Native client eligibility matching `client_index.lua`:
        // - deleted: category == 15 OR QUICKCHAT_DONT_LIST != 0
        // - bankable: NONBANKABLE == 0/absent AND not noted/loan/dummy template
        let deleted_by_cat = def.category == Some(15);
        let deleted_by_qc = pids
            .quickchat_dont_list
            .and_then(|k| def.params.get(&k))
            .and_then(|v| v.as_i32())
            .is_some_and(|v| v != 0);
        let deleted = deleted_by_cat || deleted_by_qc;

        let nonbankable_param = pids
            .nonbankable
            .and_then(|k| def.params.get(&k))
            .and_then(|v| v.as_i32())
            .is_some_and(|v| v != 0);
        let is_noted = def.note_template.is_some();
        let is_lent = def.loan_template.is_some();
        let is_dummy = def.dummy_flag || def.dummy_item.is_some_and(|d| d != 0);
        let bankable = !nonbankable_param && !is_noted && !is_lent && !is_dummy;

        let mut fields = derive_fields_from_cache(def, &display_name, &config_name, &pids);

        // Merge curated RuneQL overlay (if supplied), while keeping live OpenRS2 scalar facts fresh.
        if let Some(curated) = overlay.get(&config_name) {
            overlay_matched += 1;
            for (k, v) in curated {
                fields.insert(k.clone(), v.clone());
            }
            // Refresh live cache values that may have changed in newer RS3 builds.
            if let Some(val) = def.value.filter(|&v| v > 0) {
                fields.insert("baseValue".to_string(), FieldValue::Number(val as f64));
            }
            fields.insert("members".to_string(), FieldValue::Bool(def.members));
            fields.insert("geTradeable".to_string(), FieldValue::Bool(def.ge_tradeable));
        }

        if !fields.contains_key("category") {
            fields.insert(
                "category".to_string(),
                FieldValue::Text("needs-review".to_string()),
            );
        }

        hasher.update(config_name.as_bytes());
        hasher.update(b"\t");
        hasher.update(display_name.as_bytes());
        hasher.update(b"\n");

        items.push(CatalogItem {
            id,
            config_name,
            display_name,
            normalized_name,
            bankable,
            deleted,
            fields,
        });
    }

    let revision = hex::encode(hasher.finalize());
    let source = CatalogSourceInfo {
        openrs2_cache_id: raw.meta.id,
        build: raw.meta.build_label(),
        timestamp: raw.meta.timestamp.clone(),
        archive19_revision: raw.archive19_revision,
        overlay_applied: !overlay.is_empty(),
        overlay_matched,
        total_items: items.len(),
        eligible_items: 0, // filled in by CatalogSnapshot::build
    };

    Ok((items, revision, source))
}

fn param_i32(def: &ItemDef, key: Option<u32>) -> Option<i32> {
    key.and_then(|k| def.params.get(&k)).and_then(|v| v.as_i32())
}

fn param_present(def: &ItemDef, key: Option<u32>) -> bool {
    key.is_some_and(|k| def.params.contains_key(&k))
}

/// Derives RuneQL structured attributes directly from RS3 binary opcodes & opcode 249 params.
fn derive_fields_from_cache(
    def: &ItemDef,
    display_name: &str,
    config_name: &str,
    pids: &ParamIds,
) -> BTreeMap<String, FieldValue> {
    let mut fields = BTreeMap::new();
    let lower_name = display_name.to_ascii_lowercase();
    let ops: Vec<&str> = def
        .widget_actions
        .iter()
        .filter_map(|o| o.as_deref())
        .collect();

    if let Some(val) = def.value.filter(|&v| v > 0) {
        fields.insert("baseValue".to_string(), FieldValue::Number(val as f64));
        if param_i32(def, pids.no_alchemy).unwrap_or(0) == 0 {
            let high_alch = ((val as f64) * 0.6).floor();
            if high_alch > 0.0 {
                fields.insert("highAlch".to_string(), FieldValue::Number(high_alch));
            }
        }
    }
    if let Some(limit) = def.buy_limit.filter(|&l| l > 0) {
        fields.insert("buyLimit".to_string(), FieldValue::Number(limit as f64));
    }
    if let Some(heal) = param_i32(def, pids.food_heal).filter(|&h| h > 0) {
        fields.insert("healAmount".to_string(), FieldValue::Number(heal as f64));
    }
    fields.insert("members".to_string(), FieldValue::Bool(def.members));
    fields.insert("geTradeable".to_string(), FieldValue::Bool(def.ge_tradeable));
    if def.ge_tradeable {
        fields.insert("tradeable".to_string(), FieldValue::Bool(true));
    } else if param_present(def, pids.destroy_on_drop) || def.bind_template.is_some() {
        fields.insert("tradeable".to_string(), FieldValue::Bool(false));
    }

    // Equipment slot mapping
    let mut types: BTreeSet<String> = BTreeSet::new();
    let mut slots: BTreeSet<String> = BTreeSet::new();
    let mut combat_styles: BTreeSet<String> = BTreeSet::new();
    let mut skills: BTreeSet<String> = BTreeSet::new();
    let mut roles: BTreeSet<String> = BTreeSet::new();

    if let Some(slot_id) = def.equip_slot_id {
        types.insert("equipment".to_string());
        match slot_id {
            0 => {
                slots.insert("head".to_string());
            }
            1 => {
                slots.insert("cape".to_string());
            }
            2 => {
                slots.insert("neck".to_string());
                types.insert("jewellery".to_string());
            }
            3 => {
                if def.equip_id == Some(5) {
                    slots.insert("two-hand".to_string());
                } else {
                    slots.insert("main-hand".to_string());
                }
                types.insert("weapon".to_string());
            }
            4 => {
                slots.insert("body".to_string());
            }
            5 => {
                slots.insert("off-hand".to_string());
            }
            7 => {
                slots.insert("legs".to_string());
            }
            9 => {
                slots.insert("hands".to_string());
            }
            10 => {
                slots.insert("feet".to_string());
            }
            12 => {
                slots.insert("ring".to_string());
                types.insert("jewellery".to_string());
            }
            13 => {
                slots.insert("ammo".to_string());
                types.insert("ammunition".to_string());
            }
            14 => {
                slots.insert("aura".to_string());
            }
            17 => {
                slots.insert("pocket".to_string());
            }
            _ => {}
        }
    }

    // Combat styles from explicit RS3 params
    if param_i32(def, pids.melee_weapon).unwrap_or(0) != 0 {
        combat_styles.insert("melee".to_string());
        types.insert("weapon".to_string());
        roles.insert("combat".to_string());
    }
    if param_i32(def, pids.ranged_weapon).unwrap_or(0) != 0 {
        combat_styles.insert("ranged".to_string());
        types.insert("weapon".to_string());
        roles.insert("combat".to_string());
    }
    if param_i32(def, pids.magic_weapon).unwrap_or(0) != 0 {
        combat_styles.insert("magic".to_string());
        types.insert("weapon".to_string());
        roles.insert("combat".to_string());
    }
    if param_i32(def, pids.necromancy_weapon).unwrap_or(0) != 0 {
        combat_styles.insert("necromancy".to_string());
        types.insert("weapon".to_string());
        roles.insert("combat".to_string());
    }
    if param_i32(def, pids.player_armour).unwrap_or(0) > 0 {
        types.insert("armour".to_string());
        types.insert("equipment".to_string());
        roles.insert("combat".to_string());
    }

    // Equip skill requirements (WEAR_REQUIRES_STAT1..6)
    let mut max_combat_req: Option<i32> = None;
    let mut req_parts: Vec<String> = Vec::new();
    for i in 0..6 {
        if let (Some(stat), Some(lvl)) = (
            param_i32(def, pids.wear_stat[i]),
            param_i32(def, pids.wear_level[i]),
        ) {
            if lvl <= 0 {
                continue;
            }
            if let Some((skill_slug, skill_label, equip_field, is_combat)) = rs3_skill_info(stat) {
                skills.insert(skill_slug.to_string());
                req_parts.push(format!("{lvl} {skill_label}"));
                if let Some(ef) = equip_field {
                    fields.insert(ef.to_string(), FieldValue::Number(lvl as f64));
                }
                if is_combat {
                    max_combat_req = Some(max_combat_req.map_or(lvl, |m| m.max(lvl)));
                    match stat {
                        0 | 2 => {
                            combat_styles.insert("melee".to_string());
                        }
                        4 => {
                            combat_styles.insert("ranged".to_string());
                        }
                        6 => {
                            combat_styles.insert("magic".to_string());
                        }
                        28 => {
                            combat_styles.insert("necromancy".to_string());
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    if !req_parts.is_empty() {
        fields.insert(
            "equipRequirements".to_string(),
            FieldValue::Text(req_parts.join(", ")),
        );
    }

    // Tier from Invention tier override or max combat requirement
    let tier = param_i32(def, pids.invent_item_tier_override)
        .filter(|&t| t > 0)
        .or(max_combat_req);
    if let Some(t) = tier {
        fields.insert("tier".to_string(), FieldValue::Number(t as f64));
    }

    // Invention augmentable / augmented
    let augmented = param_present(def, pids.invent_base_object)
        || param_present(def, pids.invent_contraption_type)
        || lower_name.starts_with("augmented ");
    let augmentable = augmented
        || param_present(def, pids.invent_augmented_version)
        || param_present(def, pids.invent_nonstandard_aug);
    if augmentable {
        fields.insert("augmentable".to_string(), FieldValue::Bool(true));
    }
    if augmented {
        fields.insert("augmented".to_string(), FieldValue::Bool(true));
    }

    // Degradable
    let degradable = augmented
        || param_i32(def, pids.degradeable_item).unwrap_or(0) != 0
        || param_present(def, pids.degrade_max)
        || param_present(def, pids.degrade_recharge)
        || param_present(def, pids.barrows_degraded)
        || param_present(def, pids.invent_charge_drain);
    if degradable {
        fields.insert("degradable".to_string(), FieldValue::Bool(true));
    }

    // Diango / reclaimable
    let diango = param_present(def, pids.diango_return_fee)
        || pids.diango_filters.iter().any(|k| def.params.contains_key(k));
    if diango {
        fields.insert("reclaimable".to_string(), FieldValue::Bool(true));
        fields.insert(
            "reclaimSources".to_string(),
            FieldValue::List(vec!["diango".to_string()]),
        );
    }
    if param_present(def, pids.lore_storage_space) {
        fields.insert(
            "storageLocations".to_string(),
            FieldValue::List(vec!["poh-bookcase".to_string()]),
        );
    }

    // Food & Potion detection
    let is_food = param_present(def, pids.food_heal)
        || param_present(def, pids.food_heal_percent)
        || ops.iter().any(|o| o.eq_ignore_ascii_case("Eat"));
    let is_potion = param_present(def, pids.potion_type)
        || param_present(def, pids.potion_struct)
        || ops.iter().any(|o| o.eq_ignore_ascii_case("Drink"));

    if is_food {
        types.insert("food".to_string());
        roles.insert("healing".to_string());
    }
    if is_potion {
        types.insert("potion".to_string());
        skills.insert("herblore".to_string());
        if let Some(d) = extract_dose(&lower_name) {
            fields.insert("dose".to_string(), FieldValue::Number(d as f64));
        }
    }
    if ops.iter().any(|o| o.eq_ignore_ascii_case("Read")) {
        types.insert("book".to_string());
    }
    if ops
        .iter()
        .any(|o| o.eq_ignore_ascii_case("Teleport") || o.eq_ignore_ascii_case("Rub"))
    {
        roles.insert("teleport".to_string());
    }

    // Category synthesis from GE_CATEGORY + cache signals
    let ge_cat = param_i32(def, pids.ge_category);
    let category = infer_category(
        ge_cat,
        &lower_name,
        config_name,
        is_food,
        is_potion,
        &combat_styles,
        &slots,
        &types,
    );
    fields.insert("category".to_string(), FieldValue::Text(category.to_string()));

    if !slots.is_empty() {
        fields.insert("slot".to_string(), FieldValue::List(slots.into_iter().collect()));
    }
    if !combat_styles.is_empty() {
        fields.insert(
            "combatStyle".to_string(),
            FieldValue::List(combat_styles.into_iter().collect()),
        );
    }
    if !types.is_empty() {
        fields.insert("types".to_string(), FieldValue::List(types.into_iter().collect()));
    }
    if !skills.is_empty() {
        fields.insert(
            "skills".to_string(),
            FieldValue::List(skills.into_iter().collect()),
        );
    }
    if !roles.is_empty() {
        fields.insert("roles".to_string(), FieldValue::List(roles.into_iter().collect()));
    }

    fields
}

fn extract_dose(lower_name: &str) -> Option<u8> {
    let open = lower_name.rfind('(')?;
    let close = lower_name.rfind(')')?;
    if close <= open + 1 {
        return None;
    }
    let inner = &lower_name[open + 1..close];
    let n: u8 = inner.parse().ok()?;
    (1..=12).contains(&n).then_some(n)
}

#[allow(clippy::too_many_arguments)]
fn infer_category(
    ge_cat: Option<i32>,
    lower_name: &str,
    config_name: &str,
    is_food: bool,
    is_potion: bool,
    combat_styles: &BTreeSet<String>,
    slots: &BTreeSet<String>,
    types: &BTreeSet<String>,
) -> &'static str {
    // High-confidence GE_CATEGORY mappings (RS3 Grand Exchange categories)
    if let Some(gc) = ge_cat {
        match gc {
            1 => return "ammunition",
            2 | 3 => return "ammunition",
            4 => return "construction",
            5 => return "construction",
            6 => return "cooking",
            7 => return "cosmetics",
            8 => return "crafting",
            9 => return "familiars",
            10 => return "farming",
            11 => return "fletching-materials",
            12 => return "food",
            13 => return "herblore-secondaries",
            14 => return "hunter",
            15 => return "hunter",
            16 => return "shared-equipment",
            17 => return "magic-equipment",
            18 => return "magic-equipment",
            19 | 20 | 21 => return "melee-equipment",
            22 | 23 | 24 => return "melee-equipment",
            25 => return "mining-resources",
            26 => return "potions",
            27 => return "prayer",
            28 | 29 | 30 | 31 => return "ranged-equipment",
            32 => return "runes",
            33 => return "seeds",
            34 => return "tools",
            35 => return "invention",
            36 => return "woodcutting-resources",
            37 => return "combat-utilities",
            38 => return "divination",
            39 => return "salvage",
            40 => return "archaeology-materials",
            41 => return "archaeology-supplies",
            42 => return "necromancy-equipment",
            43 => return "necromancy-ritual-supplies",
            _ => {}
        }
    }

    // Fallback cache-driven rules for untradeable / newer items without GE_CATEGORY
    if config_name.starts_with("ARCH_") {
        if config_name.contains("MATERIAL") {
            return "archaeology-materials";
        }
        if config_name.contains("RESTORATION") || config_name.contains("ARTEFACT") {
            return "archaeology-artefacts";
        }
        return "archaeology-supplies";
    }
    if config_name.starts_with("POF_") {
        return "pof-animals";
    }
    if lower_name.contains("incense sticks") {
        return "incense-sticks";
    }
    if lower_name.ends_with(" bomb") {
        return "bombs";
    }
    if is_potion {
        if lower_name.contains("(unf)") {
            return "unfinished-potions";
        }
        return "potions";
    }
    if is_food {
        return "food";
    }
    if lower_name.ends_with(" rune") && !lower_name.contains("armour") {
        return "runes";
    }
    if lower_name.ends_with(" seed") || lower_name.ends_with(" sapling") {
        return "seeds";
    }
    if lower_name.starts_with("grimy ") || lower_name.starts_with("clean ") {
        return "herbs";
    }
    if lower_name.contains("clue scroll") || lower_name.contains("reward casket") {
        return "clues";
    }
    if lower_name.contains("salvage") {
        return "salvage";
    }
    if combat_styles.len() == 1 {
        match combat_styles.iter().next().map(String::as_str) {
            Some("melee") => return "melee-equipment",
            Some("ranged") => return "ranged-equipment",
            Some("magic") => return "magic-equipment",
            Some("necromancy") => return "necromancy-equipment",
            _ => {}
        }
    }
    if combat_styles.len() > 1 || (!slots.is_empty() && types.contains("equipment")) {
        return "shared-equipment";
    }
    if config_name.starts_with("MTX") {
        return "cosmetics";
    }
    if config_name.starts_with("QUEST_") {
        return "quests-lore";
    }

    "needs-review"
}

fn rs3_skill_info(stat: i32) -> Option<(&'static str, &'static str, Option<&'static str>, bool)> {
    Some(match stat {
        0 => ("attack", "Attack", Some("equipAttack"), true),
        1 => ("defence", "Defence", Some("equipDefence"), true),
        2 => ("strength", "Strength", Some("equipStrength"), true),
        3 => ("constitution", "Constitution", Some("equipConstitution"), true),
        4 => ("ranged", "Ranged", Some("equipRanged"), true),
        5 => ("prayer", "Prayer", Some("equipPrayer"), true),
        6 => ("magic", "Magic", Some("equipMagic"), true),
        7 => ("cooking", "Cooking", None, false),
        8 => ("woodcutting", "Woodcutting", None, false),
        9 => ("fletching", "Fletching", None, false),
        10 => ("fishing", "Fishing", None, false),
        11 => ("firemaking", "Firemaking", None, false),
        12 => ("crafting", "Crafting", None, false),
        13 => ("smithing", "Smithing", Some("equipSmithing"), false),
        14 => ("mining", "Mining", None, false),
        15 => ("herblore", "Herblore", None, false),
        16 => ("agility", "Agility", Some("equipAgility"), false),
        17 => ("thieving", "Thieving", None, false),
        18 => ("slayer", "Slayer", None, false),
        19 => ("farming", "Farming", None, false),
        20 => ("runecrafting", "Runecrafting", None, false),
        21 => ("hunter", "Hunter", None, false),
        22 => ("construction", "Construction", None, false),
        23 => ("summoning", "Summoning", None, false),
        24 => ("dungeoneering", "Dungeoneering", Some("equipDungeoneering"), false),
        25 => ("divination", "Divination", None, false),
        26 => ("invention", "Invention", None, false),
        27 => ("archaeology", "Archaeology", None, false),
        28 => ("necromancy", "Necromancy", Some("equipNecromancy"), true),
        _ => return None,
    })
}

/// Reads RuneQL's existing packed `attributes_00..31.lua` overlay directly without Lua.
fn load_packed_attributes_overlay(
    catalog_dir: &Path,
) -> Result<HashMap<String, BTreeMap<String, FieldValue>>> {
    let mut map = HashMap::new();
    for shard in 0..32 {
        let path = catalog_dir.join(format!("attributes_{shard:02}.lua"));
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(start) = contents.find("[===[") else {
            continue;
        };
        let rest = &contents[start + 5..];
        let Some(end) = rest.rfind("]===]") else {
            continue;
        };
        for line in rest[..end].lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((config_name, attrs_str)) = line.split_once('\t') else {
                continue;
            };
            let mut record = BTreeMap::new();
            for pair in attrs_str.split('\t') {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                if let Some(fv) = decode_packed_value(v) {
                    record.insert(k.to_string(), fv);
                }
            }
            map.insert(config_name.to_ascii_uppercase(), record);
        }
    }
    Ok(map)
}

fn decode_packed_value(raw: &str) -> Option<FieldValue> {
    let mut chars = raw.chars();
    let kind = chars.next()?;
    let rest = chars.as_str();
    match kind {
        's' => Some(FieldValue::Text(unescape_packed(rest))),
        'n' => rest.parse::<f64>().ok().map(FieldValue::Number),
        'b' => Some(FieldValue::Bool(rest == "1")),
        'a' => {
            if rest.is_empty() {
                return Some(FieldValue::List(Vec::new()));
            }
            let list = rest
                .split('|')
                .filter_map(|item| {
                    let mut c = item.chars();
                    let k = c.next()?;
                    let r = c.as_str();
                    match k {
                        's' => Some(unescape_packed(r)),
                        'n' => Some(r.to_string()),
                        'b' => Some(if r == "1" { "true" } else { "false" }.to_string()),
                        _ => None,
                    }
                })
                .collect();
            Some(FieldValue::List(list))
        }
        _ => None,
    }
}

fn unescape_packed(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex_str, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn default_categories() -> Vec<CategoryMeta> {
    let pairs = [
        ("currency", "Currency & tokens"),
        ("runes", "Runes"),
        ("travel", "Teleports & travel"),
        ("containers", "Bags, boxes & containers"),
        ("utilities", "Keys & general utilities"),
        ("melee-equipment", "Melee equipment"),
        ("ranged-equipment", "Ranged equipment"),
        ("magic-equipment", "Magic equipment"),
        ("necromancy-equipment", "Necromancy equipment"),
        ("shared-equipment", "Shared & hybrid equipment"),
        ("ammunition", "Ammunition"),
        ("combat-utilities", "Combat utilities"),
        ("food", "Food & drink"),
        ("potions", "Finished potions"),
        ("incense-sticks", "Incense sticks"),
        ("bombs", "Bombs"),
        ("consumable-buffs", "Consumable buffs"),
        ("familiars", "Summoning charms, pouches & scrolls"),
        ("summoning-ingredients", "Summoning ingredients"),
        ("herbs", "Herbs"),
        ("herblore-secondaries", "Herblore secondaries"),
        ("unfinished-potions", "Unfinished potions"),
        ("potion-making-supplies", "Potion-making supplies"),
        ("seeds", "Seeds & saplings"),
        ("farming", "Farming produce & supplies"),
        ("tools", "Skilling tools"),
        ("skilling-equipment", "Skilling outfits & accessories"),
        ("mining-resources", "Mining resources"),
        ("smithing-materials", "Smithing materials"),
        ("woodcutting-resources", "Woodcutting resources"),
        ("firemaking-supplies", "Firemaking supplies"),
        ("fletching-materials", "Fletching materials"),
        ("crafting", "Crafting"),
        ("construction", "Construction"),
        ("fishing", "Fishing"),
        ("cooking", "Cooking"),
        ("hunter", "Hunter"),
        ("divination", "Divination supplies"),
        ("prayer", "Prayer supplies"),
        ("runecrafting", "Runecrafting supplies"),
        ("archaeology-materials", "Archaeology materials"),
        ("archaeology-artefacts", "Archaeology artefacts"),
        ("archaeology-supplies", "Archaeology supplies & relics"),
        ("invention", "Invention devices, materials & supplies"),
        ("clues", "Clue scrolls & clue supplies"),
        ("activities", "Minigame & activity-specific items"),
        ("cosmetics", "Cosmetics & collectibles"),
        ("quests-lore", "Quest items & lore"),
        ("xp-unlocks", "XP items & unlocks"),
        ("salvage", "Salvage"),
        ("upgrades", "Upgrade components & unique materials"),
        ("necromancy-ritual-supplies", "Necromancy Ritual Supplies"),
        ("pof-animals", "POF animals"),
        ("fortunate-components", "Fortunate Components"),
        ("needs-review", "Needs review"),
    ];
    pairs
        .into_iter()
        .map(|(k, n)| CategoryMeta {
            key: k.to_string(),
            name: n.to_string(),
        })
        .collect()
}

pub fn default_field_defs() -> Vec<FieldMeta> {
    let defs = [
        ("armourRole", "Armour role", "text", true),
        ("augmentable", "Augmentable", "boolean", false),
        ("augmented", "Augmented", "boolean", false),
        ("baseValue", "Base value", "number", false),
        ("buyLimit", "GE buy limit", "number", false),
        ("category", "Category", "text", false),
        ("combatStyle", "Combat style", "text", true),
        ("containerType", "Container type", "text", false),
        ("degradable", "Degradable", "boolean", false),
        ("dose", "Dose", "number", false),
        ("healAmount", "Heal amount", "number", false),
        ("highAlch", "High alch value", "number", false),
        ("equipAgility", "Equip Agility level", "number", false),
        ("equipAttack", "Equip Attack level", "number", false),
        ("equipConstitution", "Equip Constitution level", "number", false),
        ("equipDefence", "Equip Defence level", "number", false),
        ("equipDungeoneering", "Equip Dungeoneering level", "number", false),
        ("equipMagic", "Equip Magic level", "number", false),
        ("equipNecromancy", "Equip Necromancy level", "number", false),
        ("equipPrayer", "Equip Prayer level", "number", false),
        ("equipRanged", "Equip Ranged level", "number", false),
        ("equipSmithing", "Equip Smithing level", "number", false),
        ("equipStrength", "Equip Strength level", "number", false),
        ("equipRequirements", "Equip requirements", "text", false),
        ("family", "Family", "text", true),
        ("form", "Form", "text", true),
        ("geTradeable", "GE tradeable", "boolean", false),
        ("members", "Members", "boolean", false),
        ("reclaimSources", "Reclaim sources", "text", true),
        ("reclaimable", "Reclaimable", "boolean", false),
        ("roles", "Roles", "text", true),
        ("set", "Set", "text", true),
        ("skills", "Skills", "text", true),
        ("slot", "Slot", "text", true),
        ("storageLocations", "Storage locations", "text", true),
        ("subtype", "Subtype", "text", true),
        ("tier", "Tier", "number", false),
        ("tradeable", "Tradeable", "boolean", false),
        ("types", "Types", "text", true),
        ("weaponType", "Weapon type", "text", true),
    ];
    defs.into_iter()
        .map(|(key, label, field_type, multiple)| FieldMeta {
            key: key.to_string(),
            label: label.to_string(),
            field_type: field_type.to_string(),
            multiple,
        })
        .collect()
}
