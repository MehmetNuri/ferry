use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::config;
use crate::i18n::{tr, trf, trn};
use crate::settings::settings;
use crate::window::Window;
use std::cell::Cell;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Application;

    #[glib::object_subclass]
    impl ObjectSubclass for Application {
        const NAME: &'static str = "FerryApplication";
        type Type = super::Application;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for Application {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup_actions();
        }
    }

    impl ApplicationImpl for Application {
        // A second launch only brings the existing window forward.
        fn activate(&self) {
            let app = self.obj();
            let window = app.main_window();
            // Started at login: the window exists for backups and transfers but stays hidden.
            if START_HIDDEN.swap(false, std::sync::atomic::Ordering::Relaxed) {
                crate::debug!("started hidden");
                request_background_quietly();
                return;
            }
            // A scripted run can ask for a window size, to check the adaptive layout.
            if let Some((w, h)) = std::env::var("FERRY_SMOKE_SIZE").ok().and_then(|v| v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))) {
                window.unmaximize();
                window.set_default_size(w, h);
            }
            window.present();
            crate::debug!("window presented");
            if let Ok(profile) = std::env::var("FERRY_SMOKE") {
                crate::devtools::smoke::start(&window, profile);
            }
        }

        // s3:// links opened anywhere in the desktop show their location here.
        fn open(&self, files: &[gio::File], _hint: &str) {
            let window = self.obj().main_window();
            window.present();
            for file in files {
                let uri = file.uri();
                if uri.starts_with("s3://") {
                    window.open_s3_uri(&uri);
                }
            }
        }

        fn dbus_register(&self, connection: &gio::DBusConnection, object_path: &str) -> Result<(), glib::Error> {
            self.parent_dbus_register(connection, object_path)?;
            crate::search::register(connection);
            Ok(())
        }

        fn startup(&self) {
            self.parent_startup();
            // Everything cached (thumbnails, dragged and edited copies) may come from private buckets.
            let cache = glib::user_cache_dir().join("ferry");
            let _ = std::fs::create_dir_all(&cache);
            let _ = std::fs::set_permissions(&cache, std::os::unix::fs::PermissionsExt::from_mode(0o700));
            crate::transfers::drag_out::cleanup();
            crate::window::prune_thumbnails();
            crate::transfers::mount::forget_stale_bookmarks();
        }

        // Mounted folders must not outlive the process that serves them.
        fn shutdown(&self) {
            // Unfinished transfers are kept for the next start.
            for window in self.obj().windows() {
                if let Ok(win) = window.downcast::<Window>() {
                    win.queue().save();
                    if std::env::var_os("FERRY_SMOKE").is_none() { win.save_listings(); }
                }
            }
            crate::search::save();
            crate::transfers::mount::unmount_all();
            crate::transfers::external::cleanup();
            self.parent_shutdown();
        }


    }

    impl GtkApplicationImpl for Application {}
    impl AdwApplicationImpl for Application {}
}

