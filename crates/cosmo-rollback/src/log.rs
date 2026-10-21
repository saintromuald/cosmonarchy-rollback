//! Append-only log in the game's working directory (`cosmo_rollback.log`).

use std::io::Write;

pub fn log(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("cosmo_rollback.log")
    {
        let _ = writeln!(f, "{}", msg);
    }
}
