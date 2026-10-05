use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::i18n::{tr, trf, trn};
use crate::runtime::bg;
use crate::s3::{Entry, S3};
use crate::window::actions::category;
use crate::window::{Window, format_size, format_time, icon_for};

const IMAGE_LIMIT: i64 = 64 * 1024 * 1024;
const TEXT_LIMIT: u64 = 1024 * 1024;
const ZIP_LIMIT: i64 = 64 * 1024 * 1024;

pub(crate) fn is_text(name: &str) -> bool {
    category(name) == 7
        || matches!(
            name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).as_deref(),
            Some(
                "txt"
                    | "md"
                    | "csv"
                    | "tsv"
                    | "log"
                    | "ini"
                    | "conf"
                    | "cfg"
                    | "env"
                    | "gitignore"
                    | "properties"
                    | "srt"
                    | "vtt"
            )
        )
}

pub fn present(win: &Window, client: S3, bucket: String, files: Vec<Entry>, index: usize) {
    if files.is_empty() {
        return;
    }
    let dialog = adw::Dialog::builder().content_width(960).content_height(680).build();
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("", "");
    header.set_title_widget(Some(&title));
    let nav = gtk::Box::builder().css_classes(["linked"]).build();
    let previous = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text(tr("Previous")).build();
    let next = gtk::Button::builder().icon_name("go-next-symbolic").tooltip_text(tr("Next")).build();
    nav.append(&previous);
    nav.append(&next);
    header.pack_start(&nav);
    let open = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .tooltip_text(tr("Open With Default Application"))
        .build();
    let download = gtk::Button::builder().icon_name("folder-download-symbolic").tooltip_text(tr("Download…")).build();
    let share = gtk::Button::builder().icon_name("send-to-symbolic").tooltip_text(tr("Share Link…")).build();
    let copy_image =
        gtk::Button::builder().icon_name("edit-copy-symbolic").tooltip_text(tr("Copy Image")).visible(false).build();
    header.pack_end(&open);
    header.pack_end(&share);
    header.pack_end(&download);
    header.pack_end(&copy_image);
    let current_image: Rc<RefCell<Option<gdk::Texture>>> = Rc::default();
    copy_image.connect_clicked(glib::clone!(
        #[strong]
        current_image,
        #[weak]
        dialog,
        move |_| {
            if let Some(texture) = current_image.borrow().as_ref() {
                dialog.clipboard().set_texture(texture);
                if let Some(win) = dialog.root().and_downcast::<Window>() {
                    win.toast(&tr("Image copied"));
                }
            }
        }
    ));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    let stack =
        gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).transition_duration(150).build();
    view.set_content(Some(&stack));
    dialog.set_child(Some(&view));

    let files = Rc::new(files);
    let index = Rc::new(Cell::new(index.min(files.len() - 1)));
    let generation = Rc::new(Cell::new(0u64));
    let media: Rc<RefCell<Option<gtk::MediaStream>>> = Rc::default();

    let show: Rc<dyn Fn()> = {
        let (files, index, generation, media, stack, title, previous, next, client, bucket, copy_image, current_image) = (
            files.clone(),
            index.clone(),
            generation.clone(),
            media.clone(),
            stack.clone(),
            title.clone(),
            previous.clone(),
            next.clone(),
            client.clone(),
            bucket.clone(),
            copy_image.clone(),
            current_image.clone(),
        );
        Rc::new(move || {
            let entry = files[index.get()].clone();
            let mine = generation.get() + 1;
            generation.set(mine);
            if let Some(stream) = media.borrow_mut().take() {
                stream.pause();
            }
            title.set_title(&entry.name);
            title.set_subtitle(&format!(
                "{} · {} · {}/{}",
                format_size(entry.size),
                format_time(entry.modified),
                index.get() + 1,
                files.len()
            ));
            previous.set_sensitive(index.get() > 0);
            next.set_sensitive(index.get() + 1 < files.len());
            let spinner = adw::Spinner::builder()
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .width_request(48)
                .height_request(48)
                .build();
            let name = format!("loading-{mine}");
            stack.add_named(&spinner, Some(&name));
            stack.set_visible_child(&spinner);
            let kind = category(&entry.name);
            copy_image.set_visible(false);
            current_image.replace(None);
            let (client, bucket, stack, generation, media, copy_image, current_image) = (
                client.clone(),
                bucket.clone(),
                stack.clone(),
                generation.clone(),
                media.clone(),
                copy_image.clone(),
                current_image.clone(),
            );
            glib::spawn_future_local(async move {
                let key = entry.key.clone();
                let widget: gtk::Widget = if kind == 2 && entry.size <= IMAGE_LIMIT {
                    let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
                    match bg(async move { c.read_bytes(&b, &k, IMAGE_LIMIT as u64).await })
                        .await
                        .ok()
                        .filter(|bytes| crate::widgets::image_fits(bytes))
                        .and_then(|bytes| gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok())
                    {
                        Some(texture) => {
                            // Stale result, the user already moved on.
                            if generation.get() == mine {
                                current_image.replace(Some(texture.clone()));
                                copy_image.set_visible(true);
                            }
                            zoomable(&texture)
                        }
                        None => fallback(&entry, &tr("This image could not be shown")),
                    }
                } else if kind == 3 || kind == 4 {
                    let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
                    match bg(async move { c.presign(&b, &k, 3600).await }).await {
                        Ok(url) => {
                            let file = gtk::MediaFile::for_file(&gio::File::for_uri(&url));
                            let video = gtk::Video::builder()
                                .media_stream(&file)
                                .autoplay(true)
                                .vexpand(true)
                                .hexpand(true)
                                .build();
                            media.replace(Some(file.upcast()));
                            if kind == 4 {
                                let column = gtk::Box::builder()
                                    .orientation(gtk::Orientation::Vertical)
                                    .spacing(24)
                                    .valign(gtk::Align::Center)
                                    .margin_start(48)
                                    .margin_end(48)
                                    .build();
                                column.append(
                                    &gtk::Image::builder()
                                        .gicon(&icon_for(&entry))
                                        .pixel_size(128)
                                        .css_classes(["dim-label"])
                                        .build(),
                                );
                                video.set_vexpand(false);
                                video.set_height_request(60);
                                column.append(&video);
                                column.upcast()
                            } else {
                                video.upcast()
                            }
                        }
                        Err(error) => fallback(&entry, &error),
                    }
                } else if entry.name.to_lowercase().ends_with(".zip") && entry.size <= ZIP_LIMIT {
                    let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
                    match bg(async move { c.read_bytes(&b, &k, ZIP_LIMIT as u64).await }).await {
                        Ok(bytes) => archive_view(bytes).unwrap_or_else(|error| fallback(&entry, &error)),
                        Err(error) => fallback(&entry, &error),
                    }
                } else if is_text(&entry.name) {
                    let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
                    match bg(async move { c.read_bytes(&b, &k, TEXT_LIMIT).await }).await {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes).into_owned();
                            let extension =
                                entry.name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
                            if let Some(table) = (extension == "csv" || extension == "tsv")
                                .then(|| table_view(&text, if extension == "tsv" { '\t' } else { ',' }))
                                .flatten()
                            {
                                table
                            } else {
                                let text = if extension == "json" && text.lines().count() <= 2 && text.len() > 120 {
                                    serde_json::from_str::<serde_json::Value>(&text)
                                        .ok()
                                        .and_then(|v| serde_json::to_string_pretty(&v).ok())
                                        .unwrap_or(text)
                                } else {
                                    text
                                };
                                let view = crate::widgets::code_view::new(&text, &entry.name);
                                view.set_editable(false);
                                view.set_wrap_mode(gtk::WrapMode::WordChar);
                                view.set_top_margin(12);
                                view.set_bottom_margin(12);
                                view.set_left_margin(12);
                                view.set_right_margin(12);
                                let scroller = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();
                                if entry.size as u64 > TEXT_LIMIT {
                                    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
                                    let banner = adw::Banner::builder()
                                        .title(trf(
                                            "Showing the first {size} of {total}",
                                            &[
                                                ("size", &format_size(TEXT_LIMIT as i64)),
                                                ("total", &format_size(entry.size)),
                                            ],
                                        ))
                                        .revealed(true)
                                        .build();
                                    page.append(&banner);
                                    page.append(&scroller);
                                    page.upcast()
                                } else {
                                    scroller.upcast()
                                }
                            }
                        }
                        Err(error) => fallback(&entry, &error),
                    }
                } else {
                    fallback(&entry, &tr("No preview for this file type"))
                };
                if generation.get() != mine {
                    return;
                }
                let name = format!("content-{mine}");
                stack.add_named(&widget, Some(&name));
                stack.set_visible_child(&widget);
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(300),
                    glib::clone!(
                        #[weak]
                        stack,
                        #[weak]
                        widget,
                        move || {
                            let mut child = stack.first_child();
                            while let Some(c) = child {
                                child = c.next_sibling();
                                if c != widget {
                                    stack.remove(&c);
                                }
                            }
                        }
                    ),
                );
            });
        })
    };
    show();

    let step = {
        let (index, files, show) = (index.clone(), files.clone(), show.clone());
        move |delta: i32| {
            let target = index.get() as i64 + delta as i64;
            if target >= 0 && (target as usize) < files.len() {
                index.set(target as usize);
                show();
            }
        }
    };
    let s = step.clone();
    previous.connect_clicked(move |_| s(-1));
    let s = step.clone();
    next.connect_clicked(move |_| s(1));
    let keys = gtk::EventControllerKey::new();
    let s = step.clone();
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        dialog,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            match key {
                gdk::Key::Left => {
                    s(-1);
                    glib::Propagation::Stop
                }
                gdk::Key::Right => {
                    s(1);
                    glib::Propagation::Stop
                }
                gdk::Key::space => {
                    dialog.close();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }
    ));
    dialog.add_controller(keys);
    let current = {
        let (files, index) = (files.clone(), index.clone());
        move || files[index.get()].clone()
    };
    let c = current.clone();
    open.connect_clicked(glib::clone!(
        #[weak]
        win,
        #[strong]
        client,
        #[strong]
        bucket,
        move |_| crate::transfers::external::open(&win, client.clone(), bucket.clone(), c().key)
    ));
    let c = current.clone();
    download.connect_clicked(glib::clone!(
        #[weak]
        win,
        move |_| win.download(vec![c()])
    ));
    let c = current.clone();
    share.connect_clicked(glib::clone!(
        #[weak]
        win,
        move |_| win.presign(c().key)
    ));
    dialog.connect_closed(move |_| {
        if let Some(stream) = media.borrow_mut().take() {
            stream.pause();
        }
    });
    dialog.present(Some(win));
}

