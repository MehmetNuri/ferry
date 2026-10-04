//! Backup jobs: saved folder-to-bucket syncs, run on request or on a schedule
//! while the application runs, also in the background.
use adw::prelude::*;
use gtk::glib;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use crate::i18n::{tr, trf, trn};
use crate::profile;
use crate::s3::S3;
use crate::window::Window;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub profile_id: String,
    pub bucket: String,
    pub prefix: String,
    pub dir: String,
    pub mirror: bool,
    pub interval_minutes: u32,
    /// Unix seconds of the last run; 0 when it never ran.
    pub last_run: i64,
    pub last_status: String,
    pub last_message: String,
    /// Back up changes as soon as files in the folder change.
    pub watch: bool,
}

fn path() -> PathBuf {
    profile::config_dir().join("backups.json")
}

pub fn load() -> Vec<Job> {
    std::fs::read(path()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
}

fn store(jobs: &[Job]) -> Result<(), String> {
    profile::write_private(&path(), &serde_json::to_vec_pretty(jobs).map_err(|e| e.to_string())?)
}

fn update(id: &str, f: impl FnOnce(&mut Job)) {
    let mut jobs = load();
    if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
        f(job);
        let _ = store(&jobs);
    }
}

const INTERVALS: [u32; 5] = [0, 15, 60, 360, 1440];

fn interval_labels() -> Vec<String> {
    vec![tr("Manual only"), tr("Every 15 minutes"), tr("Every hour"), tr("Every 6 hours"), tr("Every day")]
}

struct Page {
    win: glib::WeakRef<Window>,
    list: adw::PreferencesGroup,
    rows: RefCell<Vec<gtk::Widget>>,
    running: RefCell<HashSet<String>>,
    watchers: RefCell<std::collections::HashMap<String, Rc<Watcher>>>,
    /// Jobs that changed again while they were running.
    dirty: RefCell<HashSet<String>>,
}

/// Directory monitors of one watched folder, recursively.
struct Watcher {
    dir: String,
    monitors: RefCell<Vec<gtk::gio::FileMonitor>>,
    timer: RefCell<Option<glib::SourceId>>,
}

const WATCH_DIRS_LIMIT: usize = 5000;

thread_local! {
    static PAGE: RefCell<Option<Rc<Page>>> = const { RefCell::new(None) };
}

/// Re-reads the saved jobs, for example after they were changed outside the page.
pub fn refresh_jobs() {
    if let Some(page) = PAGE.with(|p| p.borrow().clone()) {
        page.reload();
        page.sync_watchers();
    }
}

/// Adds or replaces a job; used by the smoke test.
pub fn save_job(job: Job) -> Result<(), String> {
    let mut jobs = load();
    jobs.retain(|j| j.id != job.id);
    jobs.push(job);
    store(&jobs)
}

pub fn remove_job(id: &str) -> Result<(), String> {
    let mut jobs = load();
    jobs.retain(|j| j.id != id);
    store(&jobs)
}

impl Page {
    fn win(&self) -> Option<Window> {
        self.win.upgrade()
    }

