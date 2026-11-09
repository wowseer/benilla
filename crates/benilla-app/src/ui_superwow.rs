//! SuperWoW 2.2's host half: the live family that answers the questions of
//! [`benilla_ui::script::superwow`], and the `UNIT_CASTEVENT` lane off the spell and swing packets.
//!
//! SuperWoW (by Balake) is a DLL for the 1.12.1 client, not 1.12.1 itself; benilla builds its
//! surface so the addons written for it run here. What each answer reads is named on its arm.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use benilla_protocol::{SessionEvent, SessionEventKind};
use benilla_ui::script::live::LiveAsk;
use benilla_ui::script::superwow::{
    CombatLogAddQuery, CursorWorldQuery, ExportFileQuery, ImportFileQuery, MapBoundsQuery,
    PlayerManaQuery, PlayerMotion, PlayerMotionQuery, RaidMarksQuery, SpellInfo, SpellInfoQuery,
    UnitPositionQuery, WeaponEnchantIdQuery,
};
use benilla_ui::script::ScriptValue;

use crate::net::{GuidIndex, NetHandlerApp, SelfPlayer, UnitSpeeds};
use crate::ui_unit::views::ViewStores;

/// `PLAYER_VISIBLE_ITEM` slots of the main and off hand (equipment slots 16 and 17, 0-based).
const MAIN_HAND: u8 = 15;
const OFF_HAND: u8 = 16;
/// The temporary enchantment slot of a visible item's enchant block.
const TEMP_ENCHANT: u8 = 1;
/// `HITINFO_LEFTSWING`: the swing is the off hand's.
const HIT_INFO_LEFT_SWING: u32 = 0x4;
/// The auto-attack spell SuperWoW names a swing's event by.
const AUTO_ATTACK: u32 = 6603;
/// `SPELL_ATTR_EX_CHANNELED_1 | SPELL_ATTR_EX_CHANNELED_2`: a channel, whose go SuperWoW names
/// `CHANNEL`, with its duration.
const CHANNELED: u32 = 0x44;

/// What SuperWoW's getters read as they run.
#[derive(SystemParam)]
pub(crate) struct SuperWowLive<'w, 's> {
    index: Option<Res<'w, GuidIndex>>,
    stores: ViewStores<'w, 's>,
    me: Query<'w, 's, (Entity, Option<&'static UnitSpeeds>), With<SelfPlayer>>,
    player: Option<Res<'w, crate::player::Player>>,
    transforms: bevy::transform::helper::TransformHelper<'w, 's>,
    group: Option<Res<'w, crate::ui_party::GroupState>>,
    spells: Option<Res<'w, crate::ui_action::Spells>>,
    maps: Option<Res<'w, crate::ui_world_map::WorldMapUiData>>,
    logs: Option<Res<'w, crate::ui_chat::logging::ChatLogFiles>>,
    /// This frame's world hit along the cursor ray.
    cursor: Option<Res<'w, crate::target::PickOcclusion>>,
    /// The indoor bit as the frame's sync copied it.
    switches: Option<Res<'w, SuperWowSwitches>>,
}

impl SuperWowLive<'_, '_> {
    fn store(&self, guid: u64) -> Option<&crate::net::ObjectStore> {
        let entity = self.index.as_deref().and_then(|i| i.0.get(&guid).copied());
        self.stores.at(guid, entity)
    }

    fn self_store(&self) -> Option<&crate::net::ObjectStore> {
        let (entity, _) = self.me.single().ok()?;
        self.stores.get(entity).ok()
    }

    /// `UnitPosition`: our body's position, else the unit's composed transform, in WoW axes.
    fn position(&self, guid: u64) -> Option<[f32; 3]> {
        let entity = *self.index.as_deref()?.0.get(&guid)?;
        self.store(guid).filter(|s| s.is_unit())?;
        let at = if self.me.single().ok().map(|(e, _)| e) == Some(entity) {
            self.player.as_deref()?.pos
        } else {
            self.transforms
                .compute_global_transform(entity)
                .ok()?
                .translation()
        };
        Some(benilla_assets::coords::bevy_to_wow(at))
    }

