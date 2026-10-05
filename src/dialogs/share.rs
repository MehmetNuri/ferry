use adw::prelude::*;
use gtk::{gdk, glib};
use std::rc::Rc;

use crate::i18n::{tr, trf, trn};
use crate::runtime::bg;
use crate::s3::S3;
use crate::window::Window;

const LIFETIMES: [u64; 5] = [900, 3600, 86_400, 604_800, 300];

fn qr_texture(text: &str) -> Option<gdk::Texture> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let (scale, quiet) = (8usize, 4usize);
    let size = (width + quiet * 2) * scale;
    let mut pixels = vec![255u8; size * size * 3];
    for (i, color) in colors.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let (x, y) = (i % width + quiet, i / width + quiet);
        for dy in 0..scale {
            let row = (y * scale + dy) * size;
            for dx in 0..scale {
                let p = (row + x * scale + dx) * 3;
                pixels[p..p + 3].copy_from_slice(&[0, 0, 0]);
            }
        }
    }
    let bytes = glib::Bytes::from_owned(pixels);
    Some(gdk::MemoryTexture::new(size as i32, size as i32, gdk::MemoryFormat::R8g8b8, &bytes, size * 3).upcast())
}

pub fn present_many(win: &Window, client: S3, bucket: String, keys: Vec<String>) {
    let dialog = adw::AlertDialog::new(
        Some(&trn("Share {n} File", "Share {n} Files", &[("n", &keys.len().to_string())])),
        Some(&tr("Anyone with a link can download its file until the link expires. The links are copied as a list.")),
    );
    let labels = [tr("15 minutes"), tr("1 hour"), tr("1 day"), tr("7 days"), tr("5 minutes")];
    let lifetime = adw::ComboRow::builder()
        .title(tr("Valid for"))
        .model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(1)
        .build();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    list.append(&lifetime);
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", &tr("Cancel")), ("copy", &tr("Copy Links"))]);
    dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("copy"));
    dialog.set_close_response("cancel");
    let win = win.clone();
    glib::spawn_future_local(async move {
        if dialog.choose_future(Some(&win)).await != "copy" {
            return;
        }
        let seconds = LIFETIMES[lifetime.selected() as usize];
        let count = keys.len();
        match bg(async move {
            let mut links = Vec::new();
            for key in keys {
                links.push(client.presign(&bucket, &key, seconds).await?);
            }
            Ok(links)
        })
        .await
        {
            Ok(links) => {
                win.clipboard().set_text(&links.join("\n"));
                win.toast(&trn("{n} link copied", "{n} links copied", &[("n", &count.to_string())]));
            }
            Err(error) => win.toast(&error),
        }
    });
}

pub fn present(win: &Window, client: S3, bucket: String, key: String) {
    let name = key.rsplit('/').next().unwrap_or(&key).to_string();
    let dialog = adw::Dialog::builder().title(trf("Share {name}", &[("name", &name)])).content_width(440).build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    let toasts = adw::ToastOverlay::new();
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .description(tr("Anyone with the link can download this object until it expires. No account is needed."))
        .build();
    let labels = [tr("15 minutes"), tr("1 hour"), tr("1 day"), tr("7 days"), tr("5 minutes")];
    let lifetime = adw::ComboRow::builder()
        .title(tr("Valid for"))
        .model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(1)
        .build();
    let link = adw::ActionRow::builder()
        .title(tr("Web Address"))
        .subtitle(tr("Creating…"))
        .subtitle_lines(2)
        .subtitle_selectable(true)
        .build();
    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text(tr("Copy Link"))
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .sensitive(false)
        .build();
    link.add_suffix(&copy);
    group.add(&lifetime);
    group.add(&link);
    page.add(&group);
    let qr_group = adw::PreferencesGroup::builder()
        .title(tr("QR Code"))
        .description(tr("Scan with a phone camera to open the link"))
        .build();
    let picture = gtk::Picture::builder()
        .width_request(240)
        .height_request(240)
        .halign(gtk::Align::Center)
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .css_classes(["card"])
        .build();
    qr_group.add(&picture);
    page.add(&qr_group);
    toasts.set_child(Some(&page));
    view.set_content(Some(&toasts));
    dialog.set_child(Some(&view));

    let url: Rc<std::cell::RefCell<String>> = Rc::default();
    let refresh = {
        let (link, copy, picture, url, lifetime, client, bucket, key) = (
            link.clone(),
            copy.clone(),
            picture.clone(),
            url.clone(),
            lifetime.clone(),
            client.clone(),
            bucket.clone(),
            key.clone(),
        );
        move || {
            let seconds = LIFETIMES[lifetime.selected() as usize];
            let (link, copy, picture, url, client, bucket, key) =
                (link.clone(), copy.clone(), picture.clone(), url.clone(), client.clone(), bucket.clone(), key.clone());
            copy.set_sensitive(false);
            glib::spawn_future_local(async move {
                match bg(async move { client.presign(&bucket, &key, seconds).await }).await {
                    Ok(address) => {
                        link.set_subtitle(&glib::markup_escape_text(&address));
                        picture.set_paintable(qr_texture(&address).as_ref());
                        url.replace(address);
                        copy.set_sensitive(true);
                    }
                    Err(error) => link.set_subtitle(&glib::markup_escape_text(&error)),
                }
            });
        }
    };
    refresh();
    lifetime.connect_selected_notify(move |_| refresh());
    copy.connect_clicked(glib::clone!(
        #[weak]
        toasts,
        move |button| {
            button.clipboard().set_text(&url.borrow());
            toasts.add_toast(crate::window::plain_toast(&tr("Link copied")));
        }
    ));
    dialog.present(Some(win));
}