    fn reload(self: &Rc<Self>) {
        for row in self.rows.borrow_mut().drain(..) {
            self.list.remove(&row);
        }
        let jobs = load();
        let profiles = profile::load();
        let labels = interval_labels();
        if jobs.is_empty() {
            let row = adw::ActionRow::builder().title(tr("No backup jobs yet")).subtitle(tr("Link a local folder to a bucket of this connection above.")).build();
            self.list.add(&row);
            self.rows.borrow_mut().push(row.upcast());
        }
        for job in jobs {
            let running = self.running.borrow().contains(&job.id);
            let profile_name = profiles.iter().find(|p| p.id == job.profile_id).map(|p| p.name.clone()).unwrap_or_else(|| "—".into());
            let schedule = INTERVALS.iter().position(|i| *i == job.interval_minutes).map(|i| labels[i].clone()).unwrap_or_default();
            let last = if job.last_run > 0 { crate::window::format_time(job.last_run) } else { tr("never ran") };
            let mode = if job.mirror { tr("Mirror") } else { tr("Add and update only") };
            let schedule = if job.watch { tr("Watching for changes") } else { schedule };
            let mut subtitle = format!("{} → {}:{}/{}\n{schedule} · {mode} · {last}", job.dir, profile_name, job.bucket, job.prefix);
            if !job.last_message.is_empty() { subtitle.push_str(" · "); subtitle.push_str(&job.last_message); }
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(&job.name)).subtitle(glib::markup_escape_text(&subtitle)).subtitle_lines(3).build();
            let icon = if running { "emblem-synchronizing-symbolic" } else if job.last_status == "error" { "dialog-warning-symbolic" } else if job.last_status == "ok" { "emblem-ok-symbolic" } else { "document-save-symbolic" };
            let image = gtk::Image::from_icon_name(icon);
            if job.last_status == "error" && !running { image.add_css_class("error"); }
            row.add_prefix(&image);
            let run = gtk::Button::builder().label(tr("Run Now")).valign(gtk::Align::Center).sensitive(!running).build();
            let delete = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text(tr("Delete")).valign(gtk::Align::Center).sensitive(!running).build();
            delete.add_css_class("flat");
            let watch = gtk::Switch::builder().active(job.watch).valign(gtk::Align::Center).tooltip_text(tr("Back up changes automatically")).build();
            let (this, id) = (self.clone(), job.id.clone());
            watch.connect_active_notify(move |switch| {
                let on = switch.is_active();
                update(&id, |j| j.watch = on);
                this.sync_watchers();
                if on { this.run(&id); }
            });
            row.add_suffix(&watch);
            row.add_suffix(&run);
            row.add_suffix(&delete);
            let (this, id) = (self.clone(), job.id.clone());
            run.connect_clicked(move |_| this.run(&id));
            let (this, job_name, id) = (self.clone(), job.name.clone(), job.id.clone());
            delete.connect_clicked(move |button| {
                let (this, id, button) = (this.clone(), id.clone(), button.clone());
                let dialog = adw::AlertDialog::new(Some(&tr("Delete Backup Job?")), Some(&trf("“{name}” is removed. Files already backed up stay in the bucket.", &[("name", &job_name)])));
                dialog.add_responses(&[("cancel", &tr("Cancel")), ("delete", &tr("Delete"))]);
                dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                dialog.set_close_response("cancel");
                glib::spawn_future_local(async move {
                    if dialog.choose_future(Some(&button)).await != "delete" { return; }
                    let mut jobs = load();
                    jobs.retain(|j| j.id != id);
                    if let Err(error) = store(&jobs) && let Some(win) = this.win() { win.toast(&error); }
                    this.reload();
                });
            });
            self.list.add(&row);
            self.rows.borrow_mut().push(row.upcast());
        }
    }

    fn run(self: &Rc<Self>, id: &str) {
        let Some(win) = self.win() else { return };
        let Some(job) = load().into_iter().find(|j| j.id == id) else { return };
        if !self.running.borrow_mut().insert(job.id.clone()) {
            return;
        }
        self.reload();
        let Some(stored) = profile::load().into_iter().find(|p| p.id == job.profile_id) else {
            update(&job.id, |j| { j.last_status = "error".into(); j.last_message = tr("The connection of this job was deleted"); });
            self.running.borrow_mut().remove(&job.id);
            self.reload();
            return;
        };
        let message = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let out = message.clone();
        let job2 = job.clone();
        let this = self.clone();
        let detail = format!("{} → {}/{}", job.dir, job.bucket, job.prefix);
        glib::spawn_future_local(async move {
            let outcome = win.run_one("backup", &job.name, &detail, 0, Some(&job.dir), crate::transfers::queue::work(move |progress| {
                let (stored, job2, out) = (stored.clone(), job2.clone(), out.clone());
                async move {
                    let client = S3::connect(profile::with_secrets(stored).await?).await?;
                    let result = client.sync_up(&job2.bucket, &job2.prefix, std::path::Path::new(&job2.dir), job2.mirror, &progress).await?;
                    *out.lock().unwrap() = trf("{t} uploaded, {s} up to date, {d} deleted", &[("t", &result.transferred.to_string()), ("s", &result.skipped.to_string()), ("d", &result.deleted.to_string())]);
                    if result.failed > 0 { Err(trn("{n} file could not be transferred", "{n} files could not be transferred", &[("n", &result.failed.to_string())])) } else { Ok(()) }
                }
            })).await;
            let ok = outcome.done > 0;
            let text = message.lock().unwrap().clone();
            update(&job.id, |j| {
                j.last_run = glib::real_time() / 1_000_000;
                j.last_status = if ok { "ok".into() } else { "error".into() };
                j.last_message = if ok { text } else if outcome.cancelled > 0 { tr("Cancelled") } else { outcome.error.clone() };
            });
            this.running.borrow_mut().remove(&job.id);
            // A failure nobody is looking at is reported by a notification.
            if !ok && outcome.cancelled == 0 && !win.is_active() {
                crate::application::notify_backup_failed(&job.name, &outcome.error);
            }
            this.reload();
            if this.dirty.borrow_mut().remove(&job.id) { this.changed(&job.id); }
        });
    }

    /// Starts or stops folder monitors to match the jobs that watch for changes.
    fn sync_watchers(self: &Rc<Self>) {
        let jobs = load();
        let wanted: std::collections::HashMap<String, String> = jobs.iter().filter(|j| j.watch).map(|j| (j.id.clone(), j.dir.clone())).collect();
        self.watchers.borrow_mut().retain(|id, w| wanted.get(id).is_some_and(|dir| *dir == w.dir));
        for (id, dir) in wanted {
            if self.watchers.borrow().contains_key(&id) { continue; }
            let watcher = Rc::new(Watcher { dir: dir.clone(), monitors: RefCell::default(), timer: RefCell::default() });
            self.watchers.borrow_mut().insert(id.clone(), watcher.clone());
            self.watch_tree(&id, &watcher, std::path::Path::new(&dir));
        }
    }

    fn watch_tree(self: &Rc<Self>, id: &str, watcher: &Rc<Watcher>, root: &std::path::Path) {
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            if watcher.monitors.borrow().len() >= WATCH_DIRS_LIMIT { break; }
            let file = gtk::gio::File::for_path(&dir);
            let Ok(monitor) = file.monitor_directory(gtk::gio::FileMonitorFlags::WATCH_MOVES, gtk::gio::Cancellable::NONE) else { continue };
            let (this, job, w) = (Rc::downgrade(self), id.to_string(), Rc::downgrade(watcher));
            monitor.connect_changed(move |_, file, _, event| {
                let (Some(this), Some(w)) = (this.upgrade(), w.upgrade()) else { return };
                // New subfolders are watched too.
                if matches!(event, gtk::gio::FileMonitorEvent::Created | gtk::gio::FileMonitorEvent::MovedIn)
                    && file.query_file_type(gtk::gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, gtk::gio::Cancellable::NONE) == gtk::gio::FileType::Directory
                    && let Some(path) = file.path() {
                    this.watch_tree(&job, &w, &path);
                }
                if matches!(event, gtk::gio::FileMonitorEvent::ChangesDoneHint | gtk::gio::FileMonitorEvent::Created | gtk::gio::FileMonitorEvent::Deleted
                    | gtk::gio::FileMonitorEvent::MovedIn | gtk::gio::FileMonitorEvent::MovedOut | gtk::gio::FileMonitorEvent::Renamed) {
                    this.changed(&job);
                }
            });
            watcher.monitors.borrow_mut().push(monitor);
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|t| t.is_dir()) { pending.push(entry.path()); }
                }
            }
        }
    }

    /// Runs a watched job a few seconds after the last change, once the burst settles.
    fn changed(self: &Rc<Self>, id: &str) {
        let Some(watcher) = self.watchers.borrow().get(id).cloned() else { return };
        // On a metered connection the change is remembered and backed up later.
        if self.running.borrow().contains(id) || metered() {
            self.dirty.borrow_mut().insert(id.to_string());
            return;
        }
        if let Some(timer) = watcher.timer.borrow_mut().take() { timer.remove(); }
        let (this, job) = (Rc::downgrade(self), id.to_string());
        let w = Rc::downgrade(&watcher);
        let timer = glib::timeout_add_seconds_local_once(5, move || {
            if let Some(w) = w.upgrade() { w.timer.borrow_mut().take(); }
            if let Some(this) = this.upgrade() { this.run(&job); }
        });
        watcher.timer.replace(Some(timer));
    }

    /// Starts every scheduled job whose interval has passed.
    fn tick(self: &Rc<Self>) {
        // Automatic backups wait while the connection is metered, as GNOME Software waits with updates.
        if metered() { return; }
        let waiting: Vec<String> = self.dirty.borrow().iter().filter(|id| !self.running.borrow().contains(*id)).cloned().collect();
        for id in waiting {
            self.dirty.borrow_mut().remove(&id);
            self.changed(&id);
        }
        let now = glib::real_time() / 1_000_000;
        for job in load() {
            if job.interval_minutes > 0 && now - job.last_run >= job.interval_minutes as i64 * 60 && !self.running.borrow().contains(&job.id) {
                self.run(&job.id);
            }
        }
    }
}