glib::wrapper! {
    pub struct Application(ObjectSubclass<imp::Application>)
        @extends adw::Application, gtk::Application, gio::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl Application {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("application-id", config::APP_ID)
            .property("resource-base-path", "/io/github/mehmetnuri/Ferry")
            .property("flags", gio::ApplicationFlags::HANDLES_OPEN)
            .build()
    }

    fn setup_actions(&self) {
        let quit = gio::ActionEntry::builder("quit").activate(|app: &Self, _, _| app.quit()).build();
        let about = gio::ActionEntry::builder("about").activate(|app: &Self, _, _| app.show_about()).build();
        let preferences = gio::ActionEntry::builder("preferences").activate(|app: &Self, _, _| app.show_preferences()).build();
        let show = gio::ActionEntry::builder("show-window").activate(|app: &Self, _, _| app.main_window().present()).build();
        let transfers = gio::ActionEntry::builder("show-transfers").activate(|app: &Self, _, _| app.main_window().show_transfers()).build();
        let retry = gio::ActionEntry::builder("retry-failed").activate(|app: &Self, _, _| {
            let window = app.main_window();
            let retried = window.queue().retry_failed();
            if retried > 0 { window.show_transfers(); }
        }).build();
        // Opens a local folder from a notification button.
        let open_folder = gio::ActionEntry::builder("open-folder").parameter_type(Some(glib::VariantTy::STRING)).activate(|_: &Self, _, param| {
            if let Some(path) = param.and_then(|p| p.get::<String>()) {
                let uri = gio::File::for_path(path).uri();
                let _ = gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE);
            }
        }).build();
        self.add_action_entries([quit, about, preferences, show, transfers, retry, open_folder]);
        self.set_accels_for_action("app.preferences", &["<Control>comma"]);

        self.set_accels_for_action("app.quit", &["<Control>q"]);
        self.set_accels_for_action("win.close-tab", &["<Control>w"]);
        self.set_accels_for_action("win.new-tab", &["<Control>t"]);
        self.set_accels_for_action("win.new-connection", &["<Control>n"]);
        self.set_accels_for_action("win.refresh", &["F5", "<Control>r"]);
        self.set_accels_for_action("win.go-up", &["<Alt>Up"]);
        self.set_accels_for_action("win.upload-files", &["<Control>u"]);
        self.set_accels_for_action("win.new-folder", &["<Control><Shift>n"]);
        self.set_accels_for_action("win.undo", &["<Control>z"]);
        self.set_accels_for_action("win.select-matching", &["<Control>s"]);
        self.set_accels_for_action("win.show-hidden", &["<Control>h"]);
        self.set_accels_for_action("win.zoom-in", &["<Control>plus", "<Control>equal", "<Control>KP_Add"]);
        self.set_accels_for_action("win.zoom-out", &["<Control>minus", "<Control>KP_Subtract"]);
        self.set_accels_for_action("win.zoom-reset", &["<Control>0", "<Control>KP_0"]);
        self.set_accels_for_action("win.search-everywhere", &["<Control><Shift>f"]);
        self.set_accels_for_action("win.properties", &["<Alt>Return", "<Control>i"]);
        self.set_accels_for_action("win.search", &["<Control>f"]);
        self.set_accels_for_action("win.back", &["<Alt>Left"]);
        self.set_accels_for_action("win.forward", &["<Alt>Right"]);
        self.set_accels_for_action("win.open-location", &["<Control>l"]);
        self.set_accels_for_action("win.view-list", &["<Control>1"]);
        self.set_accels_for_action("win.view-grid", &["<Control>2"]);
        self.set_accels_for_action("win.show-help-overlay", &["<Control>question"]);
    }

    /// The single main window; hidden while running in the background.
    fn main_window(&self) -> Window {
        self.windows().into_iter().find_map(|w| w.downcast::<Window>().ok()).unwrap_or_else(|| Window::new(self))
    }

    fn show_preferences(&self) {
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::builder().title(tr("General")).icon_name("preferences-system-symbolic").build();
        let behavior = adw::PreferencesGroup::builder().title(tr("Behavior")).build();
        let background = adw::SwitchRow::builder().title(tr("Run in Background"))
            .subtitle(tr("Closing the window keeps transfers and scheduled backups running")).build();
        let notify = adw::SwitchRow::builder().title(tr("Notifications"))
            .subtitle(tr("Show a notification when transfers finish while the window is not focused")).build();
        let autostart = adw::SwitchRow::builder().title(tr("Start at Login"))
            .subtitle(tr("Runs in the background without a window, so scheduled and watched backups keep working")).build();
        behavior.add(&background);
        behavior.add(&autostart);
        behavior.add(&notify);
        let transfers = adw::PreferencesGroup::builder().title(tr("Transfers")).build();
        let limit = adw::SpinRow::builder().title(tr("Simultaneous Transfers")).subtitle(tr("Files uploaded or downloaded at the same time"))
            .adjustment(&gtk::Adjustment::new(4.0, 1.0, 16.0, 1.0, 4.0, 0.0)).build();
        transfers.add(&limit);
        let ask = adw::SwitchRow::builder().title(tr("Ask Where to Save Downloads")).build();
        transfers.add(&ask);
        let folder_row = adw::ActionRow::builder().title(tr("Download Folder")).subtitle_selectable(true).build();
        let choose = gtk::Button::builder().icon_name("folder-open-symbolic").tooltip_text(tr("Choose Folder…")).valign(gtk::Align::Center).css_classes(["flat"]).build();
        folder_row.add_suffix(&choose);
        folder_row.set_activatable_widget(Some(&choose));
        let show_folder = glib::clone!(#[weak] folder_row, move || {
            let path = crate::window::download_folder();
            folder_row.set_subtitle(&glib::markup_escape_text(&path.display().to_string()));
        });
        show_folder();
        choose.connect_clicked(glib::clone!(#[weak] dialog, #[strong] show_folder, move |_| {
            let chooser = gtk::FileDialog::builder().title(tr("Download folder")).modal(true)
                .initial_folder(&gio::File::for_path(crate::window::download_folder())).build();
            let root = dialog.root().and_downcast::<gtk::Window>();
            let show_folder = show_folder.clone();
            glib::spawn_future_local(async move {
                if let Ok(folder) = chooser.select_folder_future(root.as_ref()).await && let Some(path) = folder.path() {
                    let _ = settings().set_string("download-folder", &path.display().to_string());
                    show_folder();
                }
            });
        }));
        transfers.add(&folder_row);
        let skip = adw::EntryRow::builder().title(tr("Leave Out When Uploading Folders")).show_apply_button(true)
            .text(settings().strv("upload-skip").iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ")).build();
        skip.set_tooltip_text(Some(&tr("Comma-separated names; * and ? are wildcards, for example node_modules, *.tmp")));
        skip.connect_apply(|row| {
            let names: Vec<String> = row.text().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            let _ = settings().set_strv("upload-skip", names.iter().map(String::as_str).collect::<Vec<_>>());
        });
        transfers.add(&skip);
        page.add(&behavior);
        page.add(&transfers);
        dialog.add(&page);
        let settings = settings();
        settings.bind("run-in-background", &background, "active").build();
        settings.bind("notify-transfers", &notify, "active").build();
        settings.bind("autostart", &autostart, "active").build();
        autostart.connect_active_notify(|row| set_autostart(row.is_active()));
        settings.bind("transfer-limit", &limit, "value").build();
        settings.bind("ask-download-folder", &ask, "active").build();
        settings.bind("ask-download-folder", &folder_row, "sensitive").invert_boolean().build();
        dialog.present(self.active_window().as_ref());
    }

    fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("Ferry")
            .application_icon(config::APP_ID)
            .developer_name("Mehmet Nuri Öztürk")
            .version(config::VERSION)
            .comments(tr("Browse and manage S3-compatible storage"))
            .website("https://github.com/MehmetNuri/s3_browser")
            .issue_url("https://github.com/MehmetNuri/s3_browser/issues")
            .license_type(gtk::License::Apache20)
            .copyright("© 2026 Mehmet Nuri Öztürk")
            .developers(vec!["Mehmet Nuri Öztürk"])
            .translator_credits(tr("translator-credits"))
            .release_notes_version(config::VERSION)
            .release_notes(format!("<p>{}</p><ul><li>{}</li><li>{}</li><li>{}</li><li>{}</li><li>{}</li><li>{}</li><li>{}</li><li>{}</li><li>{}</li></ul>",
                tr("First GNOME edition, rewritten in Rust with GTK 4 and libadwaita."),
                tr("Transfer queue with priorities, pause, speed limit and resume after restart"),
                tr("Interrupted uploads and downloads continue where they stopped"),
                tr("Recent objects, sorting, ZIP downloads and s3:// links"),
                tr("Previews of CSV tables, ZIP contents and highlighted code; copy and paste with GNOME Files"),
                tr("Quick preview of images, videos, audio and text"),
                tr("Copy, cut and paste, moving by drag and drop, undo"),
                tr("Search from the GNOME Shell overview"),
                tr("Share links with QR codes and upload links"),
                tr("Bucket mounting, permissions, Object Lock, CloudFront")))
            .debug_info(debug_info())
            .debug_info_filename("ferry-debug.txt")
            .build();
        about.present(self.active_window().as_ref());
    }
}

