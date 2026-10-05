use gtk::gio;

use crate::config;

pub fn settings() -> gio::Settings {
    thread_local! {
        static SETTINGS: gio::Settings = create();
    }
    SETTINGS.with(Clone::clone)
}

pub fn schema(id: &str) -> Option<gio::SettingsSchema> {
    if let Some(found) = gio::SettingsSchemaSource::default().and_then(|s| s.lookup(id, true)) {
        return Some(found);
    }
    let dir = concat!(env!("OUT_DIR"), "/schemas");
    gio::SettingsSchemaSource::from_directory(dir, gio::SettingsSchemaSource::default().as_ref(), false)
        .ok()?
        .lookup(id, true)
}

fn create() -> gio::Settings {
    let schema =
        schema(config::APP_ID).unwrap_or_else(|| panic!("The settings schema {} is not installed", config::APP_ID));
    gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None)
}
