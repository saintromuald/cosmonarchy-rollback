//! Plugin entry point for the 1.16.1 plugin surface.
//!
//! Loading path: the Cosmonarchy QDP hotloader loads `plugins/*.qdp`, calls
//! `GetMPQDraftPlugin` (see `crate::mpqdraft`), whose `InitializePlugin` calls
//! [`initialize`]. `Initialize` is also exported directly (as `aise` does).
//!
//! Integration mirrors `aise` (`.../aise/src/lib.rs:189-226`): schedule work at
//! `WinMain`, bring up `samase_shim` (resolves the 1.16.1 globals/functions and owns
//! its `whack` patcher), then install our own frame trampoline (`crate::frame_hook`).

use samase_shim::PluginApi;

/// Initialize the plugin. Exported for parity with `aise` and called by the
/// MPQDraft `InitializePlugin` vtable entry.
pub(crate) fn initialize() {
    unsafe { samase_shim::on_win_main(startup) }
}

#[no_mangle]
pub extern "C" fn Initialize() {
    initialize();
}

/// Arm the plugin from a config file, so a player does not have to edit a launch script.
///
/// `cosmo_rollback.conf` is a simple `key=value` file (`#` starts a comment, blank lines and
/// malformed lines are ignored). Every key must start with `CM_ROLLBACK_`, so the file can only
/// touch this plugin's own knobs; each becomes an environment variable, which is what the rest
/// of the plugin reads. It is looked for in the working directory (the game's directory) and
/// then in `plugins/` beside it.
fn load_config() {
    for path in ["cosmo_rollback.conf", "plugins/cosmo_rollback.conf"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let mut applied = 0u32;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            if !key.starts_with("CM_ROLLBACK_") || key.len() == "CM_ROLLBACK_".len() {
                continue;
            }
            std::env::set_var(key, value);
            applied += 1;
        }
        if applied > 0 {
            crate::log::log(&format!(
                "[cosmo-rollback] armed from {path}: {applied} setting(s)"
            ));
        }
    }
}

unsafe fn startup() {
    // Arming must happen before anything reads a knob.
    load_config();
    // Brings up the 1.16.1 environment (address resolution, patching infra).
    let ctx = samase_shim::init_1161();
    let _api: *const PluginApi = ctx.api();

    crate::frame_hook::install();
    crate::bug_capture::install();
    crate::audit::on_start();
}
