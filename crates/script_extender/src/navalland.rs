//! Battles on water become land battles (notes/naval_to_land.md).
//!
//! 3K lets armies stand on water hexes (river or sea: `is_at_sea()`, no region), but ships no
//! naval battle assets (models_naval and unit_stats_naval_crew are empty). An attack between such
//! armies makes a `naval_normal` pending battle: START BATTLE is disabled (tooltip
//! `3k_cant_launch_battle_tooltip_army_on_sea`) and forcing it crashes while the battle model is
//! built. Here the battle is made an ordinary land battle instead:
//!
//! - `determine_battle_type` FUN_1419b5d00(att, def, ambush, flag) -> u16 is called by
//!   prepare_for_battle before the pending battle (PB) exists; its result is stored at PB+0x68 by
//!   the PB ctor. Sea types (11 naval_normal, 12 naval_blockade, 13 naval_breakout, 14
//!   port_assault) are returned as 0 land_normal, so every reader (Lua battle_type(), autoresolve,
//!   the battle-location request, deployment) sees a land battle. The location request then moves
//!   the battle to the nearest land cell of the battle-locations map.
//! - `could_ever_play_battle_on_battle_map` FUN_141859890(pb) -> u8 returns 2 when a primary
//!   commander is not on land: that is what disables START (CanLaunchBattle), sets the default vote
//!   and the "is autoresolved" check. For a PB whose type is not a sea type, 2 becomes 0.
//! - `build_battle_setup` FUN_141863930(pb, u8, u8, u8) asks `is_commanding_force_on_sea`
//!   FUN_141a67080(char) once (at 0x1863a8d) for the battle's `at_sea` (weather, BATTLE_SETUP_INFO
//!   +0xf8, both army setups). While it runs for a land-type PB, that query answers "not at sea" on
//!   the same thread. Every other caller of the predicate (movement, ZOC, AI, ports) is untouched.
//!
//! script_extender.cfg: `naval_to_land` (default 1; 0 = no hooks, vanilla). It changes the
//! simulation, so it is in the sync tag. Install order: on-sea, setup, type, play; a failure stops
//! the rest, so the type is never changed without the setup scope.

use crate::addrs::Table;
use crate::log;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use retour::GenericDetour;
use std::sync::OnceLock;

type P = *mut c_void;
type BattleTypeFor = unsafe extern "C" fn(P, P, u8, u8) -> u16;
type CouldEverPlay = unsafe extern "C" fn(P) -> u8;
type BuildSetup = unsafe extern "C" fn(P, u8, u8, u8);
type OnSea = unsafe extern "C" fn(P) -> u8;

static TYPE_HOOK: OnceLock<GenericDetour<BattleTypeFor>> = OnceLock::new();
static PLAY_HOOK: OnceLock<GenericDetour<CouldEverPlay>> = OnceLock::new();
static SETUP_HOOK: OnceLock<GenericDetour<BuildSetup>> = OnceLock::new();
static SEA_HOOK: OnceLock<GenericDetour<OnSea>> = OnceLock::new();

/// Thread running a land-type build_battle_setup, and its nesting depth (0 = none).
static SETUP_TID: AtomicU32 = AtomicU32::new(0);
static SETUP_DEPTH: AtomicU32 = AtomicU32::new(0);

static CONVERTED: AtomicU64 = AtomicU64::new(0);
/// Last converted attacker/defender pair, to log each pair once instead of on every call.
static LAST_PAIR: AtomicU64 = AtomicU64::new(0);
static UNBLOCKED: AtomicU64 = AtomicU64::new(0);
static SEA_OVERRIDES: AtomicU64 = AtomicU64::new(0);

const LAND_NORMAL: u16 = 0;
const PB_TYPE: usize = 0x68;
const COULD_EVER_NOT_ON_LAND: u8 = 2;

extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn IsBadReadPtr(p: *const c_void, n: usize) -> i32;
}

fn is_sea_type(t: u16) -> bool { (11..=14).contains(&t) }

/// Battle type of a pending battle, or None when the pointer is not readable.
unsafe fn pb_type(pb: P) -> Option<u16> {
    let p = pb as usize + PB_TYPE;
    if (pb as usize) < 0x10000 || IsBadReadPtr(p as *const c_void, 2) != 0 { return None; }
    Some(core::ptr::read_unaligned(p as *const u16))
}

unsafe extern "C" fn battle_type_detour(att: P, def: P, ambush: u8, flag: u8) -> u16 {
    let Some(h) = TYPE_HOOK.get() else { return LAND_NORMAL };
    let t = h.call(att, def, ambush, flag);
    if !is_sea_type(t) { return t; }
    let n = CONVERTED.fetch_add(1, Ordering::Relaxed) + 1;
    // the AI and the UI prediction ask for the type of the same pair many times per turn: log the
    // first conversion of each attacker/defender pair (and every 1000th call), not every call
    let pair = (att as u64).wrapping_mul(31) ^ (def as u64);
    if LAST_PAIR.swap(pair, Ordering::Relaxed) != pair || n % 1000 == 0 {
        log!("naval_to_land: battle type {t} (sea) -> {LAND_NORMAL} land_normal (attacker 0x{:x}, defender 0x{:x}; {n} conversion(s) so far)", att as usize, def as usize);
    }
    LAND_NORMAL
}

