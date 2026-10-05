use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::Cell;
use std::rc::Rc;

use crate::i18n::{tr, trf, trn};
use crate::settings::settings;
use crate::transfers::queue::{CANCELED, DONE, FAILED, PAUSED, QUEUED, Queue, RUNNING, Summary, TransferItem};
use crate::window::format_size;

pub const BANDWIDTHS: [u64; 9] = [0, 256, 512, 1024, 2048, 5120, 10240, 20480, 51200];

fn bandwidth_label(kbps: u64) -> String {
    match kbps {
        0 => tr("Unlimited"),
        k if k >= 1024 => format!("{} MB/s", k / 1024),
        k => format!("{k} KB/s"),
    }
}

pub fn duration_label(seconds: u64) -> String {
    match seconds {
        0..60 => trf("{n} s left", &[("n", &seconds.max(1).to_string())]),
        60..3600 => trf("{n} min left", &[("n", &seconds.div_ceil(60).to_string())]),
        _ => trf(
            "{h} h {m} min left",
            &[("h", &(seconds / 3600).to_string()), ("m", &((seconds % 3600) / 60).to_string())],
        ),
    }
}

pub fn summary_line(s: &Summary) -> String {
    let mut parts = Vec::new();
    if s.running > 0 || s.queued > 0 {
        parts.push(trf("{r} running, {q} queued", &[("r", &s.running.to_string()), ("q", &s.queued.to_string())]));
    }
    if s.speed > 0 {
        parts.push(format!("{}/s", format_size(s.speed as i64)));
    }
    if let Some(left) = s.seconds_left() {
        parts.push(duration_label(left));
    }
    if s.held > 0 {
        parts.push(trf("{n} paused", &[("n", &s.held.to_string())]));
    }
    if s.failed > 0 {
        parts.push(trf("{n} failed", &[("n", &s.failed.to_string())]));
    }
    if parts.is_empty() {
        parts.push(if s.done > 0 {
            trn("{n} transfer finished", "{n} transfers finished", &[("n", &s.done.to_string())])
        } else {
            tr("Queue is empty")
        });
    }
    if s.paused {
        parts.insert(0, tr("Paused"));
    }
    parts.join(" · ")
}

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "upload" => "transfer-upload-symbolic",
        "download" => "transfer-download-symbolic",
        "copy" => "edit-copy-symbolic",
        "move" => "edit-cut-symbolic",
        "sync" => "emblem-synchronizing-symbolic",
        "backup" => "document-save-symbolic",
        "delete" => "user-trash-symbolic",
        _ => "folder-download-symbolic",
    }
}

fn status_text(item: &TransferItem) -> String {
    match item.state().as_str() {
        QUEUED if !item.error().is_empty() => item.error(),
        QUEUED => {
            if item.total() > 0 {
                format!("{} · {}", tr("Queued"), format_size(item.total() as i64))
            } else {
                tr("Queued")
            }
        }
        RUNNING => {
            let mut text = if item.total() > 0 {
                format!("{} / {}", format_size(item.done() as i64), format_size(item.total() as i64))
            } else {
                format_size(item.done() as i64)
            };
            if item.speed() > 0 {
                text.push_str(&format!(" · {}/s", format_size(item.speed() as i64)));
                if item.total() > item.done() {
                    text.push_str(" · ");
                    text.push_str(&duration_label((item.total() - item.done()) / item.speed()));
                }
            }
            text
        }
        DONE => format_size(item.total().max(item.done()) as i64),
        CANCELED => tr("Cancelled"),
        PAUSED => {
            if item.total() > 0 {
                format!("{} · {} / {}", tr("Paused"), format_size(item.done() as i64), format_size(item.total() as i64))
            } else {
                tr("Paused")
            }
        }
        _ => item.error(),
    }
}

