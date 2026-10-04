//! Importing connections from other applications: rclone, s3cmd, Cyberduck bookmarks and
//! AWS CLI profiles found on this computer, or in a file the user picks.
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

use crate::backup::foreign::{self, Found};
use crate::i18n::{tr, trf, trn};
use crate::runtime::bg;
use crate::window::Window;

/// A row per connection, checked unless one with the same name already exists.
fn add_row(group: &adw::PreferencesGroup, rows: &Rc<RefCell<Vec<(gtk::CheckButton, Found)>>>, found: Found, existing: &[String]) {
    let p = &found.profile;
    let place = if !p.aws_profile.is_empty() {
        trf("AWS CLI profile “{name}”", &[("name", &p.aws_profile)])
    } else if p.endpoint.is_empty() {
        "Amazon S3".to_string()
    } else {
        p.endpoint.clone()
    };
    let keys = if p.secret_key.is_empty() { String::new() } else { format!(" · {}", tr("with access keys")) };
    let duplicate = existing.contains(&p.name);
    let check = gtk::CheckButton::builder().active(!duplicate).valign(gtk::Align::Center).build();
    let subtitle = if duplicate { format!("{} · {place}{keys} · {}", found.source, tr("a connection with this name exists")) } else { format!("{} · {place}{keys}", found.source) };
    let row = adw::ActionRow::builder().title(glib::markup_escape_text(&p.name)).subtitle(glib::markup_escape_text(&subtitle)).activatable_widget(&check).build();
    row.add_prefix(&check);
    group.add(&row);
    rows.borrow_mut().push((check, found));
}

pub fn present(win: &Window) {
    let dialog = adw::Dialog::builder().title(tr("Import From Other Apps")).content_width(520).content_height(560).build();
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    let cancel = gtk::Button::with_mnemonic(&tr("_Cancel"));
    let import = gtk::Button::builder().label(tr("_Import")).use_underline(true).css_classes(["suggested-action"]).sensitive(false).build();
    header.pack_start(&cancel);
    header.pack_end(&import);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    let toasts = adw::ToastOverlay::new();
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder().title(tr("Connections"))
        .description(tr("From rclone, s3cmd, Cyberduck bookmarks and the AWS CLI. Access keys found in them are stored in the system keyring; the files stay as they are.")).build();
    let choose = adw::ButtonRow::builder().title(tr("Choose File…")).start_icon_name("document-open-symbolic").build();
    let more = adw::PreferencesGroup::new();
    more.add(&choose);
    page.add(&group);
    page.add(&more);
    let empty = adw::StatusPage::builder().icon_name("edit-find-symbolic").title(tr("No Connections Found"))
        .description(tr("No settings of rclone, s3cmd, Cyberduck or the AWS CLI were found on this computer. Choose a file to import from.")).vexpand(true).build();
    empty.add_css_class("compact");
    let empty_button = gtk::Button::builder().label(tr("_Choose File…")).use_underline(true).halign(gtk::Align::Center).css_classes(["pill", "suggested-action"]).build();
    empty.set_child(Some(&empty_button));
    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&page, Some("list"));
    toasts.set_child(Some(&stack));
    view.set_content(Some(&toasts));
    dialog.set_child(Some(&view));

    let rows: Rc<RefCell<Vec<(gtk::CheckButton, Found)>>> = Rc::default();
    let existing: Vec<String> = crate::profile::load().into_iter().map(|p| p.name).collect();
    let refresh = Rc::new(glib::clone!(#[weak] import, #[weak] stack, #[strong] rows, move || {
        let any = !rows.borrow().is_empty();
        stack.set_visible_child_name(if any { "list" } else { "empty" });
        import.set_visible(any);
        import.set_sensitive(rows.borrow().iter().any(|(c, _)| c.is_active()));
    }));
    let add = {
        let (group, rows, refresh) = (group.clone(), rows.clone(), refresh.clone());
        Rc::new(move |found: Vec<Found>| {
            for item in found {
                // The same connection found twice (a file picked again) is shown once.
                if rows.borrow().iter().any(|(_, f)| f.profile.name == item.profile.name && f.source == item.source) { continue; }
                add_row(&group, &rows, item, &existing);
                if let Some((check, _)) = rows.borrow().last() {
                    let refresh = refresh.clone();
                    check.connect_toggled(move |_| refresh());
                }
            }
            refresh();
        })
    };
    let mut found = foreign::aws_cli();
    for file in foreign::default_files() {
        found.extend(foreign::read_file(&file).unwrap_or_default());
    }
    add(found);

    let pick = Rc::new(glib::clone!(#[weak] dialog, #[weak] toasts, #[strong] add, move || {
        let chooser = gtk::FileDialog::builder().title(tr("Choose File")).modal(true).build();
        let root = dialog.root().and_downcast::<gtk::Window>();
        let add = add.clone();
        glib::spawn_future_local(async move {
            let Ok(file) = chooser.open_future(root.as_ref()).await else { return };
            let Some(path) = file.path() else { return };
            match foreign::read_file(&path) {
                Ok(found) if found.is_empty() => toasts.add_toast(crate::window::plain_toast(&tr("No S3 connections were found in this file"))),
                Ok(found) => add(found),
                Err(error) => toasts.add_toast(crate::window::plain_toast(&error)),
            }
        });
    }));
    let p = pick.clone();
    choose.connect_activated(move |_| p());
    empty_button.connect_clicked(move |_| pick());
    cancel.connect_clicked(glib::clone!(#[weak] dialog, move |_| { dialog.close(); }));

    let parent = win.clone();
    import.connect_clicked(glib::clone!(#[weak] dialog, #[weak] toasts, #[strong] rows, move |button| {
        let chosen: Vec<_> = rows.borrow().iter().filter(|(c, _)| c.is_active()).map(|(_, f)| f.profile.clone()).collect();
        let (win, button) = (parent.clone(), button.clone());
        button.set_sensitive(false);
        glib::spawn_future_local(async move {
            let mut allow_plain = false;
            let mut added = 0;
            for profile in chosen {
                let mut result = bg(crate::profile::save(profile.clone(), allow_plain)).await;
                if result.as_ref().err().is_some_and(|e| e == crate::profile::KEYRING_UNAVAILABLE) {
                    let alert = adw::AlertDialog::new(Some(&tr("Keyring Unavailable")),
                        Some(&tr("The system keyring could not be opened, so the secret key cannot be stored safely. It can be saved in a file only you can read, which is less safe. It moves to the keyring as soon as one works.")));
                    alert.add_responses(&[("cancel", &tr("Cancel")), ("save", &tr("Save in File"))]);
                    alert.set_response_appearance("save", adw::ResponseAppearance::Destructive);
                    alert.set_close_response("cancel");
                    if alert.choose_future(Some(&dialog)).await != "save" { break; }
                    allow_plain = true;
                    result = bg(crate::profile::save(profile, true)).await;
                }
                match result {
                    Ok(_) => added += 1,
                    Err(error) => { toasts.add_toast(crate::window::plain_toast(&error)); button.set_sensitive(true); return; }
                }
            }
            if added > 0 {
                win.reload_profiles();
                win.toast(&trn("{n} connection imported", "{n} connections imported", &[("n", &added.to_string())]));
                dialog.close();
            } else {
                button.set_sensitive(true);
            }
        });
    }));
    dialog.present(Some(win));
}