fn fallback(entry: &Entry, message: &str) -> gtk::Widget {
    let page = adw::StatusPage::builder()
        .title(glib::markup_escape_text(&entry.name))
        .description(glib::markup_escape_text(message))
        .build();
    page.set_paintable(Some(&gtk::IconTheme::for_display(&gdk::Display::default().unwrap()).lookup_by_gicon(
        &icon_for(entry),
        128,
        1,
        gtk::TextDirection::None,
        gtk::IconLookupFlags::empty(),
    )));
    page.upcast()
}

fn archive_view(bytes: Vec<u8>) -> Result<gtk::Widget, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut entries: Vec<(String, u64, bool)> = Vec::new();
    for i in 0..archive.len().min(5000) {
        let file = archive.by_index(i).map_err(|e| e.to_string())?;
        entries.push((file.name().to_string(), file.size(), file.is_dir()));
    }
    let files = entries.iter().filter(|e| !e.2).count();
    let total: u64 = entries.iter().map(|e| e.1).sum();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    for (name, size, dir) in entries.iter().take(1000) {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(name)).title_lines(1).build();
        let icon = if *dir {
            gio::ThemedIcon::new("folder-symbolic").upcast::<gio::Icon>()
        } else {
            let (content_type, _) = gio::content_type_guess(Some(name.as_str()), None::<&[u8]>);
            gio::content_type_get_symbolic_icon(&content_type)
        };
        row.add_prefix(&gtk::Image::from_gicon(&icon));
        if !dir {
            row.add_suffix(
                &gtk::Label::builder().label(format_size(*size as i64)).css_classes(["dim-label", "numeric"]).build(),
            );
        }
        list.append(&row);
    }
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title(trn("{n} file", "{n} files", &[("n", &files.to_string())]))
        .description(trf("{size} when extracted", &[("size", &format_size(total as i64))]))
        .build();
    group.add(&list);
    page.add(&group);
    Ok(page.upcast())
}

