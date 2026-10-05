use adw::prelude::*;
use gtk::{gio, glib};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

use crate::i18n::{tr, trn};
use crate::window::Window;

const LIMIT: usize = 200;

#[derive(Clone, Serialize, Deserialize)]
pub struct Recent {
    pub profile: String,
    pub profile_name: String,
    pub bucket: String,
    pub key: String,
    pub action: String,
    pub time: i64,
}

thread_local! {
    static ITEMS: RefCell<Option<Vec<Recent>>> = const { RefCell::new(None) };
    static VIEW: RefCell<Option<View>> = const { RefCell::new(None) };
}

fn path() -> std::path::PathBuf {
    // smoke runs use their own history
    if std::env::var_os("FERRY_SMOKE").is_some() {
        return std::env::temp_dir().join(format!("ferry-smoke-recent-{}.json", std::process::id()));
    }
    crate::profile::config_dir().join("recent.json")
}

fn with_items<R>(f: impl FnOnce(&mut Vec<Recent>) -> R) -> R {
    ITEMS.with(|items| {
        let mut items = items.borrow_mut();
        let list = items.get_or_insert_with(|| {
            std::fs::read(path()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
        });
        f(list)
    })
}

pub fn remembering() -> bool {
    let Some(source) = gio::SettingsSchemaSource::default() else { return true };
    if source.lookup("org.gnome.desktop.privacy", true).is_none() {
        return true;
    }
    gio::Settings::new("org.gnome.desktop.privacy").boolean("remember-recent-files")
}

fn save(items: &[Recent]) {
    if let Ok(data) = serde_json::to_vec(items) {
        let _ = crate::profile::write_private(&path(), &data);
    }
}

pub fn record(profile: &str, profile_name: &str, bucket: &str, key: &str, action: &str) {
    if key.is_empty() || key.ends_with('/') || !remembering() {
        return;
    }
    // don't store cleartext vault names
    if crate::s3::vault::find(profile, bucket, key).is_some() {
        return;
    }
    with_items(|items| {
        items.retain(|r| !(r.profile == profile && r.bucket == bucket && r.key == key));
        items.insert(
            0,
            Recent {
                profile: profile.into(),
                profile_name: profile_name.into(),
                bucket: bucket.into(),
                key: key.into(),
                action: action.into(),
                time: glib::real_time() / 1_000_000,
            },
        );
        items.truncate(LIMIT);
        save(items);
    });
    rebuild();
}

pub fn forget(profile: &str, bucket: &str, keys: &[String]) {
    let changed = with_items(|items| {
        let before = items.len();
        items.retain(|r| {
            !(r.profile == profile
                && r.bucket == bucket
                && keys.iter().any(|k| r.key == *k || (k.ends_with('/') && r.key.starts_with(k.as_str()))))
        });
        let changed = items.len() != before;
        if changed {
            save(items);
        }
        changed
    });
    if changed {
        rebuild();
    }
}

pub fn forget_profile(profile: &str) {
    with_items(|items| {
        items.retain(|r| r.profile != profile);
        save(items);
    });
    rebuild();
}

fn clear() {
    with_items(|items| {
        items.clear();
        save(items);
    });
    rebuild();
}

pub fn count() -> usize {
    with_items(|items| items.len())
}

struct View {
    win: glib::WeakRef<Window>,
    content: glib::WeakRef<adw::Bin>,
    entry: glib::WeakRef<gtk::SearchEntry>,
}

pub fn attach(win: &Window, bin: &adw::Bin) {
    let entry = gtk::SearchEntry::builder().placeholder_text(tr("Search all connections")).hexpand(true).build();
    let clamp = adw::Clamp::builder()
        .maximum_size(600)
        .child(&entry)
        .margin_top(12)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    let content = adw::Bin::builder().vexpand(true).build();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&clamp);
    page.append(&content);
    bin.set_child(Some(&page));
    entry.connect_search_changed(|_| rebuild());
    VIEW.with(|v| {
        v.replace(Some(View { win: win.downgrade(), content: content.downgrade(), entry: entry.downgrade() }))
    });
    rebuild();
}

pub fn focus_search() {
    if let Some(entry) = VIEW.with(|v| v.borrow().as_ref().and_then(|v| v.entry.upgrade())) {
        entry.grab_focus();
    }
}

pub fn search_for(text: &str) {
    if let Some(entry) = VIEW.with(|v| v.borrow().as_ref().and_then(|v| v.entry.upgrade())) {
        entry.set_text(text);
        rebuild();
    }
}

fn show_results(win: &Window, bin: &adw::Bin, text: &str) {
    let found = crate::search::find(text);
    if found.is_empty() {
        bin.set_child(Some(
            &adw::StatusPage::builder()
                .icon_name("edit-find-symbolic")
                .title(tr("No Results Found"))
                .description(tr(
                    "Only objects in folders opened before are found. Open a bucket to search inside it completely.",
                ))
                .build(),
        ));
        return;
    }
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title(trn("{n} result", "{n} results", &[("n", &found.len().to_string())]))
        .build();
    for item in found {
        let name = item.key.trim_end_matches('/').rsplit('/').next().unwrap_or(&item.key).to_string();
        let folder = &item.key[..item.key.trim_end_matches('/').len() - name.len()];
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&name))
            .activatable(true)
            .subtitle(glib::markup_escape_text(&format!("{} · {}/{}", item.profile_name, item.bucket, folder)))
            .subtitle_lines(1)
            .build();
        let icon = if item.folder {
            gio::ThemedIcon::new("folder-symbolic").upcast::<gio::Icon>()
        } else {
            let (content_type, _) = gio::content_type_guess(Some(name.as_str()), None::<&[u8]>);
            gio::content_type_get_symbolic_icon(&content_type)
        };
        row.add_prefix(&gtk::Image::builder().gicon(&icon).pixel_size(16).build());
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        row.connect_activated(glib::clone!(
            #[weak]
            win,
            move |_| win.open_object(item.profile.clone(), item.bucket.clone(), item.key.clone())
        ));
        group.add(&row);
    }
    page.add(&group);
    bin.set_child(Some(&page));
}