/// Progress and count on the application icon in docks that show them (Dash to Dock,
/// Ubuntu Dock), through the LauncherEntry interface; sent only when they change.
pub fn set_launcher_progress(active: usize, fraction: f64) {
    thread_local! {
        static LAST: Cell<(usize, i32)> = const { Cell::new((0, -1)) };
    }
    let shown = (active, (fraction * 100.0).round() as i32);
    if LAST.with(|l| l.replace(shown)) == shown { return; }
    let Some(app) = gio::Application::default() else { return };
    let Some(connection) = app.dbus_connection() else { return };
    let properties = glib::VariantDict::new(None);
    properties.insert("progress", fraction);
    properties.insert("progress-visible", active > 0);
    properties.insert("count", active as i64);
    properties.insert("count-visible", active > 0);
    let uri = format!("application://{}.desktop", config::APP_ID);
    let _ = connection.emit_signal(None, "/io/github/mehmetnuri/Ferry/launcher", "com.canonical.Unity.LauncherEntry", "Update",
        Some(&glib::Variant::tuple_from_iter([uri.to_variant(), properties.end()])));
}

thread_local! {
    static BACKGROUND_ASKED: Cell<bool> = const { Cell::new(false) };
}

/// Asks the desktop once to let the application run without a window. GNOME
/// then lists it under "Background Apps" in the system menu (for applications
/// installed as Flatpak), where it can be reopened or quit. The first time the
/// window goes away a notification also says so and offers to quit.
/// Set by "--hidden" on the command line.
pub static START_HIDDEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Like request_background, without the notification; used when started at login.
fn request_background_quietly() {
    BACKGROUND_ASKED.with(|asked| asked.set(true));
    let reason = tr("Scheduled backups run in the background");
    crate::runtime::runtime().spawn(async move {
        let _ = ashpd::desktop::background::Background::request().reason(reason.as_str()).send().await;
    });
}

