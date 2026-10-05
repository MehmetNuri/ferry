use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::i18n::{tr, trf, trn};
use crate::runtime::bg;
use crate::s3::S3;
use crate::s3::tools::{Analysis, AnalysisEntry, IncompleteUpload};
use crate::window::{Window, format_size, format_time};

pub struct Analyzer {
    pub widget: gtk::Box,
    title: adw::WindowTitle,
    button: gtk::Button,
    stack: gtk::Stack,
    results: adw::Clamp,
    generation: Cell<u64>,
    location: RefCell<(String, String)>,
}

impl Analyzer {
    pub fn new(win: &Window) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let bar =
            gtk::Box::builder().spacing(12).margin_start(12).margin_end(12).margin_top(8).margin_bottom(8).build();
        let title = adw::WindowTitle::new(&tr("Storage Analyzer"), "");
        title.set_halign(gtk::Align::Start);
        title.set_hexpand(true);
        let button = gtk::Button::builder().label(tr("Analyze")).css_classes(["suggested-action"]).build();
        bar.append(&title);
        bar.append(&button);
        widget.append(&bar);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let stack = gtk::Stack::builder().vexpand(true).transition_type(gtk::StackTransitionType::Crossfade).build();
        stack.add_named(&adw::StatusPage::builder().icon_name("drive-harddisk-symbolic").title(tr("Storage Analyzer"))
            .description(tr("Scans every object below the open folder and shows which folders, file types and duplicate copies take up the space. Nothing is changed.")).build(), Some("intro"));
        stack.add_named(
            &adw::Spinner::builder()
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .width_request(32)
                .height_request(32)
                .build(),
            Some("running"),
        );
        let results = adw::Clamp::builder()
            .maximum_size(900)
            .margin_start(12)
            .margin_end(12)
            .margin_top(18)
            .margin_bottom(18)
            .build();
        stack.add_named(
            &gtk::ScrolledWindow::builder().child(&results).hscrollbar_policy(gtk::PolicyType::Never).build(),
            Some("results"),
        );
        widget.append(&stack);
        let analyzer = Rc::new(Analyzer {
            widget,
            title,
            button,
            stack,
            results,
            generation: Cell::new(0),
            location: RefCell::default(),
        });
        let (this, win) = (Rc::downgrade(&analyzer), win.clone());
        analyzer.button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.run(&win);
            }
        });
        analyzer
    }

    pub fn set_location(&self, bucket: &str, prefix: &str) {
        let changed = *self.location.borrow() != (bucket.to_string(), prefix.to_string());
        self.location.replace((bucket.to_string(), prefix.to_string()));
        self.title.set_subtitle(&format!("{bucket}/{prefix}"));
        self.button.set_sensitive(!bucket.is_empty());
        if changed && self.stack.visible_child_name().as_deref() != Some("running") {
            self.stack.set_visible_child_name("intro");
        }
    }

    fn run(self: &Rc<Self>, win: &Window) {
        let Some(client) = win.current_client() else { return };
        let (bucket, prefix) = self.location.borrow().clone();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.stack.set_visible_child_name("running");
        self.button.set_sensitive(false);
        let (this, win) = (self.clone(), win.clone());
        glib::spawn_future_local(async move {
            let (c, b, p) = (client.clone(), bucket.clone(), prefix.clone());
            let result = bg(async move {
                let analysis = c.analyze(&b, &p).await?;
                let uploads = c.incomplete_uploads(&b).await;
                Ok((analysis, uploads))
            })
            .await;
            if this.generation.get() != generation {
                return;
            }
            this.button.set_sensitive(true);
            match result {
                Ok((analysis, uploads)) => {
                    this.show(&win, client, bucket, prefix, analysis, uploads);
                    this.stack.set_visible_child_name("results");
                }
                Err(error) => {
                    this.stack.set_visible_child_name("intro");
                    win.toast(&error);
                }
            }
        });
    }

    fn show(
        self: &Rc<Self>,
        win: &Window,
        client: S3,
        bucket: String,
        prefix: String,
        a: Analysis,
        uploads: Option<Vec<IncompleteUpload>>,
    ) {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
        if a.truncated {
            content.append(
                &adw::Banner::builder()
                    .title(trf(
                        "The scan stopped at {n} objects; totals are incomplete.",
                        &[("n", &a.objects.to_string())],
                    ))
                    .revealed(true)
                    .build(),
            );
        }
        let summary = adw::PreferencesGroup::builder().title(tr("Summary")).build();
        for (title, value) in [
            (tr("Total size"), format_size(a.size)),
            (tr("Objects"), a.objects.to_string()),
            (
                tr("Duplicate copies"),
                format!(
                    "{} ({:.1}%)",
                    format_size(a.wasted),
                    if a.size > 0 { a.wasted as f64 * 100.0 / a.size as f64 } else { 0.0 }
                ),
            ),
            (tr("Average object"), format_size(if a.objects > 0 { a.size / a.objects } else { 0 })),
        ] {
            let row = adw::ActionRow::builder().title(title).build();
            row.add_suffix(&gtk::Label::builder().label(value).css_classes(["numeric"]).build());
            summary.add(&row);
        }
        content.append(&summary);

        let total = a.size.max(1);
        let bars = |title: String,
                    entries: &[AnalysisEntry],
                    name: &dyn Fn(&str) -> String,
                    folders: bool|
         -> adw::PreferencesGroup {
            let group = adw::PreferencesGroup::builder().title(title).build();
            let max = entries.iter().map(|e| e.size).max().unwrap_or(1).max(1);
            for entry in entries {
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&name(&entry.name)))
                    .subtitle(format!(
                        "{} · {:.1}%",
                        trn("{n} object", "{n} objects", &[("n", &entry.objects.to_string())]),
                        entry.size as f64 * 100.0 / total as f64
                    ))
                    .build();
                let level = gtk::LevelBar::builder()
                    .min_value(0.0)
                    .max_value(1.0)
                    .value(entry.size as f64 / max as f64)
                    .width_request(140)
                    .valign(gtk::Align::Center)
                    .build();
                for offset in [gtk::LEVEL_BAR_OFFSET_LOW, gtk::LEVEL_BAR_OFFSET_HIGH, gtk::LEVEL_BAR_OFFSET_FULL] {
                    level.remove_offset_value(Some(offset));
                }
                row.add_suffix(&level);
                row.add_suffix(
                    &gtk::Label::builder()
                        .label(format_size(entry.size))
                        .width_chars(9)
                        .xalign(1.0)
                        .css_classes(["numeric"])
                        .build(),
                );
                if folders && entry.name.ends_with('/') {
                    row.set_activatable(true);
                    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                    row.set_action_name(Some("win.analyze-folder"));
                    row.set_action_target_value(Some(&format!("{prefix}{}", entry.name).to_variant()));
                }
                group.add(&row);
            }
            if entries.is_empty() {
                group.add(&adw::ActionRow::builder().title("—").build());
            }
            group
        };
        let folder_name = |n: &str| {
            if n.is_empty() {
                tr("(files in this folder)")
            } else if n == "*" {
                tr("Other")
            } else {
                n.to_string()
            }
        };
        let type_name = |n: &str| {
            if n == "*" {
                tr("Other")
            } else if n.is_empty() {
                tr("(no extension)")
            } else {
                format!(".{n}")
            }
        };
        let age_name = |n: &str| match n {
            "30d" => tr("Last 30 days"),
            "90d" => tr("30–90 days"),
            "1y" => tr("90 days – 1 year"),
            _ => tr("Older than 1 year"),
        };
        let class_name = |n: &str| if n == "*" { tr("Other") } else { n.to_string() };
        content.append(&bars(tr("By Folder"), &a.folders, &folder_name, true));
        content.append(&bars(tr("By File Type"), &a.types, &type_name, false));
        content.append(&bars(tr("By Last Change"), &a.ages, &age_name, false));
        content.append(&bars(tr("By Storage Class"), &a.classes, &class_name, false));

        let largest = adw::PreferencesGroup::builder().title(tr("Largest Objects")).build();
        for entry in &a.largest {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&entry.key))
                .subtitle(format_time(entry.modified))
                .activatable(true)
                .tooltip_text(tr("Show in Browser"))
                .build();
            row.add_suffix(&gtk::Label::builder().label(format_size(entry.size)).css_classes(["numeric"]).build());
            row.set_action_name(Some("win.reveal"));
            row.set_action_target_value(Some(&entry.key.to_variant()));
            largest.add(&row);
        }
        content.append(&largest);

        let duplicates = adw::PreferencesGroup::builder()
            .title(tr("Duplicate Objects"))
            .description(tr("Same ETag and size"))
            .build();
        for group in &a.duplicates {
            let row = adw::ExpanderRow::builder()
                .title(trn(
                    "{n} copy · {size} each",
                    "{n} copies · {size} each",
                    &[("n", &group.copies.to_string()), ("size", &format_size(group.size))],
                ))
                .subtitle(trf("{size} wasted", &[("size", &format_size(group.wasted))]))
                .build();
            for key in &group.keys {
                let child = adw::ActionRow::builder().title(glib::markup_escape_text(key)).activatable(true).build();
                child.set_action_name(Some("win.reveal"));
                child.set_action_target_value(Some(&key.to_variant()));
                row.add_row(&child);
            }
            duplicates.add(&row);
        }
        if a.duplicates.is_empty() {
            duplicates.add(&adw::ActionRow::builder().title(tr("No duplicate objects found.")).build());
        }
        content.append(&duplicates);

        let incomplete = adw::PreferencesGroup::builder()
            .title(tr("Incomplete Uploads"))
            .description(tr("Unfinished multipart uploads; not visible as objects but still stored"))
            .build();
        match uploads {
            None => incomplete
                .add(&adw::ActionRow::builder().title(tr("This server does not list incomplete uploads.")).build()),
            Some(list) => {
                for upload in &list {
                    let mut subtitle = format_time(upload.initiated);
                    if !upload.stale {
                        subtitle = format!("{subtitle} · {}", tr("recent, kept"));
                    }
                    incomplete.add(
                        &adw::ActionRow::builder()
                            .title(glib::markup_escape_text(&upload.key))
                            .subtitle(subtitle)
                            .build(),
                    );
                }
                if list.is_empty() {
                    incomplete.add(&adw::ActionRow::builder().title(tr("No incomplete uploads.")).build());
                }
                let stale: Vec<IncompleteUpload> = list.into_iter().filter(|u| u.stale).collect();
                if !stale.is_empty() {
                    let clean = gtk::Button::builder()
                        .label(trf("Clean Up Older Than 1 Day ({n})", &[("n", &stale.len().to_string())]))
                        .valign(gtk::Align::Center)
                        .css_classes(["destructive-action"])
                        .build();
                    let (win, this) = (win.clone(), self.clone());
                    clean.connect_clicked(move |button| {
                        let (win, this, client, bucket, stale, button) =
                            (win.clone(), this.clone(), client.clone(), bucket.clone(), stale.clone(), button.clone());
                        glib::spawn_future_local(async move {
                            let dialog = adw::AlertDialog::new(
                                Some(&tr("Remove Incomplete Uploads?")),
                                Some(&trn(
                                    "{n} upload will be removed permanently.",
                                    "{n} uploads will be removed permanently.",
                                    &[("n", &stale.len().to_string())],
                                )),
                            );
                            dialog.add_responses(&[("cancel", &tr("Cancel")), ("remove", &tr("Remove"))]);
                            dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                            dialog.set_close_response("cancel");
                            if dialog.choose_future(Some(&button)).await != "remove" {
                                return;
                            }
                            match bg(async move { client.abort_uploads(&bucket, stale).await }).await {
                                Ok(n) => {
                                    win.toast(&trn(
                                        "{n} incomplete upload removed",
                                        "{n} incomplete uploads removed",
                                        &[("n", &n.to_string())],
                                    ));
                                    this.run(&win);
                                }
                                Err(error) => win.toast(&error),
                            }
                        });
                    });
                    incomplete.set_header_suffix(Some(&clean));
                }
            }
        }
        content.append(&incomplete);
        self.results.set_child(Some(&content));
    }

    pub fn analyze_folder(self: &Rc<Self>, win: &Window, prefix: String) {
        let bucket = self.location.borrow().0.clone();
        self.set_location(&bucket, &prefix);
        self.run(win);
    }
}
