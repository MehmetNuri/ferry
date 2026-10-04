pub mod actions;
pub mod tabs;
pub mod vault;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use std::rc::Rc;

use crate::pages::analyzer::Analyzer;
use crate::dialogs::bucket;
use crate::i18n::{tr, trf, trn};
use crate::profile::{self, Profile};
use crate::dialogs::connection;
use crate::runtime::bg;
use crate::s3::{self, BucketEntry, Entry, ObjectInfo, S3};
use crate::settings::settings;

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/mehmetnuri/Ferry/ui/window.ui")]
    pub struct Window {
        #[template_child] pub toasts: TemplateChild<adw::ToastOverlay>,
        #[template_child] pub split_view: TemplateChild<adw::NavigationSplitView>,
        #[template_child] pub content_page: TemplateChild<adw::NavigationPage>,
        #[template_child] pub profiles_list: TemplateChild<gtk::ListBox>,
        #[template_child] pub views_list: TemplateChild<gtk::ListBox>,
        #[template_child] pub content_title: TemplateChild<adw::WindowTitle>,
        #[template_child] pub profiles_placeholder: TemplateChild<gtk::Label>,
        #[template_child] pub favorites_section: TemplateChild<gtk::Box>,
        #[template_child] pub favorites_list: TemplateChild<gtk::ListBox>,
        #[template_child] pub favorite_button: TemplateChild<gtk::Button>,
        #[template_child] pub view_stack: TemplateChild<adw::ViewStack>,
        #[template_child] pub transfers_button: TemplateChild<gtk::ToggleButton>,
        #[template_child] pub buckets_section: TemplateChild<gtk::Box>,
        #[template_child] pub buckets_list: TemplateChild<gtk::ListBox>,
        #[template_child] pub buckets_placeholder: TemplateChild<gtk::Label>,
        #[template_child] pub browser_stack: TemplateChild<gtk::Stack>,
        #[template_child] pub list_stack: TemplateChild<gtk::Stack>,
        #[template_child] pub column_view: TemplateChild<gtk::ColumnView>,
        #[template_child] pub path_box: TemplateChild<gtk::Box>,
        #[template_child] pub search_bar: TemplateChild<gtk::SearchBar>,
        #[template_child] pub search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child] pub search_banner: TemplateChild<adw::Banner>,
        #[template_child] pub vault_banner: TemplateChild<adw::Banner>,
        #[template_child] pub buckets_heading: TemplateChild<gtk::Label>,
        #[template_child] pub add_bucket_button: TemplateChild<gtk::MenuButton>,
        /// The Cryptomator vault of the open folder: its folder key and whether it is unlocked.
        pub vault_here: RefCell<Option<(String, bool)>>,
        #[template_child] pub details_split: TemplateChild<adw::OverlaySplitView>,
        #[template_child] pub details_bin: TemplateChild<adw::Bin>,
        #[template_child] pub more_revealer: TemplateChild<gtk::Revealer>,
        #[template_child] pub selection_bar: TemplateChild<gtk::ActionBar>,
        #[template_child] pub selection_label: TemplateChild<gtk::Label>,
        #[template_child] pub status_label: TemplateChild<gtk::Label>,
        #[template_child] pub upload_button: TemplateChild<adw::SplitButton>,
        #[template_child] pub transfers_content: TemplateChild<gtk::Box>,
        #[template_child] pub transfers_icon: TemplateChild<gtk::Image>,
        #[template_child] pub transfers_sheet: TemplateChild<adw::BottomSheet>,
        #[template_child] pub transfers_bar: TemplateChild<gtk::Box>,
        #[template_child] pub transfers_bar_pie: TemplateChild<adw::Bin>,
        #[template_child] pub transfers_bar_title: TemplateChild<gtk::Label>,
        #[template_child] pub transfers_bar_subtitle: TemplateChild<gtk::Label>,
        #[template_child] pub grid_view: TemplateChild<gtk::GridView>,
        #[template_child] pub list_toggle: TemplateChild<gtk::ToggleButton>,
        #[template_child] pub grid_toggle: TemplateChild<gtk::ToggleButton>,
        #[template_child] pub view_toggle: TemplateChild<gtk::Box>,
        #[template_child] pub drop_overlay: TemplateChild<adw::StatusPage>,
        #[template_child] pub error_page: TemplateChild<adw::StatusPage>,
        #[template_child] pub transfers_bin: TemplateChild<adw::Bin>,
        #[template_child] pub analyzer_bin: TemplateChild<adw::Bin>,
        #[template_child] pub backups_bin: TemplateChild<adw::Bin>,
        #[template_child] pub compatibility_bin: TemplateChild<adw::Bin>,
        #[template_child] pub recent_bin: TemplateChild<adw::Bin>,
        #[template_child] pub history_box: TemplateChild<gtk::Box>,
        #[template_child] pub details_close: TemplateChild<gtk::Button>,
        #[template_child] pub tab_view: TemplateChild<adw::TabView>,
        #[template_child] pub tab_bar: TemplateChild<adw::TabBar>,
        #[template_child] pub selection_headers: TemplateChild<gtk::Button>,
        #[template_child] pub selection_class: TemplateChild<gtk::Button>,
        #[template_child] pub selection_copy: TemplateChild<gtk::Button>,
        #[template_child] pub back_button: TemplateChild<gtk::Button>,
        #[template_child] pub forward_button: TemplateChild<gtk::Button>,
        #[template_child] pub filter_button: TemplateChild<gtk::MenuButton>,
        #[template_child] pub offline_banner: TemplateChild<adw::Banner>,
        #[template_child] pub search_button: TemplateChild<gtk::ToggleButton>,
        #[template_child] pub location_button: TemplateChild<gtk::MenuButton>,
        #[template_child] pub welcome_providers: TemplateChild<gtk::FlowBox>,
        #[template_child] pub clear_filters_button: TemplateChild<gtk::Button>,

        pub profiles: RefCell<Vec<Profile>>,
        pub buckets: RefCell<Vec<BucketEntry>>,
        pub client: RefCell<Option<S3>>,
        pub bucket: RefCell<String>,
        pub prefix: RefCell<String>,
        pub next_token: RefCell<String>,
        /// Text of the search shown in the banner, when results replace the folder.
        pub search: RefCell<Option<String>>,
        pub store: RefCell<Option<gio::ListStore>>,
        pub filter: RefCell<Option<gtk::CustomFilter>>,
        pub selection: RefCell<Option<gtk::MultiSelection>>,
        /// Increases with every navigation, so late answers for an old folder are dropped.
        pub generation: Cell<u64>,
        pub queue: RefCell<Option<crate::transfers::queue::Queue>>,
        /// Active jobs at the last queue change, to notice when the queue empties.
        pub last_active: Cell<usize>,
        pub analyzer: RefCell<Option<Rc<Analyzer>>>,
        /// The object shown in the details panel.
        pub info: RefCell<Option<ObjectInfo>>,
        pub favorites: RefCell<Vec<(String, String)>>,
        /// Transfers that finished since the window was last focused, for the notification.
        pub finished: Cell<(u32, u32)>,
        /// Running transfers: total bytes and shared progress.
        pub header_pie: RefCell<Option<crate::widgets::pie::Pie>>,
        pub bar_pie: RefCell<Option<crate::widgets::pie::Pie>>,
        pub thumbnails: RefCell<std::collections::HashMap<String, gdk::Texture>>,
        /// Cells waiting for a thumbnail that is being fetched.
        pub thumbnail_pending: RefCell<std::collections::HashMap<String, Vec<(glib::WeakRef<gtk::Widget>, fn(&gtk::Widget, &gdk::Texture))>>>,
        pub mounts_section: RefCell<Option<(gtk::Box, gtk::ListBox)>>,
        /// The object whose details are shown or being loaded.
        pub details_key: RefCell<String>,
        /// Locations visited before and after the current one, for Back and Forward.
        pub history_back: RefCell<Vec<(String, String)>>,
        pub history_forward: RefCell<Vec<(String, String)>>,
        pub location: RefCell<(String, String)>,
        /// Tabs with the place each one keeps, and the tab the browser is in now.
        pub tabs: RefCell<Vec<(adw::TabPage, crate::window::tabs::TabState)>>,
        pub current_tab: RefCell<Option<adw::TabPage>>,
        pub history_moving: Cell<bool>,
        /// Objects copied or cut with Ctrl+C / Ctrl+X.
        pub clipboard: RefCell<Option<crate::window::actions::Clip>>,
        pub show_hidden: Cell<bool>,
        /// The toast whose Undo button Ctrl+Z presses, while it is shown.
        pub undo_toast: RefCell<Option<adw::Toast>>,
        /// Type, size and date filters of the search bar.
        pub type_filter: Cell<u32>,
        pub size_filter: Cell<u32>,
        pub date_filter: Cell<u32>,
        /// Cookie of the suspend inhibitor while transfers run.
        pub inhibit_cookie: Cell<u32>,
        /// The queue was paused because the network went away.
        pub auto_paused: Cell<bool>,
        pub loading_more: Cell<bool>,
        /// Another refresh of the same folder was asked for while one was running.
        pub refresh_again: Cell<bool>,
        /// First pages of recently listed folders, shown at once when a folder is opened again.
        pub listing_cache: RefCell<std::collections::HashMap<String, (Vec<Entry>, String, std::time::Instant)>>,
        /// Cache key of the folder the list shows.
        pub listed: RefCell<String>,
        /// True while a folder listing is on its way.
        pub loading: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Window {
        const NAME: &'static str = "FerryWindow";
        type Type = super::Window;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Window {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup();
        }
    }

    impl WidgetImpl for Window {}

    impl WindowImpl for Window {
        // Closing keeps the application running in the background when allowed;
        // otherwise running transfers are confirmed before they are stopped.
        fn close_request(&self) -> glib::Propagation {
            let win = self.obj();
            win.save_window_state();
            if settings().boolean("run-in-background") {
                win.set_visible(false);
                crate::application::request_background();
                return glib::Propagation::Stop;
            }
            if win.queue().summary().active() > 0 {
                let win = win.clone();
                glib::spawn_future_local(async move {
                    if win.confirm(&tr("Stop Transfers?"), &tr("Transfers are still running. They stop now; uploads and downloads continue where they stopped the next time Ferry starts."), &tr("Quit")).await
                        && let Some(app) = win.application() { app.quit(); }
                });
                return glib::Propagation::Stop;
            }
            self.parent_close_request()
        }
    }
    impl ApplicationWindowImpl for Window {}
    impl AdwApplicationWindowImpl for Window {}
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

#[derive(Clone, Copy)]
enum Conflict {
    Replace,
    /// Replaces only objects older than the local file, as a sync would.
    ReplaceOlder,
    Skip,
    KeepBoth,
}

fn modified_secs(path: &std::path::Path) -> i64 {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub(crate) fn format_size(bytes: i64) -> String {
    glib::format_size(bytes.max(0) as u64).to_string()
}

/// A toast showing `text` as it is: object names and server messages are not markup.
pub(crate) fn plain_toast(text: &str) -> adw::Toast {
    let toast = adw::Toast::new(text);
    toast.set_use_markup(false);
    toast
}

/// A full date and time in the user's locale, following the 24- or 12-hour clock chosen
/// in GNOME Settings, as the properties of GNOME Files show it.
pub(crate) fn full_time(secs: i64) -> String {
    let Ok(time) = glib::DateTime::from_unix_local(secs) else { return String::new() };
    let twelve = gio::SettingsSchemaSource::default().and_then(|s| s.lookup("org.gnome.desktop.interface", true)).is_some()
        && gio::Settings::new("org.gnome.desktop.interface").string("clock-format") == "12h";
    // Translators: a full date and time; see the GLib DateTime format codes.
    let format = if twelve { crate::i18n::tr("%-e %B %Y, %-l:%M %p") } else { crate::i18n::tr("%-e %B %Y, %H:%M") };
    time.format(&format).map(|s| s.to_string()).unwrap_or_default()
}

pub(crate) fn format_time(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    let Ok(time) = glib::DateTime::from_unix_local(secs) else { return String::new() };
    let now = glib::DateTime::now_local().unwrap();
    let format = if time.ymd() == now.ymd() { "%H:%M" } else if time.year() == now.year() { "%e %b %H:%M" } else { "%e %b %Y" };
    time.format(format).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Compares names as people expect: case does not matter and numbers compare by
/// value, so "photo 2" comes before "photo 10". Pure Rust, fast enough for huge folders.
pub(crate) fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let mut left = String::new();
                while let Some(c) = x.peek().copied().filter(char::is_ascii_digit) { left.push(c); x.next(); }
                let mut right = String::new();
                while let Some(d) = y.peek().copied().filter(char::is_ascii_digit) { right.push(d); y.next(); }
                let (l, r) = (left.trim_start_matches('0'), right.trim_start_matches('0'));
                let order = l.len().cmp(&r.len()).then_with(|| l.cmp(r));
                if order != Ordering::Equal { return order; }
            }
            (Some(c), Some(d)) => {
                let order = c.to_lowercase().cmp(d.to_lowercase());
                if order != Ordering::Equal { return order; }
                x.next();
                y.next();
            }
        }
    }
}

pub(crate) fn entry_of(object: &glib::Object) -> Entry {
    object.downcast_ref::<glib::BoxedAnyObject>().unwrap().borrow::<Entry>().clone()
}

pub(crate) fn icon_for(entry: &Entry) -> gio::Icon {
    if entry.is_folder {
        return gio::ThemedIcon::new("folder-symbolic").upcast();
    }
    let (content_type, _) = gio::content_type_guess(Some(&entry.name), None::<&[u8]>);
    gio::content_type_get_symbolic_icon(&content_type)
}

/// The full-color icon of an object, for the grid, as GNOME Files shows them.
fn grid_icon_for(entry: &Entry) -> gio::Icon {
    if entry.is_folder {
        return gio::ThemedIcon::new("folder").upcast();
    }
    let (content_type, _) = gio::content_type_guess(Some(&entry.name), None::<&[u8]>);
    gio::content_type_get_icon(&content_type)
}

/// A sidebar row with a colored avatar, for connections.
fn avatar_row(text: &str) -> gtk::ListBoxRow {
    let row_box = gtk::Box::builder().spacing(10).margin_start(4).margin_end(6).build();
    let avatar = adw::Avatar::new(26, Some(text), true);
    avatar.add_css_class("sidebar-avatar");
    row_box.append(&avatar);
    row_box.append(&gtk::Label::builder().label(text).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build());
    gtk::ListBoxRow::builder().child(&row_box).tooltip_text(text).build()
}

pub(crate) fn is_image(name: &str) -> bool {
    let lower = name.to_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".svg", ".avif", ".tif", ".tiff"].iter().any(|ext| lower.ends_with(ext))
}

/// A sidebar row with an icon and a label, the way GNOME Files draws places.
fn thumbnail_dir() -> std::path::PathBuf {
    glib::user_cache_dir().join("ferry").join("thumbnails")
}

fn thumbnail_path(identity: &str) -> std::path::PathBuf {
    use sha2::Digest;
    let dir = thumbnail_dir();
    if !dir.is_dir() {
        let _ = std::fs::create_dir_all(&dir);
        // Pictures of private buckets stay readable only by the user.
        let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    }
    let hash: String = sha2::Sha256::digest(identity.as_bytes()).iter().take(16).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{hash}.png"))
}

/// Removes thumbnails not used for a month; called at startup.
pub(crate) fn prune_thumbnails() {
    std::thread::spawn(|| {
        let Ok(entries) = std::fs::read_dir(thumbnail_dir()) else { return };
        let month = std::time::Duration::from_secs(30 * 86_400);
        for entry in entries.flatten() {
            let old = entry.metadata().ok().and_then(|m| m.accessed().or_else(|_| m.modified()).ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age > month);
            if old { let _ = std::fs::remove_file(entry.path()); }
        }
    });
}

/// Where downloads go when the user does not choose each time.
pub(crate) fn download_folder() -> std::path::PathBuf {
    let chosen = settings().string("download-folder");
    if !chosen.is_empty() && std::path::Path::new(chosen.as_str()).is_dir() {
        return chosen.as_str().into();
    }
    glib::user_special_dir(glib::UserDirectory::Downloads).unwrap_or_else(glib::home_dir)
}

/// Shell-style wildcards: `*` for any run of characters, `?` for exactly one.
pub(crate) fn wildcard_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Actions that only S3 services offer.
const S3_ONLY: &[&str] = &["link-bucket", "new-bucket", "bucket-settings", "bucket-settings-for", "deleted-objects", "upload-link", "copy-link", "headers-selected",
    "storage-class-selected", "object-versions", "object-headers", "object-tags", "object-permissions", "copy-cli"];

/// Whether a listing cache key (profile, bucket, prefix) is inside an unlocked vault.
fn cache_key_in_vault(key: &str) -> bool {
    let mut parts = key.split('\u{0}');
    let (Some(profile), Some(bucket), Some(prefix)) = (parts.next(), parts.next(), parts.next()) else { return false };
    crate::s3::vault::find(profile, bucket, prefix).is_some()
}

fn listings_path() -> std::path::PathBuf {
    glib::user_cache_dir().join("ferry").join("listings.json")
}

/// Grid item sizes: picture, icon, cell width and label width in characters.
const GRID_SIZES: [(i32, i32, i32, i32); 4] = [(64, 48, 88, 11), (96, 64, 112, 14), (144, 96, 164, 20), (200, 128, 220, 26)];

/// The views of the content area, in the order of their sidebar rows.
const VIEWS: [&str; 5] = ["browser", "recent", "analyzer", "backups", "compatibility"];

fn view_index(name: Option<&str>) -> i32 {
    VIEWS.iter().position(|v| Some(*v) == name).unwrap_or(0) as i32
}

fn sidebar_row(icon: &str, text: &str) -> gtk::ListBoxRow {
    let row_box = gtk::Box::builder().spacing(12).margin_start(6).margin_end(6).build();
    row_box.append(&gtk::Image::from_icon_name(icon));
    row_box.append(&gtk::Label::builder().label(text).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build());
    gtk::ListBoxRow::builder().child(&row_box).tooltip_text(text).build()
}

impl Window {
    pub fn new(app: &impl IsA<gtk::Application>) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    pub(crate) fn toast(&self, text: &str) {
        let toast = plain_toast(text);
        toast.set_timeout(4);
        self.imp().toasts.add_toast(toast);
    }

    pub(crate) fn client(&self) -> Option<S3> {
        self.imp().client.borrow().clone()
    }

    pub(crate) fn open_bucket_name(&self) -> String {
        self.imp().bucket.borrow().clone()
    }

    pub(crate) fn current_client(&self) -> Option<S3> {
        self.client()
    }

    fn save_window_state(&self) {
        let settings = settings();
        let (width, height) = self.default_size();
        let _ = settings.set_int("window-width", width);
        let _ = settings.set_int("window-height", height);
        let _ = settings.set_boolean("window-maximized", self.is_maximized());
    }

    pub(crate) fn show_transfers(&self) {
        self.present();
        self.reveal_transfers();
    }

    fn setup(&self) {
        let imp = self.imp();
        if config_is_devel() {
            self.add_css_class("devel");
        }
        let state = settings();
        self.set_default_size(state.int("window-width"), state.int("window-height"));
        if state.boolean("window-maximized") {
            self.maximize();
        }
        self.setup_list();
        self.setup_actions();
        self.setup_drop();
        self.setup_transfers();
        self.setup_extras();
        self.setup_tabs();
        // The properties pane has a header bar with its own close button, as a sidebar pane should.
        imp.details_close.connect_clicked(glib::clone!(#[weak(rename_to = win)] self, move |_| {
            win.imp().details_split.set_show_sidebar(false);
            win.imp().details_key.replace(String::new());
        }));

