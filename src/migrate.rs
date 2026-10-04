//! Moves the data of the earlier "S3 Browser" (io.github.mehmetnuri.S3Browser) over to
//! Ferry once: settings folder, cache with resumable transfers, preferences and the
//! start-at-login entry. Credentials in the keyring move on first use (see profile.rs).
use gtk::{gio, glib};
use gio::prelude::*;

use crate::config;

pub const LEGACY_APP_ID: &str = "io.github.mehmetnuri.S3Browser";

/// Renames `old` to `new` when only the old one exists.
fn move_dir(old: &std::path::Path, new: &std::path::Path) {
    if old.is_dir() && !new.exists() && std::fs::rename(old, new).is_err() {
        eprintln!("Could not move {} to {}", old.display(), new.display());
    }
}

/// Only the folders, for the command line, which does not touch preferences.
pub fn run_files_only() {
    let config_base = glib::user_config_dir();
    move_dir(&config_base.join(LEGACY_APP_ID), &config_base.join(config::APP_ID));
    let cache = glib::user_cache_dir();
    move_dir(&cache.join("s3-browser"), &cache.join("ferry"));
}

pub fn run() {
    run_files_only();
    let config_base = glib::user_config_dir();
    let marker = crate::profile::config_dir().join(".migrated-from-s3-browser");
    if marker.exists() {
        return;
    }
    settings();
    let legacy_autostart = config_base.join("autostart").join(format!("{LEGACY_APP_ID}.desktop"));
    if legacy_autostart.exists() {
        let _ = std::fs::remove_file(&legacy_autostart);
        crate::application::set_autostart(true);
    }
    let _ = std::fs::write(marker, b"");
}

/// Copies the preferences the user changed, read at the earlier application's path
/// through the path-less key schema.
fn settings() {
    let Some(keys) = crate::settings::schema(&format!("{}.Keys", config::APP_ID)) else { return };
    let old = gio::Settings::new_full(&keys, None::<&gio::SettingsBackend>, Some("/io/github/mehmetnuri/S3Browser/"));
    let new = crate::settings::settings();
    for key in keys.list_keys() {
        if let Some(value) = old.user_value(&key) {
            let _ = new.set_value(&key, &value);
            old.reset(&key);
        }
    }
    gio::Settings::sync();
}
