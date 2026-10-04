//! Exporting and importing connections, with the access keys protected by a password,
//! age or SSH keys (also on a YubiKey) or a GnuPG key.
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

use crate::backup::{self, age::Prompt, age::Prompts, gpg, Kind, Protection};
use crate::i18n::{tr, trn};
use crate::runtime::bg;
use crate::window::Window;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method { Password, Age, Gpg, None }

fn file_filters(name: &str, suffixes: &[&str], types: &[&str]) -> gio::ListStore {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(name));
    for suffix in suffixes { filter.add_suffix(suffix); }
    for mime in types { filter.add_mime_type(mime); }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    filters
}

/// A row in a group of choices, with a radio button in front.
fn choice(title: &str, subtitle: &str, group: Option<&gtk::CheckButton>) -> (adw::ActionRow, gtk::CheckButton) {
    let check = gtk::CheckButton::builder().valign(gtk::Align::Center).build();
    if let Some(group) = group { check.set_group(Some(group)); }
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).activatable_widget(&check).build();
    row.add_prefix(&check);
    (row, check)
}

pub fn export(win: &Window) {
    let dialog = adw::Dialog::builder().title(tr("Export Connections")).content_width(500).content_height(620).build();
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    let cancel = gtk::Button::with_mnemonic(&tr("_Cancel"));
    let save = gtk::Button::builder().label(tr("_Export…")).use_underline(true).css_classes(["suggested-action"]).build();
    header.pack_start(&cancel);
    header.pack_end(&save);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    let toasts = adw::ToastOverlay::new();
    let page = adw::PreferencesPage::new();

    let methods = adw::PreferencesGroup::builder().title(tr("Access Keys"))
        .description(tr("Connection details are always included. Choose how the access keys are protected in the file.")).build();
    let (password_row, password_check) = choice(&tr("Password"), &tr("AES-256-GCM, with the key derived from the password by Argon2id"), None);
    let (age_row, age_check) = choice(&tr("age or SSH Key"), &tr("For age and SSH public keys, and YubiKeys with age-plugin-yubikey"), Some(&password_check));
    let (gpg_row, gpg_check) = choice(&tr("GnuPG Key"), &tr("Looking for keys…"), Some(&password_check));
    let (none_row, none_check) = choice(&tr("Leave Out Access Keys"), &tr("They have to be entered again after importing"), Some(&password_check));
    password_check.set_active(true);
    gpg_row.set_sensitive(false);
    for row in [&password_row, &age_row, &gpg_row, &none_row] { methods.add(row); }
    page.add(&methods);

    let password_group = adw::PreferencesGroup::builder().title(tr("Password"))
        .description(tr("Use at least 12 characters. A forgotten password cannot be recovered.")).build();
    let first = adw::PasswordEntryRow::builder().title(tr("Password")).build();
    let second = adw::PasswordEntryRow::builder().title(tr("Repeat Password")).build();
    password_group.add(&first);
    password_group.add(&second);
    page.add(&password_group);

    let age_group = adw::PreferencesGroup::builder().title(tr("Public Keys")).visible(false)
        .description(tr("age keys (age1…), SSH keys (ssh-ed25519 …) or YubiKeys (age1yubikey1…). Separate several keys with commas; any one of them opens the file.")).build();
    let recipients = adw::EntryRow::builder().title(tr("Public Keys")).build();
    let pick = gtk::Button::builder().icon_name("document-open-symbolic").tooltip_text(tr("Add Key From File…")).valign(gtk::Align::Center).css_classes(["flat"]).build();
    recipients.add_suffix(&pick);
    age_group.add(&recipients);
    let yubikey = adw::ButtonRow::builder().title(tr("Add Connected YubiKey")).start_icon_name("auth-smartcard-symbolic").visible(backup::age::yubikey_plugin()).build();
    age_group.add(&yubikey);
    page.add(&age_group);

    let gpg_group = adw::PreferencesGroup::builder().title(tr("GnuPG Key")).visible(false)
        .description(tr("Only the secret part of this key opens the file, also when it is on a smartcard.")).build();
    let gpg_keys: Rc<RefCell<Vec<gpg::Key>>> = Rc::default();
    let key_row = adw::ComboRow::builder().title(tr("Key")).build();
    gpg_group.add(&key_row);
    page.add(&gpg_group);

    toasts.set_child(Some(&page));
    view.set_content(Some(&toasts));
    dialog.set_child(Some(&view));

    let method = {
        let (p, a, g) = (password_check.clone(), age_check.clone(), gpg_check.clone());
        move || if p.is_active() { Method::Password } else if a.is_active() { Method::Age } else if g.is_active() { Method::Gpg } else { Method::None }
    };
    let update = Rc::new(glib::clone!(#[weak] save, #[weak] first, #[weak] second, #[weak] recipients, #[weak] password_group, #[weak] age_group, #[weak] gpg_group, #[strong] gpg_keys, #[strong] method, move || {
        let chosen = method();
        password_group.set_visible(chosen == Method::Password);
        age_group.set_visible(chosen == Method::Age);
        gpg_group.set_visible(chosen == Method::Gpg);
        let lines = backup::age::split(&recipients.text());
        let bad = lines.iter().any(|l| !backup::age::valid(l));
        if bad { recipients.add_css_class("error"); } else { recipients.remove_css_class("error"); }
        let (a, b) = (first.text(), second.text());
        let short = !a.is_empty() && a.chars().count() < backup::password::MIN_LENGTH;
        if short { first.add_css_class("warning"); } else { first.remove_css_class("warning"); }
        if !b.is_empty() && a != b { second.add_css_class("error"); } else { second.remove_css_class("error"); }
        save.set_sensitive(match chosen {
            Method::Password => !short && !a.is_empty() && a == b,
            Method::Age => !lines.is_empty() && !bad,
            Method::Gpg => !gpg_keys.borrow().is_empty(),
            Method::None => true,
        });
    }));
    update();
    for check in [&password_check, &age_check, &gpg_check, &none_check] {
        let u = update.clone();
        check.connect_toggled(move |_| u());
    }
    for entry in [first.upcast_ref::<gtk::Editable>(), second.upcast_ref(), recipients.upcast_ref()] {
        let u = update.clone();
        entry.connect_changed(move |_| u());
    }

    // The keys GnuPG can encrypt to, the user's own first.
    glib::spawn_future_local(glib::clone!(#[weak] gpg_row, #[weak] key_row, #[strong] gpg_keys, #[strong] update, async move {
        if !gpg::available() {
            gpg_row.set_subtitle(&tr("GnuPG is not installed"));
            return;
        }
        let keys = bg(gpg::keys()).await.unwrap_or_default();
        if keys.is_empty() {
            gpg_row.set_subtitle(&tr("No GnuPG key that can encrypt was found"));
            return;
        }
        let labels: Vec<String> = keys.iter().map(|k| format!("{} · {}", k.user, &k.fingerprint[k.fingerprint.len().saturating_sub(16)..])).collect();
        key_row.set_model(Some(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())));
        gpg_row.set_subtitle(&tr("Encrypted with a key from your GnuPG keyring"));
        gpg_row.set_sensitive(true);
        gpg_keys.replace(keys);
        update();
    }));

    pick.connect_clicked(glib::clone!(#[weak] dialog, #[weak] recipients, #[weak] toasts, move |_| {
        let chooser = gtk::FileDialog::builder().title(tr("Add Key From File")).modal(true)
            .filters(&file_filters(&tr("Public Keys"), &["pub", "txt"], &[])).build();
        if let Some(ssh) = gio::File::for_path(glib::home_dir().join(".ssh")).query_exists(gio::Cancellable::NONE).then(|| gio::File::for_path(glib::home_dir().join(".ssh"))) {
            chooser.set_initial_folder(Some(&ssh));
        }
        let root = dialog.root().and_downcast::<gtk::Window>();
        glib::spawn_future_local(async move {
            let Ok(file) = chooser.open_future(root.as_ref()).await else { return };
            let Some(text) = file.path().and_then(|p| std::fs::read_to_string(p).ok()) else { return };
            // A private key must never end up here.
            if text.contains("PRIVATE KEY") || text.contains("AGE-SECRET-KEY") {
                toasts.add_toast(crate::window::plain_toast(&tr("This is a private key. Choose the public key, usually the file ending in .pub.")));
                return;
            }
            let mut lines = backup::age::split(&recipients.text());
            lines.extend(backup::age::split(&text));
            recipients.set_text(&lines.join(", "));
        });
    }));

    yubikey.connect_activated(glib::clone!(#[weak] recipients, #[weak] toasts, move |row| {
        row.set_sensitive(false);
        let row = row.clone();
        glib::spawn_future_local(async move {
            let found = bg(backup::age::yubikey_recipients()).await;
            row.set_sensitive(true);
            match found {
                Ok(keys) => {
                    let mut lines = backup::age::split(&recipients.text());
                    lines.extend(keys.into_iter().filter(|k| !lines.contains(k)).collect::<Vec<_>>());
                    recipients.set_text(&lines.join(", "));
                }
                Err(error) => toasts.add_toast(crate::window::plain_toast(&error)),
            }
        });
    }));

    cancel.connect_clicked(glib::clone!(#[weak] dialog, move |_| { dialog.close(); }));
    let parent = win.clone();
    save.connect_clicked(glib::clone!(#[weak] dialog, #[weak] first, #[weak] recipients, #[weak] key_row, #[weak] toasts, #[strong] gpg_keys, move |button| {
        let chosen = method();
        let protection = match chosen {
            Method::Password => Protection::Password(first.text().to_string()),
            Method::Age => Protection::Age(backup::age::split(&recipients.text())),
            Method::Gpg => match gpg_keys.borrow().get(key_row.selected() as usize) {
                Some(key) => Protection::Gpg(key.fingerprint.clone()),
                None => return,
            },
            Method::None => Protection::None,
        };
        let (name, filters) = match chosen {
            Method::Age => ("ferry-connections.age", file_filters(&tr("age Files"), &["age"], &[])),
            Method::Gpg => ("ferry-connections.asc", file_filters(&tr("OpenPGP Files"), &["asc", "gpg", "pgp"], &["application/pgp-encrypted"])),
            _ => ("ferry-connections.json", file_filters(&tr("JSON Files"), &["json"], &["application/json"])),
        };
        let chooser = gtk::FileDialog::builder().title(tr("Export Connections")).initial_name(name).filters(&filters).modal(true).build();
        let (button, win) = (button.clone(), parent.clone());
        glib::spawn_future_local(async move {
            let root = dialog.root().and_downcast::<gtk::Window>();
            let Ok(file) = chooser.save_future(root.as_ref()).await else { return };
            let Some(path) = file.path() else { return };
            button.set_sensitive(false);
            let result = bg(backup::export(path, protection)).await;
            button.set_sensitive(true);
            match result {
                Ok(n) => {
                    dialog.close();
                    win.toast(&if chosen == Method::None {
                        tr("Connections exported without access keys")
                    } else {
                        trn("{n} connection exported with its access keys, encrypted", "{n} connections exported with their access keys, encrypted", &[("n", &n.to_string())])
                    });
                }
                Err(error) => toasts.add_toast(crate::window::plain_toast(&error)),
            }
        });
    }));
    dialog.present(Some(win));
    first.grab_focus();
}