fn day_title(time: i64) -> String {
    let (Ok(then), Ok(now)) = (glib::DateTime::from_unix_local(time), glib::DateTime::now_local()) else {
        return tr("Earlier");
    };
    let days = |d: &glib::DateTime| d.year() as i64 * 400 + d.day_of_year() as i64;
    match days(&now) - days(&then) {
        0 => tr("Today"),
        1 => tr("Yesterday"),
        2..7 => then.format("%A").map(|s| s.to_string()).unwrap_or_default(),
        _ => tr("Earlier"),
    }
}

fn action_icon(action: &str) -> (&'static str, String) {
    match action {
        "uploaded" => ("transfer-upload-symbolic", tr("Uploaded")),
        "downloaded" => ("transfer-download-symbolic", tr("Downloaded")),
        "previewed" => ("view-reveal-symbolic", tr("Previewed")),
        _ => ("document-edit-symbolic", tr("Opened")),
    }
}

fn rebuild() {
    let Some((win, bin, entry)) = VIEW
        .with(|v| v.borrow().as_ref().and_then(|v| Some((v.win.upgrade()?, v.content.upgrade()?, v.entry.upgrade()?))))
    else {
        return;
    };
    let text = entry.text().trim().to_string();
    if !text.is_empty() {
        show_results(&win, &bin, &text);
        return;
    }
    let items = with_items(|items| items.clone());
    if items.is_empty() {
        let description = if remembering() {
            tr("Objects you open, preview, upload or download appear here.")
        } else {
            tr("File history is turned off in the privacy settings.")
        };
        bin.set_child(Some(
            &adw::StatusPage::builder()
                .icon_name("document-open-recent-symbolic")
                .title(tr("No Recent Objects"))
                .description(description)
                .build(),
        ));
        return;
    }
    let page = adw::PreferencesPage::new();
    let mut group: Option<(String, adw::PreferencesGroup)> = None;
    for (index, item) in items.iter().enumerate() {
        let day = day_title(item.time);
        if group.as_ref().map(|(d, _)| d != &day).unwrap_or(true) {
            let g = adw::PreferencesGroup::builder().title(glib::markup_escape_text(&day)).build();
            if index == 0 {
                let button = gtk::Button::builder()
                    .label(tr("Clear History"))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                button.connect_clicked(|_| clear());
                g.set_header_suffix(Some(&button));
            }
            page.add(&g);
            group = Some((day, g));
        }
        let name = item.key.rsplit('/').next().unwrap_or(&item.key).to_string();
        let folder = &item.key[..item.key.len() - name.len()];
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&name))
            .activatable(true)
            .subtitle(glib::markup_escape_text(&format!("{} · {}/{}", item.profile_name, item.bucket, folder)))
            .subtitle_lines(1)
            .build();
        let (content_type, _) = gio::content_type_guess(Some(name.as_str()), None::<&[u8]>);
        row.add_prefix(
            &gtk::Image::builder().gicon(&gio::content_type_get_symbolic_icon(&content_type)).pixel_size(16).build(),
        );
        let (icon, label) = action_icon(&item.action);
        let format = if glib::real_time() / 1_000_000 - item.time < 6 * 86_400 { "%H:%M" } else { "%e %b %Y" };
        let time = glib::DateTime::from_unix_local(item.time)
            .ok()
            .and_then(|d| d.format(format).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let stamp = gtk::Box::builder().spacing(6).valign(gtk::Align::Center).tooltip_text(&label).build();
        stamp.append(&gtk::Image::builder().icon_name(icon).css_classes(["dim-label"]).build());
        stamp.append(&gtk::Label::builder().label(&time).css_classes(["dim-label", "numeric", "caption"]).build());
        row.add_suffix(&stamp);
        let remove = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text(tr("Remove from Recent"))
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        let (profile, bucket, key) = (item.profile.clone(), item.bucket.clone(), item.key.clone());
        remove.connect_clicked(glib::clone!(
            #[strong]
            profile,
            #[strong]
            bucket,
            #[strong]
            key,
            move |_| forget(&profile, &bucket, std::slice::from_ref(&key))
        ));
        row.add_suffix(&remove);
        row.connect_activated(glib::clone!(
            #[weak]
            win,
            move |_| win.open_object(profile.clone(), bucket.clone(), key.clone())
        ));
        if let Some((_, g)) = &group {
            g.add(&row);
        }
    }
    let count = items.len();
    page.set_description(&trn("{n} recent object", "{n} recent objects", &[("n", &count.to_string())]));
    crate::widgets::code_view::label_icon_buttons(&page);
    bin.set_child(Some(&page));
}