fn speed_graph(queue: &Queue) -> gtk::Widget {
    const SAMPLES: usize = 120;
    let history: Rc<std::cell::RefCell<std::collections::VecDeque<u64>>> =
        Rc::new(std::cell::RefCell::new(std::iter::repeat_n(0, SAMPLES).collect()));
    let area = gtk::DrawingArea::builder().content_height(64).hexpand(true).build();
    let current = gtk::Label::builder().xalign(0.0).css_classes(["numeric", "dim-label", "caption"]).build();
    area.set_draw_func(glib::clone!(
        #[strong]
        history,
        move |area, cr, width, height| {
            let samples = history.borrow();
            let peak = samples.iter().copied().max().unwrap_or(0).max(64 * 1024) as f64;
            let (w, h) = (width as f64, height as f64);
            let color = area.color();
            let accent = adw::StyleManager::default().accent_color_rgba();
            let step = w / (SAMPLES - 1) as f64;
            let point = |i: usize, v: u64| (i as f64 * step, h - 2.0 - (v as f64 / peak) * (h - 6.0));
            cr.set_line_width(1.0);
            cr.set_source_rgba(color.red() as f64, color.green() as f64, color.blue() as f64, 0.12);
            for fraction in [0.25, 0.5, 0.75] {
                cr.move_to(0.0, (h * fraction).round() + 0.5);
                cr.line_to(w, (h * fraction).round() + 0.5);
            }
            let _ = cr.stroke();
            cr.move_to(0.0, h);
            for (i, v) in samples.iter().enumerate() {
                let (x, y) = point(i, *v);
                cr.line_to(x, y);
            }
            cr.line_to(w, h);
            cr.close_path();
            cr.set_source_rgba(accent.red() as f64, accent.green() as f64, accent.blue() as f64, 0.25);
            let _ = cr.fill();
            for (i, v) in samples.iter().enumerate() {
                let (x, y) = point(i, *v);
                if i == 0 {
                    cr.move_to(x, y);
                } else {
                    cr.line_to(x, y);
                }
            }
            cr.set_source_rgba(accent.red() as f64, accent.green() as f64, accent.blue() as f64, 1.0);
            cr.set_line_width(2.0);
            let _ = cr.stroke();
        }
    ));
    let frame = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    let heading = gtk::Box::builder().spacing(6).build();
    heading
        .append(&gtk::Label::builder().label(tr("Speed")).xalign(0.0).hexpand(true).css_classes(["heading"]).build());
    heading.append(&current);
    frame.append(&heading);
    let card = gtk::Frame::builder().child(&area).css_classes(["view"]).build();
    frame.append(&card);
    let timer: Rc<Cell<bool>> = Rc::default();
    let start = {
        let (q, timer, history) = (queue.clone(), timer.clone(), history.clone());
        let (weak_area, weak_label) = (area.downgrade(), current.downgrade());
        move || {
            if timer.replace(true) {
                return;
            }
            let (q, timer, history, weak_area, weak_label) =
                (q.clone(), timer.clone(), history.clone(), weak_area.clone(), weak_label.clone());
            glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
                let (Some(area), Some(current)) = (weak_area.upgrade(), weak_label.upgrade()) else {
                    timer.set(false);
                    return glib::ControlFlow::Break;
                };
                let summary = q.summary();
                let mut samples = history.borrow_mut();
                samples.pop_front();
                samples.push_back(summary.speed);
                let quiet = summary.active() == 0 && samples.iter().all(|v| *v == 0);
                drop(samples);
                if area.is_mapped() {
                    current.set_text(&if summary.speed > 0 {
                        format!("{}/s", format_size(summary.speed as i64))
                    } else {
                        tr("Idle")
                    });
                    area.queue_draw();
                }
                if quiet {
                    timer.set(false);
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
        }
    };
    queue.connect_changed(move |summary| {
        if summary.active() > 0 {
            start()
        }
    });
    current.set_text(&tr("Idle"));
    frame.upcast()
}

pub fn build(queue: &Queue, parent: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_height_request(420);

    let header = gtk::Box::builder().spacing(6).margin_start(18).margin_end(12).margin_top(6).margin_bottom(6).build();
    let titles =
        gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).valign(gtk::Align::Center).build();
    titles.append(&gtk::Label::builder().label(tr("Transfers")).xalign(0.0).css_classes(["title-4"]).build());
    let summary = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label", "numeric", "caption"])
        .build();
    titles.append(&summary);
    header.append(&titles);

    let pause = gtk::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text(tr("Pause the queue: running transfers finish, no new ones start"))
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build();
    let find = gtk::ToggleButton::builder()
        .icon_name("system-search-symbolic")
        .tooltip_text(tr("Find a transfer"))
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build();
    header.append(&find);
    header.append(&pause);

    let options = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .width_request(300)
        .build();
    let limits = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let limit = adw::SpinRow::builder()
        .title(tr("Simultaneous transfers"))
        .adjustment(&gtk::Adjustment::new(4.0, 1.0, 16.0, 1.0, 4.0, 0.0))
        .build();
    let bandwidth_labels: Vec<String> = BANDWIDTHS.iter().map(|k| bandwidth_label(*k)).collect();
    let bandwidth = adw::ComboRow::builder()
        .title(tr("Speed limit"))
        .model(&gtk::StringList::new(&bandwidth_labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .build();
    limits.append(&limit);
    limits.append(&bandwidth);
    options.append(&speed_graph(queue));
    options.append(&limits);
    let options_button = gtk::MenuButton::builder()
        .icon_name("speedometer-symbolic")
        .tooltip_text(tr("Queue Settings"))
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .popover(&gtk::Popover::builder().child(&options).build())
        .build();
    header.append(&options_button);

    let menu = gio::Menu::new();
    let bulk = gio::Menu::new();
    bulk.append(Some(&tr("Pause All")), Some("queue.pause-all"));
    bulk.append(Some(&tr("Resume All")), Some("queue.resume-all"));
    bulk.append(Some(&tr("Retry Failed")), Some("queue.retry-failed"));
    bulk.append(Some(&tr("Cancel Queued")), Some("queue.cancel-queued"));
    bulk.append(Some(&tr("Cancel All")), Some("queue.cancel-all"));
    menu.append_section(None, &bulk);
    let clean = gio::Menu::new();
    clean.append(Some(&tr("Clear Finished")), Some("queue.clear-finished"));
    clean.append(Some(&tr("Clear All Inactive")), Some("queue.clear-inactive"));
    menu.append_section(None, &clean);
    header.append(
        &gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text(tr("Queue Menu"))
            .menu_model(&menu)
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build(),
    );
    root.append(&header);

    let filters = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Center).margin_bottom(8).build();
    let names = [tr("All"), tr("Active"), tr("Failed"), tr("Done")];
    let mut toggles: Vec<gtk::ToggleButton> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let toggle = gtk::ToggleButton::builder().label(name).active(i == 0).build();
        if let Some(first) = toggles.first() {
            toggle.set_group(Some(first));
        }
        filters.append(&toggle);
        toggles.push(toggle);
    }
    root.append(&filters);
    let search = gtk::SearchEntry::builder().placeholder_text(tr("Find a transfer")).build();
    let search_bar = gtk::SearchBar::builder()
        .child(&adw::Clamp::builder().maximum_size(400).child(&search).build())
        .show_close_button(false)
        .build();
    search_bar.connect_entry(&search);
    root.append(&search_bar);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let mode = Rc::new(Cell::new(0usize));
    let filter = gtk::CustomFilter::new(glib::clone!(
        #[strong]
        mode,
        #[weak]
        search,
        #[upgrade_or]
        true,
        move |object| {
            let item = object.downcast_ref::<TransferItem>().unwrap();
            let needle = search.text().to_lowercase();
            if !needle.is_empty()
                && !item.name().to_lowercase().contains(&needle)
                && !item.detail().to_lowercase().contains(&needle)
            {
                return false;
            }
            match mode.get() {
                1 => item.is_active(),
                2 => item.is_failed(),
                3 => item.state() == DONE,
                _ => true,
            }
        }
    ));
    let sorter = gtk::CustomSorter::new(|a, b| {
        let (a, b) = (a.downcast_ref::<TransferItem>().unwrap(), b.downcast_ref::<TransferItem>().unwrap());
        let rank = |t: &TransferItem| match t.state().as_str() {
            RUNNING => 0,
            QUEUED | PAUSED => 1,
            FAILED => 2,
            _ => 3,
        };
        let order = rank(a).cmp(&rank(b)).then_with(|| match rank(a) {
            1 => a.seq().total_cmp(&b.seq()),
            0 => a.id().cmp(&b.id()),
            _ => b.finished().cmp(&a.finished()),
        });
        order.into()
    });
    let filtered = gtk::FilterListModel::new(Some(queue.store()), Some(filter.clone()));
    let sorted = gtk::SortListModel::new(Some(filtered), Some(sorter.clone()));
    search.connect_search_changed(glib::clone!(
        #[weak]
        filter,
        move |_| filter.changed(gtk::FilterChange::Different)
    ));
    find.bind_property("active", &search_bar, "search-mode-enabled").bidirectional().sync_create().build();
    for (i, toggle) in toggles.iter().enumerate() {
        toggle.connect_toggled(glib::clone!(
            #[strong]
            mode,
            #[weak]
            filter,
            move |t| {
                if t.is_active() {
                    mode.set(i);
                    filter.changed(gtk::FilterChange::Different);
                }
            }
        ));
    }

    let pending = Rc::new(Cell::new(false));
    let resort = Rc::new(glib::clone!(
        #[weak]
        sorter,
        #[weak]
        filter,
        #[strong]
        pending,
        move || {
            if pending.replace(true) {
                return;
            }
            glib::idle_add_local_once(glib::clone!(
                #[weak]
                sorter,
                #[weak]
                filter,
                #[strong]
                pending,
                move || {
                    pending.set(false);
                    sorter.changed(gtk::SorterChange::Different);
                    filter.changed(gtk::FilterChange::Different);
                }
            ));
        }
    ));
    queue.store().connect_items_changed(glib::clone!(
        #[strong]
        resort,
        move |store, position, _, added| {
            for i in position..position + added {
                if let Some(item) = store.item(i).and_downcast::<TransferItem>() {
                    let r = resort.clone();
                    item.connect_notify_local(Some("state"), move |_, _| r());
                    let r = resort.clone();
                    item.connect_notify_local(Some("seq"), move |_, _| r());
                }
            }
        }
    ));

    let list = gtk::ListView::builder()
        .model(&gtk::NoSelection::new(Some(sorted)))
        .factory(&row_factory(queue))
        .css_classes(["navigation-sidebar"])
        .build();
    let empty = adw::StatusPage::builder()
        .icon_name("folder-download-symbolic")
        .title(tr("No Transfers"))
        .description(tr("Uploads, downloads and copies appear here."))
        .css_classes(["compact"])
        .build();
    let stack = gtk::Stack::builder().vexpand(true).transition_type(gtk::StackTransitionType::Crossfade).build();
    stack.add_named(
        &gtk::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk::PolicyType::Never).build(),
        Some("list"),
    );
    stack.add_named(&empty, Some("empty"));
    root.append(&stack);
    let model = list.model().unwrap();
    let update_empty = glib::clone!(
        #[weak]
        stack,
        move |m: &gtk::SelectionModel| stack.set_visible_child_name(if m.n_items() == 0 { "empty" } else { "list" })
    );
    update_empty(&model);
    model.connect_items_changed(move |m, _, _, _| update_empty(m));

    let actions = gio::SimpleActionGroup::new();
    let q = queue.clone();
    let entry = |name: &str, f: Box<dyn Fn(&Queue)>| {
        let action = gio::SimpleAction::new(name, None);
        let q = q.clone();
        action.connect_activate(move |_, _| f(&q));
        actions.add_action(&action);
    };
    entry("pause-all", Box::new(|q| q.pause_all()));
    entry("resume-all", Box::new(|q| q.resume_all()));
    entry(
        "retry-failed",
        Box::new(|q| {
            q.retry_failed();
        }),
    );
    entry(
        "cancel-queued",
        Box::new(|q| {
            q.cancel_queued();
        }),
    );
    entry("cancel-all", Box::new(|q| q.cancel_all()));
    entry("clear-finished", Box::new(|q| q.clear_finished()));
    entry("clear-inactive", Box::new(|q| q.clear_inactive()));
    parent.as_ref().insert_action_group("queue", Some(&actions));

    let settings = settings();
    settings.bind("transfer-limit", &limit, "value").build();
    limit.connect_value_notify(glib::clone!(
        #[strong]
        q,
        move |row| q.set_limit(row.value() as usize)
    ));
    let saved = settings.int("bandwidth-kbps").max(0) as u64;
    bandwidth.set_selected(BANDWIDTHS.iter().position(|k| *k == saved).unwrap_or(0) as u32);
    q.set_bandwidth(saved * 1024);
    bandwidth.connect_selected_notify(glib::clone!(
        #[strong]
        q,
        move |row| {
            let kbps = BANDWIDTHS[row.selected() as usize];
            let _ = settings.set_int("bandwidth-kbps", kbps as i32);
            q.set_bandwidth(kbps * 1024);
        }
    ));
    pause.connect_toggled(glib::clone!(
        #[strong]
        q,
        move |toggle| {
            q.set_paused(toggle.is_active());
            toggle.set_icon_name(if toggle.is_active() {
                "media-playback-start-symbolic"
            } else {
                "media-playback-pause-symbolic"
            });
            toggle.set_tooltip_text(Some(&if toggle.is_active() {
                tr("Resume the queue")
            } else {
                tr("Pause the queue: running transfers finish, no new ones start")
            }));
        }
    ));
    let update = glib::clone!(
        #[weak]
        summary,
        #[weak]
        actions,
        move |s: &Summary| {
            summary.set_text(&summary_line(s));
            let enable = |name: &str, on: bool| {
                if let Some(a) = actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                    a.set_enabled(on);
                }
            };
            enable("pause-all", s.active() > 0);
            enable("resume-all", s.held > 0);
            enable("retry-failed", s.failed > 0);
            enable("cancel-queued", s.queued > 0);
            enable("cancel-all", s.active() > 0);
            enable("clear-finished", s.done > 0);
            enable("clear-inactive", s.done + s.failed > 0);
        }
    );
    update(&queue.summary());
    queue.connect_changed(update);
    root.upcast()
}

