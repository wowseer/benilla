//! SuperWoW 2.2's Lua surface, built into benilla so the addons written for it work here.
//!
//! SuperWoW (by Balake) is a DLL the 1.12.1 client loads; it adds Lua functions, events and CVars
//! and extends some stock functions. benilla has no executable to inject into, so this module
//! rebuilds that surface on benilla's own engine, from the DLL's exported strings, its code and
//! its wiki:
//!
//! - globals `SUPERWOW_VERSION` and `SUPERWOW_STRING`, which addons test to switch it on;
//! - the unit-token grammar's GUID strings, `markN` and `owner` ([`super::unit::Hops`]);
//! - `UnitExists` answering the GUID second, `UnitBuff`/`UnitDebuff` the aura's spell id last,
//!   `UnitMana`/`UnitManaMax("player")` the caster-form mana second, `CastSpellByName(name,
//!   unit)` and a nameplate's `GetName(1)`;
//! - the new functions ([`install`]) and the events [`EVENTS`], fired by the host.
//!
//! What the host answers it answers through the live seam ([`super::live`]): each question here is
//! a [`LiveQuery`] the app's SuperWoW family takes.

use std::collections::BTreeSet;

use mlua::{Function, Lua, MultiValue, Value};

use super::binding_abi::{self, optional_string};
use super::live::{ask, LiveQuery};
use super::unit::guid_string;
use super::Model;

/// `SUPERWOW_VERSION`, the version the DLL this follows reports.
pub const VERSION: &str = "2.2";

/// The globals SuperWoW adds to 1.12's, each registered by [`install`] or the families it calls;
/// the 1.12 surface gate takes them as its one listed exception. (`GetWorldLocMapPosition`, which
/// SuperWoW's wiki lists, is stock 1.12.)
pub const GLOBALS: &[&str] = &[
    "SUPERWOW_VERSION",
    "SUPERWOW_STRING",
    "SpellInfo",
    "UnitPosition",
    "UnitNameplate",
    "CanLootUnit",
    "SetMouseoverUnit",
    "Clickthrough",
    "SetAutoloot",
    "TrackUnit",
    "UntrackUnit",
    "CursorPosition",
    "IsSwimming",
    "IsMounted",
    "IsIndoors",
    "GetSpeed",
    "GetWeaponEnchantID",
    "GetMapBoundaries",
    "GetMapPositionWorldLoc",
    "ImportFile",
    "ExportFile",
    "CombatLogAdd",
    "GetNumSoundDrivers",
    "GetLoadedSoundDriver",
    "GetSoundDriverName",
    "GetSoundDevices",
    "GetPlayerBuffID",
    "GetInboxItemLink",
    "GetSendMailItemLink",
    "GetQuestIDForLogIndex",
    "GetQuestLinkForLogIndex",
    "GetQuestLinkForID",
];

/// The events SuperWoW appends to the in-game event table, in its order.
pub const EVENTS: &[&str] = &["UNIT_CASTEVENT", "RAW_COMBATLOG", "CREATE_CHATBUBBLE"];

/// SuperWoW's CVars and their defaults: registered so `GetCVar`/`SetCVar` take them, as an addon's
/// options read and write them; their effects on the renderer are not built.
const CVARS: &[(&str, &str)] = &[
    ("NameplateRange", "20"),
    ("NameplateMotion", "0"),
    ("ChatBubbleRange", "40"),
    ("ChatBubblesRaid", "0"),
    ("ChatBubblesBattleground", "0"),
    ("ChatBubblesWhisper", "0"),
    ("ChatBubblesCreatures", "1"),
    ("BackgroundSound", "0"),
    ("UncapSounds", "0"),
    ("InterfaceVolume", "1"),
    ("LootSparkle", "1"),
    ("HealingText", "1"),
    ("SelectionCircleStyle", "0"),
    ("FoV", "1.57079"),
];