    /// `UnitMana("player")`'s second return: `UNIT_FIELD_POWER1`/`MAXPOWER1`, mana in any form.
    fn mana(&self) -> Option<(u32, u32)> {
        let fields = &self.self_store()?.0;
        Some((
            fields.unit_power(0).unwrap_or(0),
            fields.unit_max_power(0).unwrap_or(0),
        ))
    }

    /// `SpellInfo`: the `Spell.dbc` row's name, rank and icon, and its `SpellRange.dbc` pair.
    fn spell_info(&self, id: u32) -> Option<SpellInfo> {
        let spells = self.spells.as_deref()?;
        let d = spells.catalog.get(id)?;
        let range = spells.ranges.get(d.range_index);
        Some(SpellInfo {
            name: d.name.clone(),
            rank: d.rank.clone().unwrap_or_default(),
            icon: d.icon.clone(),
            min_range: range.map_or(0.0, |r| r.min),
            max_range: range.map_or(0.0, |r| r.max),
        })
    }

    /// `IsSwimming`, `IsMounted`, `IsIndoors` and `GetSpeed` off our body, our descriptor and the
    /// indoor bit (the down-ray's interior group under the player).
    fn motion(&self) -> Option<PlayerMotion> {
        let player = self.player.as_deref()?;
        let (_, speeds) = self.me.single().ok()?;
        let mounted = self
            .self_store()
            .is_some_and(|s| s.0.unit_mount_display_id() != 0);
        let speeds = speeds.map(|s| s.0);
        Some(PlayerMotion {
            swimming: player.swimming,
            mounted,
            indoors: self.switches.as_deref().is_some_and(|s| s.indoors),
            run_speed: speeds.map_or(0.0, |s| s.run),
            swim_speed: speeds.map_or(0.0, |s| s.swim),
        })
    }

    /// `GetWeaponEnchantID`: the temporary enchant on the unit's broadcast main and off hand.
    fn weapon_enchants(&self, guid: u64) -> Option<(u32, u32)> {
        let fields = &self.store(guid)?.0;
        let temp = |slot| {
            fields
                .player_visible_item_enchant(slot, TEMP_ENCHANT)
                .unwrap_or(0)
        };
        Some((temp(MAIN_HAND), temp(OFF_HAND)))
    }

    /// `GetMapBoundaries`: the `WorldMapArea` row's loc box.
    fn map_bounds(&self, continent: u32, zone: u32) -> Option<[f32; 4]> {
        let r = self.maps.as_deref()?.bounds(continent, zone)?;
        Some([r.left, r.right, r.top, r.bottom])
    }

    /// `CursorPosition`: the world hit under the cursor this frame, WoW axes; `None` over the sky.
    fn cursor_world(&self) -> Option<[f32; 3]> {
        let point = self.cursor.as_deref()?.point?;
        Some(benilla_assets::coords::bevy_to_wow(point))
    }

