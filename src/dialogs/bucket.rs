//! Secondary windows of the browser, built from libadwaita widgets.
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

use crate::i18n::{tr, trf, trn};
use crate::profile;
use crate::runtime::bg;
use crate::s3::{self, Entry, ObjectInfo, S3};
use crate::s3::tools::Setting;
use crate::window::{Window, format_size, format_time};

/// A header bar with Cancel and an accept button, as GNOME dialogs that edit something use.
fn editing_dialog(title: &str, accept: &str, width: i32, height: i32) -> (adw::Dialog, adw::ToolbarView, gtk::Button, adw::ToastOverlay) {
    let dialog = adw::Dialog::builder().title(title).content_width(width).content_height(height).build();
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    let cancel = gtk::Button::with_label(&tr("Cancel"));
    cancel.connect_clicked(glib::clone!(#[weak] dialog, move |_| { dialog.close(); }));
    let ok = gtk::Button::with_label(accept);
    ok.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&ok);
    dialog.set_default_widget(Some(&ok));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    let toasts = adw::ToastOverlay::new();
    view.set_content(Some(&toasts));
    dialog.set_child(Some(&view));
    (dialog, view, ok, toasts)
}

/// A closable dialog with only the standard close button.
fn plain_dialog(title: &str, width: i32, height: i32) -> (adw::Dialog, adw::ToolbarView, adw::ToastOverlay) {
    let dialog = adw::Dialog::builder().title(title).content_width(width).content_height(height).build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    let toasts = adw::ToastOverlay::new();
    view.set_content(Some(&toasts));
    dialog.set_child(Some(&view));
    (dialog, view, toasts)
}

fn toast(overlay: &adw::ToastOverlay, text: &str) {
    overlay.add_toast(crate::window::plain_toast(text));
}

// ----- Key/value editor for tags and metadata -----

pub struct PairEditor {
    pub group: adw::PreferencesGroup,
    rows: Rc<RefCell<Vec<(adw::EntryRow, adw::EntryRow, gtk::Box)>>>,
    list: gtk::Box,
}

impl PairEditor {
    pub fn new(title: &str, description: &str, key_label: &str, pairs: &[(String, String)]) -> Self {
        let group = adw::PreferencesGroup::builder().title(title).description(description).build();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        group.add(&list);
        let add = gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text(tr("Add Entry")).valign(gtk::Align::Center).build();
        add.add_css_class("flat");
        group.set_header_suffix(Some(&add));
        let editor = PairEditor { group, rows: Rc::default(), list };
        for (key, value) in pairs {
            editor.add_row(key, value, key_label);
        }
        editor.update_placeholder();
        let (rows, list, label) = (editor.rows.clone(), editor.list.clone(), key_label.to_string());
        add.connect_clicked(move |_| {
            let temp = PairEditor { group: adw::PreferencesGroup::new(), rows: rows.clone(), list: list.clone() };
            temp.add_row("", "", &label);
            temp.update_placeholder();
            if let Some((key, _, _)) = rows.borrow().last() { key.grab_focus(); }
        });
        editor
    }

    fn add_row(&self, key: &str, value: &str, key_label: &str) {
        let line = gtk::Box::builder().spacing(6).build();
        let key_row = adw::EntryRow::builder().title(key_label).text(key).hexpand(true).build();
        let value_row = adw::EntryRow::builder().title(tr("Value")).text(value).hexpand(true).build();
        let keys = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).hexpand(true).css_classes(["boxed-list"]).build();
        keys.append(&key_row);
        let values = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).hexpand(true).css_classes(["boxed-list"]).build();
        values.append(&value_row);
        let remove = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text(tr("Remove")).valign(gtk::Align::Center).build();
        remove.add_css_class("flat");
        line.append(&keys);
        line.append(&values);
        line.append(&remove);
        self.list.append(&line);
        self.rows.borrow_mut().push((key_row, value_row, line.clone()));
        let (rows, list) = (self.rows.clone(), self.list.clone());
        remove.connect_clicked(move |_| {
            list.remove(&line);
            rows.borrow_mut().retain(|(_, _, l)| l != &line);
            PairEditor { group: adw::PreferencesGroup::new(), rows: rows.clone(), list: list.clone() }.update_placeholder();
        });
    }

    fn update_placeholder(&self) {
        let empty = self.rows.borrow().is_empty();
        let mut child = self.list.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            if widget.widget_name() == "placeholder" { self.list.remove(&widget); }
        }
        if empty {
            let label = gtk::Label::builder().label(tr("No entries")).css_classes(["dim-label"]).margin_top(6).margin_bottom(6).name("placeholder").build();
            self.list.append(&label);
        }
    }

    pub fn pairs(&self) -> Vec<(String, String)> {
        self.rows.borrow().iter()
            .map(|(k, v, _)| (k.text().trim().to_string(), v.text().to_string()))
            .filter(|(k, _)| !k.is_empty())
            .collect()
    }
}

/// A monospace editor for JSON documents.
fn code_editor(text: &str, height: i32) -> (gtk::ScrolledWindow, gtk::TextBuffer) {
    let view = crate::widgets::code_view::new(text, "document.json");
    view.set_top_margin(10); view.set_bottom_margin(10); view.set_left_margin(10); view.set_right_margin(10);
    let scroller = gtk::ScrolledWindow::builder().child(&view).min_content_height(height).css_classes(["card"]).build();
    (scroller, view.buffer())
}