pub fn present_upload(win: &Window, client: S3, bucket: String, prefix: String) {
    let dialog = adw::Dialog::builder().title(tr("Upload Link")).content_width(480).build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    let toasts = adw::ToastOverlay::new();
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder().description(trf("Anyone with the link can upload one file to {path} until it expires. The file gets the name below; uploading again replaces it.", &[("path", &format!("{bucket}/{prefix}"))])).build();
    let name = adw::EntryRow::builder().title(tr("File name")).text("upload.bin").build();
    let labels = [tr("15 minutes"), tr("1 hour"), tr("1 day"), tr("7 days"), tr("5 minutes")];
    let lifetime = adw::ComboRow::builder()
        .title(tr("Valid for"))
        .model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(1)
        .build();
    let create = adw::ButtonRow::builder().title(tr("Create Link")).build();
    group.add(&name);
    group.add(&lifetime);
    group.add(&create);
    page.add(&group);
    let result = adw::PreferencesGroup::builder().visible(false).build();
    let link = adw::ActionRow::builder().title(tr("Web Address")).subtitle_lines(2).subtitle_selectable(true).build();
    let copy_link = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text(tr("Copy Link"))
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    link.add_suffix(&copy_link);
    let command = adw::ActionRow::builder().title(tr("Command")).subtitle_lines(3).subtitle_selectable(true).build();
    let copy_command = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text(tr("Copy Command"))
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    command.add_suffix(&copy_command);
    let picture = gtk::Picture::builder()
        .width_request(220)
        .height_request(220)
        .halign(gtk::Align::Center)
        .can_shrink(true)
        .css_classes(["card"])
        .build();
    result.add(&link);
    result.add(&command);
    result.add(&picture);
    page.add(&result);
    toasts.set_child(Some(&page));
    view.set_content(Some(&toasts));
    dialog.set_child(Some(&view));
    let url: Rc<std::cell::RefCell<String>> = Rc::default();
    let line: Rc<std::cell::RefCell<String>> = Rc::default();
    create.connect_activated(glib::clone!(
        #[weak]
        toasts,
        #[weak]
        result,
        #[weak]
        link,
        #[weak]
        command,
        #[weak]
        picture,
        #[weak]
        name,
        #[weak]
        lifetime,
        #[strong]
        url,
        #[strong]
        line,
        move |row| {
            let file = name.text().trim().trim_matches('/').to_string();
            if file.is_empty() {
                return;
            }
            let key = format!("{prefix}{file}");
            let seconds = LIFETIMES[lifetime.selected() as usize];
            let (client, bucket, row, url, line) =
                (client.clone(), bucket.clone(), row.clone(), url.clone(), line.clone());
            row.set_sensitive(false);
            glib::spawn_future_local(async move {
                let made = bg(async move { client.presign_put(&bucket, &key, seconds).await }).await;
                row.set_sensitive(true);
                match made {
                    Ok((address, content_type)) => {
                        // The signed content type must be sent too.
                        let shell = format!(
                            "curl -T {} -H {} {}",
                            glib::shell_quote(&file).to_string_lossy(),
                            glib::shell_quote(format!("Content-Type: {content_type}")).to_string_lossy(),
                            glib::shell_quote(&address).to_string_lossy()
                        );
                        link.set_subtitle(&glib::markup_escape_text(&address));
                        command.set_subtitle(&glib::markup_escape_text(&shell));
                        picture.set_paintable(qr_texture(&address).as_ref());
                        url.replace(address);
                        line.replace(shell);
                        result.set_visible(true);
                    }
                    Err(error) => toasts.add_toast(crate::window::plain_toast(&error)),
                }
            });
        }
    ));
    copy_link.connect_clicked(glib::clone!(
        #[weak]
        toasts,
        #[strong]
        url,
        move |b| {
            b.clipboard().set_text(&url.borrow());
            toasts.add_toast(crate::window::plain_toast(&tr("Link copied")));
        }
    ));
    copy_command.connect_clicked(glib::clone!(
        #[weak]
        toasts,
        move |b| {
            b.clipboard().set_text(&line.borrow());
            toasts.add_toast(crate::window::plain_toast(&tr("Command copied")));
        }
    ));
    dialog.present(Some(win));
}