/// The 8-slot raid mark board, the guid on each mark, 0 for none: what `markN` reads.
pub struct RaidMarksQuery;
impl LiveQuery for RaidMarksQuery {
    type Answer = [u64; 8];
}

/// `UnitPosition`'s answer for the unit held under the guid: its world position, WoW axes.
pub struct UnitPositionQuery(pub u64);
impl LiveQuery for UnitPositionQuery {
    type Answer = [f32; 3];
}

/// The active player's mana pool whatever its form: `(UNIT_FIELD_POWER1, UNIT_FIELD_MAXPOWER1)`,
/// the druid's caster-form mana `UnitMana("player")` answers second.
pub struct PlayerManaQuery;
impl LiveQuery for PlayerManaQuery {
    type Answer = (u32, u32);
}

/// `SpellInfo(id)`'s `Spell.dbc` row: name, rank, icon and the `SpellRange.dbc` pair.
pub struct SpellInfoQuery(pub u32);
impl LiveQuery for SpellInfoQuery {
    type Answer = SpellInfo;
}

/// One spell as `SpellInfo` returns it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpellInfo {
    pub name: String,
    /// The rank text (`"Rank 3"`), empty for none.
    pub rank: String,
    /// The icon's texture path, `None` for a row with no icon.
    pub icon: Option<String>,
    pub min_range: f32,
    pub max_range: f32,
}

/// The active player's motion state: `IsSwimming`, `IsMounted`, `IsIndoors` and `GetSpeed`.
pub struct PlayerMotionQuery;
impl LiveQuery for PlayerMotionQuery {
    type Answer = PlayerMotion;
}

/// The active player's motion state, as the movement code holds it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerMotion {
    pub swimming: bool,
    pub mounted: bool,
    pub indoors: bool,
    /// Yards per second; 7 is 100 %.
    pub run_speed: f32,
    pub swim_speed: f32,
}

/// `CursorPosition()`: the world point under the cursor, WoW axes.
pub struct CursorWorldQuery;
impl LiveQuery for CursorWorldQuery {
    type Answer = [f32; 3];
}

/// `GetWeaponEnchantID(unit)`: the temporary enchant ids on the player's main and off hand, off
/// its visible item fields.
pub struct WeaponEnchantIdQuery(pub u64);
impl LiveQuery for WeaponEnchantIdQuery {
    type Answer = (u32, u32);
}

/// `GetMapBoundaries(continent, zone)`: `WorldMapArea.dbc`'s `(left, right, top, bottom)` for the
/// map `GetMapContinents`/`GetMapZones` index, zone 0 the continent itself.
pub struct MapBoundsQuery {
    pub continent: u32,
    pub zone: u32,
}
impl LiveQuery for MapBoundsQuery {
    type Answer = [f32; 4];
}

/// `ImportFile(name)`: the text of `Imports/<name>.txt` in the state folder.
pub struct ImportFileQuery(pub String);
impl LiveQuery for ImportFileQuery {
    type Answer = String;
}

/// `ExportFile(name, text)`: write `Imports/<name>.txt` in the state folder; whether it wrote.
pub struct ExportFileQuery {
    pub name: String,
    pub text: String,
}
impl LiveQuery for ExportFileQuery {
    type Answer = bool;
}

/// `CombatLogAdd(text [, raw])`: a line for the combat log file.
pub struct CombatLogAddQuery {
    pub text: String,
    pub raw: bool,
}
impl LiveQuery for CombatLogAddQuery {
    type Answer = bool;
}

/// SuperWoW's switches the VM keeps for the host to read: click-through and the tracked units.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SuperWowState {
    /// `Clickthrough(1)`: the world pick passes through corpses with no loot.
    pub clickthrough: bool,
    /// `TrackUnit`'s guids.
    pub tracked: BTreeSet<u64>,
}

impl super::UiScript {
    /// SuperWoW's switches as the VM holds them.
    pub fn superwow_state(&self) -> SuperWowState {
        self.lua
            .app_data_ref::<SuperWowState>()
            .map(|s| s.clone())
            .unwrap_or_default()
    }
}