fn buffer_text(buffer: &gtk::TextBuffer) -> String {
    buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string()
}

// ----- Bucket settings -----

pub fn bucket_settings(win: &Window, client: S3, bucket: String) {
    let win = win.clone();
    glib::spawn_future_local(async move {
        let (c, b) = (client.clone(), bucket.clone());
        let settings = match bg(async move { Ok(c.bucket_settings(&b).await) }).await {
            Ok(settings) => settings,
            Err(error) => { win.toast(&error); return; }
        };
        let dialog = adw::PreferencesDialog::builder().title(trf("Settings of {bucket}", &[("bucket", &bucket)])).search_enabled(false).content_width(980).content_height(680).build();

        // General
        let page = adw::PreferencesPage::builder().title(tr("General")).icon_name("preferences-system-symbolic").build();
        let info = adw::PreferencesGroup::new();
        info.add(&adw::ActionRow::builder().title(tr("Region")).subtitle(if settings.region.is_empty() { "—".to_string() } else { settings.region.clone() }).subtitle_selectable(true).css_classes(["property"]).build());
        page.add(&info);

        let versioning = adw::PreferencesGroup::builder().title(tr("Versioning")).description(tr("Keeps older versions of overwritten and deleted objects, which increases storage use.")).build();
        match &settings.versioning {
            Setting::Value(status) => {
                let row = adw::SwitchRow::builder().title(tr("Keep object versions")).subtitle(match status.as_str() {
                    "Enabled" => tr("Enabled"), "Suspended" => tr("Suspended; existing versions are kept"), _ => tr("Never enabled"),
                }).active(status == "Enabled").build();
                row.connect_active_notify(glib::clone!(#[strong] client, #[strong] bucket, #[weak] dialog, move |row| {
                    let (client, bucket, enabled, row, dialog) = (client.clone(), bucket.clone(), row.is_active(), row.clone(), dialog.clone());
                    row.set_sensitive(false);
                    glib::spawn_future_local(async move {
                        let result = bg(async move { client.set_versioning(&bucket, enabled).await }).await;
                        row.set_sensitive(true);
                        match result {
                            Ok(()) => { row.set_subtitle(&if enabled { tr("Enabled") } else { tr("Suspended; existing versions are kept") }); dialog.add_toast(crate::window::plain_toast(&tr("Settings saved"))); }
                            Err(error) => dialog.add_toast(crate::window::plain_toast(&error)),
                        }
                    });
                }));
                versioning.add(&row);
            }
            Setting::Unsupported(note) => versioning.set_description(Some(&glib::markup_escape_text(note))),
        }
        page.add(&versioning);

        let encryption = adw::PreferencesGroup::builder().title(tr("Default Encryption")).build();
        match &settings.encryption {
            Setting::Value((algorithm, key)) => {
                let combo = adw::ComboRow::builder().title(tr("Algorithm")).model(&gtk::StringList::new(&[&tr("None"), "SSE-S3 (AES256)", "SSE-KMS"])).build();
                combo.set_selected(match algorithm.as_str() { "AES256" => 1, "aws:kms" => 2, _ => 0 });
                let kms = adw::EntryRow::builder().title(tr("KMS key ID or alias (optional)")).text(key).visible(algorithm == "aws:kms").build();
                combo.connect_selected_notify(glib::clone!(#[weak] kms, move |c| kms.set_visible(c.selected() == 2)));
                let apply = adw::ButtonRow::builder().title(tr("Apply")).build();
                apply.connect_activated(glib::clone!(#[strong] client, #[strong] bucket, #[weak] dialog, #[weak] combo, #[weak] kms, move |_| {
                    let algorithm = ["", "AES256", "aws:kms"][combo.selected() as usize].to_string();
                    let (client, bucket, key, dialog) = (client.clone(), bucket.clone(), kms.text().to_string(), dialog.clone());
                    glib::spawn_future_local(async move {
                        let result = bg(async move { client.set_bucket_encryption(&bucket, &algorithm, &key).await }).await;
                        dialog.add_toast(crate::window::plain_toast(&result.map(|_| tr("Settings saved")).unwrap_or_else(|e| e)));
                    });
                }));
                encryption.add(&combo);
                encryption.add(&kms);
                encryption.add(&apply);
            }
            Setting::Unsupported(note) => encryption.set_description(Some(&glib::markup_escape_text(note))),
        }
        page.add(&encryption);
        dialog.add(&page);

        // JSON documents: policy, CORS and lifecycle share one layout.
        let policy_example = serde_json::to_string_pretty(&serde_json::json!({
            "Version": "2012-10-17",
            "Statement": [{ "Sid": "PublicRead", "Effect": "Allow", "Principal": "*", "Action": ["s3:GetObject"], "Resource": [format!("arn:aws:s3:::{bucket}/*")] }]
        })).unwrap();
        let cors_example = serde_json::to_string_pretty(&serde_json::json!([{ "AllowedOrigins": ["https://example.com"], "AllowedMethods": ["GET", "HEAD"], "AllowedHeaders": ["*"], "ExposeHeaders": ["ETag"], "MaxAgeSeconds": 3600 }])).unwrap();
        let lifecycle_example = serde_json::to_string_pretty(&serde_json::json!([
            { "ID": "expire-temp", "Status": "Enabled", "Filter": { "Prefix": "tmp/" }, "Expiration": { "Days": 30 } },
            { "ID": "abort-incomplete-uploads", "Status": "Enabled", "Filter": { "Prefix": "" }, "AbortIncompleteMultipartUpload": { "DaysAfterInitiation": 7 } }
        ])).unwrap();
        type Saver = fn(S3, String, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = s3::Res<()>> + Send>>;
        let documents: [(String, &str, String, &Setting<String>, String, Saver); 3] = [
            (tr("Policy"), "channel-secure-symbolic", tr("Bucket policy as JSON. Saving an empty document removes the policy."), &settings.policy, policy_example,
                |c, b, t| Box::pin(async move { c.set_bucket_policy(&b, &t).await })),
            (tr("CORS"), "network-workgroup-symbolic", tr("CORS rules as a JSON array (AllowedOrigins, AllowedMethods, AllowedHeaders, ExposeHeaders, MaxAgeSeconds). An empty document removes the rules."), &settings.cors, cors_example,
                |c, b, t| Box::pin(async move { c.set_bucket_cors(&b, &t).await })),
            (tr("Lifecycle"), "document-open-recent-symbolic", tr("Lifecycle rules as a JSON array (ID, Status, Filter, Expiration, Transitions, NoncurrentVersionExpiration, AbortIncompleteMultipartUpload). An empty document removes the rules."), &settings.lifecycle, lifecycle_example,
                |c, b, t| Box::pin(async move { c.set_bucket_lifecycle(&b, &t).await })),
        ];
        for (title, icon, hint, setting, example, save) in documents {
            let page = adw::PreferencesPage::builder().title(&title).icon_name(icon).build();
            let group = adw::PreferencesGroup::builder().description(&hint).build();
            if let Some(note) = setting.note() {
                group.set_description(Some(&glib::markup_escape_text(&format!("{hint}\n\n{}: {note}", tr("The provider reported")))));
            }
            let (editor, buffer) = code_editor(setting.value().map(String::as_str).unwrap_or(""), 320);
            group.add(&editor);
            let buttons = gtk::Box::builder().spacing(6).halign(gtk::Align::End).margin_top(12).build();
            let example_button = gtk::Button::with_label(&tr("Insert Example"));
            example_button.connect_clicked(glib::clone!(#[weak] buffer, move |_| buffer.set_text(&example)));
            let save_button = gtk::Button::with_label(&tr("Save"));
            save_button.add_css_class("suggested-action");
            save_button.connect_clicked(glib::clone!(#[strong] client, #[strong] bucket, #[weak] dialog, #[weak] buffer, move |button| {
                let (future, dialog, button) = (save(client.clone(), bucket.clone(), buffer_text(&buffer)), dialog.clone(), button.clone());
                button.set_sensitive(false);
                glib::spawn_future_local(async move {
                    let result = bg(future).await;
                    button.set_sensitive(true);
                    dialog.add_toast(crate::window::plain_toast(&result.map(|_| tr("Settings saved")).unwrap_or_else(|e| e)));
                });
            }));
            buttons.append(&example_button);
            buttons.append(&save_button);
            // A setting the provider cannot read cannot be saved either.
            if setting.note().is_some() {
                editor.set_sensitive(false);
                buttons.set_visible(false);
            }
            group.add(&buttons);
            page.add(&group);
            dialog.add(&page);
        }

        crate::dialogs::access::add_bucket_pages(&dialog, client.clone(), bucket.clone());

        // Tags
        let page = adw::PreferencesPage::builder().title(tr("Tags")).icon_name("bookmark-new-symbolic").build();
        let editor = Rc::new(PairEditor::new(&tr("Bucket Tags"), &settings.tags.note().map(|n| glib::markup_escape_text(n).to_string()).unwrap_or_default(), &tr("Key"), settings.tags.value().map(Vec::as_slice).unwrap_or(&[])));
        page.add(&editor.group);
        let save_group = adw::PreferencesGroup::new();
        let save_row = adw::ButtonRow::builder().title(tr("Save Tags")).build();
        save_row.add_css_class("suggested-action");
        save_row.connect_activated(glib::clone!(#[strong] client, #[strong] bucket, #[weak] dialog, #[strong] editor, move |_| {
            let (client, bucket, tags, dialog) = (client.clone(), bucket.clone(), editor.pairs(), dialog.clone());
            glib::spawn_future_local(async move {
                let result = bg(async move { client.set_bucket_tags(&bucket, &tags).await }).await;
                dialog.add_toast(crate::window::plain_toast(&result.map(|_| tr("Settings saved")).unwrap_or_else(|e| e)));
            });
        }));
        save_group.add(&save_row);
        page.add(&save_group);
        dialog.add(&page);

        dialog.present(Some(&win));
    });
}

// ----- Object versions -----

pub fn versions(win: &Window, client: S3, bucket: String, key: String) {
    let name = key.rsplit('/').next().unwrap_or(&key).to_string();
    let (dialog, _view, toasts) = plain_dialog(&trf("Versions of {name}", &[("name", &name)]), 560, 560);
    let page = adw::PreferencesPage::new();
    let state = adw::PreferencesGroup::new();
    let list = adw::PreferencesGroup::builder().title(tr("Stored Versions")).build();
    page.add(&state);
    page.add(&list);
    toasts.set_child(Some(&page));
    let rows: Rc<RefCell<Vec<gtk::Widget>>> = Rc::default();

    let reload: Rc<RefCell<Option<Box<dyn Fn()>>>> = Rc::default();
    let reload_fn = {
        let (client, bucket, key, list, toasts, rows, win, reload) = (client.clone(), bucket.clone(), key.clone(), list.clone(), toasts.clone(), rows.clone(), win.clone(), reload.clone());
        move || {
            let (client, bucket, key, list, toasts, rows, win, reload) = (client.clone(), bucket.clone(), key.clone(), list.clone(), toasts.clone(), rows.clone(), win.clone(), reload.clone());
            glib::spawn_future_local(async move {
                let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
                let result = bg(async move { c.list_versions(&b, &k).await }).await;
                for row in rows.borrow_mut().drain(..) { list.remove(&row); }
                let versions = match result { Ok(v) => v, Err(error) => { toast(&toasts, &error); return; } };
                if versions.is_empty() {
                    let row = adw::ActionRow::builder().title(tr("No stored versions for this object. Versioning may be disabled on the bucket.")).css_classes(["dim-label"]).build();
                    list.add(&row);
                    rows.borrow_mut().push(row.upcast());
                }
                for version in versions {
                    let mut subtitle = if version.delete_marker { tr("Delete marker") } else { format_size(version.size) };
                    if version.is_latest { subtitle = format!("{subtitle} · {}", tr("Current")); }
                    let row = adw::ActionRow::builder().title(format_time(version.modified)).subtitle(&subtitle).build();
                    row.set_tooltip_text(Some(&version.version_id));
                    let action = |icon: &str, tip: &str| {
                        let b = gtk::Button::builder().icon_name(icon).tooltip_text(tip).valign(gtk::Align::Center).build();
                        b.add_css_class("flat");
                        row.add_suffix(&b);
                        b
                    };
                    if !version.delete_marker {
                        let download = action("folder-download-symbolic", &tr("Download"));
                        let v = version.version_id.clone();
                        download.connect_clicked(glib::clone!(#[strong] win, #[strong] key, move |_| win.download_version(key.clone(), v.clone())));
                        if !version.is_latest {
                            let restore = action("edit-undo-symbolic", &tr("Restore This Version"));
                            let v = version.version_id.clone();
                            restore.connect_clicked(glib::clone!(#[strong] client, #[strong] bucket, #[strong] key, #[strong] toasts, #[strong] reload, #[strong] win, move |_| {
                                let (client, bucket, key, v, toasts, reload, win) = (client.clone(), bucket.clone(), key.clone(), v.clone(), toasts.clone(), reload.clone(), win.clone());
                                glib::spawn_future_local(async move {
                                    match bg(async move { client.restore_version(&bucket, &key, &v).await }).await {
                                        Ok(()) => { toast(&toasts, &tr("Version restored")); win.refresh(); if let Some(f) = reload.borrow().as_ref() { f(); } }
                                        Err(error) => toast(&toasts, &error),
                                    }
                                });
                            }));
                        }
                    }
                    let delete = action("user-trash-symbolic", &tr("Delete Version"));
                    delete.add_css_class("error");
                    let v = version.version_id.clone();
                    delete.connect_clicked(glib::clone!(#[strong] client, #[strong] bucket, #[strong] key, #[strong] toasts, #[strong] reload, #[strong] win, move |button| {
                        let (client, bucket, key, v, toasts, reload, win) = (client.clone(), bucket.clone(), key.clone(), v.clone(), toasts.clone(), reload.clone(), win.clone());
                        let parent = button.clone();
                        glib::spawn_future_local(async move {
                            let confirm = adw::AlertDialog::new(Some(&tr("Delete Version?")), Some(&tr("This version will be deleted permanently. This cannot be undone.")));
                            confirm.add_responses(&[("cancel", &tr("Cancel")), ("delete", &tr("Delete"))]);
                            confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                            confirm.set_close_response("cancel");
                            if confirm.choose_future(Some(&parent)).await != "delete" { return; }
                            match bg(async move { client.delete_version(&bucket, &key, &v).await }).await {
                                Ok(()) => { toast(&toasts, &tr("Version deleted")); win.refresh(); if let Some(f) = reload.borrow().as_ref() { f(); } }
                                Err(error) => toast(&toasts, &error),
                            }
                        });
                    }));
                    list.add(&row);
                    rows.borrow_mut().push(row.upcast());
                }
            });
        }
    };
    reload.replace(Some(Box::new(reload_fn)));
    if let Some(f) = reload.borrow().as_ref() { f(); }

    // Versioning switch of the bucket.
    {
        let (client, bucket, state, toasts) = (client.clone(), bucket.clone(), state.clone(), toasts.clone());
        glib::spawn_future_local(async move {
            let (c, b) = (client.clone(), bucket.clone());
            let Ok(status) = bg(async move { c.versioning(&b).await }).await else { return };
            let row = adw::SwitchRow::builder().title(tr("Versioning of this bucket")).active(status == "Enabled").build();
            row.connect_active_notify(move |row| {
                let (client, bucket, enabled, toasts) = (client.clone(), bucket.clone(), row.is_active(), toasts.clone());
                glib::spawn_future_local(async move {
                    let result = bg(async move { client.set_versioning(&bucket, enabled).await }).await;
                    toast(&toasts, &result.map(|_| tr("Settings saved")).unwrap_or_else(|e| e));
                });
            });
            state.add(&row);
        });
    }
    dialog.present(Some(win));
}

// ----- Headers, tags and metadata -----

pub fn headers(win: &Window, client: S3, bucket: String, info: ObjectInfo) {
    let (dialog, _view, save, toasts) = editing_dialog(&tr("HTTP Headers"), &tr("Save"), 520, 460);
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder().description(tr("S3 changes headers by copying the object onto itself, so its modified date is updated. Custom metadata is kept.")).build();
    let row = |title: &str, value: &str| {
        let r = adw::EntryRow::builder().title(title).text(value).build();
        group.add(&r);
        r
    };
    let content_type = row("Content-Type", &info.content_type);
    let cache_control = row("Cache-Control", &info.cache_control);
    let disposition = row("Content-Disposition", &info.content_disposition);
    let encoding = row("Content-Encoding", &info.content_encoding);
    page.add(&group);
    toasts.set_child(Some(&page));
    let win = win.clone();
    let parent = win.clone();
    save.connect_clicked(glib::clone!(#[weak] dialog, #[weak] toasts, move |button| {
        let mut updated = info.clone();
        updated.content_type = content_type.text().trim().to_string();
        updated.cache_control = cache_control.text().trim().to_string();
        updated.content_disposition = disposition.text().trim().to_string();
        updated.content_encoding = encoding.text().trim().to_string();
        let (client, bucket, win, button) = (client.clone(), bucket.clone(), win.clone(), button.clone());
        let key = updated.key.clone();
        button.set_sensitive(false);
        glib::spawn_future_local(async move {
            match bg(async move { client.rewrite(&bucket, &updated, None).await }).await {
                Ok(()) => { dialog.close(); win.toast(&tr("Headers updated")); win.refresh(); win.show_details(key); }
                Err(error) => { button.set_sensitive(true); toast(&toasts, &error); }
            }
        });
    }));
    dialog.present(Some(&parent));
}