    /// `CombatLogAdd(text [, raw])`: a line in `WoWCombatLog.txt`, or with `raw` in
    /// `WoWRawCombatLog.txt`, while combat logging is on.
    fn combat_log_add(&self, q: &CombatLogAddQuery) -> Option<bool> {
        let logs = self.logs.as_deref()?;
        if q.raw {
            logs.record_raw(&q.text);
        } else {
            logs.record(true, &q.text);
        }
        Some(true)
    }
}

/// `benilla-config/Imports/<name>.txt`, where `ImportFile` and `ExportFile` read and write; the
/// DLL's `Imports\` folder beside the executable.
fn import_path(name: &str) -> Option<std::path::PathBuf> {
    Some(
        crate::local_state::home()?
            .join("Imports")
            .join(format!("{name}.txt")),
    )
}

fn import_file(name: &str) -> Option<String> {
    std::fs::read_to_string(import_path(name)?).ok()
}

fn export_file(name: &str, text: &str) -> bool {
    let Some(path) = import_path(name) else {
        return false;
    };
    crate::local_state::write_atomic(&path, text).is_ok()
}

impl crate::script_calls::live::LiveFamily for SuperWowLive<'_, '_> {
    fn answer(&mut self, ask: &mut LiveAsk<'_>) -> bool {
        ask.answer(|_: &RaidMarksQuery| self.group.as_deref().map(|g| g.raid_targets))
            || ask.answer(|q: &UnitPositionQuery| self.position(q.0))
            || ask.answer(|_: &PlayerManaQuery| self.mana())
            || ask.answer(|q: &SpellInfoQuery| self.spell_info(q.0))
            || ask.answer(|_: &PlayerMotionQuery| self.motion())
            || ask.answer(|q: &WeaponEnchantIdQuery| self.weapon_enchants(q.0))
            || ask.answer(|q: &MapBoundsQuery| self.map_bounds(q.continent, q.zone))
            || ask.answer(|q: &ImportFileQuery| import_file(&q.0))
            || ask.answer(|q: &ExportFileQuery| Some(export_file(&q.name, &q.text)))
            || ask.answer(|q: &CombatLogAddQuery| self.combat_log_add(q))
            || ask.answer(|_: &CursorWorldQuery| self.cursor_world())
    }
}

/// One `UNIT_CASTEVENT`: caster, target (empty for none), the event word, the spell and the time.
#[derive(Debug, Clone, PartialEq)]
struct CastEvent {
    caster: u64,
    target: Option<u64>,
    kind: &'static str,
    spell: u32,
    ms: u32,
}

impl CastEvent {
    fn args(&self) -> Vec<ScriptValue> {
        use benilla_ui::script::guid_string;
        vec![
            ScriptValue::Str(guid_string(self.caster)),
            ScriptValue::Str(self.target.map(guid_string).unwrap_or_default()),
            ScriptValue::Str(self.kind.to_string()),
            ScriptValue::Int(i64::from(self.spell)),
            ScriptValue::Int(i64::from(self.ms)),
        ]
    }
}

/// The event a packet makes, given whether a guid is a held unit and a spell's channel duration
/// (`None` for a spell that does not channel).
fn cast_event(
    ev: &SessionEvent,
    held: impl Fn(u64) -> bool,
    channel_ms: impl Fn(u32) -> Option<u32>,
) -> Option<CastEvent> {
    let target = |t: Option<u64>| t.filter(|&g| g != 0 && held(g));
    Some(match ev {
        SessionEvent::SpellStart {
            caster,
            spell_id,
            cast_time_ms,
            target: t,
            ..
        } => CastEvent {
            caster: *caster,
            target: target(*t),
            kind: "START",
            spell: *spell_id,
            ms: *cast_time_ms,
        },
        SessionEvent::SpellGo {
            caster,
            spell_id,
            target: t,
            ..
        } => {
            let channel = channel_ms(*spell_id);
            CastEvent {
                caster: *caster,
                target: target(*t),
                kind: if channel.is_some() { "CHANNEL" } else { "CAST" },
                spell: *spell_id,
                ms: channel.unwrap_or(0),
            }
        }
        SessionEvent::SpellFailedOther { caster, spell_id } => CastEvent {
            caster: *caster,
            target: None,
            kind: "FAIL",
            spell: *spell_id,
            ms: 0,
        },
        SessionEvent::AttackerState(s) => CastEvent {
            caster: s.attacker,
            target: target(Some(s.victim)),
            kind: if s.hit_info & HIT_INFO_LEFT_SWING != 0 {
                "OFFHAND"
            } else {
                "MAINHAND"
            },
            spell: AUTO_ATTACK,
            ms: 0,
        },
        _ => return None,
    })
}

fn on_spell_start(In(ev): In<SessionEvent>, world: &mut World) {
    if let ev @ SessionEvent::SpellStart { .. } = ev {
        fire_cast_event(&ev, world);
    }
}

fn on_spell_go(In(ev): In<SessionEvent>, world: &mut World) {
    if let ev @ SessionEvent::SpellGo { .. } = ev {
        fire_cast_event(&ev, world);
    }
}

fn on_spell_failed_other(In(ev): In<SessionEvent>, world: &mut World) {
    if let ev @ SessionEvent::SpellFailedOther { .. } = ev {
        fire_cast_event(&ev, world);
    }
}

fn on_attacker_state(In(ev): In<SessionEvent>, world: &mut World) {
    if let ev @ SessionEvent::AttackerState(_) = ev {
        fire_cast_event(&ev, world);
    }
}

/// Fire `UNIT_CASTEVENT` for a spell or swing packet, after its own handlers.
fn fire_cast_event(ev: &SessionEvent, world: &mut World) {
    let event = {
        let index = world.get_resource::<GuidIndex>();
        let held = |guid: u64| {
            index
                .and_then(|i| i.0.get(&guid).copied())
                .and_then(|e| world.get::<crate::net::ObjectStore>(e))
                .is_some_and(|s| s.is_unit())
        };
        let spells = world.get_resource::<crate::ui_action::Spells>();
        let channel_ms = |id: u32| {
            let d = spells?.catalog.get(id)?;
            if d.attributes_ex & CHANNELED == 0 {
                return None;
            }
            let base = spells?.durations.get(d.duration_index)?.base_ms;
            Some(u32::try_from(base).unwrap_or(0))
        };
        cast_event(ev, held, channel_ms)
    };
    let Some(event) = event else {
        return;
    };
    crate::script_calls::with_hosted_vm(world, |script| {
        if let Some(script) = script {
            script.fire_event("UNIT_CASTEVENT", event.args());
        }
    });
}

/// SuperWoW's switches as the VM last held them, for the engine's systems that read them: the
/// world pick's click-through and the minimap's tracked units.
#[derive(Resource, Default, Clone, Debug, PartialEq)]
pub(crate) struct SuperWowSwitches {
    pub(crate) clickthrough: bool,
    pub(crate) tracked: std::collections::BTreeSet<u64>,
    /// SuperWoW's CVars ([`benilla_ui::script::superwow::CVARS`]) as the VM holds them, by name.
    pub(crate) cvars: std::collections::BTreeMap<&'static str, String>,
    /// `IsIndoors`: the zone resolver's indoor bit, the interior group under the player.
    pub(crate) indoors: bool,
    /// An addon called `SetAutoloot`.
    pub(crate) autoloot_owned: bool,
}

impl SuperWowSwitches {
    /// A SuperWoW CVar's value, its default before the VM is read.
    pub(crate) fn cvar(&self, name: &str) -> &str {
        self.cvars.get(name).map(String::as_str).unwrap_or_else(|| {
            benilla_ui::script::superwow::CVARS
                .iter()
                .find(|(n, _)| *n == name)
                .map_or("", |(_, d)| d)
        })
    }

