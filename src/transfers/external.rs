use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use crate::i18n::{tr, trf};
use crate::runtime::bg;
use crate::s3::{Progress, S3};
use crate::transfers::queue;
use crate::window::Window;

const BLOCKED: &[&str] = &[
    "desktop",
    "sh",
    "bash",
    "zsh",
    "fish",
    "csh",
    "ksh",
    "tcsh",
    "run",
    "bin",
    "appimage",
    "exe",
    "msi",
    "bat",
    "cmd",
    "com",
    "ps1",
    "psm1",
    "jar",
    "jnlp",
    "py",
    "pyw",
    "pl",
    "rb",
    "php",
    "lua",
    "tcl",
    "js",
    "mjs",
    "cjs",
    "vbs",
    "vbe",
    "wsf",
    "wsh",
    "hta",
    "scr",
    "cpl",
    "lnk",
    "reg",
    "deb",
    "rpm",
    "flatpak",
    "flatpakref",
    "flatpakrepo",
    "snap",
    "apk",
    "dmg",
    "pkg",
    "elf",
    "out",
    "so",
    "dll",
];

const BLOCKED_TYPES: &[&str] = &[
    "application/x-executable",
    "application/x-sharedlib",
    "application/x-shellscript",
    "application/x-desktop",
    "application/x-msdownload",
    "application/x-ms-dos-executable",
    "application/x-java-archive",
    "application/x-perl",
    "application/x-python",
    "text/x-python",
    "application/x-ruby",
    "application/javascript",
    "application/x-php",
];

pub fn safe_to_launch(name: &str, head: Option<&[u8]>) -> bool {
    // "x.sh." and "x.sh " must not slip through with an empty extension.
    let trimmed = name.trim_end_matches(['.', ' ']);
    let extension = trimmed.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    if BLOCKED.contains(&extension.as_str()) {
        return false;
    }
    let (content_type, _) = gio::content_type_guess(Some(trimmed), head);
    !BLOCKED_TYPES.iter().any(|t| gio::content_type_is_a(&content_type, t))
}

struct Edit {
    _monitor: gio::FileMonitor,
    path: PathBuf,
    timer: Option<glib::SourceId>,
}

thread_local! {
    static EDITS: RefCell<HashMap<String, Rc<RefCell<Edit>>>> = RefCell::new(HashMap::new());
}

fn edit_dir() -> PathBuf {
    let dir = glib::user_cache_dir().join("ferry").join("edit").join(glib::uuid_string_random().as_str());
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    dir
}

pub fn cleanup() {
    EDITS.with(|e| e.borrow_mut().clear());
    let _ = std::fs::remove_dir_all(glib::user_cache_dir().join("ferry").join("edit"));
}

pub fn open(win: &Window, client: S3, bucket: String, key: String) {
    open_with(win, client, bucket, key, None);
}

pub fn choose(win: &Window, client: S3, bucket: String, key: String) {
    let name = key.rsplit('/').next().unwrap_or(&key).to_string();
    let (content_type, _) = gio::content_type_guess(Some(name.as_str()), None::<&[u8]>);
    let apps = gio::AppInfo::all_for_type(&content_type);
    if apps.is_empty() {
        open(win, client, bucket, key);
        return;
    }
    let dialog = adw::Dialog::builder().title(tr("Open With")).content_width(420).build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder().description(glib::markup_escape_text(&name)).build();
    let default = gio::AppInfo::default_for_type(&content_type, false).map(|a| a.id().unwrap_or_default().to_string());
    for app in apps {
        let row =
            adw::ActionRow::builder().title(glib::markup_escape_text(&app.display_name())).activatable(true).build();
        if let Some(icon) = app.icon() {
            row.add_prefix(&gtk::Image::builder().gicon(&icon).pixel_size(32).build());
        }
        if default.as_deref() == app.id().as_deref() {
            row.set_subtitle(&tr("Default application"));
        }
        let (win, client, bucket, key, dialog) =
            (win.clone(), client.clone(), bucket.clone(), key.clone(), dialog.clone());
        row.connect_activated(move |_| {
            dialog.close();
            open_with(&win, client.clone(), bucket.clone(), key.clone(), Some(app.clone()));
        });
        group.add(&row);
    }
    page.add(&group);
    view.set_content(Some(&page));
    dialog.set_child(Some(&view));
    dialog.present(Some(win));
}