fn row_factory(queue: &Queue) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(glib::clone!(#[strong] queue, move |_, object| {
        let list_item = object.downcast_ref::<gtk::ListItem>().unwrap();
        list_item.set_activatable(false);
        let row = gtk::Box::builder().spacing(12).margin_start(6).margin_end(6).margin_top(6).margin_bottom(6).build();
        let icon = gtk::Image::builder().pixel_size(16).valign(gtk::Align::Center).build();
        let texts = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).valign(gtk::Align::Center).build();
        let title = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
        let detail = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).css_classes(["caption", "dim-label"]).build();
        let bar = gtk::ProgressBar::builder().build();
        let status = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["caption", "numeric"]).build();
        texts.append(&title);
        texts.append(&detail);
        texts.append(&bar);
        texts.append(&status);
        let buttons = gtk::Box::builder().spacing(2).valign(gtk::Align::Center).build();
        let button = |icon: &str, tip: String| gtk::Button::builder().icon_name(icon).tooltip_text(tip).css_classes(["flat", "circular"]).build();
        let front = button("go-top-symbolic", tr("Move to Front"));
        let pause = button("media-playback-pause-symbolic", tr("Pause"));
        let retry = button("view-refresh-symbolic", tr("Retry"));
        let cancel = button("process-stop-symbolic", tr("Cancel"));
        let menu = gtk::MenuButton::builder().icon_name("view-more-symbolic").tooltip_text(tr("More")).css_classes(["flat", "circular"]).build();
        for b in [front.upcast_ref::<gtk::Widget>(), pause.upcast_ref(), retry.upcast_ref(), cancel.upcast_ref(), menu.upcast_ref()] { buttons.append(b); }
        row.append(&icon);
        row.append(&texts);
        row.append(&buttons);
        crate::widgets::code_view::label_icon_buttons(&row);
        list_item.set_child(Some(&row));

        let current = |li: &glib::WeakRef<gtk::ListItem>| li.upgrade().and_then(|l| l.item()).and_downcast::<TransferItem>();
        let weak = list_item.downgrade();
        let q = queue.clone();
        front.connect_clicked(glib::clone!(#[strong] weak, #[strong] q, move |_| if let Some(t) = current(&weak) { q.move_to_front(&t) }));
        pause.connect_clicked(glib::clone!(#[strong] weak, #[strong] q, move |_| if let Some(t) = current(&weak) {
            if t.state() == PAUSED { q.resume(&t) } else { q.pause(&t) }
        }));
        retry.connect_clicked(glib::clone!(#[strong] weak, #[strong] q, move |_| if let Some(t) = current(&weak) { q.retry(&t) }));
        cancel.connect_clicked(glib::clone!(#[strong] weak, #[strong] q, move |_| if let Some(t) = current(&weak) { q.cancel(&t) }));
        let group = gio::SimpleActionGroup::new();
        let add = |name: &str, f: Box<dyn Fn(&Queue, &TransferItem)>| {
            let action = gio::SimpleAction::new(name, None);
            let (weak, q) = (weak.clone(), q.clone());
            action.connect_activate(move |_, _| if let Some(t) = current(&weak) { f(&q, &t) });
            group.add_action(&action);
        };
        add("front", Box::new(|q, t| q.move_to_front(t)));
        add("up", Box::new(|q, t| q.shift(t, -1)));
        add("down", Box::new(|q, t| q.shift(t, 1)));
        add("end", Box::new(|q, t| q.move_to_end(t)));
        add("retry", Box::new(|q, t| q.retry(t)));
        add("pause", Box::new(|q, t| q.pause(t)));
        add("resume", Box::new(|q, t| q.resume(t)));
        add("cancel", Box::new(|q, t| q.cancel(t)));
        add("remove", Box::new(|q, t| q.remove(t)));
        let row_ref = row.downgrade();
        add("copy-error", Box::new(move |_, t| if let Some(r) = row_ref.upgrade() { r.clipboard().set_text(&t.error()) }));
        let row_ref = row.downgrade();
        add("reveal", Box::new(move |q, t| {
            let Some(win) = row_ref.upgrade().and_then(|r| r.root()).and_downcast::<crate::window::Window>() else { return };
            let target = match q.spec_of(t) {
                Some(spec) => {
                    let text = |name: &str| spec.get(name).and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    Some((text("profile"), text("bucket"), text("key")))
                }
                None => win.current_client().and_then(|c| t.detail().split_once('/').map(|(bucket, folder)| (c.profile.id.clone(), bucket.to_string(), format!("{folder}{}", t.name())))),
            };
            if let Some((profile, bucket, key)) = target {
                win.close_transfers();
                win.open_object(profile, bucket, key);
            }
        }));
        let row_ref = row.downgrade();
        add("open-file", Box::new(move |_, t| {
            let path = std::path::PathBuf::from(t.local());
            let head: Vec<u8> = std::fs::File::open(&path).ok().map(|f| { use std::io::Read; let mut b = Vec::new(); let _ = f.take(4096).read_to_end(&mut b); b }).unwrap_or_default();
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if !crate::transfers::external::safe_to_launch(&name, Some(&head)) {
                if let Some(win) = row_ref.upgrade().and_then(|r| r.root()).and_downcast::<crate::window::Window>() {
                    win.toast(&tr("This file type is not opened with an application for safety; open it from GNOME Files if you trust it"));
                }
                return;
            }
            let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(t.local())));
            let root = row_ref.upgrade().and_then(|r| r.root()).and_downcast::<gtk::Window>();
            launcher.launch(root.as_ref(), gio::Cancellable::NONE, |_| {});
        }));
        let row_ref = row.downgrade();
        add("open-folder", Box::new(move |_, t| {
            let path = std::path::PathBuf::from(t.local());
            let target = if path.is_dir() { path } else { path.parent().map(|p| p.to_path_buf()).unwrap_or(path) };
            let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(target)));
            let root = row_ref.upgrade().and_then(|r| r.root()).and_downcast::<gtk::Window>();
            launcher.launch(root.as_ref(), gio::Cancellable::NONE, |_| {});
        }));
        row.insert_action_group("row", Some(&group));
        let double = gtk::GestureClick::builder().button(gdk::BUTTON_PRIMARY).build();
        double.connect_pressed(glib::clone!(#[strong] weak, #[weak] row, move |_, presses, _, _| {
            if presses != 2 { return; }
            let Some(item) = current(&weak) else { return };
            if item.state() != DONE { return; }
            let action = if item.kind() == "download" && std::path::Path::new(&item.local()).is_file() { "row.open-file" } else if item.kind() == "upload" { "row.reveal" } else { return };
            let _ = row.activate_action(action, None);
        }));
        row.add_controller(double);
        menu.set_menu_model(Some(&row_menu(QUEUED, false, false, false, false)));

        let drag = gtk::DragSource::builder().actions(gdk::DragAction::MOVE).build();
        drag.connect_prepare(glib::clone!(#[strong] weak, move |_, _, _| {
            let item = current(&weak)?;
            (item.state() == QUEUED).then(|| gdk::ContentProvider::for_value(&item.id().to_value()))
        }));
        row.add_controller(drag);
        let drop = gtk::DropTarget::new(u64::static_type(), gdk::DragAction::MOVE);
        let q2 = q.clone();
        drop.connect_drop(glib::clone!(#[strong] weak, move |_, value, _, _| {
            let (Some(target), Ok(id)) = (current(&weak), value.get::<u64>()) else { return false };
            let store = q2.store();
            let Some(moved) = (0..store.n_items()).filter_map(|i| store.item(i).and_downcast::<TransferItem>()).find(|t| t.id() == id) else { return false };
            q2.place_before(&moved, &target);
            true
        }));
        row.add_controller(drop);
    }));
    factory.connect_bind(|_, object| {
        let list_item = object.downcast_ref::<gtk::ListItem>().unwrap();
        let item = list_item.item().and_downcast::<TransferItem>().unwrap();
        let row = list_item.child().and_downcast::<gtk::Box>().unwrap();
        let refresh = glib::clone!(
            #[weak]
            row,
            move |item: &TransferItem| apply(&row, item)
        );
        refresh(&item);
        let handlers: Vec<glib::SignalHandlerId> = ["state", "done", "speed", "total", "error"]
            .iter()
            .map(|p| {
                item.connect_notify_local(
                    Some(p),
                    glib::clone!(
                        #[strong]
                        refresh,
                        move |i, _| refresh(i)
                    ),
                )
            })
            .collect();
        unsafe {
            list_item.set_data("handlers", (item.clone(), handlers));
        }
    });
    factory.connect_unbind(|_, object| {
        let list_item = object.downcast_ref::<gtk::ListItem>().unwrap();
        if let Some((item, handlers)) =
            unsafe { list_item.steal_data::<(TransferItem, Vec<glib::SignalHandlerId>)>("handlers") }
        {
            for h in handlers {
                item.disconnect(h);
            }
        }
    });
    factory
}