pub(crate) fn parse_delimited(text: &str, delimiter: char, max_rows: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let (mut row, mut field, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            c if c == delimiter => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                if rows.len() >= max_rows {
                    return rows;
                }
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn table_view(text: &str, delimiter: char) -> Option<gtk::Widget> {
    let rows = parse_delimited(text, delimiter, 2001);
    let (header, body) = rows.split_first()?;
    let columns = rows.iter().map(|r| r.len()).max().unwrap_or(0).min(60);
    if columns < 2 {
        return None;
    }
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    for row in body {
        store.append(&glib::BoxedAnyObject::new(row.clone()));
    }
    let view = gtk::ColumnView::builder()
        .model(&gtk::NoSelection::new(Some(store)))
        .show_column_separators(true)
        .show_row_separators(true)
        .css_classes(["data-table"])
        .build();
    for index in 0..columns {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let label =
                gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(40).build();
            item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&label));
        });
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = item.item().and_downcast::<glib::BoxedAnyObject>().unwrap();
            let text = row.borrow::<Vec<String>>().get(index).cloned().unwrap_or_default();
            let label = item.child().and_downcast::<gtk::Label>().unwrap();
            label.set_tooltip_text(if text.chars().count() > 40 { Some(text.as_str()) } else { None });
            label.set_text(&text);
        });
        let title = header.get(index).cloned().unwrap_or_default();
        let column = gtk::ColumnViewColumn::builder()
            .title(&title)
            .factory(&factory)
            .resizable(true)
            .expand(index + 1 == columns)
            .build();
        view.append_column(&column);
    }
    let note = gtk::Label::builder()
        .xalign(0.0)
        .margin_start(12)
        .margin_top(6)
        .margin_bottom(6)
        .css_classes(["caption", "dim-label"])
        .label(trn("{n} row shown", "{n} rows shown", &[("n", &body.len().to_string())]))
        .build();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&gtk::ScrolledWindow::builder().child(&view).vexpand(true).build());
    page.append(&note);
    Some(page.upcast())
}

