//! Build-time settings. Meson passes the installed paths; a plain `cargo build`
//! falls back to values that work from the source tree.
pub const APP_ID: &str = "io.github.mehmetnuri.Ferry";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GETTEXT_PACKAGE: &str = "ferry";
/// Set by Meson for installed builds; a source checkout uses the catalogs built by build.rs.
pub const LOCALEDIR: &str = match option_env!("FERRY_LOCALEDIR") {
    Some(dir) => dir,
    None => concat!(env!("OUT_DIR"), "/locale"),
};