/// Starts the application hidden at login, or stops doing so. Inside Flatpak
/// the background portal handles it; otherwise an XDG autostart entry is written.
pub fn set_autostart(enabled: bool) {
    if std::path::Path::new("/.flatpak-info").exists() {
        let reason = tr("Scheduled backups run in the background");
        crate::runtime::runtime().spawn(async move {
            let request = ashpd::desktop::background::Background::request().reason(reason.as_str()).auto_start(enabled)
                .command(["ferry", "--hidden"]).dbus_activatable(false);
            if let Err(error) = request.send().await.and_then(|r| r.response()) {
                eprintln!("Autostart could not be changed: {error}");
            }
        });
        return;
    }
    let path = glib::user_config_dir().join("autostart").join(format!("{}.desktop", config::APP_ID));
    if !enabled {
        let _ = std::fs::remove_file(path);
        return;
    }
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "ferry".into());
    let entry = format!("[Desktop Entry]\nType=Application\nName=Ferry\nExec={exe} --hidden\nIcon={}\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n", config::APP_ID);
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    let _ = std::fs::write(path, entry);
}

pub fn request_background() {
    if BACKGROUND_ASKED.with(|asked| asked.replace(true)) {
        return;
    }
    if let Some(app) = gio::Application::default() {
        let notification = gio::Notification::new(&tr("Ferry Is Running in the Background"));
        notification.set_body(Some(&tr("Transfers and scheduled backups continue. Open Ferry again to show the window.")));
        notification.set_icon(&gio::ThemedIcon::new(config::APP_ID));
        notification.set_default_action("app.show-window");
        notification.add_button(&tr("Quit"), "app.quit");
        app.send_notification(Some("background"), &notification);
    }
    let reason = tr("Transfers and scheduled backups continue while the window is closed");
    crate::runtime::runtime().spawn(async move {
        let request = ashpd::desktop::background::Background::request().reason(reason.as_str()).auto_start(false).dbus_activatable(false);
        if let Err(error) = request.send().await.and_then(|r| r.response()) {
            eprintln!("Background permission was not granted: {error}");
        }
    });
}

