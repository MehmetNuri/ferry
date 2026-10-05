pub const APP_ID: &str = "io.github.mehmetnuri.Ferry";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GETTEXT_PACKAGE: &str = "ferry";
pub const LOCALEDIR: &str = match option_env!("FERRY_LOCALEDIR") {
    Some(dir) => dir,
    None => concat!(env!("OUT_DIR"), "/locale"),
};
