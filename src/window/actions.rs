//! Browser conveniences: history, clipboard, moving by drag and drop, filters,
//! network awareness, suspend inhibition, "Go to Location" and endless scrolling.
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use crate::i18n::{tr, trf, trn};
use crate::transfers::queue;
use crate::runtime::bg;
use crate::s3::{Entry, S3};
use crate::window::{Window, entry_of};

/// Objects copied or cut inside the application.
#[derive(Clone)]
pub struct Clip {
    pub client: S3,
    pub bucket: String,
    pub prefix: String,
    pub entries: Vec<Entry>,
    pub cut: bool,
}

/// Marks a drag of objects from this window, followed by one key per line.
const DRAG_MARK: &str = "ferry-objects\n";

pub fn category(name: &str) -> u32 {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "tif" | "tiff" | "heic" | "heif" | "avif" | "ico" | "raw" => 2,
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp" => 3,
        "mp3" | "wav" | "ogg" | "oga" | "flac" | "aac" | "m4a" | "opus" | "wma" => 4,
        "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "xls" | "xlsx" | "ods" | "csv" | "tsv" | "ppt" | "pptx" | "odp" | "epub" => 5,
        "zip" | "gz" | "tgz" | "tar" | "7z" | "rar" | "bz2" | "xz" | "zst" | "lz4" | "iso" => 6,
        "rs" | "go" | "py" | "js" | "mjs" | "ts" | "tsx" | "jsx" | "json" | "yaml" | "yml" | "toml" | "xml" | "html" | "htm" | "css" | "scss" | "sh"
        | "c" | "h" | "cpp" | "hpp" | "java" | "kt" | "rb" | "php" | "sql" | "swift" | "dart" | "lua" | "vue" | "svelte" => 7,
        _ => 0,
    }
}

fn now() -> i64 {
    glib::real_time() / 1_000_000
}

/// "photo.jpg" → "photo (copy).jpg", "photo (copy).jpg" → "photo (copy 2).jpg".
pub fn copy_name(name: &str, attempt: u32) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    if attempt <= 1 { format!("{stem} ({}){ext}", tr("copy")) } else { format!("{stem} ({} {attempt}){ext}", tr("copy")) }
}

impl Window {
    pub(crate) fn setup_extras(&self) {
        let imp = self.imp();
        let app_actions = [
            gio::ActionEntry::builder("back").activate(|win: &Window, _, _| win.go_history(true)).build(),
            gio::ActionEntry::builder("forward").activate(|win: &Window, _, _| win.go_history(false)).build(),
            gio::ActionEntry::builder("copy").activate(|win: &Window, _, _| win.copy_selection(false)).build(),
            gio::ActionEntry::builder("cut").activate(|win: &Window, _, _| win.copy_selection(true)).build(),
            gio::ActionEntry::builder("paste").activate(|win: &Window, _, _| win.paste()).build(),
            gio::ActionEntry::builder("view-list").activate(|win: &Window, _, _| win.imp().list_toggle.set_active(true)).build(),
            gio::ActionEntry::builder("view-grid").activate(|win: &Window, _, _| win.imp().grid_toggle.set_active(true)).build(),
            gio::ActionEntry::builder("open-with").activate(|win: &Window, _, _| {
                let key = win.imp().info.borrow().as_ref().map(|i| i.key.clone())
                    .or_else(|| win.selected_entries().into_iter().find(|e| !e.is_folder).map(|e| e.key));
                if let (Some(client), Some(key)) = (win.imp().client.borrow().clone(), key) { crate::transfers::external::choose(win, client, win.imp().bucket.borrow().clone(), key); }
            }).build(),
            gio::ActionEntry::builder("copy-public-url").activate(|win: &Window, _, _| {
                let imp = win.imp();
                let Some(client) = imp.client.borrow().clone() else { return };
                let bucket = imp.bucket.borrow().clone();
                let keys: Vec<String> = win.selected_entries().into_iter().filter(|e| !e.is_folder).map(|e| e.key).collect();
                if keys.is_empty() { return; }
                let urls: Vec<String> = keys.iter().map(|k| crate::s3::public_url(&client.profile, &bucket, k)).collect();
                win.clipboard().set_text(&urls.join("\n"));
                win.toast(&tr("Public address copied; it works only when the bucket or object allows public reading"));
            }).build(),
            gio::ActionEntry::builder("copy-location").activate(|win: &Window, _, _| win.copy_location()).build(),
            // The text of a small file goes straight to the clipboard, without saving it anywhere.
            gio::ActionEntry::builder("copy-contents").activate(|win: &Window, _, _| {
                let Some(entry) = win.selected_entries().into_iter().find(|e| !e.is_folder) else { return };
                if !crate::dialogs::preview::is_text(&entry.name) { win.toast(&tr("Only text files can be copied as text")); return; }
                if entry.size > 1024 * 1024 { win.toast(&tr("The file is too large to copy as text (at most 1 MB)")); return; }
                let Some(client) = win.imp().client.borrow().clone() else { return };
                let (bucket, key, win) = (win.imp().bucket.borrow().clone(), entry.key.clone(), win.clone());
                glib::spawn_future_local(async move {
                    match bg(async move { client.read_bytes(&bucket, &key, 1024 * 1024).await }).await {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(text) => { win.clipboard().set_text(&text); win.toast(&trf("Contents of “{name}” copied", &[("name", &entry.name)])); }
                            Err(_) => win.toast(&tr("The file is not valid UTF-8 text")),
                        },
                        Err(error) => win.toast(&error),
                    }
                });
            }).build(),
            // The same download as an AWS CLI command, for scripts and servers.
            gio::ActionEntry::builder("copy-cli").activate(|win: &Window, _, _| {
                let imp = win.imp();
                let Some(client) = imp.client.borrow().clone() else { return };
                let bucket = imp.bucket.borrow().clone();
                let mut entries = win.selected_entries();
                if entries.is_empty() { entries.push(Entry { key: imp.prefix.borrow().clone(), is_folder: true, ..Default::default() }); }
                let mut options = String::new();
                let endpoint = client.profile.endpoint.trim();
                if !endpoint.is_empty() { options.push_str(&format!(" --endpoint-url {}", glib::shell_quote(endpoint).to_string_lossy())); }
                let region = client.profile.region.trim();
                if !region.is_empty() { options.push_str(&format!(" --region {}", glib::shell_quote(region).to_string_lossy())); }
                let commands: Vec<String> = entries.iter().map(|e| {
                    let source = glib::shell_quote(format!("s3://{bucket}/{}", e.key)).to_string_lossy().to_string();
                    if e.is_folder || e.key.is_empty() {
                        let name = e.key.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(&bucket).to_string();
                        format!("aws s3 cp --recursive {source} {}{options}", glib::shell_quote(format!("./{name}/")).to_string_lossy())
                    } else {
                        format!("aws s3 cp {source} .{options}")
                    }
                }).collect();
                win.clipboard().set_text(&commands.join("\n"));
                win.toast(&tr("AWS CLI command copied"));
            }).build(),
            gio::ActionEntry::builder("preview").activate(|win: &Window, _, _| win.preview_selection()).build(),
            gio::ActionEntry::builder("upload-link").activate(|win: &Window, _, _| {
                let imp = win.imp();
                if let Some(client) = imp.client.borrow().clone() {
                    crate::dialogs::share::present_upload(win, client, imp.bucket.borrow().clone(), imp.prefix.borrow().clone());
                }
            }).build(),
            gio::ActionEntry::builder("open-location").activate(|win: &Window, _, _| win.open_location()).build(),
            gio::ActionEntry::builder("share").activate(|win: &Window, _, _| {
                let key = win.imp().info.borrow().as_ref().map(|i| i.key.clone())
                    .or_else(|| win.selected_entries().into_iter().find(|e| !e.is_folder).map(|e| e.key));
                if let Some(key) = key { win.presign(key); }
            }).build(),
        ];
        self.add_action_entries(app_actions);
        // Bucket menu entries in the sidebar open the bucket first, then act on it.
        let with_bucket = |name: &str, f: fn(&Window)| {
            gio::ActionEntry::builder(name).parameter_type(Some(glib::VariantTy::STRING)).activate(move |win: &Window, _, param| {
                let Some(bucket) = param.and_then(|p| p.get::<String>()) else { return };
                if *win.imp().bucket.borrow() != bucket { win.go_to(&bucket, ""); }
                f(win);
            }).build()
        };
        self.add_action_entries([
            with_bucket("bucket-settings-for", |win| { let _ = WidgetExt::activate_action(win, "win.bucket-settings", None); }),
            with_bucket("analyze-bucket", |win| {
                win.imp().view_stack.set_visible_child_name("analyzer");
                let analyzer = win.imp().analyzer.borrow().clone();
                if let Some(analyzer) = analyzer { analyzer.analyze_folder(win, String::new()); }
            }),
            with_bucket("mount-bucket", |win| { let _ = WidgetExt::activate_action(win, "win.mount", None); }),
            with_bucket("copy-bucket-location", |win| {
                win.clipboard().set_text(&format!("s3://{}/", win.imp().bucket.borrow()));
                win.toast(&tr("Location copied"));
            }),
        ]);
        self.update_history_buttons();