/// Automatic backups wait on a metered connection and in Power Saver mode.
fn metered() -> bool {
    gio::NetworkMonitor::default().is_network_metered() || gio::PowerProfileMonitor::get_default().is_power_saver_enabled()
}

fn waiting_reason() -> Option<String> {
    if gio::NetworkMonitor::default().is_network_metered() {
        Some(tr("Metered connection: scheduled and automatic backups wait until it is not"))
    } else if gio::PowerProfileMonitor::get_default().is_power_saver_enabled() {
        Some(tr("Power Saver is on: scheduled and automatic backups wait until it is off"))
    } else {
        None
    }
}

pub fn attach(win: &Window, bin: &adw::Bin) {
    let page = adw::PreferencesPage::new();
    let form = adw::PreferencesGroup::builder().title(tr("New Backup Job"))
        .description(tr("Scheduled backups run only while the application is running, including in the background.")).build();
    let name = adw::EntryRow::builder().title(tr("Backup name")).build();
    let bucket = adw::EntryRow::builder().title(tr("Destination bucket")).build();
    let prefix = adw::EntryRow::builder().title(tr("Destination folder (optional)")).text("backups/").build();
    let mode = adw::ComboRow::builder().title(tr("Sync type")).model(&gtk::StringList::new(&[&tr("Add and update only"), &tr("Mirror: delete extras from the bucket")])).build();
    let labels = interval_labels();
    let schedule = adw::ComboRow::builder().title(tr("Schedule")).model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())).build();
    let watch = adw::SwitchRow::builder().title(tr("Back up changes automatically")).subtitle(tr("Changed files are uploaded a few seconds after they are saved")).build();
    let create = adw::ButtonRow::builder().title(tr("Choose Folder and Create…")).start_icon_name("folder-open-symbolic").build();
    mode.connect_selected_notify(|row| {
        row.set_subtitle(&if row.selected() == 1 { tr("Objects that are no longer in the folder are deleted from the bucket. Nothing is deleted if the folder is empty.") } else { String::new() });
    });
    for row in [name.upcast_ref::<gtk::Widget>(), bucket.upcast_ref(), prefix.upcast_ref(), mode.upcast_ref(), schedule.upcast_ref(), watch.upcast_ref(), create.upcast_ref()] {
        form.add(row);
    }
    page.add(&form);
    let list = adw::PreferencesGroup::builder().title(tr("Backup Jobs")).build();
    page.add(&list);
    let banner = adw::Banner::new("");
    let show_reason = glib::clone!(#[weak] banner, move || {
        let reason = waiting_reason();
        banner.set_title(reason.as_deref().unwrap_or_default());
        banner.set_revealed(reason.is_some());
    });
    show_reason();
    gio::NetworkMonitor::default().connect_network_metered_notify(glib::clone!(#[strong] show_reason, move |_| show_reason()));
    gio::PowerProfileMonitor::get_default().connect_power_saver_enabled_notify(move |_| show_reason());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    page.set_vexpand(true);
    content.append(&page);
    bin.set_child(Some(&content));

    let state = Rc::new(Page { win: win.downgrade(), list, rows: RefCell::default(), running: RefCell::default(), watchers: RefCell::default(), dirty: RefCell::default() });
    state.reload();
    state.sync_watchers();
    PAGE.with(|p| p.replace(Some(state.clone())));

    // The open bucket is the natural destination.
    {
        let (bucket, win) = (bucket.clone(), win.downgrade());
        bin.connect_map(move |_| {
            if let Some(win) = win.upgrade() && bucket.text().is_empty() {
                bucket.set_text(&win.open_bucket_name());
            }
        });
    }
    let state2 = state.clone();
    create.connect_activated(move |_| {
        let Some(win) = state2.win() else { return };
        let Some(client) = win.current_client() else { win.toast(&tr("Connect to a storage service first")); return };
        let job_name = name.text().trim().to_string();
        let bucket_name = bucket.text().trim().to_string();
        if job_name.is_empty() { win.toast(&tr("A backup name is required")); return; }
        if bucket_name.is_empty() { win.toast(&tr("A destination bucket is required")); return; }
        let mut folder = prefix.text().trim().trim_start_matches('/').to_string();
        if !folder.is_empty() && !folder.ends_with('/') { folder.push('/'); }
        let (mirror, interval, watching) = (mode.selected() == 1, INTERVALS[schedule.selected() as usize], watch.is_active());
        let (state, name) = (state2.clone(), name.clone());
        let dialog = gtk::FileDialog::builder().title(tr("Folder to Back Up")).modal(true).build();
        glib::spawn_future_local(async move {
            let Ok(file) = dialog.select_folder_future(Some(&win)).await else { return };
            let Some(dir) = file.path() else { return };
            let mut jobs = load();
            jobs.push(Job {
                id: glib::uuid_string_random().to_string(), name: job_name, profile_id: client.profile.id.clone(),
                bucket: bucket_name, prefix: folder, dir: dir.display().to_string(), mirror, interval_minutes: interval, watch: watching, ..Default::default()
            });
            match store(&jobs) {
                Ok(()) => { name.set_text(""); win.toast(&tr("Backup job created")); }
                Err(error) => win.toast(&error),
            }
            state.reload();
            state.sync_watchers();
        });
    });

    glib::timeout_add_seconds_local(60, move || {
        if state.win().is_none() { return glib::ControlFlow::Break; }
        state.tick();
        glib::ControlFlow::Continue
    });
}