/// Whether `v` is Lua-true: anything but nil and false.
fn truthy(v: &Value) -> bool {
    !matches!(v, Value::Nil | Value::Boolean(false))
}

/// A 0/1-style switch argument: a number's nonzero, a string's `"0"` or not, else its truth.
fn switch_arg(v: &Value) -> bool {
    match v {
        Value::Integer(n) => *n != 0,
        Value::Number(n) => *n != 0.0,
        Value::String(s) => s.as_bytes().as_ref() != b"0",
        other => truthy(other),
    }
}

/// The guid `token` names at the call, raising `Unknown unit name` as the stock getters do.
fn unit_guid(lua: &Lua, token: &str) -> mlua::Result<Option<u64>> {
    let model = lua.app_data_ref::<Model>().expect("model app_data");
    super::unit::guid_of(lua, &model, token)
}

/// The required unit argument of a SuperWoW verb, its usage line raised for a missing one.
fn unit_arg(lua: &Lua, v: Value, usage: &'static str) -> mlua::Result<Option<u64>> {
    let token = binding_abi::string_arg(lua, v, usage)?;
    unit_guid(lua, &token)
}

/// `n` numbers as a multi-return.
fn numbers(values: &[f64]) -> MultiValue {
    MultiValue::from_vec(values.iter().map(|&v| Value::Number(v)).collect())
}

/// A name for a file under `Imports/`: letters, digits, spaces, `_`, `-` and `.`, no `..`; the
/// DLL formats `Imports\%s.txt` unchecked, and this keeps the write inside the folder.
fn import_name(name: &str) -> Option<String> {
    let ok = !name.is_empty()
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.'));
    ok.then(|| name.to_string())
}

/// Register the CVars in the VM's table, as the host's registry would, keeping any saved value.
fn register_cvars(lua: &Lua) {
    let mut model = lua.app_data_mut::<Model>().expect("model app_data");
    for &(name, default) in CVARS {
        super::cvars::register_in(&mut model, name, default);
    }
}

/// Wrap global `name`: `extend` sees the original's returns and the arguments, and answers the
/// returns to hand back.
fn wrap(
    lua: &Lua,
    name: &str,
    extend: impl Fn(&Lua, &MultiValue, MultiValue) -> mlua::Result<MultiValue> + 'static,
) -> mlua::Result<()> {
    let g = lua.globals();
    let original: Function = g.get(name)?;
    g.set(
        name,
        lua.create_function(move |lua, args: MultiValue| {
            let out: MultiValue = original.call(args.clone())?;
            extend(lua, &args, out)
        })?,
    )
}

/// Whether the first argument names the active player, for the mana pair.
fn names_player(lua: &Lua, args: &MultiValue) -> bool {
    let Some(token) = args.front().and_then(|v| optional_string(lua, v)) else {
        return false;
    };
    let model = lua.app_data_ref::<Model>().expect("model app_data");
    super::unit::names_self(lua, &model, &token)
}