        // Clipboard, preview and rename keys act only while the list or grid has the focus.
        for view in [imp.column_view.upcast_ref::<gtk::Widget>(), imp.grid_view.upcast_ref()] {
            let shortcuts = gtk::ShortcutController::new();
            for (trigger, action) in [("<Control><Shift>c", "win.copy-location"), ("<Control>c", "win.copy"), ("<Control>x", "win.cut"), ("<Control>v", "win.paste"), ("space", "win.preview"), ("F2", "win.rename")] {
                shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(trigger), Some(gtk::NamedAction::new(action))));
            }
            view.add_controller(shortcuts);
        }

        // The side buttons of a mouse go back and forward, as in GNOME Files.
        let buttons = gtk::GestureClick::builder().button(0).propagation_phase(gtk::PropagationPhase::Capture).build();
        buttons.connect_pressed(glib::clone!(#[weak(rename_to = win)] self, move |gesture, _, _, _| {
            match gesture.current_button() {
                8 => { win.go_history(true); gesture.set_state(gtk::EventSequenceState::Claimed); }
                9 => { win.go_history(false); gesture.set_state(gtk::EventSequenceState::Claimed); }
                _ => {}
            }
        }));
        self.add_controller(buttons);

        self.setup_filters();
        self.setup_network();
        self.setup_endless_scroll();
        self.restore_queue();
    }

    /// Brings back unfinished uploads and downloads of the last session, paused until the user resumes them.
    fn restore_queue(&self) {
        let saved = queue::Queue::saved();
        if saved.is_empty() { return; }
        let queue = self.queue();
        queue.set_paused(true);
        let mut restored = 0;
        for spec in saved {
            let text = |name: &str| spec.get(name).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let (kind, profile, bucket, key, path) = (text("kind"), text("profile"), text("bucket"), text("key"), text("path"));
            let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            if kind == "sync" {
                let (prefix, title) = (text("prefix"), text("title"));
                let flag = |name: &str| spec.get(name).and_then(|v| v.as_bool()).unwrap_or(false);
                let (down, mirror) = (flag("down"), flag("mirror"));
                if profile.is_empty() || path.is_empty() { continue; }
                let detail = format!("{path} ↔ {bucket}/{prefix}");
                let local = path.clone();
                let item = queue.add(None, "sync", &title, &detail, 0, Some(&local), queue::work(move |progress| {
                    let (profile, bucket, prefix, dir) = (profile.clone(), bucket.clone(), prefix.clone(), std::path::PathBuf::from(&path));
                    async move {
                        let client = crate::s3::client_for(&profile).await?;
                        let result = if down { client.sync_down(&bucket, &prefix, &dir, mirror, &progress).await? } else { client.sync_up(&bucket, &prefix, &dir, mirror, &progress).await? };
                        if result.failed > 0 { Err(trn("{n} file could not be transferred", "{n} files could not be transferred", &[("n", &result.failed.to_string())])) } else { Ok(()) }
                    }
                }));
                queue.set_spec(&item, spec.clone());
                // A mirror deletes; after a restart it waits until the user resumes it.
                if mirror || spec.get("paused").and_then(|v| v.as_bool()).unwrap_or(false) { queue.pause(&item); }
                restored += 1;
                continue;
            }
            if matches!(kind.as_str(), "copy" | "move") {
                let (src_profile, src_bucket, src_key) = (text("src_profile"), text("src_bucket"), text("src_key"));
                if src_profile.is_empty() || profile.is_empty() || src_key.is_empty() || key.is_empty() { continue; }
                let name = src_key.trim_end_matches('/').rsplit('/').next().unwrap_or(&src_key).to_string();
                let detail = format!("{bucket}/{}", &key[..key.trim_end_matches('/').len() - name.len()]);
                let cut = kind == "move";
                let item = queue.add(None, &kind, &name, &detail, size, None, queue::work(move |progress| {
                    let (src_profile, profile, src_bucket, bucket, src_key, key) = (src_profile.clone(), profile.clone(), src_bucket.clone(), bucket.clone(), src_key.clone(), key.clone());
                    async move {
                        let src = crate::s3::client_for(&src_profile).await?;
                        let dst = crate::s3::client_for(&profile).await?;
                        run_transfer(&src, &dst, &src_bucket, &src_key, &bucket, &key, size, cut, &progress).await
                    }
                }));
                queue.set_spec(&item, spec.clone());
                if spec.get("paused").and_then(|v| v.as_bool()).unwrap_or(false) { queue.pause(&item); }
                restored += 1;
                continue;
            }
            if !matches!(kind.as_str(), "upload" | "download") || profile.is_empty() || key.is_empty() || path.is_empty() { continue; }
            let name = key.rsplit('/').next().unwrap_or(&key).to_string();
            let detail = if kind == "upload" { format!("{bucket}/{}", &key[..key.len() - name.len()]) } else { std::path::Path::new(&path).parent().map(|p| p.display().to_string()).unwrap_or_default() };
            let upload = kind == "upload";
            let local = path.clone();
            let item = queue.add(None, &kind, &name, &detail, size, Some(&local), queue::work(move |progress| {
                let (profile, bucket, key, path) = (profile.clone(), bucket.clone(), key.clone(), std::path::PathBuf::from(&path));
                async move {
                    let client = crate::s3::client_for(&profile).await?;
                    if upload { client.upload_file(&bucket, &key, &path, &progress).await } else { client.download_sized(&bucket, &key, size, &path, &progress).await }
                }
            }));
            queue.set_spec(&item, spec.clone());
            if spec.get("paused").and_then(|v| v.as_bool()).unwrap_or(false) { queue.pause(&item); }
            restored += 1;
        }
        if restored == 0 { queue.set_paused(false); return; }
        let toast = adw::Toast::builder().title(trn("{n} unfinished transfer from the last session", "{n} unfinished transfers from the last session", &[("n", &restored.to_string())]))
            .button_label(tr("Resume")).timeout(0).priority(adw::ToastPriority::High).build();
        toast.connect_button_clicked(glib::clone!(#[weak(rename_to = win)] self, move |_| win.queue().set_paused(false)));
        self.imp().toasts.add_toast(toast);
    }

    // ----- History -----

    /// Remembers the previous location when the open folder changes.
    pub(crate) fn record_location(&self) {
        let imp = self.imp();
        let current = (imp.bucket.borrow().clone(), imp.prefix.borrow().clone());
        let previous = imp.location.replace(current.clone());
        if previous == current || imp.history_moving.get() {
            self.update_history_buttons();
            return;
        }
        if !previous.0.is_empty() {
            let mut back = imp.history_back.borrow_mut();
            back.push(previous);
            if back.len() > 100 { back.remove(0); }
        }
        imp.history_forward.borrow_mut().clear();
        self.update_history_buttons();
    }

    pub(crate) fn reset_history(&self) {
        let imp = self.imp();
        imp.history_back.borrow_mut().clear();
        imp.history_forward.borrow_mut().clear();
        imp.location.replace(Default::default());
        self.update_history_buttons();
    }

    pub(crate) fn update_history_buttons(&self) {
        let imp = self.imp();
        self.set_action_enabled("back", !imp.history_back.borrow().is_empty());
        self.set_action_enabled("forward", !imp.history_forward.borrow().is_empty());
    }

    pub(crate) fn go_history(&self, back: bool) {
        let imp = self.imp();
        let target = if back { imp.history_back.borrow_mut().pop() } else { imp.history_forward.borrow_mut().pop() };
        let Some((bucket, prefix)) = target else { return };
        let current = imp.location.borrow().clone();
        if back { imp.history_forward.borrow_mut().push(current); } else { imp.history_back.borrow_mut().push(current); }
        imp.history_moving.set(true);
        self.go_to(&bucket, &prefix);
        imp.history_moving.set(false);
        self.update_history_buttons();
    }

    /// Opens a folder of any bucket of the connection.
    pub(crate) fn go_to(&self, bucket: &str, prefix: &str) {
        let imp = self.imp();
        imp.view_stack.set_visible_child_name("browser");
        if *imp.bucket.borrow() != bucket {
            imp.thumbnails.borrow_mut().clear();
            imp.bucket.replace(bucket.to_string());
            let buckets = imp.buckets.borrow().clone();
            let row = buckets.iter().position(|b| b.name == bucket).and_then(|i| imp.buckets_list.row_at_index(i as i32));
            imp.buckets_list.select_row(row.as_ref());
        }
        imp.browser_stack.set_visible_child_name("browser");
        self.navigate(prefix);
    }

    /// Ctrl+L: type or paste a location such as s3://bucket/folder/ or bucket/folder/file.txt.
    fn open_location(&self) {
        let imp = self.imp();
        if imp.client.borrow().is_none() { return; }
        let current = format!("s3://{}/{}", imp.bucket.borrow(), imp.prefix.borrow());
        let win = self.clone();
        glib::spawn_future_local(async move {
            let Some(text) = win.ask_text(&tr("Go to Location"), &tr("A bucket and path, for example s3://bucket/folder/ or bucket/folder/file.txt"), &current, &tr("Go"), false).await else { return };
            let path = text.trim().trim_start_matches("s3://").trim_start_matches('/');
            let (bucket, rest) = path.split_once('/').unwrap_or((path, ""));
            if bucket.is_empty() { return; }
            if rest.is_empty() || rest.ends_with('/') {
                win.go_to(bucket, rest);
            } else {
                let folder = &rest[..rest.rfind('/').map(|i| i + 1).unwrap_or(0)];
                win.go_to(bucket, folder);
                win.show_details(rest.to_string());
            }
        });
    }

    /// Opens an s3://bucket/key link: in the open connection when it has the bucket,
    /// otherwise in the first saved connection that does.
    pub(crate) fn open_s3_uri(&self, uri: &str) {
        let path = uri.trim_start_matches("s3://");
        let path = glib::Uri::unescape_string(path, None::<&str>).map(|s| s.to_string()).unwrap_or_else(|| path.to_string());
        let (bucket, key) = path.split_once('/').map(|(b, k)| (b.to_string(), k.to_string())).unwrap_or((path.clone(), String::new()));
        if bucket.is_empty() { return; }
        let win = self.clone();
        glib::spawn_future_local(async move {
            // At startup the last connection may still be opening.
            for _ in 0..50 {
                if win.imp().client.borrow().is_none() || !win.imp().buckets.borrow().is_empty() { break; }
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
            }
            let current = win.imp().client.borrow().as_ref().map(|c| c.profile.id.clone());
            if let Some(id) = &current && win.imp().buckets.borrow().iter().any(|b| b.name == bucket) {
                win.open_object(id.clone(), bucket, key);
                return;
            }
            for profile in crate::profile::load() {
                if current.as_ref() == Some(&profile.id) { continue; }
                let (id, wanted) = (profile.id.clone(), bucket.clone());
                let has = bg(async move { Ok(crate::s3::client_for(&id).await?.list_buckets().await?.buckets.iter().any(|b| b.name == wanted)) }).await;
                if has.unwrap_or(false) {
                    win.open_object(profile.id, bucket, key);
                    return;
                }
            }
            win.toast(&trf("No saved connection has a bucket named “{bucket}”", &[("bucket", &bucket)]));
        });
    }

    /// Opens an object of any saved connection, connecting first when needed (search results).
    pub(crate) fn open_object(&self, profile_id: String, bucket: String, key: String) {
        let folder = key[..key.trim_end_matches('/').rfind('/').map(|i| i + 1).unwrap_or(0)].to_string();
        let show = glib::clone!(#[weak(rename_to = win)] self, #[strong] bucket, #[strong] key, #[strong] folder, move || {
            if key.is_empty() || key.ends_with('/') { win.go_to(&bucket, &key); } else { win.go_to(&bucket, &folder); win.show_details(key.clone()); }
        });
        let connected = self.imp().client.borrow().as_ref().is_some_and(|c| c.profile.id == profile_id);
        if connected { show(); return; }
        let Some(profile) = crate::profile::load().into_iter().find(|p| p.id == profile_id) else { return };
        self.connect(profile);
        // Opening waits until the connection and its bucket list are ready.
        let win = self.clone();
        glib::spawn_future_local(async move {
            for _ in 0..300 {
                if win.imp().client.borrow().as_ref().is_some_and(|c| c.profile.id == profile_id) && !win.imp().buckets.borrow().is_empty() {
                    show();
                    return;
                }
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
            }
        });
    }

    /// Shows the search bar with a text, as "Search in Ferry" from GNOME Shell does.
    pub(crate) fn start_search(&self, text: &str) {
        let imp = self.imp();
        if imp.bucket.borrow().is_empty() { return; }
        imp.view_stack.set_visible_child_name("browser");
        imp.search_bar.set_search_mode(true);
        imp.search_entry.set_text(text);
        imp.search_entry.grab_focus();
    }

    // ----- Clipboard -----

    /// Ctrl+Shift+C: the s3:// locations of the selection, or of the open folder.
    fn copy_location(&self) {
        let imp = self.imp();
        let bucket = imp.bucket.borrow().clone();
        if bucket.is_empty() { return; }
        let selected = self.selected_entries();
        let text = if selected.is_empty() {
            format!("s3://{bucket}/{}", imp.prefix.borrow())
        } else {
            selected.iter().map(|e| format!("s3://{bucket}/{}", e.key)).collect::<Vec<_>>().join("\n")
        };
        self.clipboard().set_text(&text);
        self.toast(&tr("Location copied"));
    }

    pub(crate) fn copy_selection(&self, cut: bool) {
        let entries = self.selected_entries();
        let imp = self.imp();
        let Some(client) = imp.client.borrow().clone() else { return };
        if entries.is_empty() { return; }
        let bucket = imp.bucket.borrow().clone();
        // Other applications get the locations as text, and GNOME Files gets the files
        // themselves: they are downloaded when it pastes them.
        let text = entries.iter().map(|e| format!("s3://{bucket}/{}", e.key)).collect::<Vec<_>>().join("\n");
        // Only a modest choice of files is offered as files: clipboard managers may ask for
        // every format at once, and that must not start a large download.
        let total: i64 = entries.iter().map(|e| e.size.max(0)).sum();
        if entries.iter().all(|e| !e.is_folder) && entries.len() <= 100 && total <= 256 * 1024 * 1024 {
            let keys: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
            let provider = crate::transfers::drag_out::DragOut::new(client.clone(), bucket.clone(), imp.prefix.borrow().clone(), keys, text);
            let _ = self.clipboard().set_content(Some(&provider));
        } else {
            self.clipboard().set_text(&text);
        }
        let count = entries.len();
        imp.clipboard.replace(Some(Clip { client, bucket, prefix: imp.prefix.borrow().clone(), entries, cut }));
        self.toast(&if cut { trn("{n} item cut; paste it into another folder", "{n} items cut; paste them into another folder", &[("n", &count.to_string())]) } else { trn("{n} item copied", "{n} items copied", &[("n", &count.to_string())]) });
    }

    /// Pastes objects copied in this window, or uploads files copied in GNOME Files.
    pub(crate) fn paste(&self) {
        if self.imp().bucket.borrow().is_empty() { return; }
        let clipboard = self.clipboard();
        if !clipboard.is_local() && clipboard.formats().contains_type(gdk::FileList::static_type()) {
            let win = self.clone();
            glib::spawn_future_local(async move {
                if let Ok(value) = clipboard.read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT).await
                    && let Ok(files) = value.get::<gdk::FileList>() {
                    win.upload(files.files().iter().filter_map(|f| f.path()).collect());
                }
            });
            return;
        }
        // A picture on the clipboard (Print Screen copies one) becomes a PNG in this folder.
        if !clipboard.is_local() && clipboard.formats().contains_type(gdk::Texture::static_type()) {
            let win = self.clone();
            glib::spawn_future_local(async move {
                let Ok(Some(texture)) = clipboard.read_texture_future().await else { win.toast(&tr("The picture on the clipboard could not be read")); return };
                let stamp = glib::DateTime::now_local().ok().and_then(|d| d.format("%Y-%m-%d %H-%M-%S").ok()).map(|s| s.to_string()).unwrap_or_default();
                let dir = glib::user_cache_dir().join("ferry").join("paste").join(glib::uuid_string_random().as_str());
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("{} {stamp}.png", tr("Pasted image")));
                match texture.save_to_png(&path) {
                    Ok(()) => win.upload(vec![path]),
                    Err(error) => win.toast(&error.to_string()),
                }
            });
            return;
        }
        let clip = self.imp().clipboard.borrow().clone();
        // Text copied elsewhere (after anything copied here) becomes a text file, as in GNOME Files.
        if !clipboard.is_local() && clipboard.formats().contains_type(glib::GString::static_type()) {
            let win = self.clone();
            glib::spawn_future_local(async move {
                let Ok(Some(text)) = clipboard.read_text_future().await else { win.toast(&tr("Nothing to paste")); return };
                let Some(client) = win.client() else { return };
                let (bucket, prefix) = (win.imp().bucket.borrow().clone(), win.imp().prefix.borrow().clone());
                let stamp = glib::DateTime::now_local().ok().and_then(|d| d.format("%Y-%m-%d %H-%M-%S").ok()).map(|s| s.to_string()).unwrap_or_default();
                let name = format!("{} {stamp}.txt", tr("Pasted text"));
                let key = format!("{prefix}{name}");
                match bg(async move { client.create_object(&bucket, &key, text.as_bytes().to_vec(), "text/plain; charset=utf-8").await }).await {
                    Ok(()) => { win.toast(&trf("Pasted as “{name}”", &[("name", &name)])); win.refresh(); }
                    Err(error) => win.toast(&error),
                }
            });
            return;
        }
        let Some(clip) = clip else { self.toast(&tr("Nothing to paste")); return };
        let target = self.imp().prefix.borrow().clone();
        if clip.cut {
            self.imp().clipboard.replace(None);
        }
        self.transfer_objects(clip, target);
    }

    /// Copies or moves objects (folders with their content) into a folder of the open bucket.
    pub(crate) fn transfer_objects(&self, clip: Clip, target_prefix: String) {
        let bucket = self.imp().bucket.borrow().clone();
        self.transfer_objects_to(clip, bucket, target_prefix);
    }

    /// Copies or moves objects into a folder of any bucket of the open connection.
    pub(crate) fn transfer_objects_to(&self, clip: Clip, dst_bucket: String, target_prefix: String) {
        let imp = self.imp();
        let Some(dst) = imp.client.borrow().clone() else { return };
        let same_place = clip.client.profile.id == dst.profile.id && clip.bucket == dst_bucket && clip.prefix == target_prefix;
        if same_place && clip.cut { return; }
        // Moving a folder into itself would never end.
        if clip.cut && clip.client.profile.id == dst.profile.id && clip.bucket == dst_bucket && clip.entries.iter().any(|e| e.is_folder && target_prefix.starts_with(&e.key)) {
            self.toast(&tr("A folder cannot be moved into itself"));
            return;
        }
        let win = self.clone();
        glib::spawn_future_local(async move {
            let (src, bucket, entries) = (clip.client.clone(), clip.bucket.clone(), clip.entries.clone());
            let listed = bg(async move {
                let mut keys = Vec::new();
                for entry in entries {
                    if entry.key.ends_with('/') {
                        let (items, _) = src.list_all(&bucket, &entry.key, usize::MAX).await?;
                        keys.extend(items.into_iter().map(|e| (e.key, e.size.max(0) as u64)));
                        keys.push((entry.key.clone(), 0));
                    } else {
                        keys.push((entry.key, entry.size.max(0) as u64));
                    }
                }
                keys.sort();
                keys.dedup_by(|a, b| a.0 == b.0);
                Ok(keys)
            }).await;
            let keys = match listed { Ok(k) => k, Err(error) => { win.toast(&error); return; } };
            let queue = win.queue();
            let cut = clip.cut;
            let server_side = clip.client.profile.id == dst.profile.id;
            // Moves within one connection can be undone: every new key goes back to its old one.
            let moved: std::rc::Rc<std::cell::RefCell<Vec<(String, String)>>> = Default::default();
            let undo_info = (cut && server_side).then(|| (clip.client.clone(), clip.bucket.clone(), dst_bucket.clone(), moved.clone()));
            let batch = queue.batch(glib::clone!(#[weak] win, move |outcome| {
                win.refresh();
                if outcome.failed > 0 {
                    win.toast(&trn("{n} object could not be copied", "{n} objects could not be copied", &[("n", &outcome.failed.to_string())]));
                    return;
                }
                if !cut { win.toast(&tr("Objects copied")); return; }
                let toast = adw::Toast::builder().title(trn("{n} object moved", "{n} objects moved", &[("n", &outcome.done.to_string())])).timeout(8).build();
                if let Some((client, src_bucket, dst_bucket, moved)) = undo_info.clone() {
                    toast.set_button_label(Some(&tr("Undo")));
                    toast.connect_button_clicked(glib::clone!(#[weak] win, move |_| win.undo_move(client.clone(), src_bucket.clone(), dst_bucket.clone(), moved.borrow().clone())));
                    win.offer_undo(&toast);
                    return;
                }
                win.imp().toasts.add_toast(toast);
            }));
            for (key, size) in keys {
                let relative = key.strip_prefix(&clip.prefix).unwrap_or(&key).to_string();
                // Pasting a copy next to the original gets a "(copy)" name.
                let relative = if same_place {
                    let (first, rest) = relative.split_once('/').map(|(f, r)| (f.to_string(), format!("/{r}"))).unwrap_or((relative.clone(), String::new()));
                    format!("{}{rest}", copy_name(&first, 1))
                } else { relative };
                let target_key = format!("{target_prefix}{relative}");
                moved.borrow_mut().push((target_key.clone(), key.clone()));
                let name = key.trim_end_matches('/').rsplit('/').next().unwrap_or(&key).to_string();
                let detail = format!("{dst_bucket}/{target_prefix}");
                // Kept for the next start, so an interrupted copy or move finishes then.
                let spec = serde_json::json!({ "kind": if cut { "move" } else { "copy" }, "src_profile": clip.client.profile.id, "profile": dst.profile.id,
                    "src_bucket": clip.bucket, "bucket": dst_bucket, "src_key": key, "key": target_key, "size": size });
                let (src, dst, src_bucket, dst_bucket) = (clip.client.clone(), dst.clone(), clip.bucket.clone(), dst_bucket.clone());
                let item = queue.add(Some(batch), if cut { "move" } else { "copy" }, &name, &detail, size, None, queue::work(move |progress| {
                    let (src, dst, src_bucket, dst_bucket, key, target_key) = (src.clone(), dst.clone(), src_bucket.clone(), dst_bucket.clone(), key.clone(), target_key.clone());
                    async move { run_transfer(&src, &dst, &src_bucket, &key, &dst_bucket, &target_key, size, cut, &progress).await }
                }));
                queue.set_spec(&item, spec);
            }
            queue.seal(batch);
        });
    }

    /// Moves objects back to where they were before a move.
    pub(crate) fn undo_move(&self, client: S3, src_bucket: String, dst_bucket: String, moved: Vec<(String, String)>) {
        let queue = self.queue();
        let batch = queue.batch(glib::clone!(#[weak(rename_to = win)] self, move |outcome| {
            win.toast(&if outcome.failed > 0 { trn("{n} object could not be moved back", "{n} objects could not be moved back", &[("n", &outcome.failed.to_string())]) } else { tr("Move undone") });
            win.refresh();
        }));
        for (now, before) in moved {
            let name = before.trim_end_matches('/').rsplit('/').next().unwrap_or(&before).to_string();
            let (client, src_bucket, dst_bucket) = (client.clone(), src_bucket.clone(), dst_bucket.clone());
            queue.add(Some(batch), "move", &name, &format!("{src_bucket}/{before}"), 0, None, queue::work(move |_| {
                let (client, src_bucket, dst_bucket, now, before) = (client.clone(), src_bucket.clone(), dst_bucket.clone(), now.clone(), before.clone());
                async move {
                    if before.ends_with('/') { client.create_folder(&src_bucket, &before).await?; } else { client.copy_object(&dst_bucket, &now, &src_bucket, &before).await?; }
                    client.delete_object(&dst_bucket, &now).await
                }
            }));
        }
        queue.seal(batch);
    }

    // ----- Moving by drag and drop -----

    /// Lets a list or grid cell be dragged, and folder cells accept dropped objects.
    pub(crate) fn add_object_dnd(&self, cell: &gtk::Widget) {
        let drag = gtk::DragSource::builder().actions(gdk::DragAction::MOVE | gdk::DragAction::COPY).build();
        drag.connect_prepare(glib::clone!(#[weak(rename_to = win)] self, #[weak] cell, #[upgrade_or] None, move |_, _, _| {
            let key = cell.widget_name().to_string();
            if key.is_empty() { return None; }
            let mut keys: Vec<String> = win.selected_entries().into_iter().map(|e| e.key).collect();
            if !keys.contains(&key) { keys = vec![key]; }
            let imp = win.imp();
            let client = imp.client.borrow().clone()?;
            // Folders in this window get the keys; GNOME Files gets downloaded files on drop.
            let text = format!("{DRAG_MARK}{}", keys.join("\n"));
            Some(crate::transfers::drag_out::DragOut::new(client, imp.bucket.borrow().clone(), imp.prefix.borrow().clone(), keys, text).upcast())
        }));
        drag.connect_drag_begin(glib::clone!(#[weak] cell, move |source, _| {
            let paintable = gtk::WidgetPaintable::new(Some(&cell));
            source.set_icon(Some(&paintable), 12, 12);
        }));
        cell.add_controller(drag);
        self.add_move_target(cell, None);
    }

    /// A drop target that moves dragged objects into a folder: the cell's own key, or `fixed`.
    pub(crate) fn add_move_target(&self, widget: &gtk::Widget, fixed: Option<String>) {
        let drop = gtk::DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
        let folder_of = glib::clone!(#[strong] fixed, move |w: &gtk::Widget| fixed.clone().or_else(|| Some(w.widget_name().to_string()).filter(|k| k.ends_with('/'))));
        let f = folder_of.clone();
        let for_files = folder_of.clone();
        drop.connect_enter(glib::clone!(#[weak] widget, #[upgrade_or] gdk::DragAction::empty(), move |_, _, _| {
            if f(&widget).is_some() { widget.add_css_class("drop-folder"); gdk::DragAction::MOVE } else { gdk::DragAction::empty() }
        }));
        drop.connect_leave(glib::clone!(#[weak] widget, move |_| widget.remove_css_class("drop-folder")));
        drop.connect_drop(glib::clone!(#[weak(rename_to = win)] self, #[weak] widget, #[upgrade_or] false, move |_, value, _, _| {
            widget.remove_css_class("drop-folder");
            let (Some(folder), Ok(text)) = (folder_of(&widget), value.get::<String>()) else { return false };
            let Some(keys) = text.strip_prefix(DRAG_MARK) else { return false };
            let keys: Vec<String> = keys.lines().filter(|k| !k.is_empty() && *k != folder).map(str::to_string).collect();
            if keys.is_empty() { return false; }
            win.move_keys(keys, folder);
            true
        }));
        widget.add_controller(drop);
        // Files dragged from GNOME Files onto a folder are uploaded into it.
        let files = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let folder_of = for_files;
        let f = folder_of.clone();
        files.connect_enter(glib::clone!(#[weak] widget, #[upgrade_or] gdk::DragAction::empty(), move |_, _, _| {
            if f(&widget).is_some() { widget.add_css_class("drop-folder"); gdk::DragAction::COPY } else { gdk::DragAction::empty() }
        }));
        files.connect_leave(glib::clone!(#[weak] widget, move |_| widget.remove_css_class("drop-folder")));
        files.connect_drop(glib::clone!(#[weak(rename_to = win)] self, #[weak] widget, #[upgrade_or] false, move |_, value, _, _| {
            widget.remove_css_class("drop-folder");
            let (Some(folder), Ok(list)) = (folder_of(&widget), value.get::<gdk::FileList>()) else { return false };
            let paths: Vec<std::path::PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() { return false; }
            win.upload_into(paths, folder);
            true
        }));
        widget.add_controller(files);
    }

    /// Objects dropped on another bucket in the sidebar are copied to its top level,
    /// as GNOME Files copies between drives.
    pub(crate) fn add_bucket_target(&self, row: &gtk::Widget, bucket: String) {
        let drop = gtk::DropTarget::new(String::static_type(), gdk::DragAction::COPY);
        let target = bucket.clone();
        drop.connect_enter(glib::clone!(#[weak(rename_to = win)] self, #[weak] row, #[upgrade_or] gdk::DragAction::empty(), move |_, _, _| {
            if *win.imp().bucket.borrow() == target { return gdk::DragAction::empty(); }
            row.add_css_class("drop-folder");
            gdk::DragAction::COPY
        }));
        drop.connect_leave(glib::clone!(#[weak] row, move |_| row.remove_css_class("drop-folder")));
        drop.connect_drop(glib::clone!(#[weak(rename_to = win)] self, #[weak] row, #[upgrade_or] false, move |_, value, _, _| {
            row.remove_css_class("drop-folder");
            let imp = win.imp();
            if *imp.bucket.borrow() == bucket { return false; }
            let (Ok(text), Some(client)) = (value.get::<String>(), imp.client.borrow().clone()) else { return false };
            let Some(keys) = text.strip_prefix(DRAG_MARK) else { return false };
            let store = imp.store.borrow().clone();
            let known: Vec<Entry> = store.map(|s| (0..s.n_items()).filter_map(|i| s.item(i)).map(|o| entry_of(&o)).collect()).unwrap_or_default();
            let entries: Vec<Entry> = keys.lines().filter(|k| !k.is_empty()).map(|k| known.iter().find(|e| e.key == k).cloned()
                .unwrap_or(Entry { key: k.to_string(), is_folder: k.ends_with('/'), ..Default::default() })).collect();
            if entries.is_empty() { return false; }
            let clip = Clip { client, bucket: imp.bucket.borrow().clone(), prefix: imp.prefix.borrow().clone(), entries, cut: false };
            win.toast(&trf("Copying to {bucket}", &[("bucket", &bucket)]));
            win.transfer_objects_to(clip, bucket.clone(), String::new());
            true
        }));
        row.add_controller(drop);
    }

    fn move_keys(&self, keys: Vec<String>, folder: String) {
        let imp = self.imp();
        let Some(client) = imp.client.borrow().clone() else { return };
        let store = imp.store.borrow().clone();
        let known: Vec<Entry> = store.map(|s| (0..s.n_items()).filter_map(|i| s.item(i)).map(|o| entry_of(&o)).collect()).unwrap_or_default();
        let entries = keys.iter().map(|k| known.iter().find(|e| &e.key == k).cloned()
            .unwrap_or(Entry { key: k.clone(), is_folder: k.ends_with('/'), ..Default::default() })).collect();
        let clip = Clip { client, bucket: imp.bucket.borrow().clone(), prefix: imp.prefix.borrow().clone(), entries, cut: true };
        self.transfer_objects(clip, folder);
    }

    // ----- Filters -----

    pub(crate) fn passes_filters(&self, entry: &Entry) -> bool {
        let imp = self.imp();
        // Names starting with a dot (.keep, Supabase's .emptyFolderPlaceholder) are hidden
        // unless asked for, as in GNOME Files. Search results always show them.
        if !imp.show_hidden.get() && imp.search.borrow().is_none() && entry.name.trim_end_matches('/').starts_with('.') {
            return false;
        }
        let kind = imp.type_filter.get();
        match kind {
            0 => {}
            1 => if !entry.is_folder { return false },
            k => if entry.is_folder || category(&entry.name) != k { return false },
        }
        if entry.is_folder { return true; }
        let mb = 1024 * 1024;
        let size_ok = match imp.size_filter.get() {
            1 => entry.size < mb,
            2 => (mb..=100 * mb).contains(&entry.size),
            3 => entry.size > 100 * mb,
            4 => entry.size == 0,
            _ => true,
        };
        let age = now() - entry.modified;
        let date_ok = match imp.date_filter.get() {
            1 => age <= 86_400,
            2 => age <= 7 * 86_400,
            3 => age <= 30 * 86_400,
            4 => age > 365 * 86_400,
            _ => true,
        };
        size_ok && date_ok
    }

    fn setup_filters(&self) {
        let imp = self.imp();
        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_start(12).margin_end(12).margin_top(12).margin_bottom(12).width_request(320).build();
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        let combo = |title: String, items: Vec<String>| {
            let row = adw::ComboRow::builder().title(title).model(&gtk::StringList::new(&items.iter().map(String::as_str).collect::<Vec<_>>())).build();
            list.append(&row);
            row
        };
        let kind = combo(tr("Type"), vec![tr("Any"), tr("Folders"), tr("Images"), tr("Videos"), tr("Audio"), tr("Documents"), tr("Archives"), tr("Code")]);
        let size = combo(tr("Size"), vec![tr("Any"), tr("Smaller than 1 MB"), tr("1 MB – 100 MB"), tr("Larger than 100 MB"), tr("Empty")]);
        let date = combo(tr("Modified"), vec![tr("Any time"), tr("Last 24 hours"), tr("Last 7 days"), tr("Last 30 days"), tr("Older than a year")]);
        content.append(&list);
        let reset = gtk::Button::builder().label(tr("Clear Filters")).halign(gtk::Align::End).css_classes(["flat"]).build();
        content.append(&reset);
        imp.filter_button.set_popover(Some(&gtk::Popover::builder().child(&content).build()));
        let apply = glib::clone!(#[weak(rename_to = win)] self, #[weak] kind, #[weak] size, #[weak] date, move || {
            let imp = win.imp();
            imp.type_filter.set(kind.selected());
            imp.size_filter.set(size.selected());
            imp.date_filter.set(date.selected());
            let active = kind.selected() + size.selected() + date.selected() > 0;
            if active { imp.filter_button.add_css_class("accent"); } else { imp.filter_button.remove_css_class("accent"); }
            if let Some(filter) = imp.filter.borrow().as_ref() { filter.changed(gtk::FilterChange::Different); }
            win.update_status();
        });
        for row in [&kind, &size, &date] {
            let apply = apply.clone();
            row.connect_selected_notify(move |_| apply());
        }
        let (k, z, d) = (kind.clone(), size.clone(), date.clone());
        imp.clear_filters_button.connect_clicked(move |_| { k.set_selected(0); z.set_selected(0); d.set_selected(0); });
        reset.connect_clicked(move |_| { kind.set_selected(0); size.set_selected(0); date.set_selected(0); });
    }

    // ----- Network and power -----

    /// Pauses the queue while offline and resumes it when the network is back.
    fn setup_network(&self) {
        let monitor = gio::NetworkMonitor::default();
        let update = glib::clone!(#[weak(rename_to = win)] self, move |available: bool| {
            let imp = win.imp();
            imp.offline_banner.set_revealed(!available);
            let queue = win.queue();
            if !available && !queue.paused() {
                imp.auto_paused.set(true);
                queue.set_paused(true);
            } else if available && imp.auto_paused.replace(false) {
                queue.set_paused(false);
            }
        });
        update(monitor.is_network_available());
        monitor.connect_network_changed(move |_, available| update(available));
    }

    /// Keeps the computer awake while transfers run; GNOME shows the reason when suspending.
    pub(crate) fn update_inhibit(&self, active: bool) {
        let imp = self.imp();
        let Some(app) = self.application() else { return };
        let cookie = imp.inhibit_cookie.get();
        if active && cookie == 0 {
            imp.inhibit_cookie.set(app.inhibit(Some(self), gtk::ApplicationInhibitFlags::SUSPEND, Some(&tr("Transfers are running"))));
        } else if !active && cookie != 0 {
            app.uninhibit(cookie);
            imp.inhibit_cookie.set(0);
        }
    }

    // ----- Endless scrolling -----

    fn setup_endless_scroll(&self) {
        let imp = self.imp();
        for view in [imp.column_view.upcast_ref::<gtk::Widget>(), imp.grid_view.upcast_ref()] {
            let Some(scrolled) = view.parent().and_downcast::<gtk::ScrolledWindow>() else { continue };
            scrolled.vadjustment().connect_value_changed(glib::clone!(#[weak(rename_to = win)] self, move |adj| {
                if adj.value() + adj.page_size() >= adj.upper() - 600.0 && !win.imp().next_token.borrow().is_empty() {
                    win.load_more();
                }
            }));
        }
    }

    // ----- Quick preview -----

    pub(crate) fn preview_selection(&self) {
        let imp = self.imp();
        let Some(client) = imp.client.borrow().clone() else { return };
        let Some(selection) = imp.selection.borrow().clone() else { return };
        let files: Vec<Entry> = (0..selection.n_items()).filter_map(|i| selection.item(i)).map(|o| entry_of(&o)).filter(|e| !e.is_folder).collect();
        let selected = self.selected_entries().into_iter().find(|e| !e.is_folder);
        let Some(selected) = selected else { return };
        let index = files.iter().position(|e| e.key == selected.key).unwrap_or(0);
        crate::pages::recent::record(&client.profile.id, &client.profile.name, &imp.bucket.borrow(), &selected.key, "previewed");
        crate::dialogs::preview::present(self, client, imp.bucket.borrow().clone(), files, index);
    }
}

/// Copies (or moves) one object, within a connection on the server, between connections
/// through this computer. Run again after an interruption, it finishes the work: a move
/// whose copy already arrived only removes the original.
pub(crate) async fn run_transfer(src: &S3, dst: &S3, src_bucket: &str, key: &str, dst_bucket: &str, target_key: &str, size: u64, cut: bool, progress: &crate::s3::Progress) -> Result<(), String> {
    let copied = if key.ends_with('/') {
        dst.create_folder(dst_bucket, target_key).await
    } else if src.profile.id == dst.profile.id {
        let result = src.copy_object(src_bucket, key, dst_bucket, target_key).await;
        if result.is_ok() { progress.done.fetch_add(size, std::sync::atomic::Ordering::Relaxed); }
        result
    } else {
        src.copy_to(src_bucket, key, dst, dst_bucket, target_key, progress).await
    };
    if let Err(error) = copied {
        // The original is gone but the copy is there: an earlier run already moved it.
        let finished = cut && src.head_object(src_bucket, key).await.is_err() && dst.head_object(dst_bucket, target_key).await.is_ok();
        if !finished { return Err(error); }
        return Ok(());
    }
    if cut { src.delete_object(src_bucket, key).await?; }
    Ok(())
}