unsafe extern "C" fn could_ever_play_detour(pb: P) -> u8 {
    let Some(h) = PLAY_HOOK.get() else { return 0 };
    let r = h.call(pb);
    if r != COULD_EVER_NOT_ON_LAND { return r; }
    match pb_type(pb) {
        Some(t) if !is_sea_type(t) => {
            if UNBLOCKED.fetch_add(1, Ordering::Relaxed) == 0 {
                log!("naval_to_land: a land-type battle (type {t}) with a commander on water is allowed to be fought (first of many checks, logged once)");
            }
            0
        }
        _ => r,
    }
}

unsafe extern "C" fn build_setup_detour(pb: P, a: u8, b: u8, c: u8) {
    let Some(h) = SETUP_HOOK.get() else { return };
    let land = matches!(pb_type(pb), Some(t) if !is_sea_type(t));
    if !land { return h.call(pb, a, b, c); }
    let me = GetCurrentThreadId();
    let outer = SETUP_DEPTH.load(Ordering::Acquire) == 0;
    if outer {
        SETUP_TID.store(me, Ordering::Release);
    } else if SETUP_TID.load(Ordering::Acquire) != me {
        // another thread is already inside a scoped setup: run this one unscoped
        return h.call(pb, a, b, c);
    }
    SETUP_DEPTH.fetch_add(1, Ordering::AcqRel);
    h.call(pb, a, b, c);
    SETUP_DEPTH.fetch_sub(1, Ordering::AcqRel);
    if outer { SETUP_TID.store(0, Ordering::Release); }
}

/// Hot (called from movement, ZOC, AI): one atomic load outside a scoped battle setup.
unsafe extern "C" fn on_sea_detour(ch: P) -> u8 {
    if SETUP_DEPTH.load(Ordering::Acquire) != 0 && SETUP_TID.load(Ordering::Acquire) == GetCurrentThreadId() {
        SEA_OVERRIDES.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    match SEA_HOOK.get() {
        Some(h) => h.call(ch),
        None => 0,
    }
}

/// (converted battle types, START unblocks, at-sea answers overridden during setup)
#[allow(dead_code)] // for a later se.query / status field
pub fn counters() -> (u64, u64, u64) {
    (CONVERTED.load(Ordering::Relaxed), UNBLOCKED.load(Ordering::Relaxed), SEA_OVERRIDES.load(Ordering::Relaxed))
}

/// Create, publish and enable one detour. Err stops the remaining installs.
unsafe fn enable<T: retour::Function>(lock: &'static OnceLock<GenericDetour<T>>, name: &'static str, target: T, detour: T, addr: usize) -> Result<(), String> {
    let d = GenericDetour::new(target, detour).map_err(|e| format!("{name}: could not create the detour ({e})"))?;
    let _ = lock.set(d);
    let d = lock.get().ok_or_else(|| format!("{name}: detour not published"))?;
    crate::freeze::enable_detour(name, "naval_to_land", addr, d).map_err(|e| format!("{name}: could not enable the detour ({e})"))
}

pub fn install(t: &Table) {
    if !crate::build::hook_enabled("naval_to_land") {
        log!("naval_to_land off (naval_to_land=0 in script_extender.cfg): battles on water stay naval (autoresolve only)");
        return;
    }
    // SAFETY: all four prologues are anchor-verified and contain no RIP-relative operand in the
    // relocated bytes (push rbx; sub rsp,20 / mov [rsp+n],reg sequences; notes/naval_to_land.md).
    let result: Result<(), String> = unsafe {
        let sea_addr = t.get("char_force_on_sea");
        let setup_addr = t.get("pb_build_battle_setup");
        let type_addr = t.get("battle_type_for");
        let play_addr = t.get("pb_could_ever_play");
        (|| {
            enable(&SEA_HOOK, "char_force_on_sea", core::mem::transmute::<usize, OnSea>(sea_addr), on_sea_detour as OnSea, sea_addr)?;
            enable(&SETUP_HOOK, "pb_build_battle_setup", core::mem::transmute::<usize, BuildSetup>(setup_addr), build_setup_detour as BuildSetup, setup_addr)?;
            enable(&TYPE_HOOK, "battle_type_for", core::mem::transmute::<usize, BattleTypeFor>(type_addr), battle_type_detour as BattleTypeFor, type_addr)?;
            enable(&PLAY_HOOK, "pb_could_ever_play", core::mem::transmute::<usize, CouldEverPlay>(play_addr), could_ever_play_detour as CouldEverPlay, play_addr)?;
            Ok(())
        })()
    };
    match result {
        Ok(()) => log!("naval_to_land installed: battles on water are fought as land battles on the nearest land"),
        Err(e) => log!("naval_to_land: {e}; remaining hooks not installed"),
    }
}