fn zoomable(texture: &gdk::Texture) -> gtk::Widget {
    let picture =
        gtk::Picture::builder().paintable(texture).content_fit(gtk::ContentFit::ScaleDown).can_shrink(true).build();
    let scroller = gtk::ScrolledWindow::builder().child(&picture).vexpand(true).hexpand(true).focusable(true).build();
    let (width, height) = (texture.width() as f64, texture.height() as f64);
    let zoom = Rc::new(Cell::new(0.0f64));
    let apply: Rc<dyn Fn(f64)> = {
        let (picture, zoom) = (picture.clone(), zoom.clone());
        Rc::new(move |factor: f64| {
            let factor = if factor <= 0.0 { 0.0 } else { factor.clamp(0.05, 16.0) };
            zoom.set(factor);
            if factor == 0.0 {
                picture.set_size_request(-1, -1);
                picture.set_can_shrink(true);
                picture.set_content_fit(gtk::ContentFit::ScaleDown);
            } else {
                picture.set_can_shrink(false);
                picture.set_content_fit(gtk::ContentFit::Fill);
                picture.set_size_request((width * factor) as i32, (height * factor) as i32);
            }
        })
    };
    let current = {
        let (zoom, scroller) = (zoom.clone(), scroller.clone());
        move || {
            if zoom.get() > 0.0 {
                zoom.get()
            } else {
                (scroller.width() as f64 / width).min(scroller.height() as f64 / height).clamp(0.05, 1.0)
            }
        }
    };
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    let (a, c) = (apply.clone(), current.clone());
    scroll.connect_scroll(move |controller, _, dy| {
        if !controller.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
            return glib::Propagation::Proceed;
        }
        a(c() * if dy < 0.0 { 1.2 } else { 1.0 / 1.2 });
        glib::Propagation::Stop
    });
    scroller.add_controller(scroll);
    let pinch = gtk::GestureZoom::new();
    let start = Rc::new(Cell::new(1.0f64));
    let (st, c) = (start.clone(), current.clone());
    pinch.connect_begin(move |_, _| st.set(c()));
    let a = apply.clone();
    pinch.connect_scale_changed(move |_, scale| a(start.get() * scale));
    scroller.add_controller(pinch);
    let click = gtk::GestureClick::new();
    let (a, z) = (apply.clone(), zoom.clone());
    click.connect_pressed(move |_, presses, _, _| {
        if presses == 2 {
            a(if z.get() == 0.0 { 1.0 } else { 0.0 });
        }
    });
    scroller.add_controller(click);
    let drag = gtk::GestureDrag::new();
    let origin = Rc::new(Cell::new((0.0, 0.0)));
    let (o, sc) = (origin.clone(), scroller.clone());
    drag.connect_drag_begin(move |_, _, _| o.set((sc.hadjustment().value(), sc.vadjustment().value())));
    let sc = scroller.clone();
    drag.connect_drag_update(move |_, dx, dy| {
        let (x, y) = origin.get();
        sc.hadjustment().set_value(x - dx);
        sc.vadjustment().set_value(y - dy);
    });
    scroller.add_controller(drag);
    let keys = gtk::EventControllerKey::new();
    let (a, c) = (apply.clone(), current.clone());
    keys.connect_key_pressed(move |_, key, _, _| match key {
        gdk::Key::plus | gdk::Key::KP_Add | gdk::Key::equal => {
            a(c() * 1.25);
            glib::Propagation::Stop
        }
        gdk::Key::minus | gdk::Key::KP_Subtract => {
            a(c() / 1.25);
            glib::Propagation::Stop
        }
        gdk::Key::_0 | gdk::Key::KP_0 => {
            a(0.0);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    scroller.add_controller(keys);
    scroller.upcast()
}

#[cfg(test)]
mod tests {
    use super::parse_delimited;

    #[test]
    fn delimited_text() {
        let rows =
            parse_delimited("name,note\r\nbeach,\"sun, sand\"\n\"say \"\"hi\"\"\",\"two\nlines\"\nlast,row", ',', 100);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1], vec!["beach", "sun, sand"]);
        assert_eq!(rows[2], vec!["say \"hi\"", "two\nlines"]);
        assert_eq!(rows[3], vec!["last", "row"]);
        assert_eq!(parse_delimited("a\tb\n1\t2\n", '\t', 100), vec![vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(parse_delimited("a\nb\nc\n", ',', 2).len(), 2);
    }
}