        // The views of the content area are sidebar entries, as in GNOME Settings.
        for (icon, title) in [("folder-symbolic", tr("Browser")), ("document-open-recent-symbolic", tr("Recent")), ("drive-harddisk-symbolic", tr("Analyzer")), ("document-save-symbolic", tr("Backups")), ("emblem-ok-symbolic", tr("Compatibility"))] {
            imp.views_list.append(&sidebar_row(icon, &title));
        }
        imp.views_list.select_row(imp.views_list.row_at_index(0).as_ref());
        // The selection always shows the open view. Focus moving through the list
        // (for example when widgets elsewhere appear or disappear) must not change it.
        imp.views_list.connect_row_selected(glib::clone!(#[weak(rename_to = win)] self, move |list, row| {
            let expected = view_index(win.imp().view_stack.visible_child_name().as_deref());
            if row.map(|r| r.index()) != Some(expected) {
                list.select_row(list.row_at_index(expected).as_ref());
            }
        }));
        imp.views_list.connect_row_activated(glib::clone!(#[weak(rename_to = win)] self, move |_, row| {
            let name = VIEWS[row.index().clamp(0, VIEWS.len() as i32 - 1) as usize];
            win.imp().view_stack.set_visible_child_name(name);
            win.imp().split_view.set_show_content(true);
        }));
        imp.view_stack.connect_visible_child_name_notify(glib::clone!(#[weak(rename_to = win)] self, move |stack| {
            let imp = win.imp();
            let index = view_index(stack.visible_child_name().as_deref());
            imp.views_list.select_row(imp.views_list.row_at_index(index).as_ref());
            win.update_title();
            win.update_location_actions();
        }));

        let analyzer = Analyzer::new(self);
        imp.analyzer_bin.set_child(Some(&analyzer.widget));
        imp.analyzer.replace(Some(analyzer));
        crate::pages::backups::attach(self, &imp.backups_bin);
        crate::pages::compatibility::attach(self, &imp.compatibility_bin);
        crate::pages::recent::attach(self, &imp.recent_bin);
        crate::widgets::code_view::label_icon_buttons(self);
        self.setup_mounts();

        // Counting finished transfers starts over when the user comes back.
        self.connect_is_active_notify(|win| { if win.is_active() { win.imp().finished.set((0, 0)); } });
        imp.favorites_list.connect_row_activated(glib::clone!(#[weak(rename_to = win)] self, move |_, row| {
            let favorite = win.imp().favorites.borrow().get(row.index() as usize).cloned();
            if let Some((bucket, prefix)) = favorite {
                win.imp().view_stack.set_visible_child_name("browser");
                win.imp().bucket.replace(bucket);
                win.imp().browser_stack.set_visible_child_name("browser");
                win.navigate(&prefix);
                win.imp().split_view.set_show_content(true);
            }
        }));

        imp.profiles_list.connect_row_activated(glib::clone!(#[weak(rename_to = win)] self, move |_, row| {
            let profile = win.imp().profiles.borrow().get(row.index() as usize).cloned();
            if let Some(profile) = profile { win.connect(profile); }
        }));
        // Bucket and connection rows show what is open, whatever the keyboard focus does.
        imp.buckets_list.connect_row_selected(glib::clone!(#[weak(rename_to = win)] self, move |list, row| {
            let imp = win.imp();
            let open = imp.bucket.borrow().clone();
            let index = imp.buckets.borrow().iter().position(|b| b.name == open);
            if row.map(|r| r.index() as usize) != index {
                list.select_row(index.and_then(|i| list.row_at_index(i as i32)).as_ref());
            }
        }));
        imp.profiles_list.connect_row_selected(glib::clone!(#[weak(rename_to = win)] self, move |list, row| {
            let imp = win.imp();
            let active = imp.client.borrow().as_ref().map(|c| c.profile.id.clone());
            let index = active.and_then(|id| imp.profiles.borrow().iter().position(|p| p.id == id));
            if row.is_some() && row.map(|r| r.index() as usize) != index {
                list.select_row(index.and_then(|i| list.row_at_index(i as i32)).as_ref());
            }
        }));
        imp.buckets_list.connect_row_activated(glib::clone!(#[weak(rename_to = win)] self, move |_, row| {
            let name = win.imp().buckets.borrow().get(row.index() as usize).map(|b| b.name.clone());
            if let Some(name) = name {
                win.open_bucket(&name);
                win.imp().split_view.set_show_content(true);
            }
        }));
        imp.search_entry.connect_search_changed(glib::clone!(#[weak(rename_to = win)] self, move |_| {
            if let Some(filter) = win.imp().filter.borrow().as_ref() { filter.changed(gtk::FilterChange::Different); }
            win.update_status();
        }));
        imp.search_entry.connect_activate(glib::clone!(#[weak(rename_to = win)] self, move |entry| {
            let query = entry.text().trim().to_string();
            if !query.is_empty() { win.run_search(query); }
        }));
        self.setup_vault_banner();
        imp.search_banner.connect_button_clicked(glib::clone!(#[weak(rename_to = win)] self, move |_| {
            win.imp().search.replace(None);
            win.refresh();
        }));
        imp.search_bar.connect_entry(&*imp.search_entry);
        imp.search_bar.set_key_capture_widget(Some(self));

        // Provider tiles on the welcome page start a connection with that provider chosen.
        for preset in crate::profile::PRESETS.iter().filter(|p| ["aws", "supabase", "r2", "minio", "backblaze", "wasabi", "digitalocean", "gcs", "sftp", "ftp", "webdav", "nextcloud"].contains(&p.id)) {
            let tile = gtk::Button::builder().css_classes(["card", "provider-tile"]).build();
            let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
            // Initials come from the name without its parenthesized note, "Google Cloud Storage (HMAC)" → "GC".
            column.append(&adw::Avatar::new(40, Some(preset.label.split(" (").next().unwrap_or(preset.label)), true));
            column.append(&gtk::Label::builder().label(preset.label).wrap(true).justify(gtk::Justification::Center).max_width_chars(14).build());
            tile.set_child(Some(&column));
            let (id, path_style, region) = (preset.id, preset.path_style, preset.region);
            tile.connect_clicked(glib::clone!(#[weak(rename_to = win)] self, move |_| {
                win.edit_profile(Some(Profile { provider: id.into(), path_style, region: region.into(), ..Default::default() }));
            }));
            imp.welcome_providers.append(&tile);
        }

        self.load_profiles();
        self.update_location_actions();
        self.restore_session();
    }

    /// Opens the connection, bucket and folder that were open when the application last quit.
    fn restore_session(&self) {
        let state = settings();
        let (profile_id, bucket, prefix) = (state.string("last-profile").to_string(), state.string("last-bucket").to_string(), state.string("last-prefix").to_string());
        if profile_id.is_empty() || std::env::var_os("FERRY_SMOKE").is_some() { return; }
        if bucket.is_empty() {
            if let Some(profile) = profile::load().into_iter().find(|p| p.id == profile_id) { self.connect(profile); }
        } else {
            let key = if prefix.is_empty() { String::new() } else { prefix };
            self.open_object(profile_id, bucket.clone(), if key.is_empty() { String::new() } else { key });
        }
    }

    /// Remembers where the user is, for the next start.
    pub(crate) fn save_session(&self) {
        // Scripted test runs must not replace where the user really was.
        if std::env::var_os("FERRY_SMOKE").is_some() { return; }
        let imp = self.imp();
        let state = settings();
        let profile = imp.client.borrow().as_ref().map(|c| c.profile.id.clone()).unwrap_or_default();
        let _ = state.set_string("last-profile", &profile);
        let _ = state.set_string("last-bucket", &imp.bucket.borrow());
        let _ = state.set_string("last-prefix", &imp.prefix.borrow());
    }

    fn setup_list(&self) {
        let imp = self.imp();
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let filter = gtk::CustomFilter::new(glib::clone!(#[weak(rename_to = win)] self, #[upgrade_or] true, move |object| {
            let query = win.imp().search_entry.text().to_lowercase();
            let entry = entry_of(object);
            (query.is_empty() || win.imp().search.borrow().is_some() || entry.name.to_lowercase().contains(&query)) && win.passes_filters(&entry)
        }));
        let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
        // Folders stay above files; within each group the clicked column decides.
        let folders_first = gtk::CustomSorter::new(|a, b| entry_of(b).is_folder.cmp(&entry_of(a).is_folder).into());
        let sorter = gtk::MultiSorter::new();
        sorter.append(folders_first);
        sorter.append(imp.column_view.sorter().unwrap());
        let sorted = gtk::SortListModel::new(Some(filtered), Some(sorter));
        let selection = gtk::MultiSelection::new(Some(sorted));
        imp.column_view.set_model(Some(&selection));
        imp.column_view.set_enable_rubberband(true);
        imp.grid_view.set_enable_rubberband(true);
        selection.connect_selection_changed(glib::clone!(#[weak(rename_to = win)] self, move |_, _, _| {
            win.update_selection();
            win.details_for_selection();
        }));
        selection.connect_items_changed(glib::clone!(#[weak(rename_to = win)] self, move |_, _, _, _| {
            win.update_selection();
            win.update_empty_state();
        }));

        let name_factory = gtk::SignalListItemFactory::new();
        name_factory.connect_setup(glib::clone!(#[weak(rename_to = win)] self, move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = gtk::Box::builder().spacing(10).build();
            row.append(&gtk::Image::builder().pixel_size(24).build());
            row.append(&gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build());
            win.add_object_dnd(row.upcast_ref());
            // Resting the pointer on a folder lists it ahead of a click.
            let hover = gtk::EventControllerMotion::new();
            let pending: std::rc::Rc<Cell<Option<glib::SourceId>>> = Default::default();
            hover.connect_enter(glib::clone!(#[weak] win, #[weak] row, #[strong] pending, move |_, _, _| {
                let key = row.widget_name().to_string();
                if !key.ends_with('/') { return; }
                let id = glib::timeout_add_local_once(std::time::Duration::from_millis(180), glib::clone!(#[weak] win, #[strong] pending, move || { pending.set(None); win.prefetch(key); }));
                if let Some(old) = pending.replace(Some(id)) { old.remove(); }
            }));
            hover.connect_leave(move |_| { if let Some(id) = pending.take() { id.remove(); } });
            row.add_controller(hover);
            item.set_child(Some(&row));
        }));
        name_factory.connect_bind(glib::clone!(#[weak(rename_to = win)] self, move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let entry = entry_of(&item.item().unwrap());
            let row = item.child().and_downcast::<gtk::Box>().unwrap();
            let image = row.first_child().and_downcast::<gtk::Image>().unwrap();
            let label = image.next_sibling().and_downcast::<gtk::Label>().unwrap();
            image.set_from_gicon(&icon_for(&entry));
            label.set_text(&entry.name);
            row.set_tooltip_text(Some(&entry.key));
            row.set_widget_name(&entry.key);
            if !entry.is_folder && is_image(&entry.name) && entry.size > 0 && entry.size <= 3 * 1024 * 1024 {
                win.thumbnail(&entry, row.upcast_ref(), |row, texture| {
                    if let Some(image) = row.first_child().and_downcast::<gtk::Image>() { image.set_paintable(Some(texture)); }
                });
            }
        }));
        let text_factory = |dim: bool, value: fn(&Entry) -> String| {
            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(move |_, item| {
                let label = gtk::Label::builder().xalign(if dim { 0.0 } else { 1.0 }).build();
                label.add_css_class("dim-label");
                label.add_css_class("numeric");
                item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&label));
            });
            factory.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let label = item.child().and_downcast::<gtk::Label>().unwrap();
                label.set_text(&value(&entry_of(&item.item().unwrap())));
            });
            factory
        };
        let name_column = gtk::ColumnViewColumn::new(Some(&tr("Name")), Some(name_factory));
        name_column.set_expand(true);
        name_column.set_resizable(true);
        // Folders show how many items they hold once their content is known, as in GNOME Files.
        let size_factory = text_factory(false, |e| if e.is_folder { "—".into() } else { format_size(e.size) });
        size_factory.connect_bind(glib::clone!(#[weak(rename_to = win)] self, move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let entry = entry_of(&item.item().unwrap());
            if !entry.is_folder { return; }
            let key = win.cache_key(&win.imp().bucket.borrow(), &entry.key);
            if let Some((items, token, _)) = win.imp().listing_cache.borrow().get(&key) {
                let label = item.child().and_downcast::<gtk::Label>().unwrap();
                let count = items.len();
                let text = trn("{n} item", "{n} items", &[("n", &count.to_string())]);
                label.set_text(&if token.is_empty() { text } else { format!("{text}+") });
            }
        }));
        let size_column = gtk::ColumnViewColumn::new(Some(&tr("Size")), Some(size_factory));
        size_column.set_fixed_width(110);
        let modified_column = gtk::ColumnViewColumn::new(Some(&tr("Modified")), Some(text_factory(true, |e| format_time(e.modified))));
        modified_column.set_fixed_width(150);
        imp.column_view.append_column(&name_column);
        imp.column_view.append_column(&size_column);
        imp.column_view.append_column(&modified_column);
        let by = |f: fn(&Entry, &Entry) -> std::cmp::Ordering| gtk::CustomSorter::new(move |a, b| f(&entry_of(a), &entry_of(b)).into());
        name_column.set_sorter(Some(&by(|a, b| natural_cmp(&a.name, &b.name))));
        size_column.set_sorter(Some(&by(|a, b| a.size.cmp(&b.size))));
        modified_column.set_sorter(Some(&by(|a, b| a.modified.cmp(&b.modified))));
        imp.column_view.sort_by_column(Some(&name_column), gtk::SortType::Ascending);

        // On a phone-sized window the header keeps only what fits: no history buttons (Alt+Left
        // and the parent folder button remain), an icon-only Upload, and the name column alone.
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 500sp").unwrap());
        // Only one breakpoint applies at a time, so this one also collapses the split views.
        narrow.add_setter(&*imp.split_view, "collapsed", Some(&true.to_value()));
        narrow.add_setter(&*imp.details_split, "collapsed", Some(&true.to_value()));
        narrow.add_setter(&*imp.history_box, "visible", Some(&false.to_value()));
        narrow.add_setter(&*imp.upload_button, "icon-name", Some(&"transfer-upload-symbolic".to_value()));
        // The selection bar keeps Download and Delete; the rest stays in the menus.
        for button in [&*imp.selection_headers, &*imp.selection_class, &*imp.selection_copy] {
            narrow.add_setter(button, "visible", Some(&false.to_value()));
        }
        narrow.connect_apply(glib::clone!(#[weak] size_column, #[weak] modified_column, move |_| { size_column.set_visible(false); modified_column.set_visible(false); }));
        narrow.connect_unapply(glib::clone!(#[weak] size_column, #[weak] modified_column, #[weak(rename_to = win)] self, move |_| {
            size_column.set_visible(true);
            modified_column.set_visible(true);
            win.imp().upload_button.set_label(&tr("Upload"));
        }));
        self.add_breakpoint(narrow);

        imp.column_view.connect_activate(glib::clone!(#[weak(rename_to = win)] self, move |view, position| {
            let Some(object) = view.model().and_then(|m| m.item(position)) else { return };
            let entry = entry_of(&object);
            if entry.is_folder { win.navigate(&entry.key); } else { win.show_details(entry.key); }
        }));

        // Right click opens the context menu of the row under the pointer.
        let menu = gio::Menu::new();
        let open = gio::Menu::new();
        open.append(Some(&tr("Open in New Tab")), Some("win.open-in-new-tab"));
        open.append(Some(&tr("Quick Preview")), Some("win.preview"));
        open.append(Some(&tr("Open With Default Application")), Some("win.open-external"));
        open.append(Some(&tr("Open With…")), Some("win.open-with"));
        open.append(Some(&tr("Download…")), Some("win.download-selected"));
        open.append(Some(&tr("Download as Archive…")), Some("win.download-archive"));
        menu.append_section(None, &open);
        let edit = gio::Menu::new();
        edit.append(Some(&tr("Cut")), Some("win.cut"));
        edit.append(Some(&tr("Copy")), Some("win.copy"));
        edit.append(Some(&tr("Paste")), Some("win.paste"));
        edit.append(Some(&tr("Rename…")), Some("win.rename"));
        menu.append_section(None, &edit);
        let share = gio::Menu::new();
        share.append(Some(&tr("Share Link…")), Some("win.copy-link"));
        share.append(Some(&tr("Copy Public Address")), Some("win.copy-public-url"));
        let copy_as = gio::Menu::new();
        copy_as.append(Some(&tr("S3 Location")), Some("win.copy-location"));
        copy_as.append(Some(&tr("AWS CLI Command")), Some("win.copy-cli"));
        copy_as.append(Some(&tr("File Contents")), Some("win.copy-contents"));
        share.append_submenu(Some(&tr("Copy As")), &copy_as);
        menu.append_section(None, &share);
        let danger = gio::Menu::new();
        danger.append(Some(&tr("Delete")), Some("win.delete-selected"));
        menu.append_section(None, &danger);
        // Right click opens the menu of the item under the pointer, in the list and in the grid.
        // Empty space has the menu of the folder itself, as the background of GNOME Files.
        let background = gio::Menu::new();
        let create = gio::Menu::new();
        create.append(Some(&tr("New Folder…")), Some("win.new-folder"));
        create.append(Some(&tr("New Text File…")), Some("win.new-text-file"));
        background.append_section(None, &create);
        let place = gio::Menu::new();
        place.append(Some(&tr("Paste")), Some("win.paste"));
        place.append(Some(&tr("Upload Files…")), Some("win.upload-files"));
        place.append(Some(&tr("Select All")), Some("win.select-all"));
        background.append_section(None, &place);
        let folder = gio::Menu::new();
        folder.append(Some(&tr("Download as Archive…")), Some("win.download-archive"));
        folder.append(Some(&tr("Copy Location")), Some("win.copy-location"));
        background.append_section(None, &folder);
        for view in [imp.column_view.upcast_ref::<gtk::Widget>(), imp.grid_view.upcast_ref()] {
            let popover = gtk::PopoverMenu::from_model(Some(&menu));
            popover.set_parent(view);
            popover.set_has_arrow(false);
            popover.set_halign(gtk::Align::Start);
            let click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
            let (item_menu, background) = (menu.clone(), background.clone());
            click.connect_pressed(glib::clone!(#[weak(rename_to = win)] self, #[weak] popover, #[weak] view, move |_, _, x, y| {
                let imp = win.imp();
                let selection = imp.selection.borrow().clone().unwrap();
                match win.position_at(&view, x, y) {
                    Some(position) => {
                        if !selection.is_selected(position) { selection.select_item(position, true); }
                        popover.set_menu_model(Some(&item_menu));
                    }
                    None => {
                        selection.unselect_all();
                        popover.set_menu_model(Some(&background));
                    }
                }
                popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            }));
            view.add_controller(click);
            // A middle click opens a folder in a new tab, as in GNOME Files.
            let middle = gtk::GestureClick::builder().button(gdk::BUTTON_MIDDLE).build();
            middle.connect_pressed(glib::clone!(#[weak(rename_to = win)] self, #[weak] view, move |_, _, x, y| {
                let Some(position) = win.position_at(&view, x, y) else { return };
                let Some(object) = win.imp().selection.borrow().as_ref().and_then(|s| s.item(position)) else { return };
                let entry = entry_of(&object);
                if entry.is_folder { let bucket = win.imp().bucket.borrow().clone(); win.open_tab(bucket, entry.key); }
            }));
            view.add_controller(middle);
        }

        // Delete acts only while the list has the focus, so it never steals the key from text fields.
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string("Delete"), Some(gtk::NamedAction::new("win.delete-selected"))));
        shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string("<Control>a"), Some(gtk::NamedAction::new("win.select-all"))));
        shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string("BackSpace"), Some(gtk::NamedAction::new("win.go-up"))));
        imp.column_view.add_controller(shortcuts);

        self.setup_grid(&selection);
        imp.store.replace(Some(store));
        imp.filter.replace(Some(filter));
        imp.selection.replace(Some(selection));

        // The sort menu and the column headers set the same order; the menu also serves the grid.
        let order = |name: &str| -> (u32, gtk::SortType) {
            match name {
                "name-desc" => (0, gtk::SortType::Descending),
                "modified-desc" => (2, gtk::SortType::Descending),
                "modified-asc" => (2, gtk::SortType::Ascending),
                "size-desc" => (1, gtk::SortType::Descending),
                _ => (0, gtk::SortType::Ascending),
            }
        };
        let saved = settings().string("sort-order").to_string();
        let sort = gio::SimpleAction::new_stateful("sort", Some(glib::VariantTy::STRING), &saved.to_variant());
        let apply = glib::clone!(#[weak(rename_to = win)] self, move |name: &str| {
            let view = &win.imp().column_view;
            let (index, direction) = order(name);
            let column = view.columns().item(index).and_downcast::<gtk::ColumnViewColumn>();
            // Earlier columns would stay as secondary keys, each with its own arrow.
            view.sort_by_column(None, direction);
            view.sort_by_column(column.as_ref(), direction);
        });
        apply(&saved);
        sort.connect_change_state(move |action, value| {
            let Some(name) = value.and_then(|v| v.get::<String>()) else { return };
            action.set_state(&name.to_variant());
            let _ = settings().set_string("sort-order", &name);
            apply(&name);
        });
        self.add_action(&sort);
        if let Some(sorter) = imp.column_view.sorter().and_downcast::<gtk::ColumnViewSorter>() {
            sorter.connect_changed(glib::clone!(#[weak(rename_to = win)] self, move |sorter, _| {
                let columns = win.imp().column_view.columns();
                let index = sorter.primary_sort_column().and_then(|c| (0..columns.n_items()).find(|i| columns.item(*i).as_ref() == Some(c.upcast_ref())));
                let descending = sorter.primary_sort_order() == gtk::SortType::Descending;
                let name = match (index, descending) {
                    (Some(0), true) => "name-desc",
                    (Some(1), _) => "size-desc",
                    (Some(2), true) => "modified-desc",
                    (Some(2), false) => "modified-asc",
                    _ => "name-asc",
                };
                if let Some(action) = win.lookup_action("sort").and_downcast::<gio::SimpleAction>()
                    && action.state().and_then(|s| s.get::<String>()).as_deref() != Some(name) {
                    action.set_state(&name.to_variant());
                    let _ = settings().set_string("sort-order", name);
                }
            }));
        }

        if std::env::var_os("FERRY_SMOKE").is_none() { self.load_listings(); }

        // Names left out of folder uploads follow the preference.
        let load_skip = |settings: &gio::Settings| {
            if let Ok(mut patterns) = s3::UPLOAD_SKIP.write() {
                *patterns = settings.strv("upload-skip").iter().map(|s| s.to_string()).filter(|s| !s.trim().is_empty()).collect();
            }
        };
        load_skip(&settings());
        settings().connect_changed(Some("upload-skip"), move |s, _| load_skip(s));

        // Ctrl+H, as in GNOME Files.
        imp.show_hidden.set(settings().boolean("show-hidden"));
        let hidden = gio::SimpleAction::new_stateful("show-hidden", None, &imp.show_hidden.get().to_variant());
        hidden.connect_change_state(glib::clone!(#[weak(rename_to = win)] self, move |action, value| {
            let Some(on) = value.and_then(|v| v.get::<bool>()) else { return };
            action.set_state(&on.to_variant());
            win.imp().show_hidden.set(on);
            let _ = settings().set_boolean("show-hidden", on);
            if let Some(filter) = win.imp().filter.borrow().as_ref() { filter.changed(gtk::FilterChange::Different); }
            win.update_status();
            win.update_empty_state();
        }));
        self.add_action(&hidden);

        let grid = settings().string("view-mode") == "grid";
        imp.grid_toggle.set_active(grid);
        imp.grid_toggle.connect_toggled(glib::clone!(#[weak(rename_to = win)] self, move |toggle| {
            let _ = settings().set_string("view-mode", if toggle.is_active() { "grid" } else { "list" });
            win.show_items_page();
        }));
    }

    /// Shows "No Matches" when the folder has objects but the filter hides all of them.
    fn update_empty_state(&self) {
        let imp = self.imp();
        let total = imp.store.borrow().as_ref().map(|s| s.n_items()).unwrap_or(0);
        let shown = imp.selection.borrow().as_ref().map(|s| s.n_items()).unwrap_or(0);
        let current = imp.list_stack.visible_child_name();
        let page = imp.list_stack.child_by_name("empty").and_downcast::<adw::StatusPage>().unwrap();
        imp.clear_filters_button.set_visible(total > 0 && shown == 0 && imp.type_filter.get() + imp.size_filter.get() + imp.date_filter.get() > 0);
        let filtering = imp.type_filter.get() + imp.size_filter.get() + imp.date_filter.get() > 0 || !imp.search_entry.text().is_empty();
        if total > 0 && shown == 0 && !filtering && !imp.show_hidden.get() {
            // Only hidden objects (such as .emptyFolderPlaceholder) are here.
            page.set_title(&tr("Empty Folder"));
            page.set_description(Some(&tr("Only hidden files are here. Press Ctrl+H to show them.")));
            page.set_icon_name(Some("folder-open-symbolic"));
            imp.list_stack.set_visible_child_name("empty");
        } else if total > 0 && shown == 0 {
            page.set_title(&tr("No Matches"));
            page.set_description(Some(&tr("No item in this folder matches the filter.")));
            page.set_icon_name(Some("system-search-symbolic"));
            imp.list_stack.set_visible_child_name("empty");
        } else if total > 0 && current.as_deref() == Some("empty") {
            page.set_title(&tr("Empty Folder"));
            page.set_icon_name(Some("folder-open-symbolic"));
            page.set_description(Some(&tr("Drag and drop files here to upload them.")));
            imp.list_stack.set_visible_child_name(if self.grid_mode() { "grid" } else { "list" });
        }
    }

    fn grid_mode(&self) -> bool {
        self.imp().grid_toggle.is_active()
    }

    /// Shows the list or the grid, whichever is chosen, when there is something to show.
    fn show_items_page(&self) {
        let imp = self.imp();
        let current = imp.list_stack.visible_child_name();
        if matches!(current.as_deref(), Some("list") | Some("grid")) {
            imp.list_stack.set_visible_child_name(if self.grid_mode() { "grid" } else { "list" });
        }
    }

    /// The icon grid of GNOME Files, with thumbnails for small images.
    /// Ctrl+plus and Ctrl+minus change the size of the grid items, as in GNOME Files.
    fn zoom_grid(&self, step: i32) {
        let current = settings().int("grid-zoom");
        let level = if step == 0 { 1 } else { (current + step).clamp(0, GRID_SIZES.len() as i32 - 1) };
        if !self.imp().grid_toggle.is_active() { self.imp().grid_toggle.set_active(true); }
        if level == current { return; }
        let _ = settings().set_int("grid-zoom", level);
        // Cells take their size when they are created; new ones are made for the new size.
        let view = &self.imp().grid_view;
        let factory = view.factory();
        view.set_factory(None::<&gtk::ListItemFactory>);
        view.set_factory(factory.as_ref());
    }

    fn setup_grid(&self, selection: &gtk::MultiSelection) {
        let imp = self.imp();
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(glib::clone!(#[weak(rename_to = win)] self, move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let (picture_size, icon_size, cell_width, chars) = GRID_SIZES[settings().int("grid-zoom").clamp(0, GRID_SIZES.len() as i32 - 1) as usize];
            let cell = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).width_request(cell_width).build();
            win.add_object_dnd(cell.upcast_ref());
            let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).width_request(picture_size).height_request(picture_size).halign(gtk::Align::Center).build();
            let image = gtk::Image::builder().pixel_size(icon_size).css_classes(["file-icon"]).build();
            let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).can_shrink(true).css_classes(["thumbnail", "card"]).build();
            stack.add_named(&image, Some("icon"));
            stack.add_named(&picture, Some("thumbnail"));
            let label = gtk::Label::builder().wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar).lines(2).ellipsize(gtk::pango::EllipsizeMode::Middle).justify(gtk::Justification::Center).max_width_chars(chars).build();
            // A small caption under the name: the size of a file, the item count of a folder.
            let caption = gtk::Label::builder().css_classes(["caption", "dim-label", "numeric"]).build();
            cell.append(&stack);
            cell.append(&label);
            cell.append(&caption);
            item.set_child(Some(&cell));
        }));
        factory.connect_bind(glib::clone!(#[weak(rename_to = win)] self, move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let entry = entry_of(&item.item().unwrap());
            let cell = item.child().and_downcast::<gtk::Box>().unwrap();
            let stack = cell.first_child().and_downcast::<gtk::Stack>().unwrap();
            let label = stack.next_sibling().and_downcast::<gtk::Label>().unwrap();
            label.set_text(&entry.name);
            let caption = label.next_sibling().and_downcast::<gtk::Label>().unwrap();
            let caption_text = if entry.is_folder {
                let key = win.cache_key(&win.imp().bucket.borrow(), &entry.key);
                win.imp().listing_cache.borrow().get(&key).map(|(items, token, _)| {
                    let text = trn("{n} item", "{n} items", &[("n", &items.len().to_string())]);
                    if token.is_empty() { text } else { format!("{text}+") }
                }).unwrap_or_default()
            } else {
                format_size(entry.size)
            };
            caption.set_visible(!caption_text.is_empty());
            caption.set_text(&caption_text);
            cell.set_widget_name(&entry.key);
            cell.set_tooltip_text(Some(&format!("{}\n{}", entry.name, if entry.is_folder { tr("Folder") } else { format_size(entry.size) })));
            stack.child_by_name("icon").and_downcast::<gtk::Image>().unwrap().set_from_gicon(&grid_icon_for(&entry));
            stack.set_widget_name(&entry.key);
            stack.set_transition_duration(0);
            stack.set_visible_child_name("icon");
            stack.set_transition_duration(200);
            if !entry.is_folder && is_image(&entry.name) && entry.size > 0 && entry.size <= 6 * 1024 * 1024 {
                win.load_thumbnail(&entry, &stack);
            }
        }));
        imp.grid_view.set_factory(Some(&factory));
        imp.grid_view.set_model(Some(selection));
        imp.grid_view.connect_activate(glib::clone!(#[weak(rename_to = win)] self, move |view, position| {
            let Some(object) = view.model().and_then(|m| m.item(position)) else { return };
            let entry = entry_of(&object);
            if entry.is_folder { win.navigate(&entry.key); } else { win.show_details(entry.key); }
        }));
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string("Delete"), Some(gtk::NamedAction::new("win.delete-selected"))));
        shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string("BackSpace"), Some(gtk::NamedAction::new("win.go-up"))));
        imp.grid_view.add_controller(shortcuts);
    }

    /// Fetches a thumbnail once and shows it with a crossfade if the cell still shows that object.
    fn load_thumbnail(&self, entry: &Entry, stack: &gtk::Stack) {
        self.thumbnail(entry, stack.upcast_ref(), |widget, texture| {
            let stack = widget.downcast_ref::<gtk::Stack>().unwrap();
            stack.child_by_name("thumbnail").and_downcast::<gtk::Picture>().unwrap().set_paintable(Some(texture));
            stack.set_visible_child_name("thumbnail");
        });
    }

    /// Calls `apply` with a small texture of an image object once it is available,
    /// if `widget` still shows that object (its widget name is the key).
    pub(crate) fn thumbnail(&self, entry: &Entry, widget: &gtk::Widget, apply: fn(&gtk::Widget, &gdk::Texture)) {
        let key = entry.key.as_str();
        let imp = self.imp();
        // A changed object (same key, new date) gets a new thumbnail.
        let version = format!("{key}\n{}\n{}", entry.size, entry.modified);
        if let Some(texture) = imp.thumbnails.borrow().get(&version) {
            apply(widget, texture);
            return;
        }
        let mut pending = imp.thumbnail_pending.borrow_mut();
        let waiting = pending.contains_key(key);
        pending.entry(key.to_string()).or_default().push((widget.downgrade(), apply));
        drop(pending);
        if waiting { return; }
        let Some(client) = self.client() else { return };
        let (bucket, key, win) = (imp.bucket.borrow().clone(), key.to_string(), self.clone());
        // Thumbnails are also kept on disk, named by the object and its version, so a folder
        // opened again (even after a restart) shows them without downloading anything.
        // Pictures from an unlocked vault are never written to disk unencrypted.
        let in_vault = crate::s3::vault::find(&client.profile.id, &bucket, &key).is_some();
        let cached = (!in_vault).then(|| thumbnail_path(&format!("{}\n{bucket}\n{key}\n{}\n{}", client.profile.id, entry.size, entry.modified)));
        glib::spawn_future_local(async move {
            let k = key.clone();
            // Decoding and scaling happen off the main thread, so large images never stall scrolling.
            let result = bg(async move {
                let from_disk = tokio::task::spawn_blocking({ let cached = cached.clone(); move || {
                    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file(cached.as_ref()?).ok()?;
                    let pixbuf = if pixbuf.has_alpha() { pixbuf } else { pixbuf.add_alpha(false, 0, 0, 0).ok()? };
                    Some((pixbuf.read_pixel_bytes().to_vec(), pixbuf.width(), pixbuf.height(), pixbuf.rowstride() as usize))
                }}).await.map_err(|e| e.to_string())?;
                if let Some(found) = from_disk { return Ok(found); }
                let bytes = client.read_bytes(&bucket, &k, 6 * 1024 * 1024).await?;
                tokio::task::spawn_blocking(move || {
                    if !crate::widgets::image_fits(&bytes) { return Err("image too large".to_string()); }
                    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes));
                    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(&stream, 256, 256, true, gio::Cancellable::NONE).map_err(|e| e.to_string())?;
                    if let Some(cached) = &cached { let _ = pixbuf.savev(cached, "png", &[]); }
                    let pixbuf = if pixbuf.has_alpha() { pixbuf } else { pixbuf.add_alpha(false, 0, 0, 0).map_err(|e| e.to_string())? };
                    Ok::<_, String>((pixbuf.read_pixel_bytes().to_vec(), pixbuf.width(), pixbuf.height(), pixbuf.rowstride() as usize))
                }).await.map_err(|e| e.to_string())?
            }).await;
            let waiters = win.imp().thumbnail_pending.borrow_mut().remove(&key).unwrap_or_default();
            let Ok((pixels, width, height, stride)) = result else { return };
            let texture: gdk::Texture = gdk::MemoryTexture::new(width, height, gdk::MemoryFormat::R8g8b8a8, &glib::Bytes::from_owned(pixels), stride).upcast();
            {
                // A bounded cache: the oldest pictures go when it is full.
                let mut cache = win.imp().thumbnails.borrow_mut();
                if cache.len() >= 400 {
                    let drop: Vec<String> = cache.keys().take(100).cloned().collect();
                    for k in drop { cache.remove(&k); }
                }
                cache.insert(version.clone(), texture.clone());
            }
            for (widget, apply) in waiters {
                if let Some(widget) = widget.upgrade() && widget.widget_name() == key.as_str() {
                    apply(&widget, &texture);
                }
            }
        });
    }

    /// The position of the item under a point of the list or grid: the cell under the
    /// pointer carries the key of its object as widget name.
    fn position_at(&self, view: &gtk::Widget, x: f64, y: f64) -> Option<u32> {
        let mut widget = view.pick(x, y, gtk::PickFlags::DEFAULT)?;
        // Up to the row of a list or the child of a grid.
        while !matches!(widget.css_name().as_str(), "row" | "child") {
            widget = widget.parent()?;
            if widget == *view { return None; }
        }
        fn named(widget: &gtk::Widget) -> Option<String> {
            let name = widget.widget_name();
            if !name.is_empty() && !name.starts_with("Gtk") { return Some(name.to_string()); }
            let mut child = widget.first_child();
            while let Some(c) = child {
                if let Some(found) = named(&c) { return Some(found); }
                child = c.next_sibling();
            }
            None
        }
        let key = named(&widget)?;
        let selection = self.imp().selection.borrow().clone()?;
        (0..selection.n_items()).find(|&i| selection.item(i).is_some_and(|o| entry_of(&o).key == key))
    }

    pub(crate) fn selected_entries(&self) -> Vec<Entry> {
        let Some(selection) = self.imp().selection.borrow().clone() else { return Vec::new() };
        let set = selection.selection();
        let mut result = Vec::new();
        for i in 0..set.size() {
            if let Some(object) = selection.item(set.nth(i as u32)) {
                result.push(entry_of(&object));
            }
        }
        result
    }

    fn update_selection(&self) {
        let imp = self.imp();
        let count = imp.selection.borrow().as_ref().map(|s| s.selection().size()).unwrap_or(0);
        imp.selection_bar.set_revealed(count > 1);
        imp.selection_label.set_text(&trf("{n} selected", &[("n", &count.to_string())]));
        let has = count > 0;
        let single_file = count == 1 && self.selected_entries().first().is_some_and(|e| !e.is_folder);
        for name in ["download-selected", "delete-selected", "copy-selected", "clear-selection"] {
            self.set_action_enabled(name, has);
        }
        self.set_action_enabled("storage-class-selected", self.selected_entries().iter().any(|e| !e.is_folder));
        self.set_action_enabled("headers-selected", self.selected_entries().iter().any(|e| !e.is_folder));
        self.set_action_enabled("open-in-new-tab", self.selected_entries().iter().any(|e| e.is_folder));
        self.set_action_enabled("copy-link", self.selected_entries().iter().any(|e| !e.is_folder));
        self.set_action_enabled("open-external", single_file || self.imp().info.borrow().is_some());
        self.set_action_enabled("rename", self.selected_entries().iter().any(|e| !e.is_folder));
        if self.in_vault() || self.imp().client.borrow().as_ref().is_some_and(|c| !c.is_s3()) {
            for name in ["copy-link", "headers-selected", "storage-class-selected", "copy-cli"] { self.set_action_enabled(name, false); }
        }
        self.update_status();
    }

    /// Selecting a single file shows its details, as the info pane of a file manager.
    /// A short delay keeps drag selections and keyboard navigation from flooding requests.
    fn details_for_selection(&self) {
        glib::timeout_add_local_once(std::time::Duration::from_millis(150), glib::clone!(#[weak(rename_to = win)] self, move || {
            let entries = win.selected_entries();
            if let [entry] = entries.as_slice() && *win.imp().details_key.borrow() != entry.key {
                if entry.is_folder { win.show_folder_details(entry.key.clone()); } else { win.show_details(entry.key.clone()); }
            } else if entries.len() > 1 && win.imp().details_split.shows_sidebar() {
                win.show_selection_summary(&entries);
            }
        }));
    }

    pub(crate) fn update_status(&self) {
        let imp = self.imp();
        let Some(store) = imp.store.borrow().clone() else { return };
        let shown = imp.selection.borrow().as_ref().map(|s| s.n_items()).unwrap_or(0);
        let total: i64 = (0..store.n_items()).filter_map(|i| store.item(i)).map(|o| entry_of(&o).size).sum();
        let more = if imp.next_token.borrow().is_empty() { "" } else { "+" };
        let selected = self.selected_entries();
        // With a selection the line describes it, as GNOME Files does.
        let text = if selected.len() > 1 {
            let size: i64 = selected.iter().map(|e| e.size).sum();
            format!("{} · {}", trf("{n} selected", &[("n", &selected.len().to_string())]), format_size(size))
        } else {
            format!("{} · {}", trn("{n} item", "{n} items", &[("n", &format!("{shown}{more}"))]), format_size(total))
        };
        imp.status_label.set_text(&text);
    }

    pub(crate) fn set_action_enabled(&self, name: &str, enabled: bool) {
        if let Some(action) = self.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            action.set_enabled(enabled);
        }
    }

    fn setup_actions(&self) {
        let action = |name: &str, f: fn(&Window)| {
            gio::ActionEntry::builder(name).activate(move |win: &Window, _, _| f(win)).build()
        };
        let with_id = |name: &str, f: fn(&Window, String)| {
            gio::ActionEntry::builder(name).parameter_type(Some(glib::VariantTy::STRING))
                .activate(move |win: &Window, _, param| {
                    if let Some(id) = param.and_then(|p| p.get::<String>()) { f(win, id); }
                }).build()
        };
        self.add_action_entries([
            action("new-connection", |win| win.edit_profile(None)),
            with_id("edit-profile", |win, id| {
                let profile = win.imp().profiles.borrow().iter().find(|p| p.id == id).cloned();
                if let Some(profile) = profile {
                    let win2 = win.clone();
                    glib::spawn_future_local(async move {
                        // The stored credentials are shown so they can be changed.
                        let profile = bg(profile::with_secrets(profile.clone())).await.unwrap_or(profile);
                        win2.edit_profile(Some(profile));
                    });
                }
            }),
            with_id("delete-profile", |win, id| win.delete_profile(id)),
            // A new connection that starts as a copy of another, credentials included.
            with_id("duplicate-profile", |win, id| {
                let profile = win.imp().profiles.borrow().iter().find(|p| p.id == id).cloned();
                if let Some(profile) = profile {
                    let win2 = win.clone();
                    glib::spawn_future_local(async move {
                        let mut copy = bg(profile::with_secrets(profile.clone())).await.unwrap_or(profile);
                        copy.id.clear();
                        copy.name = crate::window::actions::copy_name(&copy.name, 1);
                        win2.edit_profile(Some(copy));
                    });
                }
            }),
            action("reload-buckets", |win| win.reload_buckets(None)),
            action("link-bucket", |win| win.link_bucket()),
            action("new-bucket", |win| win.new_bucket()),
            with_id("unlink-bucket", |win, name| win.unlink_bucket(name)),
            with_id("delete-bucket", |win, name| win.delete_bucket(name)),
            with_id("empty-bucket", |win, name| win.empty_bucket(name)),
            action("refresh", |win| win.refresh()),
            action("go-up", |win| win.go_up()),
            action("load-more", |win| win.load_more()),
            action("new-folder", |win| win.new_folder()),
            action("new-text-file", |win| win.new_text_file()),
            action("undo", |win| win.undo()),
            action("new-tab", |win| win.new_tab()),
            action("close-tab", |win| win.close_tab()),
            action("open-in-new-tab", |win| {
                let bucket = win.imp().bucket.borrow().clone();
                let folders: Vec<String> = win.selected_entries().into_iter().filter(|e| e.is_folder).map(|e| e.key).collect();
                for folder in folders { win.open_tab(bucket.clone(), folder); }
            }),
            action("zoom-in", |win| win.zoom_grid(1)),
            action("zoom-out", |win| win.zoom_grid(-1)),
            action("zoom-reset", |win| win.zoom_grid(0)),
            action("search-everywhere", |win| {
                win.imp().view_stack.set_visible_child_name("recent");
                win.imp().split_view.set_show_content(true);
                crate::pages::recent::focus_search();
            }),
            action("properties", |win| win.toggle_properties()),
            action("download-archive", |win| win.download_archive()),
            action("upload-files", |win| win.pick_upload(false)),
            action("upload-folder", |win| win.pick_upload(true)),
            action("download-selected", |win| win.download_selected()),
            action("delete-selected", |win| win.delete_selected()),
            action("copy-link", |win| win.copy_link()),
            action("rename", |win| win.rename_selected()),
            action("storage-class-selected", |win| win.storage_class_selected()),
            action("headers-selected", |win| win.headers_selected()),
            action("select-all", |win| { if let Some(s) = win.imp().selection.borrow().as_ref() { s.select_all(); } }),
            action("select-matching", |win| win.select_matching()),
            action("export-listing", |win| win.export_listing()),
            action("deleted-objects", |win| {
                let Some(client) = win.client() else { return };
                let (bucket, prefix) = (win.imp().bucket.borrow().clone(), win.imp().prefix.borrow().clone());
                crate::dialogs::bucket::deleted_objects(win, client, bucket, prefix);
            }),
            action("clear-selection", |win| { if let Some(s) = win.imp().selection.borrow().as_ref() { s.unselect_all(); } }),
            action("search", |win| {
                if win.imp().view_stack.visible_child_name().as_deref() == Some("recent") { crate::pages::recent::focus_search(); return; }
                let bar = &win.imp().search_bar;
                bar.set_search_mode(!bar.is_search_mode());
            }),
            action("import-profiles", |win| crate::dialogs::backup::import(win)),
            action("import-other-apps", |win| crate::dialogs::import_apps::present(win)),
            action("new-vault", |win| win.new_vault()),
            action("bucket-settings", |win| {
                if let Some(client) = win.client() { bucket::bucket_settings(win, client, win.imp().bucket.borrow().clone()); }
            }),
            action("sync", |win| {
                if let Some(client) = win.client() { bucket::sync(win, client, win.imp().bucket.borrow().clone(), win.imp().prefix.borrow().clone()); }
            }),
            action("copy-selected", |win| {
                let entries = win.selected_entries();
                if let (Some(client), false) = (win.client(), entries.is_empty()) {
                    bucket::copy_to(win, client, win.imp().bucket.borrow().clone(), win.imp().prefix.borrow().clone(), entries);
                }
            }),
            action("toggle-favorite", |win| win.toggle_favorite()),
            action("open-external", |win| {
                let key = win.imp().info.borrow().as_ref().map(|i| i.key.clone())
                    .or_else(|| win.selected_entries().into_iter().find(|e| !e.is_folder).map(|e| e.key));
                if let (Some(client), Some(key)) = (win.client(), key) { crate::transfers::external::open(win, client, win.imp().bucket.borrow().clone(), key); }
            }),
            action("mount", |win| win.mount_current()),
            action("object-versions", |win| {
                let info = win.imp().info.borrow().clone();
                if let (Some(client), Some(info)) = (win.client(), info) { bucket::versions(win, client, win.imp().bucket.borrow().clone(), info.key); }
            }),
            action("object-headers", |win| {
                let info = win.imp().info.borrow().clone();
                if let (Some(client), Some(info)) = (win.client(), info) { bucket::headers(win, client, win.imp().bucket.borrow().clone(), info); }
            }),
            action("object-tags", |win| {
                let info = win.imp().info.borrow().clone();
                if let (Some(client), Some(info)) = (win.client(), info) { bucket::tags_and_metadata(win, client, win.imp().bucket.borrow().clone(), info); }
            }),
            action("object-permissions", |win| {
                let info = win.imp().info.borrow().clone();
                if let (Some(client), Some(info)) = (win.client(), info) { crate::dialogs::access::object_permissions(win, client, win.imp().bucket.borrow().clone(), info); }
            }),
            with_id("reveal", |win, key| {
                win.imp().view_stack.set_visible_child_name("browser");
                let folder = key[..key.rfind('/').map(|i| i + 1).unwrap_or(0)].to_string();
                win.navigate(&folder);
                win.show_details(key);
            }),
            with_id("analyze-folder", |win, prefix| {
                let analyzer = win.imp().analyzer.borrow().clone();
                if let Some(analyzer) = analyzer { analyzer.analyze_folder(win, prefix); }
            }),
            action("export-profiles", |win| crate::dialogs::backup::export(win)),
        ]);
    }

    /// Location actions only make sense with an open bucket.
    pub(crate) fn update_location_actions(&self) {
        let imp = self.imp();
        let connected = imp.client.borrow().is_some();
        let open = connected && !imp.bucket.borrow().is_empty();
        for name in ["refresh", "new-folder", "new-text-file", "new-vault", "download-archive", "upload-files", "upload-folder", "select-all", "select-matching", "export-listing", "deleted-objects", "new-tab", "search", "bucket-settings", "sync", "toggle-favorite", "mount", "upload-link"] {
            self.set_action_enabled(name, open);
        }
        self.set_action_enabled("go-up", open && !imp.prefix.borrow().is_empty());
        for name in ["reload-buckets", "link-bucket", "new-bucket"] {
            self.set_action_enabled(name, connected);
        }
        // File servers have no buckets, versions, links or bucket settings.
        if imp.client.borrow().as_ref().is_some_and(|c| !c.is_s3()) {
            for name in S3_ONLY {
                self.set_action_enabled(name, false);
            }
        }
        self.set_action_enabled("export-profiles", !imp.profiles.borrow().is_empty());
        let browsing = imp.view_stack.visible_child_name().as_deref() == Some("browser");
        // In Recent, Ctrl+F searches every connection, with or without an open bucket.
        self.set_action_enabled("search", open || imp.view_stack.visible_child_name().as_deref() == Some("recent"));
        imp.upload_button.set_visible(open && browsing);
        imp.search_button.set_visible(open && browsing);
        imp.location_button.set_visible(open && browsing);
        imp.view_toggle.set_visible(open);
        if let Some(analyzer) = imp.analyzer.borrow().as_ref() {
            analyzer.set_location(&imp.bucket.borrow(), &imp.prefix.borrow());
        }
        self.update_favorites();
        self.update_title();
        self.update_selection();
    }

    // ----- Connections -----

    fn load_profiles(&self) {
        let imp = self.imp();
        let profiles = profile::load();
        let active = imp.client.borrow().as_ref().map(|c| c.profile.id.clone());
        imp.profiles_list.remove_all();
        for p in &profiles {
            let row = avatar_row(&p.name);
            let row_box = row.child().and_downcast::<gtk::Box>().unwrap();
            let menu = gio::Menu::new();
            let edit = gio::MenuItem::new(Some(&tr("Edit…")), None);
            edit.set_action_and_target_value(Some("win.edit-profile"), Some(&p.id.to_variant()));
            menu.append_item(&edit);
            let duplicate = gio::MenuItem::new(Some(&tr("Duplicate…")), None);
            duplicate.set_action_and_target_value(Some("win.duplicate-profile"), Some(&p.id.to_variant()));
            menu.append_item(&duplicate);
            let delete = gio::MenuItem::new(Some(&tr("Delete")), None);
            delete.set_action_and_target_value(Some("win.delete-profile"), Some(&p.id.to_variant()));
            menu.append_item(&delete);
            let button = gtk::MenuButton::builder().icon_name("view-more-symbolic").menu_model(&menu)
                .tooltip_text(tr("Connection Menu")).valign(gtk::Align::Center).build();
            button.add_css_class("flat");
            row_box.append(&button);
            imp.profiles_list.append(&row);
            if active.as_deref() == Some(p.id.as_str()) {
                imp.profiles_list.select_row(Some(&row));
            }
        }
        imp.profiles_placeholder.set_visible(profiles.is_empty());
        imp.profiles.replace(profiles);
        self.update_location_actions();
    }

    pub(crate) fn edit_profile(&self, existing: Option<Profile>) {
        connection::present(self, existing, glib::clone!(#[weak(rename_to = win)] self, move |saved, in_keyring| {
            if !in_keyring && !saved.secret_key.is_empty() {
                win.toast(&tr("No usable keyring was found; the access key is stored unencrypted"));
            }
            win.load_profiles();
            win.connect(saved);
        }));
    }

    fn delete_profile(&self, id: String) {
        let Some(p) = self.imp().profiles.borrow().iter().find(|p| p.id == id).cloned() else { return };
        let dialog = adw::AlertDialog::new(Some(&tr("Delete Connection?")), Some(&trf("“{name}” and its stored credentials will be removed. Buckets are not affected.", &[("name", &p.name)])));
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("delete", &tr("Delete"))]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        let win = self.clone();
        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&win)).await != "delete" { return; }
            if let Err(error) = bg(profile::delete(id.clone())).await { win.toast(&error); return; }
            crate::search::forget_profile(&id);
            crate::pages::recent::forget_profile(&id);
            if win.imp().client.borrow().as_ref().is_some_and(|c| c.profile.id == id) {
                win.disconnect();
            }
            win.load_profiles();
        });
    }

    fn disconnect(&self) {
        let imp = self.imp();
        imp.client.replace(None);
        imp.bucket.replace(String::new());
        imp.prefix.replace(String::new());
        imp.buckets_section.set_visible(false);
        imp.browser_stack.set_visible_child_name("welcome");
        imp.content_page.set_title("Ferry");
        self.update_location_actions();
    }

    pub(crate) fn connect(&self, profile: Profile) {
        crate::debug!("connect {}", profile.name);
        let imp = self.imp();
        self.reset_history();
        let generation = imp.generation.get() + 1;
        imp.generation.set(generation);
        imp.client.replace(None);
        imp.bucket.replace(String::new());
        imp.prefix.replace(String::new());
        imp.browser_stack.set_visible_child_name("connecting");
        imp.content_page.set_title(&profile.name);
        imp.buckets_section.set_visible(true);
        imp.buckets_list.remove_all();
        imp.buckets_placeholder.set_text(&tr("Loading…"));
        imp.buckets_placeholder.set_visible(true);
        imp.split_view.set_show_content(true);
        self.update_location_actions();
        let win = self.clone();
        let id_for_trust = profile.id.clone();
        glib::spawn_future_local(async move {
            if s3::connection::needs_mfa(&profile) && !crate::dialogs::connection::unlock_mfa(&win, &profile).await {
                win.imp().browser_stack.set_visible_child_name("welcome");
                win.update_location_actions();
                return;
            }
            let result = bg(async move {
                let profile = profile::with_secrets(profile).await?;
                let client = S3::connect(profile).await?;
                let buckets = client.list_buckets().await?;
                Ok((client, buckets))
            }).await;
            if win.imp().generation.get() != generation { return; }
            match result {
                Ok((client, buckets)) => {
                    win.imp().client.replace(Some(client));
                    win.show_buckets(buckets);
                }
                Err(error) if crate::remote::sftp::UnknownHost::decode(&error).is_some() => {
                    // A server seen for the first time: once its key is trusted, connect again.
                    let unknown = crate::remote::sftp::UnknownHost::decode(&error).unwrap();
                    win.imp().browser_stack.set_visible_child_name("welcome");
                    if crate::dialogs::connection::trust_host_key(&win, &unknown).await {
                        let id = id_for_trust.clone();
                        let saved = bg(async move {
                            let stored = profile::load().into_iter().find(|p| p.id == id).ok_or_else(|| tr("The connection was deleted"))?;
                            let mut stored = profile::with_secrets(stored).await?;
                            if unknown.jump { stored.jump_host_key = unknown.fingerprint; } else { stored.host_key = unknown.fingerprint; }
                            profile::save(stored, true).await.map(|(p, _)| p)
                        }).await;
                        match saved {
                            Ok(updated) => { win.load_profiles(); win.connect(updated); return; }
                            Err(error) => win.toast(&error),
                        }
                    }
                }
                Err(error) => {
                    win.imp().browser_stack.set_visible_child_name("welcome");
                    win.imp().buckets_placeholder.set_text(&error);
                    win.toast(&error);
                }
            }
            win.update_location_actions();
        });
    }

    fn show_buckets(&self, list: s3::BucketList) {
        let imp = self.imp();
        imp.buckets_list.remove_all();
        // A file server is one place, not a list of buckets.
        let file_server = imp.client.borrow().as_ref().is_some_and(|c| !c.is_s3());
        imp.buckets_heading.set_label(&if file_server { tr("Server") } else { tr("Buckets") });
        imp.add_bucket_button.set_visible(!file_server);
        for bucket in &list.buckets {
            let row = sidebar_row(if file_server { "network-server-symbolic" } else if bucket.pinned { "emblem-shared-symbolic" } else { "package-x-generic-symbolic" }, &bucket.name);
            let row_box = row.child().and_downcast::<gtk::Box>().unwrap();
            let menu = gio::Menu::new();
            let tools = gio::Menu::new();
            for (label, action) in [(tr("Bucket Settings"), "win.bucket-settings-for"), (tr("Analyze"), "win.analyze-bucket"), (tr("Mount as Folder…"), "win.mount-bucket"), (tr("Copy Location"), "win.copy-bucket-location")] {
                let entry = gio::MenuItem::new(Some(&label), None);
                entry.set_action_and_target_value(Some(action), Some(&bucket.name.to_variant()));
                tools.append_item(&entry);
            }
            menu.append_section(None, &tools);
            let empty = gio::MenuItem::new(Some(&tr("Empty Bucket…")), None);
            empty.set_action_and_target_value(Some("win.empty-bucket"), Some(&bucket.name.to_variant()));
            let danger_items = gio::Menu::new();
            danger_items.append_item(&empty);
            let item = if bucket.pinned {
                let item = gio::MenuItem::new(Some(&tr("Remove from Connection")), None);
                item.set_action_and_target_value(Some("win.unlink-bucket"), Some(&bucket.name.to_variant()));
                item
            } else {
                let item = gio::MenuItem::new(Some(&tr("Delete Bucket…")), None);
                item.set_action_and_target_value(Some("win.delete-bucket"), Some(&bucket.name.to_variant()));
                item
            };
            let danger = danger_items;
            danger.append_item(&item);
            menu.append_section(None, &danger);
            let button = gtk::MenuButton::builder().icon_name("view-more-symbolic").menu_model(&menu)
                .tooltip_text(tr("Bucket Menu")).valign(gtk::Align::Center).build();
            button.add_css_class("flat");
            row_box.append(&button);
            self.add_bucket_target(row.upcast_ref(), bucket.name.clone());
            imp.buckets_list.append(&row);
        }
        let empty = list.buckets.is_empty();
        imp.buckets_placeholder.set_visible(empty);
        if empty {
            imp.buckets_placeholder.set_text(&if list.warning.is_empty() {
                tr("No buckets")
            } else {
                format!("{}\n\n{}", tr("These credentials cannot list buckets. Link a bucket you have access to by entering its name."), list.warning)
            });
        }
        let first = list.buckets.first().map(|b| b.name.clone());
        imp.buckets.replace(list.buckets);
        match first {
            Some(name) if imp.bucket.borrow().is_empty() => self.open_bucket(&name),
            _ if empty => imp.browser_stack.set_visible_child_name("nobucket"),
            _ => {}
        }
    }

    fn reload_buckets(&self, open: Option<String>) {
        let Some(client) = self.client() else { return };
        let win = self.clone();
        glib::spawn_future_local(async move {
            match bg(async move { client.list_buckets().await }).await {
                Ok(list) => {
                    win.show_buckets(list);
                    if let Some(name) = open { win.open_bucket(&name); }
                }
                Err(error) => win.toast(&error),
            }
        });
    }

    /// Asks for one line of text; returns None when cancelled.
    pub(crate) async fn ask_text(&self, heading: &str, body: &str, initial: &str, accept: &str, destructive: bool) -> Option<String> {
        let dialog = adw::AlertDialog::new(Some(heading), if body.is_empty() { None } else { Some(body) });
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("ok", accept)]);
        dialog.set_response_appearance("ok", if destructive { adw::ResponseAppearance::Destructive } else { adw::ResponseAppearance::Suggested });
        // Enter never confirms a destructive action.
        if !destructive { dialog.set_default_response(Some("ok")); }
        dialog.set_close_response("cancel");
        let entry = gtk::Entry::builder().text(initial).activates_default(true).build();
        dialog.set_extra_child(Some(&entry));
        entry.grab_focus();
        // A file name is selected without its extension, as GNOME Files does when renaming.
        if let Some(dot) = initial.rfind('.').filter(|&i| i > 0 && !initial.contains('/')) {
            entry.select_region(0, initial[..dot].chars().count() as i32);
        }
        let response = dialog.choose_future(Some(self)).await;
        (response == "ok").then(|| entry.text().trim().to_string()).filter(|text| !text.is_empty())
    }

    /// A destructive confirmation that needs a name typed exactly: the button stays off
    /// until it matches, and there is no default response, so Enter cannot destroy anything.
    pub(crate) async fn confirm_typed(&self, heading: &str, body: &str, expected: &str, accept: &str) -> bool {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("ok", accept)]);
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
        dialog.set_response_enabled("ok", false);
        dialog.set_close_response("cancel");
        let entry = gtk::Entry::builder().placeholder_text(expected).build();
        let expected_owned = expected.to_string();
        entry.connect_changed(glib::clone!(#[weak] dialog, move |e| dialog.set_response_enabled("ok", e.text() == expected_owned)));
        dialog.set_extra_child(Some(&entry));
        entry.grab_focus();
        dialog.choose_future(Some(self)).await == "ok" && entry.text() == expected
    }

    pub(crate) async fn confirm(&self, heading: &str, body: &str, accept: &str) -> bool {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("ok", accept)]);
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        dialog.choose_future(Some(self)).await == "ok"
    }

    fn link_bucket(&self) {
        let Some(client) = self.client() else { return };
        let win = self.clone();
        glib::spawn_future_local(async move {
            let Some(name) = win.ask_text(&tr("Link Bucket"), &tr("Enter the name of an existing bucket you can access. Nothing is created; it is only added to this connection."), "", &tr("Link"), false).await else { return };
            if let Err(error) = profile::set_linked(&client.profile.id, &name, true) { win.toast(&error); return; }
            win.relink(&client, Some(name));
        });
    }

    /// Reloads the active profile after its linked buckets changed.
    fn relink(&self, client: &S3, open: Option<String>) {
        let mut updated = client.clone();
        if let Some(stored) = profile::load().into_iter().find(|p| p.id == client.profile.id) {
            updated.profile.buckets = stored.buckets;
        }
        self.imp().client.replace(Some(updated));
        self.load_profiles();
        self.reload_buckets(open);
    }

    fn unlink_bucket(&self, name: String) {
        let Some(client) = self.client() else { return };
        if let Err(error) = profile::set_linked(&client.profile.id, &name, false) { self.toast(&error); return; }
        if *self.imp().bucket.borrow() == name {
            self.imp().bucket.replace(String::new());
            self.imp().browser_stack.set_visible_child_name("nobucket");
        }
        self.relink(&client, None);
    }

    fn new_bucket(&self) {
        let Some(client) = self.client() else { return };
        let win = self.clone();
        glib::spawn_future_local(async move {
            let Some(name) = win.ask_text(&tr("New Bucket"), "", "", &tr("Create"), false).await else { return };
            let open = name.clone();
            match bg(async move { client.create_bucket(&name).await }).await {
                Ok(()) => { win.toast(&tr("Bucket created")); win.reload_buckets(Some(open)); }
                Err(error) => win.toast(&error),
            }
        });
    }

    fn delete_bucket(&self, name: String) {
        let Some(client) = self.client() else { return };
        let win = self.clone();
        glib::spawn_future_local(async move {
            let body = trf("“{name}” and all objects in it will be deleted permanently. Type the bucket name to confirm.", &[("name", &name)]);
            if !win.confirm_typed(&tr("Delete Bucket?"), &body, &name, &tr("Delete")).await { return; }
            let target = name.clone();
            match bg(async move { client.delete_bucket(&target).await }).await {
                Ok(()) => {
                    if *win.imp().bucket.borrow() == name {
                        win.imp().bucket.replace(String::new());
                        win.imp().browser_stack.set_visible_child_name("nobucket");
                    }
                    win.toast(&tr("Bucket deleted"));
                    win.reload_buckets(None);
                }
                Err(error) => win.toast(&error),
            }
        });
    }

    /// Deletes all objects of a bucket after the user typed its name; the bucket stays.
    fn empty_bucket(&self, name: String) {
        let Some(client) = self.client() else { return };
        let win = self.clone();
        glib::spawn_future_local(async move {
            let body = trf("All objects in “{name}” will be deleted permanently; the bucket itself stays. Type the bucket name to confirm.", &[("name", &name)]);
            if !win.confirm_typed(&tr("Empty Bucket?"), &body, &name, &tr("Empty")).await { return; }
            let removed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let out = removed.clone();
            let outcome = win.run_one("delete", &trf("Empty {name}", &[("name", &name)]), &name.clone(), 0, None, crate::transfers::queue::work(move |progress| {
                let (client, name, out) = (client.clone(), name.clone(), out.clone());
                async move {
                    let n = client.empty_bucket(&name, &progress).await?;
                    out.store(n, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                }
            })).await;
            if outcome.done > 0 {
                let n = removed.load(std::sync::atomic::Ordering::Relaxed);
                win.toast(&trn("{n} object deleted", "{n} objects deleted", &[("n", &n.to_string())]));
                win.refresh();
            } else if outcome.failed > 0 {
                win.toast(&outcome.error);
            }
        });
    }

    // ----- Browsing -----

    pub(crate) fn open_bucket(&self, name: &str) {
        let imp = self.imp();
        imp.thumbnails.borrow_mut().clear();
        imp.bucket.replace(name.to_string());
        imp.browser_stack.set_visible_child_name("browser");
        let buckets = imp.buckets.borrow().clone();
        if let Some(index) = buckets.iter().position(|b| b.name == name) {
            imp.buckets_list.select_row(imp.buckets_list.row_at_index(index as i32).as_ref());
        }
        self.navigate("");
    }

    pub(crate) fn navigate(&self, prefix: &str) {
        crate::debug!("navigate {}/{}", self.imp().bucket.borrow(), prefix);
        let imp = self.imp();
        imp.prefix.replace(prefix.to_string());
        self.record_location();
        self.save_session();
        imp.search.replace(None);
        imp.search_entry.set_text("");
        imp.details_split.set_show_sidebar(false);
        imp.details_key.replace(String::new());
        // The list takes the keyboard after moving to a folder, as in GNOME Files, unless
        // the user is typing somewhere.
        let typing = gtk::prelude::GtkWindowExt::focus(self).is_some_and(|w| w.is::<gtk::Text>() || w.is::<gtk::TextView>());
        if !typing {
            let view: gtk::Widget = if self.grid_mode() { imp.grid_view.get().upcast() } else { imp.column_view.get().upcast() };
            glib::idle_add_local_once(move || { view.grab_focus(); });
        }
        self.rebuild_path();
        self.refresh();
    }

    fn go_up(&self) {
        let prefix = self.imp().prefix.borrow().clone();
        let trimmed = prefix.trim_end_matches('/');
        let parent = match trimmed.rfind('/') { Some(i) => &trimmed[..=i], None => "" };
        self.navigate(parent);
    }

    pub(crate) fn rebuild_path(&self) {
        let imp = self.imp();
        while let Some(child) = imp.path_box.first_child() {
            imp.path_box.remove(&child);
        }
        let bucket = imp.bucket.borrow().clone();
        let prefix = imp.prefix.borrow().clone();
        imp.content_page.set_title(&bucket);
        self.update_title();
        let crumb = |label: &str, target: String, icon: Option<&str>| {
            let button = gtk::Button::new();
            button.add_css_class("flat");
            match icon {
                Some(icon) => button.set_child(Some(&adw::ButtonContent::builder().icon_name(icon).label(label).build())),
                None => button.set_label(label),
            }
            button.connect_clicked(glib::clone!(#[weak(rename_to = win)] self, #[strong] target, move |_| win.navigate(&target)));
            // Objects dropped on a path segment move into that folder.
            self.add_move_target(button.upcast_ref(), Some(target));
            imp.path_box.append(&button);
        };
        crumb(&bucket, String::new(), Some("package-x-generic-symbolic"));
        let mut path = String::new();
        for part in prefix.split('/').filter(|p| !p.is_empty()) {
            path.push_str(part);
            path.push('/');
            let separator = gtk::Label::new(Some("/"));
            separator.add_css_class("path-separator");
            imp.path_box.append(&separator);
            crumb(part, path.clone(), None);
        }
        self.update_location_actions();
    }

    /// The header shows the open location: bucket as title, folder as subtitle.
    fn update_title(&self) {
        let imp = self.imp();
        let view = imp.view_stack.visible_child_name();
        let bucket = imp.bucket.borrow().clone();
        let profile = imp.client.borrow().as_ref().map(|c| c.profile.name.clone());
        let (title, subtitle) = match view.as_deref() {
            Some("analyzer") => (tr("Analyzer"), bucket),
            Some("recent") => (tr("Recent"), String::new()),
            Some("backups") => (tr("Backups"), profile.unwrap_or_default()),
            Some("compatibility") => (tr("Compatibility"), if bucket.is_empty() { profile.unwrap_or_default() } else { bucket }),
            _ if !bucket.is_empty() => {
                let prefix = imp.prefix.borrow().clone();
                (bucket, if prefix.is_empty() { profile.clone().unwrap_or_default() } else { format!("/{prefix}") })
            }
            _ => (profile.clone().unwrap_or_else(|| "Ferry".into()), String::new()),
        };
        imp.content_title.set_title(&title);
        imp.content_title.set_subtitle(&subtitle);
        self.update_tab_title();
        // The window title names the place, for the overview, Alt+Tab and screen readers.
        self.set_title(Some(&if title == "Ferry" { title.clone() } else { format!("{title} – Ferry") }));
    }

    fn fill(&self, items: Vec<Entry>, append: bool) {
        let imp = self.imp();
        // Listed objects become findable from the GNOME Shell search.
        if let Some(client) = imp.client.borrow().as_ref() {
            let prefix = imp.prefix.borrow().clone();
            let complete = !append && imp.next_token.borrow().is_empty() && imp.search.borrow().is_none();
            crate::search::record(&client.profile.id, &client.profile.name, &imp.bucket.borrow(), items.iter().map(|e| (e.key.clone(), e.is_folder)), complete.then_some(prefix.as_str()));
        }
        let store = imp.store.borrow().clone().unwrap();
        let mut items = items;
        items.sort_by(|a, b| b.is_folder.cmp(&a.is_folder).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        let objects: Vec<glib::BoxedAnyObject> = items.into_iter().map(glib::BoxedAnyObject::new).collect();
        if append {
            store.extend_from_slice(&objects);
        } else {
            store.splice(0, store.n_items(), &objects);
        }
        let empty = store.n_items() == 0;
        if empty {
            let searching = imp.search.borrow().is_some();
            imp.list_stack.child_by_name("empty").and_downcast::<adw::StatusPage>().unwrap()
                .set_title(&if searching { tr("No Results") } else { tr("Empty Folder") });
        }
        imp.list_stack.set_visible_child_name(if empty { "empty" } else if self.grid_mode() { "grid" } else { "list" });
        imp.more_revealer.set_reveal_child(!imp.next_token.borrow().is_empty() && imp.search.borrow().is_none());
        if !append { self.update_vault_state(); }
        self.update_selection();
    }

    pub(crate) fn refresh(&self) {
        let imp = self.imp();
        let Some(client) = self.client() else { return };
        let bucket = imp.bucket.borrow().clone();
        if bucket.is_empty() { return; }
        if let Some(query) = imp.search.borrow().clone() {
            self.run_search(query);
            return;
        }
        imp.search_banner.set_revealed(false);
        let prefix = imp.prefix.borrow().clone();
        // Refreshing the folder that is already being listed waits for that answer instead of replacing it.
        if imp.loading.get() && *imp.listed.borrow() == self.cache_key(&bucket, &prefix) {
            imp.refresh_again.set(true);
            return;
        }
        imp.refresh_again.set(false);
        let generation = imp.generation.get() + 1;
        imp.generation.set(generation);
        imp.loading.set(true);
        // Another folder: show its cached content at once, or an empty list instead of the old folder.
        let key = self.cache_key(&bucket, &prefix);
        if *imp.listed.borrow() != key {
            imp.listed.replace(key.clone());
            let cached = imp.listing_cache.borrow().get(&key).cloned();
            match cached {
                Some((items, token, _)) => { imp.next_token.replace(token); self.fill(items, false); }
                None => if let Some(store) = imp.store.borrow().as_ref() { store.remove_all(); },
            }
        }
        // The spinner only appears when loading takes a moment, so quick folders do not flicker.
        if imp.store.borrow().as_ref().map(|s| s.n_items()).unwrap_or(0) == 0 || imp.list_stack.visible_child_name().as_deref() == Some("error") {
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), glib::clone!(#[weak(rename_to = win)] self, move || {
                let imp = win.imp();
                if imp.generation.get() == generation && imp.loading.get() { imp.list_stack.set_visible_child_name("loading"); }
            }));
        }
        let win = self.clone();
        glib::spawn_future_local(async move {
            let result = bg(async move { client.list_objects(&bucket, &prefix, "").await }).await;
            if win.imp().generation.get() != generation { return; }
            win.imp().loading.set(false);
            if win.imp().refresh_again.replace(false) {
                glib::idle_add_local_once(glib::clone!(#[weak] win, move || win.refresh()));
            }
            match result {
                Ok(listing) => {
                    // Unchanged content is not refilled, so the selection and scroll position stay.
                    let unchanged = win.imp().listing_cache.borrow().get(&key).is_some_and(|(items, token, _)| *items == listing.items && *token == listing.next_token)
                        && win.imp().store.borrow().as_ref().is_some_and(|s| s.n_items() > 0 || listing.items.is_empty())
                        && win.imp().list_stack.visible_child_name().as_deref() != Some("loading");
                    win.remember_listing(key.clone(), listing.items.clone(), listing.next_token.clone());
                    if !unchanged {
                        win.imp().next_token.replace(listing.next_token);
                        win.fill(listing.items, false);
                    }
                }
                Err(error) => {
                    let imp = win.imp();
                    imp.next_token.replace(String::new());
                    if let Some(store) = imp.store.borrow().as_ref() { store.remove_all(); }
                    imp.error_page.set_description(Some(&glib::markup_escape_text(&error)));
                    imp.list_stack.set_visible_child_name("error");
                    imp.more_revealer.set_reveal_child(false);
                }
            }
        });
    }

    /// Recent folder listings are kept on disk, so after a restart the last folders show
    /// at once while they are listed again in the background.
    pub(crate) fn save_listings(&self) {
        let cache = self.imp().listing_cache.borrow();
        // Listings inside an unlocked vault hold cleartext names: they stay in memory only.
        let mut recent: Vec<(&String, &(Vec<Entry>, String, std::time::Instant))> = cache.iter()
            .filter(|(k, v)| v.0.len() <= 5000 && !cache_key_in_vault(k)).collect();
        recent.sort_by_key(|(_, v)| std::cmp::Reverse(v.2));
        let saved: Vec<(&String, &Vec<Entry>, &String)> = recent.into_iter().take(40).map(|(k, v)| (k, &v.0, &v.1)).collect();
        if let Ok(data) = serde_json::to_vec(&saved) {
            let _ = std::fs::write(listings_path(), data);
        }
    }

    fn load_listings(&self) {
        let Some(saved) = std::fs::read(listings_path()).ok().and_then(|d| serde_json::from_slice::<Vec<(String, Vec<Entry>, String)>>(&d).ok()) else { return };
        // Marked as old, so they are listed again as soon as they are opened.
        let stale = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(3600)).unwrap_or_else(std::time::Instant::now);
        let mut cache = self.imp().listing_cache.borrow_mut();
        for (key, items, token) in saved {
            cache.entry(key).or_insert((items, token, stale));
        }
    }

    fn cache_key(&self, bucket: &str, prefix: &str) -> String {
        let profile = self.imp().client.borrow().as_ref().map(|c| c.profile.id.clone()).unwrap_or_default();
        format!("{profile}\u{0}{bucket}\u{0}{prefix}")
    }

    fn remember_listing(&self, key: String, items: Vec<Entry>, token: String) {
        let mut cache = self.imp().listing_cache.borrow_mut();
        cache.insert(key, (items, token, std::time::Instant::now()));
        if cache.len() > 300 {
            let oldest = cache.iter().min_by_key(|(_, v)| v.2).map(|(k, _)| k.clone());
            if let Some(oldest) = oldest { cache.remove(&oldest); }
        }
    }

    /// Binds the row of a folder again, after its content (and so its item count) became known.
    fn refresh_folder_row(&self, key: &str) {
        let Some(store) = self.imp().store.borrow().clone() else { return };
        if let Some(position) = (0..store.n_items()).find(|i| store.item(*i).is_some_and(|o| entry_of(&o).key == key)) {
            store.items_changed(position, 1, 1);
        }
    }

    /// Lists a folder in the background so opening it shows its content at once.
    pub(crate) fn prefetch(&self, prefix: String) {
        let imp = self.imp();
        let Some(client) = self.client() else { return };
        let bucket = imp.bucket.borrow().clone();
        let key = self.cache_key(&bucket, &prefix);
        if imp.listing_cache.borrow().get(&key).is_some_and(|(_, _, at)| at.elapsed().as_secs() < 20) { return; }
        let (win, prefix_for_row) = (self.clone(), prefix.clone());
        glib::spawn_future_local(async move {
            if let Ok(listing) = bg(async move { client.list_objects(&bucket, &prefix, "").await }).await {
                win.remember_listing(key, listing.items, listing.next_token);
                win.refresh_folder_row(&prefix_for_row);
            }
        });
    }

    pub(crate) fn load_more(&self) {
        let imp = self.imp();
        let Some(client) = self.client() else { return };
        let (bucket, prefix, token) = (imp.bucket.borrow().clone(), imp.prefix.borrow().clone(), imp.next_token.borrow().clone());
        if token.is_empty() || imp.loading_more.replace(true) { return; }
        let generation = imp.generation.get();
        let win = self.clone();
        glib::spawn_future_local(async move {
            let result = bg(async move { client.list_objects(&bucket, &prefix, &token).await }).await;
            win.imp().loading_more.set(false);
            if win.imp().generation.get() != generation { return; }
            match result {
                Ok(listing) => { win.imp().next_token.replace(listing.next_token); win.fill(listing.items, true); }
                Err(error) => win.toast(&error),
            }
        });
    }

    /// Searches every object below the current folder by name.
    fn run_search(&self, query: String) {
        self.imp().listed.replace(String::new());
        let imp = self.imp();
        let Some(client) = self.client() else { return };
        let (bucket, prefix) = (imp.bucket.borrow().clone(), imp.prefix.borrow().clone());
        let generation = imp.generation.get() + 1;
        imp.generation.set(generation);
        imp.search.replace(Some(query.clone()));
        imp.list_stack.set_visible_child_name("loading");
        let win = self.clone();
        glib::spawn_future_local(async move {
            let needle = query.to_lowercase();
            let result = bg(async move {
                let (items, truncated) = client.list_all(&bucket, &prefix, s3::SCAN_LIMIT).await?;
                let found: Vec<Entry> = items.into_iter()
                    .filter(|e| e.key.rsplit('/').next().unwrap_or("").to_lowercase().contains(&needle))
                    .map(|mut e| { e.name = e.key.strip_prefix(&prefix).unwrap_or(&e.key).to_string(); e })
                    .take(5000)
                    .collect();
                Ok((found, truncated))
            }).await;
            if win.imp().generation.get() != generation { return; }
            match result {
                Ok((found, truncated)) => {
                    let mut title = trn("{n} result for “{q}”", "{n} results for “{q}”", &[("n", &found.len().to_string()), ("q", &query)]);
                    if truncated { title.push_str(" · "); title.push_str(&tr("Only the first results are shown; narrow the search.")); }
                    win.imp().search_banner.set_title(&title);
                    win.imp().search_banner.set_revealed(true);
                    win.imp().next_token.replace(String::new());
                    win.fill(found, false);
                }
                Err(error) => { win.imp().search.replace(None); win.toast(&error); win.refresh(); }
            }
        });
    }

    fn new_folder(&self) {
        let Some(client) = self.client() else { return };
        let (bucket, prefix) = (self.imp().bucket.borrow().clone(), self.imp().prefix.borrow().clone());
        let win = self.clone();
        glib::spawn_future_local(async move {
            let Some(name) = win.ask_text(&tr("New Folder"), "", "", &tr("Create"), false).await else { return };
            let name = name.trim_matches('/').to_string();
            if name.is_empty() { return; }
            let key = format!("{prefix}{name}/");
            match bg(async move { client.create_folder(&bucket, &key).await }).await {
                Ok(()) => win.refresh(),
                Err(error) => win.toast(&error),
            }
        });
    }

    /// Creates an empty text file here and opens it in the text editor; saving uploads it.
    fn new_text_file(&self) {
        let Some(client) = self.client() else { return };
        let (bucket, prefix) = (self.imp().bucket.borrow().clone(), self.imp().prefix.borrow().clone());
        let win = self.clone();
        glib::spawn_future_local(async move {
            let Some(name) = win.ask_text(&tr("New Text File"), "", &format!("{}.txt", tr("New Document")), &tr("Create"), false).await else { return };
            let name = name.trim().trim_matches('/').to_string();
            if name.is_empty() { return; }
            let key = format!("{prefix}{name}");
            let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
            match bg(async move { c.create_object(&b, &k, Vec::new(), "text/plain; charset=utf-8").await }).await {
                Ok(()) => {
                    win.refresh();
                    crate::transfers::external::open(&win, client, bucket, key);
                }
                Err(error) => win.toast(&error),
            }
        });
    }

    // ----- Object details -----

    /// With several items selected, the details pane sums them up and offers what can be
    /// done with all of them, as the properties of a multiple selection in GNOME Files.
    fn show_selection_summary(&self, entries: &[Entry]) {
        let imp = self.imp();
        imp.details_key.replace(String::new());
        imp.info.replace(None);
        let folders = entries.iter().filter(|e| e.is_folder).count();
        let size: i64 = entries.iter().filter(|e| !e.is_folder).map(|e| e.size).sum();
        let page = adw::PreferencesPage::new();
        let header = adw::PreferencesGroup::new();
        let top = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
        top.append(&gtk::Image::builder().icon_name("emblem-documents-symbolic").pixel_size(80).css_classes(["dim-label"]).margin_bottom(6).build());
        top.append(&gtk::Label::builder().label(trn("{n} item selected", "{n} items selected", &[("n", &entries.len().to_string())])).css_classes(["title-3"]).build());
        let mut detail = format_size(size);
        if folders > 0 {
            detail.push_str(" · ");
            detail.push_str(&trn("{n} folder not counted", "{n} folders not counted", &[("n", &folders.to_string())]));
        }
        top.append(&gtk::Label::builder().label(&detail).css_classes(["dim-label", "numeric"]).build());
        header.add(&top);
        page.add(&header);
        // What can be done with the selection is in the selection bar below; this pane only describes it.
        crate::widgets::code_view::label_icon_buttons(&page);
        imp.details_bin.set_child(Some(&page));
    }

    /// Counts the objects and bytes below a folder, as the properties of a folder in a file manager.
    fn show_folder_details(&self, key: String) {
        let imp = self.imp();
        imp.details_key.replace(key.clone());
        imp.info.replace(None);
        let Some(client) = self.client() else { return };
        let bucket = imp.bucket.borrow().clone();
        let name = key.trim_end_matches('/').rsplit('/').next().unwrap_or(&key).to_string();
        let page = adw::PreferencesPage::new();
        let header = adw::PreferencesGroup::builder().title(glib::markup_escape_text(&name)).build();
        let icon = gtk::Image::builder().icon_name("folder").pixel_size(96).margin_top(12).margin_bottom(12).build();
        header.add(&icon);
        page.add(&header);
        let props = adw::PreferencesGroup::builder().title(tr("Properties")).build();
        let size_row = adw::ActionRow::builder().title(tr("Size")).subtitle(tr("Calculating…")).css_classes(["property"]).build();
        let count_row = adw::ActionRow::builder().title(tr("Objects")).subtitle(tr("Calculating…")).css_classes(["property"]).build();
        let spinner = adw::Spinner::new();
        size_row.add_suffix(&spinner);
        props.add(&adw::ActionRow::builder().title(tr("Key")).subtitle(glib::markup_escape_text(&key)).subtitle_selectable(true).css_classes(["property"]).build());
        props.add(&size_row);
        props.add(&count_row);
        page.add(&props);
        let actions = adw::PreferencesGroup::new();
        let open = adw::ButtonRow::builder().title(tr("Open")).build();
        open.connect_activated(glib::clone!(#[weak(rename_to = win)] self, #[strong] key, move |_| win.navigate(&key)));
        let download = adw::ButtonRow::builder().title(tr("Download…")).build();
        download.connect_activated(glib::clone!(#[weak(rename_to = win)] self, #[strong] key, move |_| win.download(vec![Entry { key: key.clone(), is_folder: true, ..Default::default() }])));
        actions.add(&open);
        actions.add(&download);
        page.add(&actions);
        imp.details_bin.set_child(Some(&page));
        imp.details_split.set_show_sidebar(true);
        let win = self.clone();
        glib::spawn_future_local(async move {
            let k = key.clone();
            let result = bg(async move { client.list_all(&bucket, &k, s3::SCAN_LIMIT).await }).await;
            if *win.imp().details_key.borrow() != key { return; }
            spinner.set_visible(false);
            match result {
                Ok((items, truncated)) => {
                    let files: Vec<&Entry> = items.iter().filter(|e| !e.key.ends_with('/')).collect();
                    let size: i64 = files.iter().map(|e| e.size).sum();
                    let more = if truncated { "+" } else { "" };
                    size_row.set_subtitle(&format!("{}{more} ({} bytes)", format_size(size), size));
                    count_row.set_subtitle(&format!("{}{more}", files.len()));
                }
                Err(error) => { size_row.set_subtitle(&glib::markup_escape_text(&error)); count_row.set_subtitle("—"); }
            }
        });
    }

    pub(crate) fn show_details(&self, key: String) {
        crate::debug!("details {key}");
        let imp = self.imp();
        imp.details_key.replace(key.clone());
        let Some(client) = self.client() else { return };
        let bucket = imp.bucket.borrow().clone();
        let spinner = adw::Spinner::builder().halign(gtk::Align::Center).valign(gtk::Align::Center).width_request(32).height_request(32).build();
        imp.details_bin.set_child(Some(&spinner));
        imp.details_split.set_show_sidebar(true);
        let win = self.clone();
        let requested = key.clone();
        let requested_bucket = bucket.clone();
        glib::spawn_future_local(async move {
            let result = bg(async move {
                let info = client.head_object(&bucket, &key).await?;
                let text_like = info.content_type.starts_with("text/")
                    || ["application/json", "application/xml", "application/javascript", "application/x-yaml", "application/yaml", "application/toml"].iter().any(|t| info.content_type.starts_with(t));
                let image_like = ["image/png", "image/jpeg", "image/gif", "image/webp", "image/bmp", "image/svg+xml"].iter().any(|t| info.content_type.starts_with(t));
                let preview = if (text_like && info.size <= 2_000_000) || (image_like && info.size <= 8 * 1024 * 1024) {
                    client.read_bytes(&bucket, &key, if text_like { 256 * 1024 } else { 8 * 1024 * 1024 }).await.ok()
                } else { None };
                Ok((info, preview, image_like))
            }).await;
            // A newer selection or another folder replaced this request meanwhile.
            // Only a newer selection replaces this request; folder refreshes do not.
            if *win.imp().details_key.borrow() != requested || *win.imp().bucket.borrow() != requested_bucket { return; }
            match result {
                Ok((info, preview, image)) => win.build_details(info, preview, image),
                // The panel stays open and says what went wrong, with a way to retry.
                Err(error) => {
                    let page = adw::StatusPage::builder().icon_name("dialog-warning-symbolic").title(tr("Details Could Not Be Loaded")).description(glib::markup_escape_text(&error)).build();
                    page.add_css_class("compact");
                    let retry = gtk::Button::builder().label(tr("Try Again")).halign(gtk::Align::Center).css_classes(["pill"]).build();
                    retry.connect_clicked(glib::clone!(#[weak] win, move |_| {
                        let key = win.imp().details_key.borrow().clone();
                        win.show_details(key);
                    }));
                    page.set_child(Some(&retry));
                    win.imp().details_bin.set_child(Some(&page));
                }
            }
        });
    }

    fn build_details(&self, info: ObjectInfo, preview: Option<Vec<u8>>, image: bool) {
        let imp = self.imp();
        imp.info.replace(Some(info.clone()));
        let page = adw::PreferencesPage::new();
        let name = info.key.rsplit('/').next().unwrap_or(&info.key).to_string();
        let (key, size) = (info.key.clone(), info.size);
        let win = self.clone();

        // Preview, name and the main actions on top, as the properties of GNOME Files.
        let header = adw::PreferencesGroup::new();
        let top = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
        let picture = preview.as_ref().filter(|_| image).filter(|bytes| crate::widgets::image_fits(bytes)).and_then(|bytes| gdk::Texture::from_bytes(&glib::Bytes::from(bytes)).ok());
        let dimensions = picture.as_ref().map(|t| (t.width(), t.height()));
        match picture {
            Some(texture) => {
                let picture = gtk::Picture::builder().paintable(&texture).content_fit(gtk::ContentFit::Contain).height_request(220).css_classes(["card"]).build();
                top.append(&picture);
            }
            None => {
                let entry = Entry { name: name.clone(), ..Default::default() };
                top.append(&gtk::Image::builder().gicon(&grid_icon_for(&entry)).pixel_size(96).margin_bottom(6).build());
            }
        }
        top.append(&gtk::Label::builder().label(&name).wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar).justify(gtk::Justification::Center).selectable(true).css_classes(["title-3"]).build());
        top.append(&gtk::Label::builder().label(format!("{} · {}", format_size(info.size), format_time(info.modified))).css_classes(["dim-label", "numeric"]).build());
        let buttons = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).margin_top(6).build();
        let download = gtk::Button::builder().label(tr("Download")).css_classes(["pill", "suggested-action"]).build();
        download.connect_clicked(glib::clone!(#[strong] key, #[strong] win, move |_| win.download(vec![Entry { key: key.clone(), size, ..Default::default() }])));
        buttons.append(&download);
        let icon_button = |icon: &str, tip: String| gtk::Button::builder().icon_name(icon).tooltip_text(tip).valign(gtk::Align::Center).css_classes(["circular"]).build();
        let open = icon_button("document-open-symbolic", tr("Open With Default Application"));
        open.set_action_name(Some("win.open-external"));
        let share = icon_button("send-to-symbolic", tr("Share Link…"));
        share.connect_clicked(glib::clone!(#[strong] key, #[strong] win, move |_| win.presign(key.clone())));
        let copy = icon_button("edit-copy-symbolic", tr("Copy Key"));
        copy.connect_clicked(glib::clone!(#[strong] key, #[strong] win, move |_| { win.clipboard().set_text(&key); win.toast(&tr("Key copied")); }));
        let delete = icon_button("user-trash-symbolic", tr("Delete"));
        delete.add_css_class("destructive-action");
        delete.connect_clicked(glib::clone!(#[strong] key, #[strong] win, move |_| win.delete(vec![Entry { key: key.clone(), ..Default::default() }])));
        for b in [&open, &share, &copy, &delete] { buttons.append(b); }
        top.append(&buttons);
        header.add(&top);
        page.add(&header);

        let props = adw::PreferencesGroup::new();
        props.set_title(&tr("Properties"));
        let add = |title: &str, value: &str| {
            if value.is_empty() { return; }
            let row = adw::ActionRow::builder().title(title).subtitle(glib::markup_escape_text(value)).subtitle_selectable(true).build();
            row.add_css_class("property");
            props.add(&row);
        };
        add(&tr("Key"), &info.key);
        if let Some((width, height)) = dimensions {
            add(&tr("Dimensions"), &trf("{w} × {h} pixels", &[("w", &width.to_string()), ("h", &height.to_string())]));
        }
        add(&tr("Size"), &if info.size >= 1000 { format!("{} ({})", format_size(info.size), trn("{n} byte", "{n} bytes", &[("n", &info.size.to_string())])) } else { format_size(info.size) });
        add("Content-Type", &info.content_type);
        add(&tr("Modified"), &full_time(info.modified));
        add("ETag", &info.etag);
        add(&tr("Storage class"), &info.storage_class);
        add("Cache-Control", &info.cache_control);
        add(&tr("Encryption"), &info.encryption);
        add(&tr("Version"), &info.version_id);
        for (k, v) in &info.metadata {
            // The uploaded file's own date, as rclone and this application record it.
            if k == "mtime" && let Some(secs) = v.split('.').next().and_then(|s| s.parse::<i64>().ok()) {
                add(&tr("Original file date"), &full_time(secs));
                continue;
            }
            add(&format!("x-amz-meta-{k}"), v);
        }
        page.add(&props);

        let hash = adw::ActionRow::builder().title("SHA-256").subtitle(tr("Not calculated")).subtitle_selectable(true).subtitle_lines(2).css_classes(["property"]).build();
        let compute = gtk::Button::builder().label(tr("Calculate")).valign(gtk::Align::Center).build();
        hash.add_suffix(&compute);
        let (hash_key, hash_size) = (info.key.clone(), info.size);
        compute.connect_clicked(glib::clone!(#[weak] hash, #[strong] win, move |button| {
            let Some(client) = win.client() else { return };
            let bucket = win.imp().bucket.borrow().clone();
            let (key, button) = (hash_key.clone(), button.clone());
            button.set_visible(false);
            hash.set_subtitle(&tr("Calculating…"));
            let name = key.rsplit('/').next().unwrap_or(&key).to_string();
            let result = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let out = result.clone();
            glib::spawn_future_local(glib::clone!(#[strong] win, async move {
                // Large objects go through the queue, so they show progress and can be cancelled.
                let outcome = win.run_one("download", &name, &tr("SHA-256"), hash_size.max(0) as u64, None, crate::transfers::queue::work(move |progress| {
                    let (client, bucket, key, out) = (client.clone(), bucket.clone(), key.clone(), out.clone());
                    async move { let digest = client.sha256(&bucket, &key, &progress).await?; *out.lock().unwrap() = digest; Ok(()) }
                })).await;
                if outcome.done > 0 { hash.set_subtitle(&result.lock().unwrap()); } else { hash.set_subtitle(&outcome.error); button.set_visible(true); }
            }));
        }));
        props.add(&hash);
        let more = adw::PreferencesGroup::new();
        for (title, icon, action) in [
            (tr("Versions"), "document-open-recent-symbolic", "win.object-versions"),
            (tr("HTTP Headers"), "text-x-generic-symbolic", "win.object-headers"),
            (tr("Tags and Metadata"), "bookmark-new-symbolic", "win.object-tags"),
            (tr("Permissions and Retention"), "system-users-symbolic", "win.object-permissions"),
        ] {
            let row = adw::ActionRow::builder().title(title).activatable(true).action_name(action).build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            more.add(&row);
        }
        page.add(&more);

        if let (Some(bytes), false) = (&preview, image) {
            let text = String::from_utf8_lossy(bytes).into_owned();
            let group = adw::PreferencesGroup::new();
            group.set_title(&tr("Preview"));
            let view = crate::widgets::code_view::new(&text, &info.key);
            view.set_editable(false);
            view.set_wrap_mode(gtk::WrapMode::WordChar);
            view.set_top_margin(8); view.set_bottom_margin(8); view.set_left_margin(8); view.set_right_margin(8);
            // Only a complete text object can be edited and saved back.
            let editable = info.size <= 64 * 1024 && info.content_encoding.is_empty() && !text.contains('\u{FFFD}');
            let frame = gtk::ScrolledWindow::builder().child(&view).min_content_height(220).max_content_height(420).propagate_natural_height(true).build();
            frame.add_css_class("card");
            group.add(&frame);
            if editable {
                let edit = gtk::ToggleButton::builder().label(tr("Edit")).valign(gtk::Align::Center).build();
                edit.add_css_class("flat");
                let save = gtk::Button::builder().label(tr("Save")).valign(gtk::Align::Center).visible(false).build();
                save.add_css_class("suggested-action");
                let suffix = gtk::Box::builder().spacing(6).build();
                suffix.append(&save);
                suffix.append(&edit);
                group.set_header_suffix(Some(&suffix));
                edit.connect_toggled(glib::clone!(#[weak] view, #[weak] save, move |toggle| {
                    view.set_editable(toggle.is_active());
                    save.set_visible(toggle.is_active());
                    if toggle.is_active() { view.grab_focus(); }
                }));
                let (key, etag) = (info.key.clone(), info.etag.clone());
                save.connect_clicked(glib::clone!(#[weak] view, #[strong] win, move |button| {
                    let buffer = view.buffer();
                    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
                    let Some(client) = win.client() else { return };
                    let bucket = win.imp().bucket.borrow().clone();
                    let (key, etag, win, button) = (key.clone(), etag.clone(), win.clone(), button.clone());
                    button.set_sensitive(false);
                    glib::spawn_future_local(async move {
                        let target = key.clone();
                        match bg(async move { client.save_text(&bucket, &target, text, &etag).await }).await {
                            Ok(()) => { win.toast(&tr("Changes saved")); win.refresh(); win.show_details(key); }
                            Err(error) => { button.set_sensitive(true); win.toast(&error); }
                        }
                    });
                }));
                // Ctrl+S in the editor saves, before the window's own Ctrl+S (select matching).
                let shortcuts = gtk::ShortcutController::new();
                shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string("<Control>s"),
                    Some(gtk::CallbackAction::new(glib::clone!(#[weak] save, #[upgrade_or] glib::Propagation::Proceed, move |_, _| {
                        if save.is_visible() && save.is_sensitive() { save.emit_clicked(); }
                        glib::Propagation::Stop
                    })))));
                view.add_controller(shortcuts);
            }
            page.add(&group);
        }
        crate::widgets::code_view::label_icon_buttons(&page);
        imp.details_bin.set_child(Some(&page));
    }

    pub(crate) fn presign(&self, key: String) {
        let Some(client) = self.client() else { return };
        crate::dialogs::share::present(self, client, self.imp().bucket.borrow().clone(), key);
    }

    fn copy_link(&self) {
        let keys: Vec<String> = self.selected_entries().into_iter().filter(|e| !e.is_folder).map(|e| e.key).collect();
        match keys.as_slice() {
            [] => {}
            [key] => self.presign(key.clone()),
            _ => if let Some(client) = self.client() {
                crate::dialogs::share::present_many(self, client, self.imp().bucket.borrow().clone(), keys);
            },
        }
    }

    pub(crate) fn rename_selected(&self) {
        let files: Vec<Entry> = self.selected_entries().into_iter().filter(|e| !e.is_folder).collect();
        if files.len() > 1 {
            let Some(client) = self.client() else { return };
            let imp = self.imp();
            let existing = imp.store.borrow().as_ref().map(|s| (0..s.n_items()).filter_map(|i| s.item(i)).map(|o| entry_of(&o).name).collect()).unwrap_or_default();
            crate::dialogs::batch_rename::present(self, client, imp.bucket.borrow().clone(), imp.prefix.borrow().clone(), files, existing);
            return;
        }
        let Some(entry) = files.into_iter().next() else { return };
        let Some(client) = self.client() else { return };
        let bucket = self.imp().bucket.borrow().clone();
        let win = self.clone();
        glib::spawn_future_local(async move {
            let folder = entry.key[..entry.key.len() - entry.name.len()].to_string();
            let Some(name) = win.ask_text(&tr("Rename"), &tr("A name with “/” moves the file into that folder."), &entry.name, &tr("Rename"), false).await else { return };
            let target = format!("{folder}{}", name.trim_start_matches('/'));
            if target == entry.key { return; }
            let (c, b, from, to) = (client.clone(), bucket.clone(), entry.key.clone(), target.clone());
            match bg(async move { c.rename(&b, &from, &to).await }).await {
                Ok(()) => {
                    win.refresh();
                    crate::pages::recent::forget(&client.profile.id, &bucket, std::slice::from_ref(&entry.key));
                    // Undo (or Ctrl+Z) gives the object its old name back.
                    let shown = target.rsplit('/').next().unwrap_or(&target).to_string();
                    let toast = adw::Toast::builder().title(glib::markup_escape_text(&trf("Renamed to “{name}”", &[("name", &shown)]))).button_label(tr("Undo")).timeout(6).build();
                    toast.connect_button_clicked(glib::clone!(#[weak] win, move |_| {
                        let (client, bucket, from, to) = (client.clone(), bucket.clone(), target.clone(), entry.key.clone());
                        glib::spawn_future_local(async move {
                            if let Err(error) = bg(async move { client.rename(&bucket, &from, &to).await }).await { win.toast(&error); }
                            win.refresh();
                        });
                    }));
                    win.offer_undo(&toast);
                }
                Err(error) => win.toast(&error),
            }
        });
    }

    fn storage_class_selected(&self) {
        let keys: Vec<String> = self.selected_entries().into_iter().filter(|e| !e.is_folder).map(|e| e.key).collect();
        if keys.is_empty() { return; }
        let Some(client) = self.client() else { return };
        let bucket = self.imp().bucket.borrow().clone();
        let dialog = adw::AlertDialog::new(Some(&tr("Change Storage Class")), Some(&trn("{n} object will be copied in place into the new storage class.", "{n} objects will be copied in place into the new storage class.", &[("n", &keys.len().to_string())])));
        let dropdown = gtk::DropDown::from_strings(connection::STORAGE_CLASSES);
        dialog.set_extra_child(Some(&dropdown));
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("apply", &tr("Apply"))]);
        dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
        dialog.set_close_response("cancel");
        let win = self.clone();
        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&win)).await != "apply" { return; }
            let class = connection::STORAGE_CLASSES[dropdown.selected() as usize].to_string();
            match bg(async move { client.set_storage_class(&bucket, keys, &class).await }).await {
                Ok(n) => { win.toast(&trn("Storage class of {n} object changed", "Storage class of {n} objects changed", &[("n", &n.to_string())])); win.refresh(); }
                Err(error) => win.toast(&error),
            }
        });
    }

    /// Sets Cache-Control or Content-Type on every selected file at once.
    fn headers_selected(&self) {
        let keys: Vec<String> = self.selected_entries().into_iter().filter(|e| !e.is_folder).map(|e| e.key).collect();
        if keys.is_empty() { return; }
        let Some(client) = self.client() else { return };
        let bucket = self.imp().bucket.borrow().clone();
        let dialog = adw::AlertDialog::new(Some(&tr("Edit Headers")),
            Some(&trn("Empty fields keep the current value of the object.", "Empty fields keep the current value of each of the {n} objects.", &[("n", &keys.len().to_string())])));
        let group = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        let cache = adw::EntryRow::builder().title("Cache-Control").build();
        let presets = gio::Menu::new();
        for value in ["no-cache", "public, max-age=3600", "public, max-age=86400", "public, max-age=31536000, immutable"] {
            let item = gio::MenuItem::new(Some(value), None);
            item.set_action_and_target_value(Some("headers.preset"), Some(&value.to_variant()));
            presets.append_item(&item);
        }
        let preset_button = gtk::MenuButton::builder().icon_name("view-more-symbolic").tooltip_text(tr("Common Values")).menu_model(&presets).valign(gtk::Align::Center).css_classes(["flat"]).build();
        cache.add_suffix(&preset_button);
        let actions = gio::SimpleActionGroup::new();
        let preset = gio::SimpleAction::new("preset", Some(glib::VariantTy::STRING));
        preset.connect_activate(glib::clone!(#[weak] cache, move |_, value| if let Some(v) = value.and_then(|v| v.get::<String>()) { cache.set_text(&v) }));
        actions.add_action(&preset);
        group.insert_action_group("headers", Some(&actions));
        let content_type = adw::EntryRow::builder().title("Content-Type").build();
        group.append(&cache);
        group.append(&content_type);
        dialog.set_extra_child(Some(&group));
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("apply", &tr("Apply"))]);
        dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("apply"));
        dialog.set_close_response("cancel");
        let win = self.clone();
        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&win)).await != "apply" { return; }
            let value = |row: &adw::EntryRow| Some(row.text().trim().to_string()).filter(|t| !t.is_empty());
            let (cache_control, content_type) = (value(&cache), value(&content_type));
            if cache_control.is_none() && content_type.is_none() { return; }
            let count = keys.len();
            let summary = std::sync::Arc::new(std::sync::Mutex::new((0usize, 0usize, String::new())));
            let out = summary.clone();
            let outcome = win.run_one("copy", &tr("Edit Headers"), &bucket.clone(), count as u64, None, crate::transfers::queue::work(move |progress| {
                let (client, bucket, keys, cache_control, content_type, out) = (client.clone(), bucket.clone(), keys.clone(), cache_control.clone(), content_type.clone(), out.clone());
                async move {
                    let result = client.set_headers(&bucket, keys, cache_control, content_type, &progress).await?;
                    *out.lock().unwrap() = result;
                    Ok(())
                }
            })).await;
            if outcome.done == 0 { return; }
            let (changed, failed, error) = summary.lock().unwrap().clone();
            if failed > 0 {
                win.toast(&trf("{n} objects could not be changed: {error}", &[("n", &failed.to_string()), ("error", &error)]));
            } else {
                win.toast(&trn("Headers of {n} object changed", "Headers of {n} objects changed", &[("n", &changed.to_string())]));
            }
            win.refresh();
        });
    }

    fn delete_selected(&self) {
        self.delete(self.selected_entries());
    }

    fn delete(&self, entries: Vec<Entry>) {
        if entries.is_empty() { return; }
        let Some(client) = self.client() else { return };
        let bucket = self.imp().bucket.borrow().clone();
        let folders = entries.iter().filter(|e| e.key.ends_with('/')).count();
        let body = if folders > 0 {
            trf("{n} items will be deleted ({f} folders, including their contents).", &[("n", &entries.len().to_string()), ("f", &folders.to_string())])
        } else if entries.len() == 1 {
            trf("“{name}” will be deleted permanently.", &[("name", entries[0].key.rsplit('/').next().unwrap_or(&entries[0].key))])
        } else {
            trn("{n} item will be deleted permanently.", "{n} items will be deleted permanently.", &[("n", &entries.len().to_string())])
        };
        let win = self.clone();
        glib::spawn_future_local(async move {
            // Folders take everything below them, so they are still confirmed;
            // single objects use the undo toast GNOME prefers over confirmation.
            let heading = if entries.len() == 1 { tr("Delete Folder?") } else { tr("Delete Items?") };
            if folders > 0 && !win.confirm(&heading, &body, &tr("Delete")).await { return; }
            let keys: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
            let imp = win.imp();
            imp.details_split.set_show_sidebar(false);
            if let Some(store) = imp.store.borrow().as_ref() {
                store.retain(|object| !keys.contains(&entry_of(object).key));
            }
            win.update_selection();
            let title = if keys.len() == 1 {
                trf("“{name}” deleted", &[("name", keys[0].trim_end_matches('/').rsplit('/').next().unwrap_or(&keys[0]))])
            } else {
                trn("{n} item deleted", "{n} items deleted", &[("n", &keys.len().to_string())])
            };
            let toast = adw::Toast::builder().title(glib::markup_escape_text(&title)).button_label(tr("Undo")).timeout(5).priority(adw::ToastPriority::High).build();
            let undone = Rc::new(Cell::new(false));
            toast.connect_button_clicked(glib::clone!(#[strong] undone, #[weak] win, move |_| { undone.set(true); win.refresh(); }));
            toast.connect_dismissed(glib::clone!(#[weak] win, move |_| {
                if undone.get() { return; }
                let (client, bucket, keys) = (client.clone(), bucket.clone(), keys.clone());
                crate::pages::recent::forget(&client.profile.id, &bucket, &keys);
                glib::spawn_future_local(async move {
                    // Folders may hold many objects: that work is shown in the transfers.
                    if folders > 0 {
                        let title = trn("Delete {n} item", "Delete {n} items", &[("n", &keys.len().to_string())]);
                        let outcome = win.run_one("delete", &title, &bucket.clone(), 0, None, crate::transfers::queue::work(move |_| {
                            let (client, bucket, keys) = (client.clone(), bucket.clone(), keys.clone());
                            async move { client.delete_keys(&bucket, keys).await.map(|_| ()) }
                        })).await;
                        if outcome.failed > 0 { win.toast(&outcome.error); }
                    } else if let Err(error) = bg(async move { client.delete_keys(&bucket, keys).await }).await {
                        win.toast(&error);
                    }
                    // Part of a batch may be gone even when it failed.
                    win.refresh();
                });
            }));
            win.offer_undo(&toast);
        });
    }

    /// Ctrl+S: selects the items whose names match a pattern such as *.jpg, as in GNOME Files.
    fn select_matching(&self) {
        let win = self.clone();
        glib::spawn_future_local(async move {
            let Some(pattern) = win.ask_text(&tr("Select Items Matching"), &tr("Use * for any characters and ? for one, for example *.jpg"), "*", &tr("Select"), false).await else { return };
            let Some(selection) = win.imp().selection.borrow().clone() else { return };
            let pattern = pattern.to_lowercase();
            let mut count = 0;
            selection.unselect_all();
            for i in 0..selection.n_items() {
                let Some(object) = selection.item(i) else { continue };
                if wildcard_match(&pattern, &entry_of(&object).name.trim_end_matches('/').to_lowercase()) {
                    selection.select_item(i, false);
                    count += 1;
                }
            }
            if count == 0 { win.toast(&trf("No item matches “{pattern}”", &[("pattern", &pattern)])); }
        });
    }

    /// Writes the objects below the open folder to a CSV file, for spreadsheets and audits.
    fn export_listing(&self) {
        let Some(client) = self.client() else { return };
        let (bucket, prefix) = (self.imp().bucket.borrow().clone(), self.imp().prefix.borrow().clone());
        let base = prefix.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(&bucket).to_string();
        let dialog = gtk::FileDialog::builder().title(tr("Export List")).modal(true).initial_name(format!("{base}.csv")).build();
        dialog.set_initial_folder(Some(&gio::File::for_path(download_folder())));
        let win = self.clone();
        glib::spawn_future_local(async move {
            let Ok(file) = dialog.save_future(Some(&win)).await else { return };
            let Some(path) = file.path() else { return };
            win.toast(&tr("Listing the folder…"));
            let written = bg(async move {
                let (items, truncated) = client.list_all(&bucket, &prefix, usize::MAX).await?;
                let count = items.iter().filter(|e| !e.key.ends_with('/')).count();
                std::fs::write(&path, s3::listing_csv(&items)).map_err(|e| e.to_string())?;
                Ok((count, truncated))
            }).await;
            match written {
                Ok((count, _)) => win.toast(&trn("{n} object exported", "{n} objects exported", &[("n", &count.to_string())])),
                Err(error) => win.toast(&error),
            }
        });
    }

    /// Alt+Return: shows the details of the selection, or of the open folder; hides them when shown.
    fn toggle_properties(&self) {
        let imp = self.imp();
        if imp.bucket.borrow().is_empty() { return; }
        if imp.details_split.shows_sidebar() {
            imp.details_split.set_show_sidebar(false);
            imp.details_key.replace(String::new());
            return;
        }
        match self.selected_entries().as_slice() {
            [entry] if entry.is_folder => self.show_folder_details(entry.key.clone()),
            [entry] => self.show_details(entry.key.clone()),
            _ => {
                let prefix = imp.prefix.borrow().clone();
                if !prefix.is_empty() { self.show_folder_details(prefix); }
            }
        }
    }

    /// Shows a toast with an Undo button that Ctrl+Z also presses.
    pub(crate) fn offer_undo(&self, toast: &adw::Toast) {
        let imp = self.imp();
        imp.undo_toast.replace(Some(toast.clone()));
        toast.connect_dismissed(glib::clone!(#[weak(rename_to = win)] self, move |toast| {
            let mut current = win.imp().undo_toast.borrow_mut();
            if current.as_ref() == Some(toast) { *current = None; }
        }));
        imp.toasts.add_toast(toast.clone());
    }

    fn undo(&self) {
        let toast = self.imp().undo_toast.take();
        match toast {
            Some(toast) => {
                toast.emit_by_name::<()>("button-clicked", &[]);
                toast.dismiss();
            }
            None => self.toast(&tr("Nothing to undo")),
        }
    }

    // ----- Transfers -----

    fn setup_transfers(&self) {
        let imp = self.imp();
        let queue = crate::transfers::queue::Queue::new(settings().int("transfer-limit").clamp(1, 16) as usize);
        imp.transfers_bin.set_child(Some(&crate::transfers::view::build(&queue, self)));
        let header_pie = crate::widgets::pie::Pie::new(16);
        header_pie.widget.set_visible(false);
        imp.transfers_content.prepend(&header_pie.widget);
        let bar_pie = crate::widgets::pie::Pie::new(28);
        imp.transfers_bar_pie.set_child(Some(&bar_pie.widget));
        imp.header_pie.replace(Some(header_pie));
        imp.bar_pie.replace(Some(bar_pie));
        // The summary bar only exists while the queue has something to show.
        imp.transfers_sheet.set_bottom_bar(None::<&gtk::Widget>);
        // The summary bar lies over the content; the content makes room for it so the
        // status line and the selection bar stay visible.
        imp.transfers_sheet.connect_bottom_bar_height_notify(|sheet| {
            if let Some(content) = sheet.content() { content.set_margin_bottom(sheet.bottom_bar_height()); }
        });
        queue.connect_changed(glib::clone!(#[weak(rename_to = win)] self, move |summary| win.queue_changed(summary)));
        // Closing the panel after the work is done also lets the bar go.
        imp.transfers_sheet.connect_open_notify(glib::clone!(#[weak(rename_to = win)] self, move |sheet| {
            if sheet.is_open() { return; }
            glib::timeout_add_local_once(std::time::Duration::from_secs(2), glib::clone!(#[weak] win, move || {
                let s = win.queue().summary();
                if s.active() == 0 && s.failed == 0 && s.held == 0 && !win.imp().transfers_sheet.is_open() { win.set_transfers_bar(false); }
            }));
        }));
        queue.connect_finished(glib::clone!(#[weak(rename_to = win)] self, #[strong] queue, move |item, quiet| {
            if quiet { return; }
            if item.state() == crate::transfers::queue::DONE && let Some(spec) = queue.spec_of(item) {
                let text = |name: &str| spec.get(name).and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let profile = text("profile");
                let profile_name = win.imp().profiles.borrow().iter().find(|p| p.id == profile).map(|p| p.name.clone()).unwrap_or_default();
                let action = if text("kind") == "upload" { "uploaded" } else { "downloaded" };
                crate::pages::recent::record(&profile, &profile_name, &text("bucket"), &text("key"), action);
            }
            let (ok, failed) = win.imp().finished.get();
            match item.state().as_str() {
                crate::transfers::queue::DONE => win.imp().finished.set((ok + 1, failed)),
                crate::transfers::queue::FAILED => win.imp().finished.set((ok, failed + 1)),
                _ => {}
            }
        }));
        imp.queue.replace(Some(queue));
    }

    /// Closes the transfers sheet, to show what a transfer row points at.
    pub(crate) fn close_transfers(&self) {
        self.imp().transfers_sheet.set_open(false);
    }

    pub(crate) fn queue(&self) -> crate::transfers::queue::Queue {
        self.imp().queue.borrow().clone().expect("queue is set up with the window")
    }

    fn queue_changed(&self, summary: &crate::transfers::queue::Summary) {
        let imp = self.imp();
        let active = summary.active();
        let fraction = summary.fraction();
        // The pies fill completely when the work is done, and stay a moment before the
        // header pie hides and the bar shows how it went.
        let fraction = if active == 0 && summary.done > 0 { 1.0 } else { fraction };
        if let Some(pie) = imp.header_pie.borrow().as_ref() {
            pie.set_fraction(fraction);
            if active > 0 { pie.widget.set_visible(true); }
        }
        if let Some(pie) = imp.bar_pie.borrow().as_ref() {
            pie.set_fraction(fraction);
            if active > 0 && imp.transfers_bar_pie.child().as_ref() != Some(pie.widget.upcast_ref()) {
                imp.transfers_bar_pie.set_child(Some(&pie.widget));
            }
        }
        if active == 0 {
            let failed = summary.failed > 0;
            glib::timeout_add_local_once(std::time::Duration::from_millis(900), glib::clone!(#[weak(rename_to = win)] self, move || {
                let imp = win.imp();
                if win.queue().summary().active() > 0 { return; }
                if let Some(pie) = imp.header_pie.borrow().as_ref() { pie.widget.set_visible(false); }
                let (icon, class) = if failed { ("dialog-error-symbolic", "error") } else { ("emblem-ok-symbolic", "success") };
                let image = gtk::Image::builder().icon_name(icon).pixel_size(24).css_classes([class]).build();
                imp.transfers_bar_pie.set_child(Some(&image));
            }));
        }
        imp.transfers_icon.set_visible(active == 0);
        self.update_inhibit(active > 0);
        imp.transfers_bar_title.set_text(&if active > 0 {
            trn("{n} transfer running", "{n} transfers running", &[("n", &active.to_string())])
        } else if summary.failed > 0 {
            trn("{n} transfer failed", "{n} transfers failed", &[("n", &summary.failed.to_string())])
        } else {
            tr("Transfers finished")
        });
        // While idle the title already says how it went; the subtitle only counts.
        imp.transfers_bar_subtitle.set_text(&if active > 0 { crate::transfers::view::summary_line(summary) } else {
            trn("{n} transfer", "{n} transfers", &[("n", &(summary.done + summary.failed).to_string())])
        });
        // The summary bar shows while work runs, and when something failed or is paused.
        // Finished work leaves it a few seconds later, as the operations button of GNOME
        // Files does; the transfers button in the header still opens the list.
        let needs_bar = active > 0 || summary.failed > 0 || summary.held > 0;
        if needs_bar {
            self.set_transfers_bar(true);
        } else if imp.transfers_sheet.bottom_bar().is_some() {
            glib::timeout_add_local_once(std::time::Duration::from_secs(5), glib::clone!(#[weak(rename_to = win)] self, move || {
                let s = win.queue().summary();
                let open = win.imp().transfers_sheet.is_open();
                if s.active() == 0 && s.failed == 0 && s.held == 0 && !open { win.set_transfers_bar(false); }
            }));
        }
        crate::application::set_background_status(&if active > 0 { crate::transfers::view::summary_line(summary) } else { String::new() });
        crate::application::set_launcher_progress(active, fraction);
        // The queue just emptied while the user was elsewhere.
        if imp.last_active.replace(active) > 0 && active == 0 && !self.is_active() {
            let (ok, failed) = imp.finished.get();
            crate::application::notify_transfers(ok, failed);
        }
    }

    /// Attaches or removes the summary bar under the content, only when that changes.
    fn set_transfers_bar(&self, shown: bool) {
        let imp = self.imp();
        if shown == imp.transfers_sheet.bottom_bar().is_some() { return; }
        imp.transfers_sheet.set_bottom_bar(if shown { Some(imp.transfers_bar.upcast_ref::<gtk::Widget>()) } else { None });
    }

    /// Queues one job and resolves when it ended.
    pub(crate) async fn run_one(&self, kind: &str, name: &str, detail: &str, total: u64, local: Option<&str>, work: crate::transfers::queue::Work) -> crate::transfers::queue::Outcome {
        self.run_one_kept(kind, name, detail, total, local, None, work).await
    }

    /// Like `run_one`; with a spec the job is kept for the next start if it does not finish.
    pub(crate) async fn run_one_kept(&self, kind: &str, name: &str, detail: &str, total: u64, local: Option<&str>, spec: Option<serde_json::Value>, work: crate::transfers::queue::Work) -> crate::transfers::queue::Outcome {
        let queue = self.queue();
        let (sender, receiver) = futures_channel::oneshot::channel();
        let batch = queue.batch(move |outcome| { let _ = sender.send(outcome); });
        let item = queue.add(Some(batch), kind, name, detail, total, local, work);
        if let Some(spec) = spec { queue.set_spec(&item, spec); }
        queue.seal(batch);
        receiver.await.unwrap_or_default()
    }

    fn pick_upload(&self, folder: bool) {
        let dialog = gtk::FileDialog::builder().title(if folder { tr("Upload Folder") } else { tr("Upload Files") }).modal(true).build();
        let win = self.clone();
        glib::spawn_future_local(async move {
            let paths: Vec<PathBuf> = if folder {
                match dialog.select_folder_future(Some(&win)).await { Ok(file) => file.path().into_iter().collect(), Err(_) => return }
            } else {
                match dialog.open_multiple_future(Some(&win)).await {
                    Ok(files) => (0..files.n_items()).filter_map(|i| files.item(i).and_downcast::<gio::File>()).filter_map(|f| f.path()).collect(),
                    Err(_) => return,
                }
            };
            win.upload(paths);
        });
    }

    pub(crate) fn upload(&self, paths: Vec<PathBuf>) {
        let prefix = self.imp().prefix.borrow().clone();
        self.upload_into(paths, prefix);
    }

    /// Uploads local files and folders into a folder of the open bucket.
    pub(crate) fn upload_into(&self, paths: Vec<PathBuf>, prefix: String) {
        let imp = self.imp();
        let Some(client) = self.client() else { return };
        let bucket = imp.bucket.borrow().clone();
        if bucket.is_empty() || paths.is_empty() { return; }
        let win = self.clone();
        glib::spawn_future_local(async move {
            // Walking large local folders happens off the main thread.
            let (p, local) = (prefix.clone(), paths.clone());
            let jobs = match bg(async move { tokio::task::spawn_blocking(move || s3::collect_uploads(&p, &local)).await.map_err(|e| e.to_string())? }).await {
                Ok(jobs) => jobs,
                Err(error) => { win.toast(&error); return; }
            };
            if jobs.is_empty() { return; }
            // Existing objects with the same names are found first; the user decides what to do with them.
            // Only the places the upload writes to are listed: the folder itself for files, and each uploaded folder.
            let (c, b, p) = (client.clone(), bucket.clone(), prefix.clone());
            let folders: Vec<String> = paths.iter().filter(|path| path.is_dir())
                .filter_map(|path| path.file_name().map(|n| format!("{p}{}/", n.to_string_lossy()))).collect();
            let has_files = paths.iter().any(|path| !path.is_dir());
            let existing = bg(async move {
                let mut found = Vec::new();
                if has_files {
                    let mut token = String::new();
                    loop {
                        let listing = c.list_objects(&b, &p, &token).await?;
                        found.extend(listing.items.into_iter().filter(|e| !e.is_folder));
                        if listing.next_token.is_empty() || found.len() > 200_000 { break; }
                        token = listing.next_token;
                    }
                }
                for folder in folders {
                    found.extend(c.list_all(&b, &folder, 200_000).await?.0);
                }
                Ok(found)
            }).await
                .map(|items| items.into_iter().map(|e| (e.key.clone(), e)).collect::<std::collections::HashMap<_, _>>())
                .unwrap_or_default();
            let conflicts: Vec<&(PathBuf, String, u64)> = jobs.iter().filter(|j| existing.contains_key(&j.1)).collect();
            let jobs = if conflicts.is_empty() {
                jobs
            } else {
                let Some(choice) = win.ask_conflict(&conflicts, &existing).await else { return };
                let taken: std::collections::HashSet<String> = existing.keys().cloned().collect();
                jobs.into_iter().filter_map(|(path, key, size)| {
                    if !taken.contains(&key) { return Some((path, key, size)); }
                    match choice {
                        Conflict::Replace => Some((path, key, size)),
                        Conflict::ReplaceOlder => (modified_secs(&path) > existing[&key].modified).then_some((path, key, size)),
                        Conflict::Skip => None,
                        Conflict::KeepBoth => {
                            let (folder, name) = key.rsplit_once('/').map(|(f, n)| (format!("{f}/"), n.to_string())).unwrap_or((String::new(), key.clone()));
                            let free = (1..).map(|i| format!("{folder}{}", crate::window::actions::copy_name(&name, i))).find(|k| !taken.contains(k)).unwrap();
                            Some((path, free, size))
                        }
                    }
                }).collect()
            };
            if !jobs.is_empty() { win.enqueue_uploads(client, bucket, prefix, jobs); }
        });
    }

    /// Asks what to do with files whose names already exist; None when cancelled.
    async fn ask_conflict(&self, conflicts: &[&(PathBuf, String, u64)], existing: &std::collections::HashMap<String, Entry>) -> Option<Conflict> {
        let newer = conflicts.iter().filter(|c| modified_secs(&c.0) > existing[&c.1].modified).count();
        let (heading, body) = if let [one] = conflicts {
            let name = one.1.rsplit('/').next().unwrap_or(&one.1);
            let old = &existing[&one.1];
            let local = std::fs::metadata(&one.0).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
            (trf("Replace “{name}”?", &[("name", name)]),
             trf("An object with this name already exists.\n\nIn the bucket: {old_size}, {old_date}\nNew file: {new_size}, {new_date}", &[
                ("old_size", &format_size(old.size)), ("old_date", &format_time(old.modified)),
                ("new_size", &format_size(one.2 as i64)), ("new_date", &format_time(local))]))
        } else {
            let mut body = tr("Objects with the same names are already in the bucket. Replacing them overwrites their content.");
            if newer > 0 && newer < conflicts.len() {
                body.push(' ');
                body.push_str(&trn("{n} of the files is newer than its object.", "{n} of the files are newer than their objects.", &[("n", &newer.to_string())]));
            }
            (trn("{n} File Already Exists", "{n} Files Already Exist", &[("n", &conflicts.len().to_string())]), body)
        };
        let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("skip", &tr("Skip")), ("keep", &tr("Keep Both"))]);
        // With several files, only the outdated objects can be replaced.
        if newer > 0 && newer < conflicts.len() { dialog.add_response("older", &tr("Replace Older")); }
        dialog.add_response("replace", &tr("Replace"));
        dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("keep"));
        dialog.set_close_response("cancel");
        match dialog.choose_future(Some(self)).await.as_str() {
            "replace" => Some(Conflict::Replace),
            "older" => Some(Conflict::ReplaceOlder),
            "skip" => Some(Conflict::Skip),
            "keep" => Some(Conflict::KeepBoth),
            _ => None,
        }
    }

    fn enqueue_uploads(&self, client: S3, bucket: String, prefix: String, jobs: Vec<(PathBuf, String, u64)>) {
        let count = jobs.len();
        let queue = self.queue();
        let batch = queue.batch(glib::clone!(#[weak(rename_to = win)] self, move |outcome| {
            win.toast(&if outcome.failed > 0 {
                trn("{n} file could not be uploaded", "{n} files could not be uploaded", &[("n", &outcome.failed.to_string())])
            } else if outcome.done > 0 {
                trn("{n} file uploaded", "{n} files uploaded", &[("n", &outcome.done.to_string())])
            } else {
                tr("Upload cancelled")
            });
            win.refresh();
        }));
        let destination = format!("{bucket}/{prefix}");
        for (path, key, size) in jobs {
            let name = key.rsplit('/').next().unwrap_or(&key).to_string();
            let folder = format!("{bucket}/{}", &key[..key.len() - name.len()]);
            let spec = serde_json::json!({ "kind": "upload", "profile": client.profile.id, "bucket": bucket, "key": key, "path": path, "size": size });
            let (client, bucket) = (client.clone(), bucket.clone());
            let item = queue.add(Some(batch), "upload", &name, &folder, size, Some(&path.display().to_string()), crate::transfers::queue::work(move |progress| {
                let (client, bucket, key, path) = (client.clone(), bucket.clone(), key.clone(), path.clone());
                async move { client.upload_file(&bucket, &key, &path, &progress).await }
            }));
            queue.set_spec(&item, spec);
        }
        queue.seal(batch);
        if count > 1 {
            self.toast(&trn("{n} file queued for {path}", "{n} files queued for {path}", &[("n", &count.to_string()), ("path", &destination)]));
        }
    }

    fn reveal_transfers(&self) {
        self.imp().transfers_sheet.set_open(true);
    }

    pub(crate) fn download_version(&self, key: String, version: String) {
        let Some(client) = self.client() else { return };
        let bucket = self.imp().bucket.borrow().clone();
        let name = key.rsplit('/').next().unwrap_or(&key).to_string();
        let dialog = gtk::FileDialog::builder().title(tr("Save Version")).initial_name(&name).modal(true).build();
        let win = self.clone();
        glib::spawn_future_local(async move {
            let Ok(file) = dialog.save_future(Some(&win)).await else { return };
            let Some(path) = file.path() else { return };
            let local = path.display().to_string();
            let detail = format!("{} · {}", tr("Version"), version);
            let outcome = win.run_one("download", &name, &detail, 0, Some(&local), crate::transfers::queue::work(move |progress| {
                let (client, bucket, key, version, path) = (client.clone(), bucket.clone(), key.clone(), version.clone(), path.clone());
                async move { client.download_file(&bucket, &key, Some(&version), &path, &progress).await }
            })).await;
            if outcome.done > 0 { win.toast(&tr("Download finished")); }
        });
    }

    // ----- Mounts -----

    /// A "Mounted" section in the sidebar, as GNOME Files lists mounted volumes.
    fn setup_mounts(&self) {
        // Changes made in a mounted folder (by GNOME Files, an editor…) show here at once.
        crate::transfers::mount::on_change(glib::clone!(#[weak(rename_to = win)] self, move |profile, bucket, folder| {
            let imp = win.imp();
            let here = imp.client.borrow().as_ref().is_some_and(|c| c.profile.id == profile) && *imp.bucket.borrow() == bucket;
            if !here { return; }
            let key = win.cache_key(bucket, folder);
            imp.listing_cache.borrow_mut().remove(&key);
            if *imp.prefix.borrow() == folder { win.refresh(); }
        }));
        let imp = self.imp();
        let Some(sidebar) = imp.buckets_section.parent().and_downcast::<gtk::Box>() else { return };
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).visible(false).build();
        section.append(&gtk::Label::builder().label(tr("Mounted")).xalign(0.0).css_classes(["sidebar-heading"]).build());
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["navigation-sidebar"]).build();
        section.append(&list);
        sidebar.append(&section);
        imp.mounts_section.replace(Some((section, list)));
    }

    fn update_mounts(&self) {
        let Some((section, list)) = self.imp().mounts_section.borrow().clone() else { return };
        list.remove_all();
        let mounts = crate::transfers::mount::list();
        section.set_visible(!mounts.is_empty());
        for (id, label, path, read_only, ..) in mounts {
            let row = sidebar_row("drive-harddisk-symbolic", &label);
            row.set_tooltip_text(Some(&format!("{}{}", path.display(), if read_only { format!(" · {}", tr("read only")) } else { String::new() })));
            let row_box = row.child().and_downcast::<gtk::Box>().unwrap();
            let eject = gtk::Button::builder().icon_name("media-eject-symbolic").tooltip_text(tr("Unmount")).valign(gtk::Align::Center).css_classes(["flat"]).build();
            eject.connect_clicked(glib::clone!(#[weak(rename_to = win)] self, move |_| {
                match crate::transfers::mount::unmount(id) {
                    Ok(()) => win.toast(&tr("Unmounted")),
                    Err(error) => win.toast(&error),
                }
                win.update_mounts();
            }));
            row_box.append(&eject);
            let gesture = gtk::GestureClick::new();
            gesture.connect_released(glib::clone!(#[weak(rename_to = win)] self, move |_, _, _, _| win.open_folder(&path)));
            row.add_controller(gesture);
            list.append(&row);
        }
    }

    pub(crate) fn open_folder(&self, path: &std::path::Path) {
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
        let win = self.clone();
        launcher.launch(Some(self), gio::Cancellable::NONE, move |result| {
            if let Err(error) = result { win.toast(&error.to_string()); }
        });
    }

    fn mount_current(&self) {
        let Some(client) = self.client() else { return };
        let (bucket, prefix) = (self.imp().bucket.borrow().clone(), self.imp().prefix.borrow().clone());
        let dialog = adw::AlertDialog::new(Some(&tr("Mount as Folder")),
            Some(&tr("The location appears as a normal folder in GNOME Files. The mount point is created in the “Ferry” folder of your home directory.")));
        let group = adw::PreferencesGroup::new();
        let write = adw::SwitchRow::builder().title(tr("Allow changes")).subtitle(tr("Saved files are uploaded when closed; deleting and renaming change the bucket")).active(true).build();
        group.add(&write);
        dialog.set_extra_child(Some(&group));
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("mount", &tr("Mount"))]);
        dialog.set_response_appearance("mount", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("mount"));
        dialog.set_close_response("cancel");
        let win = self.clone();
        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&win)).await != "mount" { return; }
            match crate::transfers::mount::mount(client, &bucket, &prefix, !write.is_active()) {
                Ok((_, path)) => {
                    win.update_mounts();
                    let toast = adw::Toast::builder().title(glib::markup_escape_text(&trf("Mounted at {path}", &[("path", &path.display().to_string())]))).button_label(tr("Open")).build();
                    toast.connect_button_clicked(glib::clone!(#[weak] win, move |_| win.open_folder(&path)));
                    win.imp().toasts.add_toast(toast);
                }
                Err(error) => win.toast(&error),
            }
        });
    }

    // ----- Favorites -----

    fn favorites_store() -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(&settings().string("favorites")).unwrap_or_default()
    }

    fn update_favorites(&self) {
        let imp = self.imp();
        let profile_id = imp.client.borrow().as_ref().map(|c| c.profile.id.clone()).unwrap_or_default();
        let list: Vec<(String, String)> = Self::favorites_store().get(&profile_id).and_then(|v| v.as_array()).map(|items| {
            items.iter().filter_map(|i| Some((i.get("bucket")?.as_str()?.to_string(), i.get("prefix")?.as_str()?.to_string()))).collect()
        }).unwrap_or_default();
        imp.favorites_list.remove_all();
        for (bucket, prefix) in &list {
            let label = if prefix.is_empty() { bucket.clone() } else { prefix.trim_end_matches('/').rsplit('/').next().unwrap_or(prefix).to_string() };
            let row = sidebar_row("starred-symbolic", &label);
            row.set_tooltip_text(Some(&format!("{bucket}/{prefix}")));
            imp.favorites_list.append(&row);
        }
        imp.favorites_section.set_visible(!list.is_empty());
        let current = (imp.bucket.borrow().clone(), imp.prefix.borrow().clone());
        let starred = list.contains(&current);
        imp.favorite_button.set_icon_name(if starred { "starred-symbolic" } else { "non-starred-symbolic" });
        imp.favorite_button.set_tooltip_text(Some(&if starred { tr("Remove from Favorites") } else { tr("Add to Favorites") }));
        imp.favorites.replace(list);
    }

    fn toggle_favorite(&self) {
        let imp = self.imp();
        let Some(profile_id) = imp.client.borrow().as_ref().map(|c| c.profile.id.clone()) else { return };
        let (bucket, prefix) = (imp.bucket.borrow().clone(), imp.prefix.borrow().clone());
        let mut store = Self::favorites_store();
        let mut items: Vec<serde_json::Value> = store.get(&profile_id).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let entry = serde_json::json!({ "bucket": bucket, "prefix": prefix });
        if let Some(index) = items.iter().position(|i| *i == entry) { items.remove(index); } else { items.push(entry); }
        store.insert(profile_id, serde_json::Value::Array(items));
        let _ = settings().set_string("favorites", &serde_json::Value::Object(store).to_string());
        self.update_favorites();
    }

    fn download_selected(&self) {
        self.download(self.selected_entries());
    }

    pub(crate) fn download(&self, entries: Vec<Entry>) {
        if entries.is_empty() { return; }
        let Some(client) = self.client() else { return };
        let (bucket, prefix) = (self.imp().bucket.borrow().clone(), self.imp().prefix.borrow().clone());
        let dialog = gtk::FileDialog::builder().title(tr("Download To")).modal(true).build();
        dialog.set_initial_folder(Some(&gio::File::for_path(download_folder())));
        let win = self.clone();
        glib::spawn_future_local(async move {
            let dir = if settings().boolean("ask-download-folder") {
                let Ok(folder) = dialog.select_folder_future(Some(&win)).await else { return };
                let Some(dir) = folder.path() else { return };
                dir
            } else {
                download_folder()
            };
            // Folders are listed first; then every file becomes one job.
            let (c, b) = (client.clone(), bucket.clone());
            let listed = bg(async move {
                let mut targets = Vec::new();
                for entry in entries {
                    if entry.key.ends_with('/') {
                        let (items, _) = c.list_all(&b, &entry.key, usize::MAX).await?;
                        targets.extend(items.into_iter().filter(|e| !e.key.ends_with('/')).map(|e| (e.key, e.size.max(0) as u64)));
                    } else {
                        targets.push((entry.key, entry.size.max(0) as u64));
                    }
                }
                Ok(targets)
            }).await;
            let targets = match listed { Ok(t) => t, Err(error) => { win.toast(&error); return; } };
            let queue = win.queue();
            let folder = dir.clone();
            let batch = queue.batch(glib::clone!(#[weak] win, move |outcome| {
                if outcome.failed > 0 {
                    win.toast(&trn("{n} file could not be downloaded", "{n} files could not be downloaded", &[("n", &outcome.failed.to_string())]));
                } else if outcome.done > 0 {
                    if !win.is_active() {
                        crate::application::notify_with_folder(&tr("Download Finished"), &trn("{n} file downloaded", "{n} files downloaded", &[("n", &outcome.done.to_string())]), &folder);
                    }
                    let toast = adw::Toast::builder().title(tr("Download finished")).button_label(tr("Open Folder")).build();
                    toast.connect_button_clicked(glib::clone!(#[weak] win, #[strong] folder, move |_| win.open_folder(&folder)));
                    win.imp().toasts.add_toast(toast);
                }
            }));
            for (key, size) in targets {
                let path = match s3::download_target(&dir, &prefix, &key) { Ok(p) => p, Err(error) => { win.toast(&error); continue; } };
                let name = key.rsplit('/').next().unwrap_or(&key).to_string();
                let detail = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                let spec = serde_json::json!({ "kind": "download", "profile": client.profile.id, "bucket": bucket, "key": key, "path": path, "size": size });
                let (client, bucket) = (client.clone(), bucket.clone());
                let item = queue.add(Some(batch), "download", &name, &detail, size, Some(&path.display().to_string()), crate::transfers::queue::work(move |progress| {
                    let (client, bucket, key, path) = (client.clone(), bucket.clone(), key.clone(), path.clone());
                    async move { client.download_sized(&bucket, &key, size, &path, &progress).await }
                }));
                queue.set_spec(&item, spec);
            }
            queue.seal(batch);
        });
    }

    /// Downloads the selection, or the open folder, as one ZIP file.
    pub(crate) fn download_archive(&self) {
        let Some(client) = self.client() else { return };
        let (bucket, prefix) = (self.imp().bucket.borrow().clone(), self.imp().prefix.borrow().clone());
        if bucket.is_empty() { return; }
        let mut entries = self.selected_entries();
        let base = match entries.as_slice() {
            [] => prefix.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(&bucket).to_string(),
            [one] => one.key.trim_end_matches('/').rsplit('/').next().unwrap_or(&one.key).rsplit_once('.').map(|(s, _)| s.to_string())
                .unwrap_or_else(|| one.key.trim_end_matches('/').rsplit('/').next().unwrap_or(&one.key).to_string()),
            _ => prefix.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(&bucket).to_string(),
        };
        if entries.is_empty() {
            entries.push(Entry { key: prefix.clone(), is_folder: true, ..Default::default() });
        }
        let dialog = gtk::FileDialog::builder().title(tr("Download as Archive")).modal(true).initial_name(format!("{base}.zip")).build();
        dialog.set_initial_folder(Some(&gio::File::for_path(download_folder())));
        let win = self.clone();
        glib::spawn_future_local(async move {
            let Ok(file) = dialog.save_future(Some(&win)).await else { return };
            let Some(path) = file.path() else { return };
            let (c, b) = (client.clone(), bucket.clone());
            let listed = bg(async move {
                let mut keys = Vec::new();
                let mut total = 0u64;
                for entry in entries {
                    if entry.key.ends_with('/') || entry.key.is_empty() {
                        let (items, _) = c.list_all(&b, &entry.key, usize::MAX).await?;
                        for item in items {
                            total += item.size.max(0) as u64;
                            keys.push(item.key);
                        }
                    } else {
                        total += entry.size.max(0) as u64;
                        keys.push(entry.key);
                    }
                }
                Ok((keys, total))
            }).await;
            let (keys, total) = match listed { Ok(l) => l, Err(error) => { win.toast(&error); return; } };
            if keys.is_empty() { win.toast(&tr("There is nothing to put in the archive")); return; }
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let detail = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
            let local = path.display().to_string();
            let outcome = win.run_one("download", &name, &detail, total, Some(&local), crate::transfers::queue::work(move |progress| {
                let (client, bucket, prefix, keys, path) = (client.clone(), bucket.clone(), prefix.clone(), keys.clone(), path.clone());
                async move { client.download_archive(&bucket, &prefix, keys, &path, &progress).await }
            })).await;
            if outcome.done > 0 {
                let toast = adw::Toast::builder().title(glib::markup_escape_text(&trf("“{name}” is ready", &[("name", &name)]))).button_label(tr("Open Folder")).build();
                let folder = std::path::PathBuf::from(&detail);
                toast.connect_button_clicked(glib::clone!(#[weak] win, move |_| win.open_folder(&folder)));
                win.imp().toasts.add_toast(toast);
            } else if outcome.failed > 0 {
                win.toast(&outcome.error);
            }
        });
    }

    /// Files dropped on the list are uploaded into the open folder.
    fn setup_drop(&self) {
        let imp = self.imp();
        let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let area = imp.details_split.get();
        let overlay = imp.drop_overlay.get();
        target.connect_enter(glib::clone!(#[weak] overlay, #[weak(rename_to = win)] self, #[upgrade_or] gdk::DragAction::empty(), move |_, _, _| {
            let destination = format!("{}/{}", win.imp().bucket.borrow(), win.imp().prefix.borrow());
            overlay.set_description(Some(&glib::markup_escape_text(&trf("Files are uploaded to {path}", &[("path", &destination)]))));
            crate::widgets::pie::fade(&overlay, true);
            gdk::DragAction::COPY
        }));
        target.connect_leave(glib::clone!(#[weak] overlay, move |_| crate::widgets::pie::fade(&overlay, false)));
        target.connect_drop(glib::clone!(#[weak(rename_to = win)] self, #[weak] overlay, #[upgrade_or] false, move |_, value, _, _| {
            crate::widgets::pie::fade(&overlay, false);
            let Ok(files) = value.get::<gdk::FileList>() else { return false };
            win.upload(files.files().iter().filter_map(|f| f.path()).collect());
            true
        }));
        area.add_controller(target);
    }

    /// The toast overlay, for dialogs that keep their toasts on screen.
    pub(crate) fn imp_toasts(&self) -> adw::ToastOverlay {
        self.imp().toasts.get()
    }

    pub(crate) fn reload_profiles(&self) {
        self.load_profiles();
    }
}

fn config_is_devel() -> bool {
    cfg!(debug_assertions)
}

#[cfg(test)]
mod tests {
    use super::{natural_cmp, wildcard_match};

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.jpg", "beach.jpg"));
        assert!(!wildcard_match("*.jpg", "beach.jpeg"));
        assert!(wildcard_match("img_??.png", "img_07.png"));
        assert!(!wildcard_match("img_??.png", "img_7.png"));
        assert!(wildcard_match("*report*", "2024 report final.pdf"));
        assert!(wildcard_match("*", ""));
        assert!(!wildcard_match("a*b", "acd"));
    }

    #[test]
    fn natural_order() {
        let mut names = vec!["photo 10.jpg", "Photo 2.jpg", "photo 1.jpg", "notes", "a", "B", "file007", "file7b", "file10"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["a", "B", "file007", "file7b", "file10", "notes", "photo 1.jpg", "Photo 2.jpg", "photo 10.jpg"]);
        // Speed: sorting 20,000 names stays well below what a user would notice.
        let mut many: Vec<String> = (0..20_000).rev().map(|i| format!("IMG_{i}.jpg")).collect();
        let start = std::time::Instant::now();
        many.sort_by(|a, b| natural_cmp(a, b));
        assert!(start.elapsed().as_millis() < 500, "{:?}", start.elapsed());
        assert_eq!(many[1], "IMG_1.jpg");
    }
}
