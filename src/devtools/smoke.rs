//! A scripted run through the interface against a real connection, for checking
//! the application end to end without anyone at the keyboard. Started with
//! FERRY_SMOKE=<profile name>; it works below "ferry-smoke/" only,
//! removes that folder and quits, printing one line per step.
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, gio, glib};
use std::time::{Duration, Instant};

use crate::window::{Window, entry_of};

fn log(ok: bool, step: &str, detail: &str) {
    println!("{}  {step:<36} {detail}", if ok { "OK  " } else { "FAIL" });
}

/// Iterates the main loop until the condition holds or the timeout passes.
async fn until(seconds: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !condition() {
        if start.elapsed() > Duration::from_secs(seconds) { return false; }
        glib::timeout_future(Duration::from_millis(100)).await;
    }
    true
}

/// Saves a picture of the window when FERRY_SHOTS names a folder, rendered by GTK itself.
async fn shot(win: &Window, name: &str) {
    let Ok(dir) = std::env::var("FERRY_SHOTS") else { return };
    // Animations finish before the picture is taken.
    glib::timeout_future(Duration::from_millis(700)).await;
    let (width, height) = (win.width(), win.height());
    let paintable = gtk::WidgetPaintable::new(Some(win));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width as f64, height as f64);
    let (Some(node), Some(renderer)) = (snapshot.to_node(), win.native().and_then(|n| n.renderer())) else { return };
    let texture = renderer.render_texture(&node, None);
    let _ = std::fs::create_dir_all(&dir);
    let _ = texture.save_to_png(std::path::Path::new(&dir).join(format!("{name}.png")));
}

/// A picture of one widget, for popovers that live on their own surface.
async fn shot_widget(widget: &gtk::Widget, name: &str) {
    let Ok(dir) = std::env::var("FERRY_SHOTS") else { return };
    glib::timeout_future(Duration::from_millis(300)).await;
    let (width, height) = (widget.width(), widget.height());
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width as f64, height as f64);
    let (Some(node), Some(renderer)) = (snapshot.to_node(), widget.native().and_then(|n| n.renderer())) else { return };
    let texture = renderer.render_texture(&node, None);
    let _ = texture.save_to_png(std::path::Path::new(&dir).join(format!("{name}.png")));
}

/// Waits for a dialog, takes its picture and closes it; true when it opened.
async fn dialog_shot(win: &Window, name: &str, wait_ms: u64) -> bool {
    let opened = until(5, || win.visible_dialog().is_some()).await;
    glib::timeout_future(Duration::from_millis(wait_ms)).await;
    shot(win, name).await;
    if let Some(d) = win.visible_dialog() { d.force_close(); }
    glib::timeout_future(Duration::from_millis(300)).await;
    opened
}

fn find_by_tooltip(root: &gtk::Widget, tip: &str) -> Option<gtk::Widget> {
    if root.tooltip_text().as_deref() == Some(tip) { return Some(root.clone()); }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(found) = find_by_tooltip(&c, tip) { return Some(found); }
        child = c.next_sibling();
    }
    None
}

fn find_label(root: &gtk::Widget, text: &str) -> Option<gtk::Widget> {
    if root.downcast_ref::<gtk::Label>().is_some_and(|l| l.text() == text) && root.is_mapped() { return Some(root.clone()); }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(found) = find_label(&c, text) { return Some(found); }
        child = c.next_sibling();
    }
    None
}

fn find_label_matching(root: &gtk::Widget, test: impl Fn(&str) -> bool + Copy) -> Option<gtk::Widget> {
    if let Some(label) = root.downcast_ref::<gtk::Label>() && label.is_mapped() && test(&label.text()) { return Some(root.clone()); }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(found) = find_label_matching(&c, test) { return Some(found); }
        child = c.next_sibling();
    }
    None
}

fn find_by_title<T: IsA<gtk::Widget> + IsA<adw::PreferencesRow>>(root: &gtk::Widget, title: &str) -> Option<T> {
    if let Some(row) = root.downcast_ref::<T>() && row.title() == title { return Some(row.clone()); }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(found) = find_by_title::<T>(&c, title) { return Some(found); }
        child = c.next_sibling();
    }
    None
}

fn names(win: &Window) -> Vec<String> {
    let imp = win.imp();
    let Some(selection) = imp.selection.borrow().clone() else { return Vec::new() };
    (0..selection.n_items()).filter_map(|i| selection.item(i)).map(|o| entry_of(&o).name).collect()
}