/// Register SuperWoW's surface over the stock one: the in-game install's last step, so each
/// extension wraps the stock function it extends.
pub(super) fn install(lua: &Lua) -> mlua::Result<()> {
    lua.set_app_data(SuperWowState::default());
    register_cvars(lua);
    let g = lua.globals();
    g.set("SUPERWOW_VERSION", VERSION)?;
    g.set(
        "SUPERWOW_STRING",
        format!("SuperWoW {VERSION} by Balake (benilla)"),
    )?;

    super::mail::install_superwow(lua)?;
    super::quest_log::install_superwow(lua)?;

    // UnitExists(unit) → 1, guid: the GUID string second for a unit that exists.
    wrap(lua, "UnitExists", |lua, args, out| {
        if !out.front().is_some_and(truthy) {
            return Ok(out);
        }
        let Some(token) = args.front().and_then(|v| optional_string(lua, v)) else {
            return Ok(out);
        };
        let guid = {
            let model = lua.app_data_ref::<Model>().expect("model app_data");
            super::unit::view(lua, &model, &token).map(|u| u.guid)
        };
        match guid.filter(|&g| g != 0) {
            Some(g) => Ok(MultiValue::from_vec(vec![
                Value::Integer(1),
                Value::String(lua.create_string(guid_string(g))?),
            ])),
            None => Ok(out),
        }
    })?;

    // UnitMana/UnitManaMax("player") → power, mana: the druid's caster-form mana second.
    for (name, max) in [("UnitMana", false), ("UnitManaMax", true)] {
        wrap(lua, name, move |lua, args, out| {
            if !names_player(lua, args) {
                return Ok(out);
            }
            let Some((mana, max_mana)) = ask(lua, &PlayerManaQuery) else {
                return Ok(out);
            };
            let mut out = out.into_vec();
            out.truncate(1);
            out.push(Value::Integer(i64::from(if max { max_mana } else { mana })));
            Ok(MultiValue::from_vec(out))
        })?;
    }

    // SpellInfo(spellId) → name, rank, texture, minRange, maxRange; nil for no such spell.
    g.set(
        "SpellInfo",
        lua.create_function(|lua, id: Value| {
            let id = binding_abi::index_arg32(lua, id, "Usage: SpellInfo(spellId)")?;
            let Some(info) = ask(lua, &SpellInfoQuery(id)) else {
                return Ok(MultiValue::from_vec(vec![Value::Nil]));
            };
            Ok(MultiValue::from_vec(vec![
                Value::String(lua.create_string(&info.name)?),
                Value::String(lua.create_string(&info.rank)?),
                match &info.icon {
                    Some(t) => Value::String(lua.create_string(t)?),
                    None => Value::Nil,
                },
                Value::Number(f64::from(info.min_range)),
                Value::Number(f64::from(info.max_range)),
            ]))
        })?,
    )?;

    // UnitPosition(unit) → x, y, z in world coordinates, for a friendly unit; nothing for a
    // hostile one or a unit not held.
    g.set(
        "UnitPosition",
        lua.create_function(|lua, token: Value| {
            let text = binding_abi::string_arg(lua, token, r#"Usage: UnitPosition("unit")"#)?;
            let Some(guid) = unit_guid(lua, &text)? else {
                return Ok(MultiValue::new());
            };
            let friend: Function = lua.globals().get("UnitIsFriend")?;
            let friendly = friend
                .call::<Value>(("player", text.as_str()))
                .is_ok_and(|v| truthy(&v));
            if !friendly {
                return Ok(MultiValue::new());
            }
            Ok(match ask(lua, &UnitPositionQuery(guid)) {
                Some([x, y, z]) => numbers(&[f64::from(x), f64::from(y), f64::from(z)]),
                None => MultiValue::new(),
            })
        })?,
    )?;

    // UnitNameplate(unit) → the nameplate frame shown for the unit, or nil.
    g.set(
        "UnitNameplate",
        lua.create_function(|lua, token: Value| {
            let Some(guid) = unit_arg(lua, token, r#"Usage: UnitNameplate("unit")"#)? else {
                return Ok(Value::Nil);
            };
            let id = {
                let mut model = lua.app_data_mut::<Model>().expect("model app_data");
                let plate = model.nameplates.frame_of_guid(guid);
                plate.map(|h| model.frame_id(h))
            };
            match id {
                Some(id) => Ok(Value::Table(super::object::frame_wrapper(lua, id)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    // CanLootUnit(unit) → 1 or nil: the unit's `UNIT_DYNFLAG_LOOTABLE`.
    g.set(
        "CanLootUnit",
        lua.create_function(|lua, token: Value| {
            let token = binding_abi::string_arg(lua, token, r#"Usage: CanlootUnit("unit")"#)?;
            super::unit::check_unit_token(&Some(token.clone()))?;
            let lootable = super::unit::read_object(lua, &token, |u, o| {
                u.exists && o.dynamic_flags() & 1 != 0
            })
            .unwrap_or(false);
            Ok(binding_abi::flag(lootable))
        })?,
    )?;

    // SetMouseoverUnit([unit]): the "mouseover" token names the unit until the next publish, as a
    // unit frame's OnEnter sets it; no argument clears it.
    g.set(
        "SetMouseoverUnit",
        lua.create_function(|lua, token: Option<Value>| {
            let token = token.and_then(|v| optional_string(lua, &v));
            let guid = match token.as_deref() {
                Some(t) if !t.is_empty() => unit_guid(lua, t)?,
                _ => None,
            };
            if !super::mouseover::publish(lua, guid) {
                return Ok(());
            }
            if let Some(g) = guid {
                let target = super::unit::with_live_objects(lua, |o| o.target_of(g))
                    .flatten()
                    .unwrap_or(0);
                let mut model = lua.app_data_mut::<Model>().expect("model app_data");
                model.unit_guids.mouseover = g;
                model.unit_guids.held.insert(g, target);
            }
            super::tick::verb(lua, |model| {
                model.raise("UPDATE_MOUSEOVER_UNIT", Vec::new())
            });
            Ok(())
        })?,
    )?;

    // Clickthrough([0/1]): set the switch, or with no argument answer it as 1 or 0.
    g.set(
        "Clickthrough",
        lua.create_function(|lua, on: Option<Value>| {
            let mut state = lua
                .app_data_mut::<SuperWowState>()
                .expect("superwow app_data");
            match on {
                Some(v) if !matches!(v, Value::Nil) => {
                    state.clickthrough = switch_arg(&v);
                    Ok(Value::Nil)
                }
                _ => Ok(Value::Integer(i64::from(state.clickthrough))),
            }
        })?,
    )?;

    // SetAutoloot([0/1]): benilla's auto-loot option, `autoLootDefault`; with no argument its
    // state as 1 or 0. On, a loot window loots itself unless Shift is held.
    g.set(
        "SetAutoloot",
        lua.create_function(|lua, on: Option<Value>| {
            let g = lua.globals();
            match on {
                Some(v) if !matches!(v, Value::Nil) => {
                    let value = if switch_arg(&v) { "1" } else { "0" };
                    let set: Function = g.get("SetCVar")?;
                    // A host with no such option leaves the call a no-op.
                    let _ = set.call::<()>(("autoLootDefault", value));
                    Ok(Value::Nil)
                }
                _ => {
                    let get: Function = g.get("GetCVar")?;
                    let on = get
                        .call::<Option<String>>("autoLootDefault")
                        .ok()
                        .flatten()
                        .is_some_and(|v| v != "0");
                    Ok(Value::Integer(i64::from(on)))
                }
            }
        })?,
    )?;

    // TrackUnit(unit) / UntrackUnit(unit | "all"): the tracked set the host may show.
    g.set(
        "TrackUnit",
        lua.create_function(|lua, token: Value| {
            if let Some(guid) = unit_arg(lua, token, r#"Usage: TrackUnit("unit")"#)? {
                lua.app_data_mut::<SuperWowState>()
                    .expect("superwow app_data")
                    .tracked
                    .insert(guid);
            }
            Ok(())
        })?,
    )?;
    g.set(
        "UntrackUnit",
        lua.create_function(|lua, token: Value| {
            let text = binding_abi::string_arg(lua, token, r#"Usage: UntrackUnit("unit")"#)?;
            if text.eq_ignore_ascii_case("all") {
                lua.app_data_mut::<SuperWowState>()
                    .expect("superwow app_data")
                    .tracked
                    .clear();
                return Ok(());
            }
            if let Some(guid) = unit_guid(lua, &text)? {
                lua.app_data_mut::<SuperWowState>()
                    .expect("superwow app_data")
                    .tracked
                    .remove(&guid);
            }
            Ok(())
        })?,
    )?;

    // CursorPosition() → x, y, z: the world point under the cursor.
    g.set(
        "CursorPosition",
        lua.create_function(|lua, ()| {
            Ok(match ask(lua, &CursorWorldQuery) {
                Some([x, y, z]) => numbers(&[f64::from(x), f64::from(y), f64::from(z)]),
                None => MultiValue::new(),
            })
        })?,
    )?;

    // IsSwimming(), IsMounted(), IsIndoors() → 1 or nil; GetSpeed() → run and swim speed in yd/s.
    for (name, pick) in [
        (
            "IsSwimming",
            (|m: &PlayerMotion| m.swimming) as fn(&PlayerMotion) -> bool,
        ),
        ("IsMounted", |m: &PlayerMotion| m.mounted),
        ("IsIndoors", |m: &PlayerMotion| m.indoors),
    ] {
        g.set(
            name,
            lua.create_function(move |lua, ()| {
                Ok(binding_abi::flag(
                    ask(lua, &PlayerMotionQuery).is_some_and(|m| pick(&m)),
                ))
            })?,
        )?;
    }
    g.set(
        "GetSpeed",
        lua.create_function(|lua, ()| {
            let m = ask(lua, &PlayerMotionQuery).unwrap_or_default();
            Ok(numbers(&[f64::from(m.run_speed), f64::from(m.swim_speed)]))
        })?,
    )?;

    // GetWeaponEnchantID([unit]) → main hand and off hand temporary enchant ids, 0 for none.
    g.set(
        "GetWeaponEnchantID",
        lua.create_function(|lua, token: Option<Value>| {
            let token = token
                .and_then(|v| optional_string(lua, &v))
                .unwrap_or_else(|| "player".to_string());
            let Some(guid) = unit_guid(lua, &token)? else {
                return Ok(MultiValue::new());
            };
            let (main, off) = ask(lua, &WeaponEnchantIdQuery(guid)).unwrap_or((0, 0));
            Ok(MultiValue::from_vec(vec![
                Value::Integer(i64::from(main)),
                Value::Integer(i64::from(off)),
            ]))
        })?,
    )?;

    // GetMapBoundaries(continentIndex, zoneIndex) → left, right, top, bottom.
    g.set(
        "GetMapBoundaries",
        lua.create_function(|lua, (c, z): (Value, Value)| {
            const USAGE: &str = "Usage: GetMapBoundaries(continentIndex, zoneIndex)";
            let continent = binding_abi::index_arg32(lua, c, USAGE)?;
            let zone = binding_abi::index_arg32(lua, z, USAGE)?;
            Ok(match ask(lua, &MapBoundsQuery { continent, zone }) {
                Some(b) => numbers(&b.map(f64::from)),
                None => MultiValue::new(),
            })
        })?,
    )?;

    // GetMapPositionWorldLoc(continentIndex, zoneIndex, mapX, mapY) → worldX, worldY: the map's
    // `WorldMapArea` box, map x running along world y (left to right) and map y along world x.
    g.set(
        "GetMapPositionWorldLoc",
        lua.create_function(|lua, (c, z, mx, my): (Value, Value, Value, Value)| {
            const USAGE: &str =
                "Usage: GetMapPositionWorldLoc(continentIndex, zoneIndex, mapX, mapY)";
            let continent = binding_abi::index_arg32(lua, c, USAGE)?;
            let zone = binding_abi::index_arg32(lua, z, USAGE)?;
            let mx = binding_abi::float_arg(lua, mx, USAGE)?;
            let my = binding_abi::float_arg(lua, my, USAGE)?;
            let Some([left, right, top, bottom]) = ask(lua, &MapBoundsQuery { continent, zone })
            else {
                return Ok(MultiValue::new());
            };
            let (left, right, top, bottom) = (
                f64::from(left),
                f64::from(right),
                f64::from(top),
                f64::from(bottom),
            );
            let world_y = left - mx * (left - right);
            let world_x = top - my * (top - bottom);
            Ok(numbers(&[world_x, world_y]))
        })?,
    )?;

    // ImportFile(name) → the text of `Imports/<name>.txt` in benilla-config, or nil.
    g.set(
        "ImportFile",
        lua.create_function(|lua, name: Value| {
            let name = binding_abi::string_arg(lua, name, r#"Usage: ImportFile("filename")"#)?;
            Ok(import_name(&name).and_then(|n| ask(lua, &ImportFileQuery(n))))
        })?,
    )?;

    // ExportFile(name, text): write `Imports/<name>.txt` in benilla-config.
    g.set(
        "ExportFile",
        lua.create_function(|lua, (name, text): (Value, Value)| {
            const USAGE: &str = r#"Usage: ExportFile("filename","text")"#;
            let name = binding_abi::string_arg(lua, name, USAGE)?;
            let text = binding_abi::string_arg(lua, text, USAGE)?;
            if let Some(name) = import_name(&name) {
                ask(lua, &ExportFileQuery { name, text });
            }
            Ok(())
        })?,
    )?;

    // CombatLogAdd(text [, addToRawLog]): a line for the combat log file, while it logs.
    g.set(
        "CombatLogAdd",
        lua.create_function(|lua, (text, raw): (Value, Option<Value>)| {
            let text = binding_abi::string_arg(
                lua,
                text,
                r#"Usage: CombatLogAdd("text" [, addToRawLog])"#,
            )?;
            let raw = raw.as_ref().is_some_and(switch_arg);
            ask(lua, &CombatLogAddQuery { text, raw });
            Ok(())
        })?,
    )?;

    // The sound driver list: benilla plays through one device, the system's default.
    g.set("GetNumSoundDrivers", lua.create_function(|_, ()| Ok(1i64))?)?;
    g.set(
        "GetLoadedSoundDriver",
        lua.create_function(|_, ()| Ok(1i64))?,
    )?;
    g.set(
        "GetSoundDriverName",
        lua.create_function(|lua, index: Value| {
            let index =
                binding_abi::index_arg(lua, index, "Usage: GetSoundDriverName(driverIndex)")?;
            Ok((index == 1).then_some("Default"))
        })?,
    )?;
    g.set(
        "GetSoundDevices",
        lua.create_function(|_, ()| Ok("Default"))?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::UiScript;

    #[test]
    fn the_globals_and_the_new_functions_are_there() {
        let s = UiScript::new().unwrap();
        assert_eq!(
            s.eval::<String>("return SUPERWOW_VERSION").unwrap(),
            VERSION
        );
        for g in GLOBALS {
            assert!(
                s.eval::<bool>(&format!("return {g} ~= nil")).unwrap(),
                "{g}"
            );
        }
    }

    #[test]
    fn the_events_register_and_the_cvars_read() {
        let s = UiScript::new().unwrap();
        for event in EVENTS {
            assert!(
                super::super::glue::in_game_events().contains(event),
                "{event}"
            );
        }
        assert_eq!(
            s.eval::<String>("return GetCVar('NameplateRange')")
                .unwrap(),
            "20"
        );
        s.eval::<()>("SetCVar('FoV', '2')").unwrap();
        assert_eq!(s.eval::<String>("return GetCVar('FoV')").unwrap(), "2");
    }

    #[test]
    fn switches_answer_their_state() {
        let s = UiScript::new().unwrap();
        assert_eq!(s.eval::<i64>("return Clickthrough()").unwrap(), 0);
        s.eval::<()>("Clickthrough(1)").unwrap();
        assert_eq!(s.eval::<i64>("return Clickthrough()").unwrap(), 1);
        assert!(s.superwow_state().clickthrough);
    }

    #[test]
    fn import_names_stay_inside_the_folder() {
        assert_eq!(
            import_name("raid-notes_1.v2").as_deref(),
            Some("raid-notes_1.v2")
        );
        assert_eq!(import_name("../etc/passwd"), None);
        assert_eq!(import_name("a/b"), None);
        assert_eq!(import_name("a\\b"), None);
        assert_eq!(import_name(""), None);
    }
}