pub fn tags_and_metadata(win: &Window, client: S3, bucket: String, info: ObjectInfo) {
    let win = win.clone();
    glib::spawn_future_local(async move {
        let (c, b, k) = (client.clone(), bucket.clone(), info.key.clone());
        let tags = bg(async move { Ok(c.object_tags(&b, &k).await) }).await.unwrap_or_else(Err);
        let (dialog, _view, toasts) = plain_dialog(&tr("Tags and Metadata"), 620, 600);
        let page = adw::PreferencesPage::new();
        let (tag_pairs, tag_note) = match tags { Ok(t) => (t, String::new()), Err(e) => (Vec::new(), e) };
        let tag_editor = Rc::new(PairEditor::new(&tr("Tags"), &glib::markup_escape_text(&tag_note), &tr("Key"), &tag_pairs));
        page.add(&tag_editor.group);
        let tag_save = adw::PreferencesGroup::new();
        let tag_row = adw::ButtonRow::builder().title(tr("Save Tags")).build();
        tag_save.add(&tag_row);
        page.add(&tag_save);
        let meta_editor = Rc::new(PairEditor::new(&tr("User Metadata"), &tr("x-amz-meta-* headers. Saving rewrites the object in place."), &tr("Name"), &info.metadata));
        page.add(&meta_editor.group);
        let meta_save = adw::PreferencesGroup::new();
        let meta_row = adw::ButtonRow::builder().title(tr("Save Metadata")).build();
        meta_save.add(&meta_row);
        page.add(&meta_save);
        toasts.set_child(Some(&page));

        tag_row.connect_activated(glib::clone!(#[strong] client, #[strong] bucket, #[strong] info, #[weak] toasts, #[strong] tag_editor, move |_| {
            let (client, bucket, key, tags, toasts) = (client.clone(), bucket.clone(), info.key.clone(), tag_editor.pairs(), toasts.clone());
            glib::spawn_future_local(async move {
                let result = bg(async move { client.set_object_tags(&bucket, &key, &tags).await }).await;
                toast(&toasts, &result.map(|_| tr("Tags saved")).unwrap_or_else(|e| e));
            });
        }));
        meta_row.connect_activated(glib::clone!(#[strong] win, #[weak] toasts, #[strong] meta_editor, move |_| {
            let mut updated = info.clone();
            updated.metadata = meta_editor.pairs();
            if updated.metadata.iter().any(|(k, v)| k.contains([' ', ':']) || v.contains('\n')) {
                toast(&toasts, &tr("A metadata name cannot contain spaces or colons, and the value must be one line"));
                return;
            }
            let (client, bucket, toasts, win) = (client.clone(), bucket.clone(), toasts.clone(), win.clone());
            let key = updated.key.clone();
            glib::spawn_future_local(async move {
                match bg(async move { client.rewrite(&bucket, &updated, None).await }).await {
                    Ok(()) => { toast(&toasts, &tr("Metadata saved")); win.refresh(); win.show_details(key); }
                    Err(error) => toast(&toasts, &error),
                }
            });
        }));
        dialog.present(Some(&win));
    });
}