    /// A 0/1 CVar: on unless `"0"` or empty.
    pub(crate) fn on(&self, name: &str) -> bool {
        let v = self.cvar(name);
        !v.is_empty() && v != "0"
    }

    /// A numeric CVar, `None` for text that is no number.
    pub(crate) fn number(&self, name: &str) -> Option<f32> {
        self.cvar(name).trim().parse().ok()
    }
}

/// Copy the VM's switches into [`SuperWowSwitches`] each frame, written only on a change.
fn sync_switches(
    script: Option<NonSend<benilla_ui::script::UiScript>>,
    mut switches: ResMut<SuperWowSwitches>,
) {
    let Some(script) = script else {
        return;
    };
    let state = script.superwow_state();
    let cvars = benilla_ui::script::superwow::CVARS
        .iter()
        .map(|&(name, default)| {
            (
                name,
                script.cvar(name).unwrap_or_else(|| default.to_string()),
            )
        })
        .collect();
    let next = SuperWowSwitches {
        clickthrough: state.clickthrough,
        tracked: state.tracked,
        cvars,
        indoors: switches.indoors,
        autoloot_owned: state.autoloot_owned,
    };
    if *switches != next {
        *switches = next;
    }
}

/// SuperWoW's `FoV`: the camera's field of view, 0.1 to 3.14 radians, as `[cam+0x40]`.
fn apply_fov(switches: Res<SuperWowSwitches>, fov: Option<ResMut<benilla_world::view::CamFov>>) {
    let Some(mut fov) = fov else {
        return;
    };
    let want = switches
        .number("FoV")
        .filter(|v| v.is_finite())
        .map_or(std::f32::consts::FRAC_PI_2, |v| {
            v.clamp(0.1, std::f32::consts::PI)
        });
    if (fov.0 - want).abs() > f32::EPSILON {
        fov.0 = want;
    }
}

/// SuperWoW's chat bubble CVars into [`crate::chat_bubble::BubbleExtras`].
fn apply_bubbles(
    switches: Res<SuperWowSwitches>,
    extras: Option<ResMut<crate::chat_bubble::BubbleExtras>>,
) {
    let Some(mut extras) = extras else {
        return;
    };
    let range = switches
        .number("ChatBubbleRange")
        .filter(|v| v.is_finite())
        .map_or(20.0, |v| v.clamp(10.0, 200.0));
    let next = crate::chat_bubble::BubbleExtras {
        raid: switches.on("ChatBubblesRaid"),
        battleground: switches.on("ChatBubblesBattleground"),
        whisper: switches.on("ChatBubblesWhisper"),
        creatures: switches.on("ChatBubblesCreatures"),
        range_sq: range * range,
    };
    if *extras != next {
        *extras = next;
    }
}

/// `IsIndoors`'s bit, the zone resolver's interior under the player; skipped where the world's
/// point queries do not run.
fn sync_indoors(
    world: bevy::ecs::system::If<benilla_world::world_point::WorldPoint>,
    mut switches: ResMut<SuperWowSwitches>,
) {
    let indoors = world.area_interior().is_some();
    if switches.indoors != indoors {
        switches.indoors = indoors;
    }
}

/// An addon's `SetAutoloot` takes the auto-loot switch over ([`crate::ui_loot::LootConfig`]).
fn apply_autoloot(
    switches: Res<SuperWowSwitches>,
    cfg: Option<ResMut<crate::ui_loot::LootConfig>>,
) {
    if let Some(mut cfg) = cfg {
        if cfg.superwow_owned != switches.autoloot_owned {
            cfg.superwow_owned = switches.autoloot_owned;
        }
    }
}

/// The `UNIT_CASTEVENT` lane. Added after the spell and combat-log plugins, so the event follows
/// each packet's own handlers.
pub(crate) struct SuperWowPlugin;

impl Plugin for SuperWowPlugin {
    fn build(&self, app: &mut App) {
        use SessionEventKind as K;
        app.init_resource::<SuperWowSwitches>()
            .init_resource::<crate::chat_bubble::BubbleExtras>()
            .add_systems(
                PreUpdate,
                (
                    sync_switches,
                    sync_indoors,
                    apply_fov,
                    apply_bubbles,
                    apply_autoloot,
                )
                    .chain(),
            );
        app.net_handler(K::SpellStart, on_spell_start)
            .net_handler(K::SpellGo, on_spell_go)
            .net_handler(K::SpellFailedOther, on_spell_failed_other)
            .net_handler(K::AttackerState, on_attacker_state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_names_its_held_target_and_its_cast_time() {
        let ev = SessionEvent::SpellStart {
            caster: 0x10,
            spell_id: 133,
            cast_flags: 0,
            cast_time_ms: 1500,
            target: Some(0x20),
            ammo_display_id: None,
        };
        let e = cast_event(&ev, |g| g == 0x20, |_| None).unwrap();
        assert_eq!(
            (e.kind, e.spell, e.ms, e.target),
            ("START", 133, 1500, Some(0x20))
        );
        let e = cast_event(&ev, |_| false, |_| None).unwrap();
        assert_eq!(e.target, None, "a target not held is empty");
        assert_eq!(e.args()[1], ScriptValue::Str(String::new()));
    }

    #[test]
    fn a_failure_and_a_swing_name_their_kind() {
        let fail = SessionEvent::SpellFailedOther {
            caster: 1,
            spell_id: 7,
        };
        assert_eq!(cast_event(&fail, |_| true, |_| None).unwrap().kind, "FAIL");
        let s = benilla_protocol::messages::AttackerState {
            attacker: 1,
            victim: 2,
            hit_info: HIT_INFO_LEFT_SWING,
            damage: 0,
            victim_state: 0,
            absorb: 0,
            resist: 0,
            blocked: 0,
            melee_spell_id: 0,
            school: 0,
        };
        let e = cast_event(&SessionEvent::AttackerState(s), |_| true, |_| None).unwrap();
        assert_eq!(
            (e.kind, e.spell, e.target),
            ("OFFHAND", AUTO_ATTACK, Some(2))
        );
    }

    #[test]
    fn the_caster_is_a_guid_string() {
        let e = CastEvent {
            caster: 0xF130_0008_5D27_A6BA,
            target: None,
            kind: "CAST",
            spell: 1,
            ms: 0,
        };
        assert_eq!(
            e.args()[0],
            ScriptValue::Str("0xF13000085D27A6BA".to_string())
        );
    }
}
