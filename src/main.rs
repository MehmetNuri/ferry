/// Prints a diagnostic line to stderr when FERRY_DEBUG is set.
macro_rules! debug {
    ($($arg:tt)*) => {
        if std::env::var_os("FERRY_DEBUG").is_some() { eprintln!("[ferry] {}", format!($($arg)*)); }
    };
}
pub(crate) use debug;

mod application;
mod backup;
mod cli;
mod config;
mod devtools;
mod dialogs;
mod i18n;
mod pages;
mod profile;
mod runtime;
mod s3;
mod search;
mod settings;
mod transfers;
mod widgets;
mod window;

use gettextrs::{LocaleCategory, bind_textdomain_codeset, bindtextdomain, setlocale, textdomain};
use gtk::{gio, glib, prelude::*};

fn main() -> glib::ExitCode {
    // SAFETY: called first, before any other thread exists.
    unsafe { setlocale(LocaleCategory::LcAll, "") };
    let _ = bindtextdomain(config::GETTEXT_PACKAGE, config::LOCALEDIR);
    let _ = bind_textdomain_codeset(config::GETTEXT_PACKAGE, "UTF-8");
    let _ = textdomain(config::GETTEXT_PACKAGE);

    gio::resources_register_include!("ferry.gresource").expect("Resources could not be registered");
    glib::set_application_name("Ferry");

    let mut args: Vec<String> = std::env::args().collect();
    // A subcommand runs in the terminal and never opens a window.
    if args.get(1).is_some_and(|a| cli::COMMANDS.contains(&a.as_str())) {
        return glib::ExitCode::from(cli::run(&args[1..]) as u8);
    }
    // "--hidden" starts in the background without a window, as at login.
    if let Some(index) = args.iter().position(|a| a == "--hidden") {
        args.remove(index);
        application::START_HIDDEN.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    application::Application::new().run_with_args(&args)
}