fn open_with(win: &Window, client: S3, bucket: String, key: String, app: Option<gio::AppInfo>) {
    let name = key.rsplit('/').next().unwrap_or(&key).to_string();
    if !safe_to_launch(&name, None) || name == "." || name == ".." {
        win.toast(&tr("This file type is not opened with an application for safety; download it and open it yourself"));
        return;
    }
    let id = format!("{}\n{}\n{}", client.profile.id, bucket, key);
    if let Some(edit) = EDITS.with(|e| e.borrow().get(&id).cloned()) {
        launch(win, &edit.borrow().path, app.as_ref());
        return;
    }
    if EDITS.with(|e| e.borrow().len()) >= 20 {
        win.toast(&trf("At most {n} objects can be open in other applications at the same time", &[("n", "20")]));
        return;
    }
    let path = edit_dir().join(&name);
    let win = win.clone();
    glib::spawn_future_local(async move {
        let (c, b, k, p) = (client.clone(), bucket.clone(), key.clone(), path.clone());
        if let Err(error) = bg(async move { c.download_file(&b, &k, None, &p, &Progress::default()).await }).await {
            win.toast(&error);
            return;
        }
        let head: Vec<u8> = std::fs::read(&path).map(|d| d.into_iter().take(4096).collect()).unwrap_or_default();
        if !safe_to_launch(&name, Some(&head)) {
            let _ = std::fs::remove_file(&path);
            win.toast(&tr(
                "This file type is not opened with an application for safety; download it and open it yourself",
            ));
            return;
        }
        let file = gio::File::for_path(&path);
        let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::WATCH_HARD_LINKS, gio::Cancellable::NONE) else {
            launch(&win, &path, app.as_ref());
            return;
        };
        let edit = Rc::new(RefCell::new(Edit { _monitor: monitor.clone(), path: path.clone(), timer: None }));
        // Editors save in bursts (temp file, rename, attrs), so wait for it to settle.
        monitor.connect_changed(glib::clone!(
            #[weak]
            win,
            #[strong]
            edit,
            #[strong]
            client,
            #[strong]
            bucket,
            #[strong]
            key,
            move |_, _, _, event| {
                if !matches!(
                    event,
                    gio::FileMonitorEvent::ChangesDoneHint
                        | gio::FileMonitorEvent::Created
                        | gio::FileMonitorEvent::Renamed
                        | gio::FileMonitorEvent::MovedIn
                ) {
                    return;
                }
                if let Some(timer) = edit.borrow_mut().timer.take() {
                    timer.remove();
                }
                let (client, bucket, key, path, win2, edit2) = (
                    client.clone(),
                    bucket.clone(),
                    key.clone(),
                    edit.borrow().path.clone(),
                    win.clone(),
                    edit.clone(),
                );
                let timer = glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
                    edit2.borrow_mut().timer = None;
                    upload(&win2, client, bucket, key, path);
                });
                edit.borrow_mut().timer = Some(timer);
            }
        ));
        EDITS.with(|e| e.borrow_mut().insert(id, edit));
        crate::pages::recent::record(&client.profile.id, &client.profile.name, &bucket, &key, "opened");
        launch(&win, &path, app.as_ref());
        win.toast(&tr("Opened in the default application; it is uploaded again when you save"));
    });
}

fn upload(win: &Window, client: S3, bucket: String, key: String, path: PathBuf) {
    if !path.exists() {
        return;
    }
    let name = key.rsplit('/').next().unwrap_or(&key).to_string();
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let win = win.clone();
    glib::spawn_future_local(async move {
        let local = path.display().to_string();
        let outcome = win
            .run_one(
                "upload",
                &name,
                &format!("{bucket}/{key}"),
                size,
                Some(&local),
                queue::work(move |progress| {
                    let (client, bucket, key, path) = (client.clone(), bucket.clone(), key.clone(), path.clone());
                    async move { client.upload_file(&bucket, &key, &path, &progress).await }
                }),
            )
            .await;
        if outcome.done > 0 {
            win.toast(&trf("{name} changed and was uploaded again", &[("name", &name)]));
            win.refresh();
        } else if outcome.failed > 0 {
            win.toast(&trf(
                "{name} could not be uploaded again: {error}",
                &[("name", &name), ("error", &outcome.error)],
            ));
        }
    });
}

fn launch(win: &Window, path: &std::path::Path, app: Option<&gio::AppInfo>) {
    if let Some(app) = app {
        let context = gtk::prelude::WidgetExt::display(win).app_launch_context();
        if let Err(error) = app.launch(&[gio::File::for_path(path)], Some(&context)) {
            win.toast(&error.to_string());
        }
        return;
    }
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    let win2 = win.clone();
    launcher.launch(Some(win), gio::Cancellable::NONE, move |result| {
        if let Err(error) = result {
            win2.toast(&error.to_string());
        }
    });
}