// ----- Copy to another connection -----

pub fn copy_to(win: &Window, client: S3, bucket: String, prefix: String, entries: Vec<Entry>) {
    let profiles = profile::load();
    let (dialog, _view, ok, toasts) = editing_dialog(&tr("Copy to Another Connection"), &tr("Copy"), 520, 470);
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder().description(trf("{n} selected items are copied to the destination. Data is streamed through this computer, so it also works between different providers and accounts.", &[("n", &entries.len().to_string())])).build();
    let names: Vec<String> = profiles.iter().map(|p| if p.id == client.profile.id { format!("{} {}", p.name, tr("(this connection)")) } else { p.name.clone() }).collect();
    let target = adw::ComboRow::builder().title(tr("Destination connection")).model(&gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>())).build();
    target.set_selected(profiles.iter().position(|p| p.id != client.profile.id).unwrap_or(0) as u32);
    let dst_bucket = adw::EntryRow::builder().title(tr("Destination bucket")).build();
    let dst_prefix = adw::EntryRow::builder().title(tr("Destination folder (optional)")).build();
    let move_row = adw::SwitchRow::builder().title(tr("Move")).subtitle(tr("Delete the source objects after they are copied")).build();
    group.add(&target);
    group.add(&dst_bucket);
    group.add(&dst_prefix);
    group.add(&move_row);
    page.add(&group);
    toasts.set_child(Some(&page));
    move_row.connect_active_notify(glib::clone!(#[weak] ok, move |row| {
        ok.set_label(&if row.is_active() { tr("Move") } else { tr("Copy") });
        if row.is_active() { ok.remove_css_class("suggested-action"); ok.add_css_class("destructive-action"); } else { ok.remove_css_class("destructive-action"); ok.add_css_class("suggested-action"); }
    }));
    let win = win.clone();
    let parent = win.clone();
    ok.connect_clicked(glib::clone!(#[weak] dialog, #[weak] toasts, move |_| {
        let Some(destination) = profiles.get(target.selected() as usize).cloned() else { return };
        let dst_bucket_name = dst_bucket.text().trim().to_string();
        if dst_bucket_name.is_empty() { toast(&toasts, &tr("A destination bucket is required")); return; }
        let mut dst_folder = dst_prefix.text().trim().trim_start_matches('/').to_string();
        if !dst_folder.is_empty() && !dst_folder.ends_with('/') { dst_folder.push('/'); }
        if destination.id == client.profile.id && dst_bucket_name == bucket && dst_folder == prefix {
            toast(&toasts, &tr("The destination cannot be the same location as the source"));
            return;
        }
        dialog.close();
        let moving = move_row.is_active();
        let (client, bucket, prefix, entries, win) = (client.clone(), bucket.clone(), prefix.clone(), entries.clone(), win.clone());
        let kind = if moving { "move" } else { "copy" };
        glib::spawn_future_local(async move {
            // The destination is connected and folders are listed once; then every object is one job.
            let (c, b) = (client.clone(), bucket.clone());
            let prepared = bg(async move {
                let dst = S3::connect(profile::with_secrets(destination).await?).await?;
                let mut keys = Vec::new();
                for entry in entries {
                    if entry.key.ends_with('/') {
                        keys.extend(c.list_all(&b, &entry.key, usize::MAX).await?.0.into_iter().map(|e| (e.key, e.size.max(0) as u64)));
                    } else {
                        keys.push((entry.key, entry.size.max(0) as u64));
                    }
                }
                Ok((dst, keys))
            }).await;
            let (dst, keys) = match prepared { Ok(p) => p, Err(error) => { win.toast(&error); return; } };
            let queue = win.queue();
            let batch = queue.batch(glib::clone!(#[weak] win, move |outcome| {
                win.toast(&if outcome.failed > 0 {
                    trn("{n} object could not be copied", "{n} objects could not be copied", &[("n", &outcome.failed.to_string())])
                } else if moving { tr("Objects moved") } else { tr("Objects copied") });
                win.refresh();
            }));
            for (key, size) in keys {
                let relative = key.strip_prefix(&prefix).unwrap_or(&key).to_string();
                let target_key = format!("{dst_folder}{relative}");
                let name = key.trim_end_matches('/').rsplit('/').next().unwrap_or(&key).to_string();
                let detail = format!("{}:{}/{}", dst.profile.name, dst_bucket_name, &target_key[..target_key.len() - target_key.rsplit('/').next().unwrap_or("").len()]);
                let (client, bucket, dst, dst_bucket) = (client.clone(), bucket.clone(), dst.clone(), dst_bucket_name.clone());
                queue.add(Some(batch), kind, &name, &detail, size, None, crate::transfers::queue::work(move |progress| {
                    let (client, bucket, dst, dst_bucket, key, target_key) = (client.clone(), bucket.clone(), dst.clone(), dst_bucket.clone(), key.clone(), target_key.clone());
                    async move {
                        if key.ends_with('/') {
                            dst.create_folder(&dst_bucket, &target_key).await?;
                        } else {
                            client.copy_to(&bucket, &key, &dst, &dst_bucket, &target_key, &progress).await?;
                        }
                        // A move removes each source right after its copy succeeded.
                        if moving { client.delete_object(&bucket, &key).await?; }
                        Ok(())
                    }
                }));
            }
            queue.seal(batch);
        });
    }));
    dialog.present(Some(&parent));
}