/// Asks for the password of a backup. None when cancelled.
async fn ask_password(win: &Window, body: &str) -> Option<String> {
    let dialog = adw::AlertDialog::new(Some(&tr("Enter Backup Password")), Some(body));
    let group = adw::PreferencesGroup::new();
    let entry = adw::PasswordEntryRow::builder().title(tr("Password")).activates_default(true).build();
    group.add(&entry);
    dialog.set_extra_child(Some(&group));
    dialog.add_responses(&[("cancel", &tr("Cancel")), ("ok", &tr("Unlock"))]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("ok", false);
    entry.connect_changed(glib::clone!(#[weak] dialog, move |e| dialog.set_response_enabled("ok", !e.text().is_empty())));
    entry.grab_focus();
    (dialog.choose_future(Some(win)).await == "ok").then(|| entry.text().to_string())
}

/// Runs an age job on a worker thread and answers the questions of keys and plugins
/// (passphrases, PINs, "touch your YubiKey") in the interface meanwhile.
async fn with_prompts<T: Send + 'static>(win: &Window, job: impl FnOnce(Prompts) -> Result<T, String> + Send + 'static) -> Result<T, String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = crate::runtime::runtime().spawn_blocking(move || job(Prompts(tx)));
    let mut notes: Vec<adw::Toast> = Vec::new();
    // The channel closes once the job is done with every key.
    while let Some(prompt) = rx.recv().await {
        match prompt {
            Prompt::Message(text) => {
                let toast = crate::window::plain_toast(&text);
                toast.set_timeout(0);
                win.imp_toasts().add_toast(toast.clone());
                notes.push(toast);
            }
            Prompt::Confirm { message, yes, no, reply } => {
                let alert = adw::AlertDialog::new(None, Some(&message));
                alert.add_responses(&[("no", &no.unwrap_or_else(|| tr("Cancel"))), ("yes", &yes)]);
                alert.set_response_appearance("yes", adw::ResponseAppearance::Suggested);
                alert.set_close_response("no");
                let _ = reply.send(Some(alert.choose_future(Some(win)).await == "yes"));
            }
            Prompt::Text { description, secret, reply } => {
                let alert = adw::AlertDialog::new(None, Some(&description));
                let group = adw::PreferencesGroup::new();
                let entry: adw::EntryRow = if secret { adw::PasswordEntryRow::new().upcast() } else { adw::EntryRow::new() };
                entry.set_activates_default(true);
                group.add(&entry);
                alert.set_extra_child(Some(&group));
                alert.add_responses(&[("cancel", &tr("Cancel")), ("ok", &tr("Continue"))]);
                alert.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
                alert.set_default_response(Some("ok"));
                alert.set_close_response("cancel");
                entry.grab_focus();
                let ok = alert.choose_future(Some(win)).await == "ok";
                let _ = reply.send(ok.then(|| entry.text().to_string()));
            }
        }
    }
    for toast in notes { toast.dismiss(); }
    task.await.map_err(|e| e.to_string())?
}

/// Opens a backup of any kind and adds its connections.
pub fn import(win: &Window) {
    let filters = file_filters(&tr("Connection Backups"), &["json", "age", "asc", "gpg", "pgp"], &["application/json", "application/pgp-encrypted"]);
    let chooser = gtk::FileDialog::builder().title(tr("Import Connections")).filters(&filters).modal(true).build();
    let win = win.clone();
    glib::spawn_future_local(async move {
        let Ok(file) = chooser.open_future(Some(&win)).await else { return };
        let Some(path) = file.path() else { return };
        let data = match backup::read(&path).and_then(|d| backup::detect(&d).map(|k| (d, k))) {
            Ok(found) => found,
            Err(error) => { win.toast(&error); return; }
        };
        let (data, kind) = (std::sync::Arc::new(data.0), data.1);
        let profiles = match kind {
            Kind::Plain => backup::open_plain(&data),
            Kind::Password | Kind::AgePassphrase => loop {
                let Some(password) = ask_password(&win, &tr("This backup contains access keys and is protected with a password.")).await else { return };
                let data = data.clone();
                let opened = bg(async move {
                    tokio::task::spawn_blocking(move || if kind == Kind::Password { backup::password::open(&data, &password) } else { backup::age::open_passphrase(&data, &password) })
                        .await.map_err(|e| e.to_string())?
                }).await;
                match opened {
                    Ok(list) => break Ok(list),
                    Err(error) => win.toast(&error),
                }
            },
            Kind::Age => {
                let mut key_file: Option<std::path::PathBuf> = None;
                loop {
                    let (data, chosen) = (data.clone(), key_file.clone());
                    match with_prompts(&win, move |prompts| backup::age::open(&data, chosen.as_deref(), &prompts)).await {
                        Ok(list) => break Ok(list),
                        Err(error) => {
                            // No key here fits: the user can point at the right one.
                            let alert = adw::AlertDialog::new(Some(&tr("Choose Key File?")),
                                Some(&format!("{error}.\n\n{}", tr("Choose the age identity file or SSH private key the backup was made for."))));
                            alert.add_responses(&[("cancel", &tr("Cancel")), ("choose", &tr("_Choose File…"))]);
                            alert.set_response_appearance("choose", adw::ResponseAppearance::Suggested);
                            alert.set_default_response(Some("choose"));
                            alert.set_close_response("cancel");
                            if alert.choose_future(Some(&win)).await != "choose" { return; }
                            let picker = gtk::FileDialog::builder().title(tr("Choose Key File")).modal(true).build();
                            let ssh = glib::home_dir().join(".ssh");
                            if ssh.is_dir() { picker.set_initial_folder(Some(&gio::File::for_path(ssh))); }
                            let Ok(file) = picker.open_future(Some(&win)).await else { return };
                            key_file = file.path();
                        }
                    }
                }
            }
            Kind::Gpg => {
                let data = data.clone();
                bg(async move { gpg::open(&data).await }).await
            }
        };
        let profiles = match profiles {
            Ok(list) => list,
            Err(error) => { win.toast(&error); return; }
        };
        let with_keys = profiles.iter().any(|p| !p.secret_key.is_empty());
        match bg(backup::restore(profiles)).await {
            Ok(n) => {
                win.reload_profiles();
                win.toast(&if with_keys {
                    trn("{n} connection imported with its access keys", "{n} connections imported with their access keys", &[("n", &n.to_string())])
                } else {
                    trn("{n} connection added. Enter its access keys before connecting.", "{n} connections added. Enter their access keys before connecting.", &[("n", &n.to_string())])
                });
            }
            Err(error) => win.toast(&error),
        }
    });
}