pub fn start(win: &Window, profile_name: String) {
    let win = win.clone();
    glib::spawn_future_local(async move {
        let mut failures = 0;
        let mut step = |ok: bool, name: &str, detail: String| { if !ok { failures += 1; } log(ok, name, &detail); ok };
        shot(&win, "00-welcome").await;
        let Some(profile) = crate::profile::load().into_iter().find(|p| p.name == profile_name) else {
            step(false, "profile", profile_name.clone());
            std::process::exit(2);
        };
        win.connect(profile);
        let connected = until(30, || win.current_client().is_some() && !win.open_bucket_name().is_empty()).await;
        if !step(connected, "connect and open first bucket", win.open_bucket_name()) { std::process::exit(1); }
        shot(&win, "01-connected").await;
        // In a phone-sized run, the whole window fits its narrowest allowed width.
        let narrow_run = std::env::var_os("FERRY_SMOKE_SIZE").is_some();
        if narrow_run {
            let (min, _, _, _) = win.imp().toasts.measure(gtk::Orientation::Horizontal, -1);
            step(min <= win.width_request(), "fits a phone-sized window", format!("{min} px needed, {} px allowed", win.width_request()));
        }
        let bucket = win.open_bucket_name();
        let client = win.current_client().unwrap();
        let base = "ferry-smoke/";
        let _ = crate::runtime::bg({ let (c, b) = (client.clone(), bucket.clone()); async move { c.delete_keys(&b, vec![base.to_string()]).await } }).await;

        // Upload a folder through the queue.
        let dir = std::env::temp_dir().join(format!("ferry-smoke-{}", std::process::id())).join("ferry-smoke");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("readme.txt"), b"smoke test\n").unwrap();
        std::fs::write(dir.join("inner/photo.png"), include_bytes!("../../data/icons/hicolor/scalable/apps/io.github.mehmetnuri.Ferry.svg")).unwrap();
        win.navigate("");
        until(10, || !win.imp().loading.get()).await;
        win.upload(vec![dir.clone()]);
        let queued = until(20, || win.queue().summary().active() > 0 || win.queue().summary().done > 0).await;
        let uploaded = until(60, || { let s = win.queue().summary(); s.active() == 0 && s.done >= 2 }).await;
        let summary = win.queue().summary();
        step(queued && uploaded && summary.failed == 0, "upload folder through the queue", format!("{summary:?}"));
        win.imp().transfers_sheet.set_open(true);
        shot(&win, "02-transfers").await;
        win.imp().transfers_sheet.set_open(false);

        // When everything has finished, the summary bar leaves after a few seconds.
        let left = until(10, || win.imp().transfers_sheet.bottom_bar().is_none()).await;
        step(left, "finished transfers bar goes away", String::new());

        // "Show in Bucket" on a finished upload opens its object.
        win.imp().transfers_sheet.set_open(true);
        glib::timeout_future(Duration::from_millis(500)).await;
        if let Some(label) = find_label(win.imp().transfers_bin.upcast_ref(), "readme.txt") {
            let _ = label.activate_action("row.reveal", None);
            let shown = until(15, || *win.imp().details_key.borrow() == format!("{base}readme.txt")).await;
            step(shown && !win.imp().transfers_sheet.is_open(), "show a finished upload in its bucket", win.imp().details_key.borrow().clone());
        }
        win.imp().transfers_sheet.set_open(false);
        win.navigate("");
        until(10, || !win.imp().loading.get()).await;

        // Uploading the same folder again asks what to do with the existing objects.
        win.upload(vec![dir.clone()]);
        step(dialog_shot(&win, "02b-conflict", 500).await, "conflict dialog for existing objects", String::new());

        // A speed limit holds while a larger file uploads.
        let big = dir.parent().unwrap().join("throttled.bin");
        std::fs::write(&big, vec![9u8; 2 * 1024 * 1024]).unwrap();
        win.queue().set_bandwidth(256 * 1024);
        let started = std::time::Instant::now();
        win.upload(vec![big.clone()]);
        until(10, || win.queue().summary().running > 0).await;
        glib::timeout_future(Duration::from_millis(2500)).await;
        win.imp().transfers_sheet.set_open(true);
        shot(&win, "02c-transfer-running").await;
        // Pausing stops the running transfer in its place; resuming starts it again.
        win.queue().pause_all();
        let paused = until(10, || win.queue().summary().held == 1 && win.queue().summary().running == 0).await;
        shot(&win, "02d-transfer-paused").await;
        win.queue().resume_all();
        step(paused && until(10, || win.queue().summary().running == 1).await, "pause and resume a transfer", String::new());
        // The queue settings show the speed of the last minute.
        if let Some(button) = find_by_tooltip(win.imp().transfers_bin.upcast_ref(), &crate::i18n::tr("Queue Settings")).and_downcast::<gtk::MenuButton>() {
            button.popup();
            glib::timeout_future(Duration::from_millis(2500)).await;
            if let Some(child) = button.popover().and_then(|p| p.child()) { shot_widget(&child, "02e-speed-graph").await; }
            button.popdown();
        }
        // The transfer search opens below the filters.
        if let Some(find) = find_by_tooltip(win.imp().transfers_bin.upcast_ref(), &crate::i18n::tr("Find a transfer")).and_downcast::<gtk::ToggleButton>() {
            find.set_active(true);
            shot(&win, "02f-transfer-search").await;
            find.set_active(false);
        }
        win.imp().transfers_sheet.set_open(false);
        let finished = until(60, || win.queue().summary().active() == 0).await;
        let seconds = started.elapsed().as_secs_f64();
        win.queue().set_bandwidth(0);
        // 2 MiB at 256 KiB/s takes about 7 s after the first second's burst.
        step(finished && seconds > 5.0, "speed limit holds", format!("{seconds:.1} s for 2 MiB at 256 KiB/s"));
        let (c, b) = (client.clone(), bucket.clone());
        let _ = crate::runtime::bg(async move { c.delete_object(&b, "throttled.bin").await }).await;

        // Uploading the same folder again finds the existing objects.
        let (c, b) = (client.clone(), bucket.clone());
        let existing = crate::runtime::bg(async move { c.list_all(&b, "", 200_000).await }).await.map(|(i, _)| i.into_iter().filter(|e| e.key.starts_with(base)).count()).unwrap_or(0);
        step(existing >= 2, "conflict detection sees existing objects", format!("{existing} objects below {base}"));

        // Browse into the folder.
        win.navigate(base);
        let listed = until(20, || names(&win).iter().any(|n| n == "readme.txt")).await;
        step(listed, "navigate and list folder", names(&win).join(", "));
        win.imp().grid_toggle.set_active(true);
        step(win.imp().list_stack.visible_child_name().as_deref() == Some("grid"), "grid view", String::new());
        shot(&win, "03-grid").await;
        win.imp().list_toggle.set_active(true);

        // Selecting a file opens its details.
        let selection = win.imp().selection.borrow().clone().unwrap();
        let index = names(&win).iter().position(|n| n == "readme.txt").unwrap_or(0) as u32;
        selection.select_item(index, true);
        let details = until(15, || win.imp().info.borrow().as_ref().is_some_and(|i| i.key.ends_with("readme.txt"))).await;
        step(details, "single selection shows details", win.imp().details_key.borrow().clone());
        shot(&win, "04-details").await;

        // History.
        win.navigate(&format!("{base}inner/"));
        until(15, || names(&win).iter().any(|n| n == "photo.png")).await;
        win.go_history(true);
        let back = *win.imp().prefix.borrow() == base;
        win.go_history(false);
        let forward = win.imp().prefix.borrow().ends_with("inner/");
        step(back && forward, "back and forward", win.imp().prefix.borrow().clone());

        // A folder visited before shows its content at once, from the cache.
        win.navigate(base);
        step(names(&win).iter().any(|n| n == "readme.txt"), "instant navigation from cache", names(&win).join(", "));

        // Copy and paste next to the original creates a "(copy)" object.
        until(15, || names(&win).iter().any(|n| n == "readme.txt")).await;
        let selection = win.imp().selection.borrow().clone().unwrap();
        let index = names(&win).iter().position(|n| n == "readme.txt").unwrap_or(0) as u32;
        selection.select_item(index, true);
        win.copy_selection(false);
        win.paste();
        // A refresh needs time to answer; asking again too often would cancel every answer.
        let mut pasted = false;
        for _ in 0..15 {
            win.refresh();
            if until(2, || names(&win).iter().any(|n| n.contains("readme ("))).await { pasted = true; break; }
        }
        step(pasted, "copy and paste in place", names(&win).join(", "));

        // Moving into a folder, then undoing the move.
        let copy_name = crate::window::actions::copy_name("readme.txt", 1);
        let copy_key = format!("{base}{copy_name}");
        let entry = crate::s3::Entry { key: copy_key.clone(), name: copy_name.clone(), size: 11, ..Default::default() };
        let clip = crate::window::actions::Clip { client: client.clone(), bucket: bucket.clone(), prefix: base.to_string(), entries: vec![entry], cut: true };
        win.transfer_objects(clip, format!("{base}inner/"));
        let exists = |key: String| { let (c, b) = (client.clone(), bucket.clone()); async move { crate::runtime::bg(async move { c.head_object(&b, &key).await }).await.is_ok() } };
        let moved_key = format!("{base}inner/{copy_name}");
        let mut moved = false;
        for _ in 0..100 { if exists(moved_key.clone()).await && !exists(copy_key.clone()).await { moved = true; break; } glib::timeout_future(Duration::from_millis(200)).await; }
        step(moved, "move into a folder", String::new());
        // Ctrl+Z presses the Undo button of the "moved" toast.
        until(10, || win.imp().undo_toast.borrow().is_some()).await;
        let _ = WidgetExt::activate_action(&win, "win.undo", None);
        let mut restored = false;
        for _ in 0..100 { if exists(copy_key.clone()).await && !exists(moved_key.clone()).await { restored = true; break; } glib::timeout_future(Duration::from_millis(200)).await; }
        step(restored, "undo the move", String::new());
        win.refresh();
        until(10, || names(&win).contains(&copy_name)).await;

        // The type filter hides what does not match.
        win.imp().type_filter.set(2);
        if let Some(filter) = win.imp().filter.borrow().as_ref() { filter.changed(gtk::FilterChange::Different); }
        let filtered = names(&win);
        shot(&win, "07-filter").await;
        step(!filtered.iter().any(|n| n.ends_with(".txt")), "type filter (images)", filtered.join(", "));
        win.imp().type_filter.set(0);
        if let Some(filter) = win.imp().filter.borrow().as_ref() { filter.changed(gtk::FilterChange::Different); }

        // Preview and share dialogs open.
        let selection = win.imp().selection.borrow().clone().unwrap();
        let index = names(&win).iter().position(|n| n == "readme.txt").unwrap_or(0) as u32;
        selection.select_item(index, true);
        win.preview_selection();
        let preview = until(5, || win.visible_dialog().is_some()).await;
        glib::timeout_future(Duration::from_millis(1500)).await;
        shot(&win, "05-preview").await;
        if let Some(d) = win.visible_dialog() { d.force_close(); }
        step(preview, "quick preview opens", String::new());
        win.presign(format!("{base}readme.txt"));
        let share = until(5, || win.visible_dialog().is_some()).await;
        glib::timeout_future(Duration::from_millis(1500)).await;
        shot(&win, "06-share").await;
        if let Some(d) = win.visible_dialog() { d.force_close(); }
        step(share, "share dialog opens", String::new());

        // Dragging out: GNOME Files asks for file URIs on drop and gets downloaded files.
        let provider = crate::transfers::drag_out::DragOut::new(client.clone(), bucket.clone(), base.to_string(), vec![format!("{base}readme.txt"), format!("{base}inner/")], "marker".into());
        let output = gio::MemoryOutputStream::new_resizable();
        let written = provider.write_mime_type_future("text/uri-list", &output, glib::Priority::DEFAULT).await;
        let _ = output.close(gio::Cancellable::NONE);
        let uris = String::from_utf8_lossy(&output.steal_as_bytes()).to_string();
        let paths: Vec<std::path::PathBuf> = uris.lines().filter_map(|u| gio::File::for_uri(u.trim()).path()).collect();
        let ok = written.is_ok() && paths.len() == 2 && paths[0].is_file() && paths[1].join("photo.png").is_file();
        step(ok, "drag out downloads files for GNOME Files", format!("{written:?} {paths:?}"));
        let text_out = gio::MemoryOutputStream::new_resizable();
        let _ = provider.write_mime_type_future("text/plain;charset=utf-8", &text_out, glib::Priority::DEFAULT).await;
        let _ = text_out.close(gio::Cancellable::NONE);
        step(text_out.steal_as_bytes().as_ref() == b"marker", "drag inside the window carries the keys", String::new());

        // Every secondary window opens without problems; pictures are taken when asked.
        let _ = WidgetExt::activate_action(&win, "win.bucket-settings", None);
        step(dialog_shot(&win, "08-bucket-settings", 4000).await, "bucket settings open", String::new());
        let stored = crate::profile::load().into_iter().find(|p| p.id == client.profile.id).unwrap();
        let _ = WidgetExt::activate_action(&win, "win.edit-profile", Some(&stored.id.to_variant()));
        step(dialog_shot(&win, "09-edit-connection", 1500).await, "connection editor opens", String::new());
        let _ = WidgetExt::activate_action(&win, "win.export-profiles", None);
        step(dialog_shot(&win, "09b-export-connections", 1500).await, "export dialog with protection choices", String::new());
        let _ = WidgetExt::activate_action(&win, "win.import-other-apps", None);
        step(dialog_shot(&win, "09c-import-other-apps", 800).await, "import from other apps opens", String::new());
        if let Some(app) = win.application() { app.activate_action("preferences", None); }
        step(dialog_shot(&win, "10-preferences", 500).await, "preferences open", String::new());
        if let Some(app) = win.application() { app.activate_action("about", None); }
        step(dialog_shot(&win, "11-about", 500).await, "about opens", String::new());
        let selection = win.imp().selection.borrow().clone().unwrap();
        selection.unselect_all();
        for (i, n) in names(&win).iter().enumerate() { if n.starts_with("readme") { selection.select_item(i as u32, false); } }
        win.rename_selected();
        step(dialog_shot(&win, "12-batch-rename", 500).await, "batch rename opens", String::new());
        let selection = win.imp().selection.borrow().clone().unwrap();
        if let Some(i) = names(&win).iter().position(|n| n == "readme.txt") { selection.select_item(i as u32, true); }
        let _ = WidgetExt::activate_action(&win, "win.open-with", None);
        step(dialog_shot(&win, "12b-open-with", 500).await, "open with chooser opens", String::new());
        // An s3:// link opens its folder and shows the object.
        win.navigate("");
        win.open_s3_uri(&format!("s3://{bucket}/{base}readme.txt"));
        let shown = until(15, || *win.imp().details_key.borrow() == format!("{base}readme.txt") && *win.imp().prefix.borrow() == base).await;
        step(shown, "s3:// link opens the object", win.imp().details_key.borrow().clone());
        // A code file shows highlighted in the details (with the "sourceview" feature).
        let code_key = format!("{base}settings.json");
        let (c, b, k) = (client.clone(), bucket.clone(), code_key.clone());
        let _ = crate::runtime::bg(async move { c.create_object(&b, &k, b"{\n  \"name\": \"Ferry\",\n  \"parts\": [1, 2, 3],\n  \"public\": false\n}\n".to_vec(), "application/json").await }).await;
        win.open_s3_uri(&format!("s3://{bucket}/{code_key}"));
        until(15, || win.imp().info.borrow().as_ref().is_some_and(|i| i.key == code_key)).await;
        if let Some(index) = names(&win).iter().position(|n| n == "settings.json") {
            win.imp().selection.borrow().clone().unwrap().select_item(index as u32, true);
            win.preview_selection();
            step(dialog_shot(&win, "04c-code-preview", 1500).await, "text preview opens", String::new());
        }
        // Reviewing a mirror sync shows deletions before anything happens.
        let plan = crate::s3::tools::SyncPlan { transfer: vec![("photos/beach.jpg".into(), 2_400_000), ("notes.txt".into(), 1200)], delete: vec!["old/report.pdf".into()], skipped: 14 };
        let reviewer = win.clone();
        glib::spawn_future_local(async move { let _ = crate::dialogs::bucket::review_sync(&reviewer, &plan, false).await; });
        step(dialog_shot(&win, "17-sync-review", 500).await, "sync review lists changes", String::new());
        // Ctrl+plus shows larger grid items; the user's own sizes are put back afterwards.
        let (zoom, grid) = (crate::settings::settings().int("grid-zoom"), win.imp().grid_toggle.is_active());
        let _ = WidgetExt::activate_action(&win, "win.zoom-in", None);
        let _ = WidgetExt::activate_action(&win, "win.zoom-in", None);
        glib::timeout_future(Duration::from_millis(800)).await;
        step(crate::settings::settings().int("grid-zoom") == (zoom + 2).min(3) && win.imp().grid_toggle.is_active(), "grid zoom", String::new());
        shot(&win, "18-grid-zoom").await;
        let _ = crate::settings::settings().set_int("grid-zoom", zoom);
        win.imp().grid_toggle.set_active(grid);
        if !grid { win.imp().list_toggle.set_active(true); }
        // Names starting with a dot are hidden until Ctrl+H; the user's choice is put back.
        let (c, b) = (client.clone(), bucket.clone());
        let _ = crate::runtime::bg(async move { c.create_object(&b, &format!("{base}.hidden-marker"), Vec::new(), "text/plain").await }).await;
        let was_shown = crate::settings::settings().boolean("show-hidden");
        if was_shown { win.change_action_state("show-hidden", &false.to_variant()); }
        win.refresh();
        until(10, || names(&win).iter().any(|n| n == "readme.txt")).await;
        let hidden_first = !names(&win).iter().any(|n| n == ".hidden-marker");
        win.change_action_state("show-hidden", &true.to_variant());
        let shown_after = until(5, || names(&win).iter().any(|n| n == ".hidden-marker")).await;
        win.change_action_state("show-hidden", &was_shown.to_variant());
        step(hidden_first && shown_after, "hidden files toggle", format!("hidden first {hidden_first}, shown after {shown_after}: {:?}", names(&win)));
        // Copy As › AWS CLI Command puts a ready command on the clipboard.
        if let Some(index) = names(&win).iter().position(|n| n == "readme.txt") {
            win.imp().selection.borrow().clone().unwrap().select_item(index as u32, true);
            let _ = WidgetExt::activate_action(&win, "win.copy-cli", None);
            let text = win.clipboard().read_text_future().await.ok().flatten().map(|t| t.to_string()).unwrap_or_default();
            step(text.starts_with(&format!("aws s3 cp 's3://{bucket}/{base}readme.txt' .")) && text.contains("--endpoint-url"), "copy as AWS CLI command", text);
        }
        // A mounted folder shows its files and is bookmarked in GNOME Files until unmounted.
        let bookmarks = || std::fs::read_to_string(glib::user_config_dir().join("gtk-3.0").join("bookmarks")).unwrap_or_default();
        let before = bookmarks();
        match crate::transfers::mount::mount(client.clone(), &bucket, base, true) {
            Ok((id, path)) => {
                let listed = std::fs::read_dir(&path).map(|d| d.flatten().any(|e| e.file_name() == "readme.txt")).unwrap_or(false);
                let uri = gio::File::for_path(&path).uri().to_string();
                let marked = bookmarks().lines().any(|l| l.starts_with(&format!("{uri} ")));
                let unmounted = crate::transfers::mount::unmount(id).is_ok();
                step(listed && marked && unmounted && bookmarks() == before, "mount lists files and bookmarks them", format!("listed {listed}, marked {marked}, unmounted {unmounted}"));
            }
            Err(error) => { step(false, "mount lists files and bookmarks them", error); }
        }
        // Files dropped on a folder go into that folder, not the open one.
        let dropped = dir.parent().unwrap().join("dropped.txt");
        std::fs::write(&dropped, b"dropped on a folder\n").unwrap();
        win.upload_into(vec![dropped], format!("{base}inner/"));
        let mut landed = false;
        for _ in 0..100 {
            let (c, b) = (client.clone(), bucket.clone());
            if crate::runtime::bg(async move { c.head_object(&b, &format!("{base}inner/dropped.txt")).await }).await.is_ok() { landed = true; break; }
            glib::timeout_future(Duration::from_millis(200)).await;
        }
        step(landed, "upload into a dropped-on folder", String::new());
        // Renaming, then Ctrl+Z, gives the old name back.
        if let Some(index) = names(&win).iter().position(|n| n == "settings.json") {
            let selection = win.imp().selection.borrow().clone().unwrap();
            selection.select_item(index as u32, true);
            win.rename_selected();
            until(5, || win.visible_dialog().is_some()).await;
            if let Some(dialog) = win.visible_dialog().and_downcast::<adw::AlertDialog>() {
                let entry = dialog.extra_child().and_then(|c| if c.is::<gtk::Editable>() { Some(c) } else { c.first_child() });
                if let Some(editable) = entry.and_downcast::<gtk::Editable>() { editable.set_text("renamed.json"); }
                dialog.emit_by_name::<()>("response", &[&"ok"]);
                // A response sent by the script does not close the dialog as a click would.
                dialog.force_close();
            }
            let renamed = until(15, || names(&win).iter().any(|n| n == "renamed.json") && win.imp().undo_toast.borrow().is_some()).await;
            let _ = WidgetExt::activate_action(&win, "win.undo", None);
            let restored = until(15, || names(&win).iter().any(|n| n == "settings.json") && !names(&win).iter().any(|n| n == "renamed.json")).await;
            step(renamed && restored, "rename and undo", format!("renamed {renamed}, restored {restored}"));
        }
        // Copy As › File Contents puts the text of a small file on the clipboard.
        if let Some(index) = names(&win).iter().position(|n| n == "readme.txt") {
            win.imp().selection.borrow().clone().unwrap().select_item(index as u32, true);
            let _ = WidgetExt::activate_action(&win, "win.copy-contents", None);
            glib::timeout_future(Duration::from_millis(1500)).await;
            let text = win.clipboard().read_text_future().await.ok().flatten().map(|t| t.to_string()).unwrap_or_default();
            step(text == "smoke test\n", "copy file contents", format!("{text:?}"));
        }
        // Deleted objects: on a bucket without versioning the dialog says so.
        let _ = WidgetExt::activate_action(&win, "win.deleted-objects", None);
        step(dialog_shot(&win, "20-deleted-objects", 2500).await, "deleted objects dialog", String::new());
        // Copied objects can be pasted in GNOME Files: the clipboard offers them as files.
        if let Some(index) = names(&win).iter().position(|n| n == "readme.txt") {
            win.imp().selection.borrow().clone().unwrap().select_item(index as u32, true);
            win.copy_selection(false);
            let value = win.clipboard().read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT).await;
            let files: Vec<std::path::PathBuf> = value.ok().and_then(|v| v.get::<gdk::FileList>().ok()).map(|l| l.files().iter().filter_map(|f| f.path()).collect()).unwrap_or_default();
            let content = files.first().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
            step(content == "smoke test\n", "copied objects paste as files", format!("{files:?}"));
            win.imp().clipboard.replace(None);
        }
        // Deleting a folder: confirmed, then done in the transfers once the undo time is over.
        let (c, b) = (client.clone(), bucket.clone());
        let _ = crate::runtime::bg(async move { c.create_object(&b, &format!("{base}trash-me/inside.txt"), b"x".to_vec(), "text/plain").await }).await;
        win.refresh();
        until(10, || names(&win).iter().any(|n| n == "trash-me")).await;
        if let Some(index) = names(&win).iter().position(|n| n == "trash-me") {
            win.imp().selection.borrow().clone().unwrap().select_item(index as u32, true);
            let _ = WidgetExt::activate_action(&win, "win.delete-selected", None);
            until(5, || win.visible_dialog().is_some()).await;
            if let Some(dialog) = win.visible_dialog().and_downcast::<adw::AlertDialog>() {
                dialog.emit_by_name::<()>("response", &[&"ok"]);
                dialog.force_close();
            }
            // The undo toast is dismissed early, as if its time ran out.
            until(5, || win.imp().undo_toast.borrow().is_some()).await;
            let toast = win.imp().undo_toast.borrow().clone();
            if let Some(toast) = toast { toast.dismiss(); }
            let mut gone = false;
            for _ in 0..100 {
                let (c, b) = (client.clone(), bucket.clone());
                if crate::runtime::bg(async move { c.head_object(&b, &format!("{base}trash-me/inside.txt")).await }).await.is_err() { gone = true; break; }
                glib::timeout_future(Duration::from_millis(200)).await;
            }
            let queued = win.queue().store().n_items() > 0 && (0..win.queue().store().n_items()).filter_map(|i| win.queue().store().item(i).and_downcast::<crate::transfers::queue::TransferItem>()).any(|t| t.kind() == "delete");
            step(gone && queued, "delete a folder through the transfers", format!("gone {gone}, in transfers {queued}"));
        }
        // Duplicating a connection opens a new one filled in from it (closed here unsaved).
        let _ = WidgetExt::activate_action(&win, "win.duplicate-profile", Some(&client.profile.id.to_variant()));
        step(dialog_shot(&win, "21-duplicate-connection", 1500).await, "duplicate a connection", String::new());
        // A folder whose content is known shows its item count in the Size column.
        win.navigate("");
        until(10, || names(&win).iter().any(|n| n == "ferry-smoke")).await;
        glib::timeout_future(Duration::from_millis(500)).await;
        let count_label = find_label_matching(win.imp().column_view.upcast_ref(), |t| t.ends_with(&crate::i18n::trn("{n} item", "{n} items", &[("n", "2")])[1..]));
        step(count_label.is_some() || narrow_run, "folder item count", count_label.and_downcast::<gtk::Label>().map(|l| l.text().to_string()).unwrap_or_default());
        win.navigate(base);
        until(10, || names(&win).iter().any(|n| n == "readme.txt")).await;
        // Minified JSON previews indented.
        let (c, b) = (client.clone(), bucket.clone());
        let _ = crate::runtime::bg(async move { c.create_object(&b, &format!("{base}min.json"), br#"{"app":"Ferry","features":["queue","resume","preview","sync","share"],"limits":{"parts":10000,"part_size_mib":8},"public":false,"tags":{"desktop":"GNOME","toolkit":"GTK 4"}}"#.to_vec(), "application/json").await }).await;
        win.refresh();
        until(10, || names(&win).iter().any(|n| n == "min.json")).await;
        if let Some(index) = names(&win).iter().position(|n| n == "min.json") {
            win.imp().selection.borrow().clone().unwrap().select_item(index as u32, true);
            win.preview_selection();
            step(dialog_shot(&win, "23-json-pretty", 1500).await, "minified JSON previews indented", String::new());
        }
        // A ZIP archive previews as the list of its files.
        let archive = {
            use std::io::Write;
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default();
            for (name, body) in [("docs/readme.md", "# Hello\n"), ("docs/notes.txt", "notes"), ("photos/beach.jpg", "not really a picture")] {
                let _ = writer.start_file(name, options);
                let _ = writer.write_all(body.as_bytes());
            }
            writer.finish().map(|c| c.into_inner()).unwrap_or_default()
        };
        let (c, b) = (client.clone(), bucket.clone());
        let _ = crate::runtime::bg(async move { c.create_object(&b, &format!("{base}bundle.zip"), archive, "application/zip").await }).await;
        win.refresh();
        until(10, || names(&win).iter().any(|n| n == "bundle.zip")).await;
        if let Some(index) = names(&win).iter().position(|n| n == "bundle.zip") {
            win.imp().selection.borrow().clone().unwrap().select_item(index as u32, true);
            win.preview_selection();
            step(dialog_shot(&win, "24-zip-contents", 1500).await, "ZIP previews its files", String::new());
        }
        // A CSV file previews as a table.
        let (c, b) = (client.clone(), bucket.clone());
        let _ = crate::runtime::bg(async move { c.create_object(&b, &format!("{base}cities.csv"), "city,country,population\nIstanbul,Türkiye,15655924\nAnkara,Türkiye,5803482\n\"Paris, Île-de-France\",France,2102650\nTokyo,Japan,14094034\n".as_bytes().to_vec(), "text/csv").await }).await;
        win.refresh();
        until(10, || names(&win).iter().any(|n| n == "cities.csv")).await;
        if let Some(index) = names(&win).iter().position(|n| n == "cities.csv") {
            win.imp().selection.borrow().clone().unwrap().select_item(index as u32, true);
            win.preview_selection();
            step(dialog_shot(&win, "22-csv-table", 1500).await, "CSV previews as a table", String::new());
        }
        // Tabs: a new tab opens here, each tab keeps its own folder, closing returns to one tab.
        {
            let tab_view = win.imp().tab_view.get();
            let _ = WidgetExt::activate_action(&win, "win.new-tab", None);
            let two = tab_view.n_pages() == 2;
            win.navigate(&format!("{base}inner/"));
            until(10, || names(&win).iter().any(|n| n == "photo.png")).await;
            shot(&win, "25-tabs").await;
            tab_view.set_selected_page(&tab_view.nth_page(0));
            let first_kept = until(10, || *win.imp().prefix.borrow() == base && names(&win).iter().any(|n| n == "readme.txt")).await;
            tab_view.set_selected_page(&tab_view.nth_page(1));
            let second_kept = until(10, || *win.imp().prefix.borrow() == format!("{base}inner/")).await;
            let _ = WidgetExt::activate_action(&win, "win.close-tab", None);
            let back_to_one = until(5, || tab_view.n_pages() == 1 && *win.imp().prefix.borrow() == base).await;
            step(two && first_kept && second_kept && back_to_one, "tabs keep their own folders", format!("two {two}, first {first_kept}, second {second_kept}, closed {back_to_one}"));
            until(10, || names(&win).iter().any(|n| n == "readme.txt")).await;
        }
        // Ctrl+S selects the items matching a pattern.
        let _ = WidgetExt::activate_action(&win, "win.select-matching", None);
        until(5, || win.visible_dialog().is_some()).await;
        if let Some(dialog) = win.visible_dialog().and_downcast::<adw::AlertDialog>() {
            if let Some(editable) = dialog.extra_child().and_downcast::<gtk::Editable>() { editable.set_text("readme*.txt"); }
            dialog.emit_by_name::<()>("response", &[&"ok"]);
            dialog.force_close();
        }
        glib::timeout_future(Duration::from_millis(300)).await;
        let picked: Vec<String> = win.selected_entries().into_iter().map(|e| e.name).collect();
        step(picked.len() == 2 && picked.iter().all(|n| n.starts_with("readme")), "select items matching", picked.join(", "));
        // Sharing several files copies a list of links.
        {
            let selection = win.imp().selection.borrow().clone().unwrap();
            selection.unselect_all();
            for (i, n) in names(&win).iter().enumerate() { if n.starts_with("readme") { selection.select_item(i as u32, false); } }
            win.imp().details_split.set_show_sidebar(true);
            glib::timeout_future(Duration::from_millis(400)).await;
            shot(&win, "19a-selection-summary").await;
            let _ = WidgetExt::activate_action(&win, "win.copy-link", None);
            until(5, || win.visible_dialog().is_some()).await;
            shot(&win, "19-share-many").await;
            if let Some(dialog) = win.visible_dialog().and_downcast::<adw::AlertDialog>() {
                dialog.emit_by_name::<()>("response", &[&"copy"]);
                dialog.force_close();
            }
            glib::timeout_future(Duration::from_millis(1500)).await;
            let text = win.clipboard().read_text_future().await.ok().flatten().map(|t| t.to_string()).unwrap_or_default();
            step(text.lines().count() == 2 && text.lines().all(|l| l.starts_with("https://")), "share links for several files", format!("{} links", text.lines().count()));
        }
        // A folder made in the mounted folder (as GNOME Files does: create, then rename)
        // appears in the window without refreshing.
        match crate::transfers::mount::mount(client.clone(), &bucket, base, false) {
            Ok((id, path)) => {
                let made = std::fs::create_dir(path.join("Untitled Folder")).and_then(|_| std::fs::rename(path.join("Untitled Folder"), path.join("from-files"))).is_ok();
                let shown = until(10, || names(&win).iter().any(|n| n == "from-files")).await;
                let removed = std::fs::remove_dir(path.join("from-files"));
                let gone = until(10, || !names(&win).iter().any(|n| n == "from-files")).await;
                if let Err(error) = &removed { println!("rmdir: {error}"); }
                let _ = crate::transfers::mount::unmount(id);
                step(made && shown && gone, "mount changes show in the window", format!("made {made}, shown {shown}, removed {gone}"));
            }
            Err(error) => { step(false, "mount changes show in the window", error); }
        }
        // The sort menu orders the list; the user's own choice is put back afterwards.
        let previous = crate::settings::settings().string("sort-order");
        let _ = WidgetExt::activate_action(&win, "win.sort", Some(&"size-desc".to_variant()));
        glib::timeout_future(Duration::from_millis(300)).await;
        let sizes: Vec<i64> = { let model = win.imp().selection.borrow().clone().unwrap(); (0..model.n_items()).filter_map(|i| model.item(i)).map(|o| entry_of(&o)).filter(|e| !e.is_folder).map(|e| e.size).collect() };
        step(sizes.windows(2).all(|w| w[0] >= w[1]), "sort by size", format!("{sizes:?}"));
        let _ = WidgetExt::activate_action(&win, "win.sort", Some(&previous.to_variant()));
        glib::timeout_future(Duration::from_millis(300)).await;
        let sorter = win.imp().column_view.sorter().and_downcast::<gtk::ColumnViewSorter>();
        let primary = sorter.as_ref().and_then(|s| s.primary_sort_column()).and_then(|c| c.title()).map(|t| t.to_string()).unwrap_or_default();
        let order = sorter.as_ref().map(|s| s.primary_sort_order());
        step(previous != "name-asc" || (primary == crate::i18n::tr("Name") && order == Some(gtk::SortType::Ascending)), "sort order put back", format!("{previous}: {primary} {order:?}"));
        win.imp().view_stack.set_visible_child_name("analyzer");
        let analyzer = win.imp().analyzer.borrow().clone();
        if let Some(analyzer) = analyzer { analyzer.analyze_folder(&win, base.to_string()); }
        glib::timeout_future(Duration::from_millis(4000)).await;
        shot(&win, "13-analyzer").await;
        win.imp().view_stack.set_visible_child_name("backups");
        shot(&win, "14-backups").await;
        win.imp().view_stack.set_visible_child_name("compatibility");
        shot(&win, "15-compatibility").await;
        // The compatibility test shows its progress and results while it runs (read-only here).
        {
            let bin: gtk::Widget = win.imp().compatibility_bin.get().upcast();
            if let Some(switch) = find_by_title::<adw::SwitchRow>(&bin, &crate::i18n::tr("Include write tests")) { switch.set_active(false); }
            if let Some(run) = find_by_title::<adw::ButtonRow>(&bin, &crate::i18n::tr("Run Test")) {
                run.emit_by_name::<()>("activated", &[]);
                glib::timeout_future(Duration::from_millis(900)).await;
                shot(&win, "15d-compatibility-running").await;
                let finished = until(60, || find_by_title::<adw::ButtonRow>(&bin, &crate::i18n::tr("Run Again")).is_some()).await;
                shot(&win, "15e-compatibility-done").await;
                step(finished, "compatibility test shows progress and results", String::new());
            }
        }
        win.imp().view_stack.set_visible_child_name("recent");
        step(crate::pages::recent::count() > 0, "recent lists transferred objects", format!("{} objects", crate::pages::recent::count()));
        shot(&win, "15b-recent").await;
        crate::pages::recent::search_for("readme");
        let found = crate::search::find("readme").len();
        step(found > 0, "search all connections", format!("{found} results"));
        shot(&win, "15c-search-everywhere").await;
        crate::pages::recent::search_for("");
        win.imp().view_stack.set_visible_child_name("browser");

        // An image of the bucket root in the quick preview.
        win.navigate("");
        until(15, || names(&win).iter().any(|n| n.ends_with(".png") || n.ends_with(".jpg"))).await;
        if let Some(index) = names(&win).iter().position(|n| n.ends_with(".png") || n.ends_with(".jpg")) {
            let selection = win.imp().selection.borrow().clone().unwrap();
            selection.select_item(index as u32, true);
            win.preview_selection();
            step(dialog_shot(&win, "16-image-preview", 3000).await, "image preview opens", String::new());
        }
        win.navigate(base);
        until(15, || names(&win).iter().any(|n| n == "readme.txt")).await;

        // A watched backup job uploads a new file by itself.
        let watched = std::env::temp_dir().join(format!("ferry-watch-{}", std::process::id()));
        std::fs::create_dir_all(&watched).unwrap();
        let job = crate::pages::backups::Job { id: "smoke-watch".into(), name: "smoke watch".into(), profile_id: client.profile.id.clone(), bucket: bucket.clone(),
            prefix: format!("{base}watched/"), dir: watched.display().to_string(), watch: true, ..Default::default() };
        let _ = crate::pages::backups::save_job(job);
        crate::pages::backups::refresh_jobs();
        glib::timeout_future(Duration::from_millis(500)).await;
        std::fs::write(watched.join("new-file.txt"), b"written while watched\n").unwrap();
        let key = format!("{base}watched/new-file.txt");
        let mut uploaded = false;
        for _ in 0..150 {
            let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
            if crate::runtime::bg(async move { c.head_object(&b, &k).await }).await.is_ok() { uploaded = true; break; }
            glib::timeout_future(Duration::from_millis(200)).await;
        }
        step(uploaded, "watched folder uploads a new file", key.clone());
        let _ = crate::pages::backups::remove_job("smoke-watch");
        crate::pages::backups::refresh_jobs();
        let _ = std::fs::remove_dir_all(&watched);

        // A large folder: opening, filtering and sorting stay quick (only with FERRY_SMOKE_BIG).
        if let Some(count) = std::env::var("FERRY_SMOKE_BIG").ok().and_then(|v| v.parse::<usize>().ok()) {
            let folder = format!("{base}many/");
            let started = Instant::now();
            let (c, b, f) = (client.clone(), bucket.clone(), folder.clone());
            let made = crate::runtime::bg(async move {
                use futures_util::{StreamExt, TryStreamExt};
                futures_util::stream::iter(0..count).map(|i| {
                    let (c, b, key) = (c.clone(), b.clone(), format!("{f}file-{i:05}.txt"));
                    async move { c.create_object(&b, &key, format!("{i}").into_bytes(), "text/plain").await }
                }).buffer_unordered(32).try_collect::<Vec<()>>().await.map(|v| v.len())
            }).await;
            println!("BIG   created {made:?} in {:?}", started.elapsed());
            let opened = Instant::now();
            win.navigate(&folder);
            let listed = until(120, || names(&win).len() >= count.min(1000)).await;
            let first_page = opened.elapsed();
            // Endless scrolling loads the rest; the whole listing is timed with the cache warm.
            let all = Instant::now();
            let (c, b, f) = (client.clone(), bucket.clone(), folder.clone());
            let full = crate::runtime::bg(async move { c.list_all(&b, &f, usize::MAX).await.map(|(i, _)| i.len()) }).await;
            let list_all = all.elapsed();
            let filter = Instant::now();
            win.imp().search_entry.set_text("file-01");
            let filtered = names(&win).len();
            let filter_time = filter.elapsed();
            win.imp().search_entry.set_text("");
            let sort = Instant::now();
            let _ = WidgetExt::activate_action(&win, "win.sort", Some(&"name-desc".to_variant()));
            let sort_time = sort.elapsed();
            let _ = WidgetExt::activate_action(&win, "win.sort", Some(&"name-asc".to_variant()));
            step(listed && first_page.as_secs_f64() < 10.0, "large folder", format!("first page {first_page:?}, all {full:?} objects listed in {list_all:?}, filter {filtered} in {filter_time:?}, sort {sort_time:?}"));
            win.navigate(base);
        }

        // A Cryptomator vault: unlocked it lists cleartext names, locked its files.
        {
            let root = format!("{base}vault/");
            let (c, b, r) = (client.clone(), bucket.clone(), root.clone());
            let made = crate::runtime::bg(async move {
                crate::s3::vault::create(&c, &b, &r, "smoke vault password".into()).await?;
                crate::s3::vault::unlock(&c, &b, &r, "smoke vault password".into()).await?;
                c.create_object(&b, &format!("{r}secret notes.txt"), b"inside".to_vec(), "text/plain").await?;
                c.create_folder(&b, &format!("{r}Private Folder/")).await
            }).await;
            win.navigate(&root);
            let shown = until(10, || names(&win).iter().any(|n| n == "secret notes.txt")).await;
            let banner = win.imp().vault_banner.is_revealed() && win.in_vault();
            step(made.is_ok() && shown && banner, "vault shows cleartext names and its banner", format!("{made:?} {:?}", names(&win)));
            step(!win.lookup_action("copy-link").and_downcast::<gtk::gio::SimpleAction>().is_some_and(|a| a.is_enabled()), "links are off inside a vault", String::new());
            shot(&win, "30-vault-unlocked").await;
            win.imp().vault_banner.emit_by_name::<()>("button-clicked", &[]);
            let locked = until(10, || names(&win).iter().any(|n| n == "masterkey.cryptomator")).await;
            let offered = win.imp().vault_banner.is_revealed() && !win.in_vault();
            step(locked && offered, "locking shows the encrypted files and offers to unlock", format!("{:?}", names(&win)));
            shot(&win, "31-vault-locked").await;
            win.navigate(base);
        }

        // Clean up.
        let (c, b) = (client.clone(), bucket.clone());
        let removed = crate::runtime::bg(async move { c.delete_keys(&b, vec![base.to_string()]).await }).await;
        step(removed.is_ok(), "remove smoke folder", format!("{removed:?}"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        crate::search::save();
        crate::pages::recent::forget(&client.profile.id, &bucket, &[base.to_string()]);
        println!("SMOKE {}", if failures == 0 { "PASSED".to_string() } else { format!("FAILED ({failures})") });
        std::process::exit(if failures == 0 { 0 } else { 1 });
    });
}