// ----- Deleted objects -----

/// The deleted objects of a folder in a versioned bucket, as the Trash of GNOME Files:
/// each can be restored, or all at once.
pub fn deleted_objects(win: &Window, client: S3, bucket: String, prefix: String) {
    let dialog = adw::Dialog::builder().title(tr("Deleted Objects")).content_width(560).content_height(520).build();
    let view = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let restore_all = gtk::Button::builder().label(tr("Restore All")).visible(false).build();
    restore_all.add_css_class("suggested-action");
    header.pack_end(&restore_all);
    view.add_top_bar(&header);
    let stack = gtk::Stack::new();
    stack.add_named(&adw::Spinner::builder().halign(gtk::Align::Center).valign(gtk::Align::Center).width_request(32).height_request(32).build(), Some("loading"));
    view.set_content(Some(&stack));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&view));
    dialog.set_child(Some(&toasts));
    dialog.present(Some(win));
    let win = win.clone();
    glib::spawn_future_local(async move {
        let (c, b, p) = (client.clone(), bucket.clone(), prefix.clone());
        let found = crate::runtime::bg(async move { c.deleted_objects(&b, &p).await }).await;
        let found = match found {
            Ok(found) if !found.is_empty() => found,
            Ok(_) => {
                stack.add_named(&adw::StatusPage::builder().icon_name("user-trash-symbolic").title(tr("No Deleted Objects"))
                    .description(tr("Deleted objects can be restored only in buckets with versioning turned on.")).build(), Some("empty"));
                stack.set_visible_child_name("empty");
                return;
            }
            Err(error) => {
                stack.add_named(&adw::StatusPage::builder().icon_name("dialog-warning-symbolic").title(tr("Deleted Objects Could Not Be Listed"))
                    .description(glib::markup_escape_text(&error)).build(), Some("error"));
                stack.set_visible_child_name("error");
                return;
            }
        };
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder().title(trn("{n} deleted object", "{n} deleted objects", &[("n", &found.len().to_string())]))
            .description(glib::markup_escape_text(&format!("{bucket}/{prefix}"))).build();
        for (key, version, when) in found.iter().take(500).cloned() {
            let shown = key.strip_prefix(&prefix).unwrap_or(&key).to_string();
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(&shown)).subtitle(trf("Deleted {date}", &[("date", &crate::window::format_time(when))])).build();
            let restore = gtk::Button::builder().label(tr("Restore")).valign(gtk::Align::Center).build();
            restore.connect_clicked(glib::clone!(#[weak] row, #[weak] toasts, #[strong] client, #[strong] bucket, #[strong] win, move |button| {
                button.set_sensitive(false);
                let (client, bucket, key, version) = (client.clone(), bucket.clone(), key.clone(), version.clone());
                let win = win.clone();
                glib::spawn_future_local(async move {
                    match crate::runtime::bg(async move { client.undelete(&bucket, vec![(key, version)]).await }).await {
                        Ok(_) => { row.set_visible(false); toasts.add_toast(crate::window::plain_toast(&tr("Object restored"))); win.refresh(); }
                        Err(error) => toasts.add_toast(crate::window::plain_toast(&error)),
                    }
                });
            }));
            row.add_suffix(&restore);
            group.add(&row);
        }
        page.add(&group);
        stack.add_named(&page, Some("list"));
        stack.set_visible_child_name("list");
        restore_all.set_visible(true);
        restore_all.connect_clicked(glib::clone!(#[weak] toasts, #[weak] stack, move |button| {
            button.set_sensitive(false);
            let markers: Vec<(String, String)> = found.iter().map(|(k, v, _)| (k.clone(), v.clone())).collect();
            let (client, bucket, win) = (client.clone(), bucket.clone(), win.clone());
            glib::spawn_future_local(async move {
                match crate::runtime::bg(async move { client.undelete(&bucket, markers).await }).await {
                    Ok(n) => {
                        toasts.add_toast(crate::window::plain_toast(&trn("{n} object restored", "{n} objects restored", &[("n", &n.to_string())])));
                        stack.add_named(&adw::StatusPage::builder().icon_name("emblem-ok-symbolic").title(tr("All Objects Restored")).build(), Some("done"));
                        stack.set_visible_child_name("done");
                        win.refresh();
                    }
                    Err(error) => toasts.add_toast(crate::window::plain_toast(&error)),
                }
            });
        }));
    });
}

