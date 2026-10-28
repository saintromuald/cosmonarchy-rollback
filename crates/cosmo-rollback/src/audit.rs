//! Startup diagnostics: log the resolved snapshot range layout.

use crate::log::log;

pub fn on_start() {
    log(&format!("[cosmo-rollback] v{} loaded", env!("CARGO_PKG_VERSION")));
    // NOTE: the range layout is logged later (`log_layout`, on the first frame). It resolves
    // the UnitPath/AiScript/AiTown pools, which are heap-allocated at *game* init — at plugin
    // startup they do not exist yet, so building the (now cached) table here would silently
    // drop those ranges.

    let shadow = std::env::var("CM_ROLLBACK_SHADOW").unwrap_or_else(|_| "off".into());
    log(&format!("[cosmo-rollback] shadow={shadow}"));

    // Bring up the plugin serializers (ADR-0004/0005 Layers B & C): log what is loaded,
    // resolve each plugin's exports, and enable them for this session.
    log("[cosmo-rollback] loaded modules:");
    crate::modules::log_modules();
    // Warm the module-list cache here, at load time: every later lookup walks the
    // cache, and the snapshot API must never run on the game thread at frame time
    // (the loader lock it takes deadlocks the game's first frame — see modules.rs).
    crate::modules::module_list();
    let names = crate::plugin_state::plugin_names();
    log(&format!("[cosmo-rollback] plugin serializers: {names:?}"));
    crate::plugin_state::enable_all();

    // Resolve the object pools (CCMU-relocated on Cosmonarchy) and log where each one landed.
    let pools = crate::pools::get();
    log(&format!(
        "[cosmo-rollback] pools: units=0x{:08x} sprites=0x{:08x} orders=0x{:08x} \
         images=0x{:08x} thingies=0x{:08x} fow=0x{:08x} bullets=0x{:08x}",
        pools.unit_table,
        pools.sprite_table,
        pools.order_table,
        pools.image_table,
        pools.thingy_table,
        pools.fow_sprite_table,
        pools.bullet_table
    ));
}

/// Log the resolved range layout, once, on the first frame (see `on_start`).
pub fn log_layout() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    log("[cosmo-rollback] layout: entering");
    let rl = crate::ranges::engine_ranges();
    log("[cosmo-rollback] layout: ranges resolved");
    log(&format!(
        "[cosmo-rollback] ranges: {} ranges, {} bytes",
        rl.ranges.len(),
        rl.total_bytes()
    ));
    // The per-object pool fields (`units.sim`, `images.render`, …) are thousands of ranges;
    // logging each one tells nobody anything. Log the named ranges and summarise the rest.
    let mut per_object = 0usize;
    for r in &rl.ranges {
        if r.name.ends_with(".sim") || r.name.ends_with(".render") {
            per_object += 1;
            continue;
        }
        log(&format!(
            "  range {:<20} 0x{:08x} len 0x{:x}",
            r.name, r.addr, r.len
        ));
    }
    log(&format!(
        "  (+ {per_object} per-object pool field ranges)"
    ));
    let shadow = std::env::var("CM_ROLLBACK_SHADOW").unwrap_or_else(|_| "off".into());
    log(&format!("[cosmo-rollback] shadow={}", shadow));

    // Enable the plugin serializers if present (ADR-0004/0005 Layers B & C).
    log("[cosmo-rollback] loaded modules:");
    crate::modules::log_modules();
    let names = crate::plugin_state::plugin_names();
    log(&format!("[cosmo-rollback] plugin serializers: {:?}", names));
    crate::plugin_state::enable_all();

    // Resolve CrownLink's rollback transport channel if the loaded `.snp` has it
    // (ADR-0001 §2.1). Lazy: the `.snp` is bound after plugin startup, so this is a
    // one-shot diagnostic at the first frame (`CM_ROLLBACK_CHANNEL_PROBE=1`).
    crate::transport::probe_channel_once();

    // Resolve the object pools (CCMU-relocated on Cosmonarchy) and log where each
    // one actually landed.
    let pools = crate::pools::get();
    log(&format!(
        "[cosmo-rollback] pools: units=0x{:08x} sprites=0x{:08x} orders=0x{:08x} \
         images=0x{:08x} thingies=0x{:08x} fow=0x{:08x} bullets=0x{:08x}",
        pools.unit_table,
        pools.sprite_table,
        pools.order_table,
        pools.image_table,
        pools.thingy_table,
        pools.fow_sprite_table,
        pools.bullet_table
    ));
}