fn row_menu(state: &str, has_error: bool, has_local: bool, is_file: bool, reveal: bool) -> gio::Menu {
    let menu = gio::Menu::new();
    let order = gio::Menu::new();
    if matches!(state, QUEUED | PAUSED) {
        order.append(Some(&tr("Move to Front")), Some("row.front"));
        order.append(Some(&tr("Move Up")), Some("row.up"));
        order.append(Some(&tr("Move Down")), Some("row.down"));
        order.append(Some(&tr("Move to End")), Some("row.end"));
        menu.append_section(None, &order);
    }
    let other = gio::Menu::new();
    if matches!(state, FAILED | CANCELED) {
        other.append(Some(&tr("Retry")), Some("row.retry"));
    }
    if matches!(state, QUEUED | RUNNING) {
        other.append(Some(&tr("Pause")), Some("row.pause"));
    }
    if state == PAUSED {
        other.append(Some(&tr("Resume")), Some("row.resume"));
    }
    if matches!(state, QUEUED | RUNNING | PAUSED) {
        other.append(Some(&tr("Cancel")), Some("row.cancel"));
    }
    if has_error {
        other.append(Some(&tr("Copy Error")), Some("row.copy-error"));
    }
    if state == DONE && reveal {
        other.append(Some(&tr("Show in Bucket")), Some("row.reveal"));
    }
    if has_local && state == DONE {
        if is_file {
            other.append(Some(&tr("Open")), Some("row.open-file"));
        }
        other.append(Some(&tr("Open Containing Folder")), Some("row.open-folder"));
    }
    other.append(Some(&tr("Remove from List")), Some("row.remove"));
    menu.append_section(None, &other);
    menu
}