// ----- Folder sync -----

pub fn sync(win: &Window, client: S3, bucket: String, prefix: String) {
    let modes = [
        (tr("Upload new and changed files"), tr("New and changed files are uploaded; nothing is deleted from the bucket."), false, false),
        (tr("Upload and mirror"), tr("New and changed files are uploaded, then objects that are no longer in the folder are deleted from the bucket. Nothing is deleted if the source folder is empty."), false, true),
        (tr("Download new and changed objects"), tr("New and changed objects are downloaded into the folder; nothing is deleted locally."), true, false),
        (tr("Download and mirror"), tr("New and changed objects are downloaded, then local files that have no object anymore are deleted from the folder. Nothing is deleted when the location has no objects."), true, true),
    ];
    let dialog = adw::AlertDialog::new(Some(&tr("Sync a Local Folder")), Some(&trf("You choose the local folder next. Location: {path}", &[("path", &format!("{bucket}/{prefix}"))])));
    let dropdown = gtk::DropDown::from_strings(&modes.iter().map(|m| m.0.as_str()).collect::<Vec<_>>());
    let note = gtk::Label::builder().label(&modes[0].1).wrap(true).xalign(0.0).margin_top(12).build();
    let extra = gtk::Box::new(gtk::Orientation::Vertical, 0);
    extra.append(&dropdown);
    extra.append(&note);
    dialog.set_extra_child(Some(&extra));
    let notes: Vec<(String, bool)> = modes.iter().map(|m| (m.1.clone(), m.3)).collect();
    dropdown.connect_selected_notify(glib::clone!(#[weak] note, move |d| {
        let (text, danger) = &notes[d.selected() as usize];
        note.set_label(text);
        if *danger { note.add_css_class("error"); } else { note.remove_css_class("error"); }
    }));
    dialog.add_responses(&[("cancel", &tr("Cancel")), ("choose", &tr("Choose Folder…"))]);
    dialog.set_response_appearance("choose", adw::ResponseAppearance::Suggested);
    dialog.set_close_response("cancel");
    let win = win.clone();
    glib::spawn_future_local(async move {
        if dialog.choose_future(Some(&win)).await != "choose" { return; }
        let (title, _, down, mirror) = modes[dropdown.selected() as usize].clone();
        let chooser = gtk::FileDialog::builder().title(&title).modal(true).build();
        let Ok(folder) = chooser.select_folder_future(Some(&win)).await else { return };
        let Some(dir) = folder.path() else { return };
        // The changes are shown first; deletions need a second, deliberate confirmation.
        let (c, b, p, d) = (client.clone(), bucket.clone(), prefix.clone(), dir.clone());
        let plan = match crate::runtime::bg(async move { c.sync_plan(&b, &p, &d, down, mirror).await }).await {
            Ok(plan) => plan,
            Err(error) => { win.toast(&error); return; }
        };
        if plan.transfer.is_empty() && plan.delete.is_empty() {
            win.toast(&trn("Already in sync; {n} file is up to date", "Already in sync; {n} files are up to date", &[("n", &plan.skipped.to_string())]));
            return;
        }
        if !review_sync(&win, &plan, down).await { return; }
        let summary_out = Arc::new(std::sync::Mutex::new(String::new()));
        let out = summary_out.clone();
        let detail = format!("{} ↔ {bucket}/{prefix}", dir.display());
        let local = dir.display().to_string();
        let spec = serde_json::json!({ "kind": "sync", "profile": client.profile.id, "bucket": bucket, "prefix": prefix, "path": local, "down": down, "mirror": mirror, "title": title });
        let outcome = win.run_one_kept("sync", &title, &detail, 0, Some(&local), Some(spec), crate::transfers::queue::work(move |progress| {
            let (client, bucket, prefix, dir, out) = (client.clone(), bucket.clone(), prefix.clone(), dir.clone(), out.clone());
            async move {
                let result = if down { client.sync_down(&bucket, &prefix, &dir, mirror, &progress).await? } else { client.sync_up(&bucket, &prefix, &dir, mirror, &progress).await? };
                *out.lock().unwrap() = trf("{t} transferred, {s} up to date, {d} deleted", &[("t", &result.transferred.to_string()), ("s", &result.skipped.to_string()), ("d", &result.deleted.to_string())]);
                if result.failed > 0 { Err(trn("{n} file could not be transferred", "{n} files could not be transferred", &[("n", &result.failed.to_string())])) } else { Ok(()) }
            }
        })).await;
        if outcome.done > 0 { win.toast(&summary_out.lock().unwrap()); }
        win.refresh();
    });
}