/// The status line GNOME shows for the application in the background apps menu.
pub fn set_background_status(status: &str) {
    let message = if status.is_empty() { tr("Idle") } else { status.to_string() };
    crate::runtime::runtime().spawn(async move {
        if let Ok(proxy) = ashpd::desktop::background::BackgroundProxy::new().await {
            let _ = proxy.set_status(ashpd::desktop::background::SetStatusOptions::default().set_message(&message)).await;
        }
    });
}

pub fn notify_transfers(done: u32, failed: u32) {
    if !settings().boolean("notify-transfers") || done + failed == 0 {
        return;
    }
    let Some(app) = gio::Application::default() else { return };
    let (title, body) = if failed > 0 {
        (tr("Transfers Failed"), trf("{n} transfers failed, {d} finished", &[("n", &failed.to_string()), ("d", &done.to_string())]))
    } else {
        (tr("Transfers Finished"), trn("{n} transfer finished", "{n} transfers finished", &[("n", &done.to_string())]))
    };
    let notification = gio::Notification::new(&title);
    notification.set_body(Some(&body));
    notification.set_icon(&gio::ThemedIcon::new(config::APP_ID));
    notification.set_default_action("app.show-transfers");
    if failed > 0 {
        notification.set_priority(gio::NotificationPriority::High);
        notification.add_button(&tr("Retry"), "app.retry-failed");
        notification.add_button(&tr("Show Transfers"), "app.show-transfers");
    }
    app.send_notification(Some("transfers"), &notification);
}

pub fn notify_with_folder(title: &str, body: &str, folder: &std::path::Path) {
    if !settings().boolean("notify-transfers") { return; }
    let Some(app) = gio::Application::default() else { return };
    let notification = gio::Notification::new(title);
    notification.set_body(Some(body));
    notification.set_icon(&gio::ThemedIcon::new(config::APP_ID));
    let target = folder.display().to_string().to_variant();
    notification.set_default_action_and_target_value("app.open-folder", Some(&target));
    notification.add_button_with_target_value(&tr("Open Folder"), "app.open-folder", Some(&target));
    app.send_notification(Some("download"), &notification);
}

/// Details for bug reports, shown under Troubleshooting in the About dialog. No credentials.
fn debug_info() -> String {
    let profiles = crate::profile::load();
    let providers: Vec<String> = profiles.iter().map(|p| format!("{} ({})", p.provider, if crate::s3::endpoint_of(p).is_empty() { "default endpoint" } else { "custom endpoint" })).collect();
    format!(
        "Ferry {}\nGTK {}.{}.{}\nlibadwaita {}.{}.{}\nSession: {}\nDesktop: {}\nFlatpak: {}\nLanguage: {}\nConnections: {}\nQueue limit: {}, speed limit: {} KB/s\n",
        config::VERSION,
        gtk::major_version(), gtk::minor_version(), gtk::micro_version(),
        adw::major_version(), adw::minor_version(), adw::micro_version(),
        std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        std::path::Path::new("/.flatpak-info").exists(),
        glib::language_names().first().map(|s| s.to_string()).unwrap_or_default(),
        if providers.is_empty() { "none".to_string() } else { providers.join(", ") },
        settings().int("transfer-limit"), settings().int("bandwidth-kbps"),
    )
}

pub fn notify_backup_failed(name: &str, error: &str) {
    let Some(app) = gio::Application::default() else { return };
    let notification = gio::Notification::new(&trf("Backup “{name}” Failed", &[("name", name)]));
    notification.set_body(Some(error));
    notification.set_icon(&gio::ThemedIcon::new(config::APP_ID));
    notification.set_priority(gio::NotificationPriority::High);
    notification.set_default_action("app.show-transfers");
    app.send_notification(Some("backup-failed"), &notification);
}