fn apply(row: &gtk::Box, item: &TransferItem) {
    let icon = row.first_child().and_downcast::<gtk::Image>().unwrap();
    let texts = icon.next_sibling().and_downcast::<gtk::Box>().unwrap();
    let buttons = texts.next_sibling().and_downcast::<gtk::Box>().unwrap();
    let title = texts.first_child().and_downcast::<gtk::Label>().unwrap();
    let detail = title.next_sibling().and_downcast::<gtk::Label>().unwrap();
    let bar = detail.next_sibling().and_downcast::<gtk::ProgressBar>().unwrap();
    let status = bar.next_sibling().and_downcast::<gtk::Label>().unwrap();
    let front = buttons.first_child().unwrap();
    let pause = front.next_sibling().and_downcast::<gtk::Button>().unwrap();
    let retry = pause.next_sibling().unwrap();
    let cancel = retry.next_sibling().unwrap();
    let menu = cancel.next_sibling().and_downcast::<gtk::MenuButton>().unwrap();

    let state = item.state();
    icon.set_icon_name(Some(match state.as_str() {
        DONE => "emblem-ok-symbolic",
        FAILED => "dialog-error-symbolic",
        CANCELED => "process-stop-symbolic",
        PAUSED => "media-playback-pause-symbolic",
        _ => kind_icon(&item.kind()),
    }));
    icon.set_css_classes(match state.as_str() {
        DONE => &["success"],
        FAILED => &["error"],
        CANCELED | PAUSED => &["dim-label"],
        _ => &[],
    });
    title.set_text(&item.name());
    detail.set_text(&home_relative(&item.detail()));
    detail.set_visible(!item.detail().is_empty());
    bar.set_visible(matches!(state.as_str(), RUNNING | QUEUED | PAUSED));
    if item.total() > 0 {
        bar.set_fraction((item.done() as f64 / item.total() as f64).min(1.0));
    } else if state == RUNNING {
        bar.pulse();
    } else {
        bar.set_fraction(0.0);
    }
    bar.set_opacity(if state == QUEUED || state == PAUSED { 0.4 } else { 1.0 });
    status.set_text(&status_text(item));
    status.set_tooltip_text(if state == FAILED { Some(item.error()) } else { None }.as_deref());
    status.set_css_classes(if state == FAILED { &["caption", "error"] } else { &["caption", "numeric", "dim-label"] });
    front.set_visible(state == QUEUED);
    pause.set_visible(matches!(state.as_str(), RUNNING | QUEUED | PAUSED));
    let (icon, tip) = if state == PAUSED {
        ("media-playback-start-symbolic", tr("Resume"))
    } else {
        ("media-playback-pause-symbolic", tr("Pause"))
    };
    pause.set_icon_name(icon);
    pause.set_tooltip_text(Some(&tip));
    pause.update_property(&[gtk::accessible::Property::Label(&tip)]);
    retry.set_visible(matches!(state.as_str(), FAILED | CANCELED));
    cancel.set_visible(matches!(state.as_str(), QUEUED | RUNNING | PAUSED));
    let is_file = state == DONE && item.kind() == "download" && std::path::Path::new(&item.local()).is_file();
    let flags = (!item.error().is_empty(), !item.local().is_empty(), is_file, item.kind() == "upload");
    let signature = format!("{state}:{flags:?}");
    if menu.widget_name() != signature.as_str() {
        menu.set_widget_name(&signature);
        menu.set_menu_model(Some(&row_menu(&state, flags.0, flags.1, flags.2, flags.3)));
    }
}

fn home_relative(text: &str) -> String {
    let home = glib::home_dir();
    let home = home.to_string_lossy();
    match text.strip_prefix(home.as_ref()) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => text.to_string(),
    }
}