use std::sync::Arc;

/// Lists what a sync will transfer and delete; true when the user goes ahead.
pub(crate) async fn review_sync(win: &Window, plan: &crate::s3::tools::SyncPlan, down: bool) -> bool {
    let size: u64 = plan.transfer.iter().map(|(_, s)| s).sum();
    let mut body = if down {
        trn("{n} file will be downloaded ({size}).", "{n} files will be downloaded ({size}).", &[("n", &plan.transfer.len().to_string()), ("size", &glib::format_size(size))])
    } else {
        trn("{n} file will be uploaded ({size}).", "{n} files will be uploaded ({size}).", &[("n", &plan.transfer.len().to_string()), ("size", &glib::format_size(size))])
    };
    if !plan.delete.is_empty() {
        body.push(' ');
        body.push_str(&if down {
            trn("{n} local file will be deleted.", "{n} local files will be deleted.", &[("n", &plan.delete.len().to_string())])
        } else {
            trn("{n} object will be deleted from the bucket.", "{n} objects will be deleted from the bucket.", &[("n", &plan.delete.len().to_string())])
        });
    }
    let dialog = adw::AlertDialog::new(Some(&tr("Review Changes")), Some(&body));
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    const SHOWN: usize = 300;
    let row = |path: &str, icon: &str, class: Option<&str>| {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(path)).title_lines(1).build();
        let image = gtk::Image::from_icon_name(icon);
        if let Some(class) = class { image.add_css_class(class); row.add_css_class(class); }
        row.add_prefix(&image);
        row
    };
    for path in plan.delete.iter().take(SHOWN) { list.append(&row(path, "user-trash-symbolic", Some("error"))); }
    let transfer_icon = if down { "transfer-download-symbolic" } else { "transfer-upload-symbolic" };
    for (path, _) in plan.transfer.iter().take(SHOWN.saturating_sub(plan.delete.len().min(SHOWN))) { list.append(&row(path, transfer_icon, None)); }
    let hidden = (plan.delete.len() + plan.transfer.len()).saturating_sub(SHOWN);
    if hidden > 0 {
        list.append(&adw::ActionRow::builder().title(trf("and {n} more", &[("n", &hidden.to_string())])).css_classes(["dim-label"]).build());
    }
    let scroller = gtk::ScrolledWindow::builder().child(&list).min_content_height(160).max_content_height(320).propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never).build();
    dialog.set_extra_child(Some(&scroller));
    dialog.add_responses(&[("cancel", &tr("Cancel")), ("sync", &tr("Sync"))]);
    dialog.set_response_appearance("sync", if plan.delete.is_empty() { adw::ResponseAppearance::Suggested } else { adw::ResponseAppearance::Destructive });
    dialog.set_default_response(Some(if plan.delete.is_empty() { "sync" } else { "cancel" }));
    dialog.set_close_response("cancel");
    dialog.choose_future(Some(win)).await == "sync"
}
