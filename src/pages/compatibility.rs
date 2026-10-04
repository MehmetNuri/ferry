//! Compatibility test: calls every S3 operation the application uses and shows
//! which ones the provider supports. Write tests use a temporary folder that is removed afterwards.
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;
use std::time::Instant;

use crate::i18n::{tr, trf, trn};
use crate::runtime::bg;
use crate::s3::{Progress, S3};
use crate::window::Window;

const TEST_DIR: &str = ".ferry-captest/";

#[derive(Clone, Debug)]
pub struct Check {
    pub group: &'static str,
    pub name: &'static str,
    /// "ok", "unsupported", "denied", "error" or "skipped".
    pub status: &'static str,
    pub detail: String,
    pub ms: u128,
}

fn classify(error: &str) -> &'static str {
    let lower = error.to_lowercase();
    if lower.contains("does not support") || lower.contains("notimplemented") || lower.contains("not implemented") || lower.contains("featurenotenabled") {
        "unsupported"
    } else if lower.contains("accessdenied") || lower.contains("forbidden") || lower.contains("http 403") {
        "denied"
    } else {
        "error"
    }
}

async fn measure<T>(group: &'static str, name: &'static str, future: impl std::future::Future<Output = Result<T, String>>, describe: impl FnOnce(&T) -> String) -> (Check, Option<T>) {
    let start = Instant::now();
    let result = future.await;
    let ms = start.elapsed().as_millis();
    match result {
        Ok(value) => (Check { group, name, status: "ok", detail: describe(&value), ms }, Some(value)),
        Err(error) => (Check { group, name, status: classify(&error), detail: error, ms }, None),
    }
}

fn skipped(group: &'static str, name: &'static str) -> Check {
    Check { group, name, status: "skipped", detail: tr("Needs an earlier step that did not succeed"), ms: 0 }
}

/// Runs every check; each result is also sent to `live` as soon as it is known.
pub async fn run(client: S3, bucket: String, write: bool, live: futures_channel::mpsc::UnboundedSender<Check>) -> Vec<Check> {
    let mut checks: Vec<Check> = Vec::new();
    let mut add = |check: Check| { let _ = live.unbounded_send(check.clone()); checks.push(check); };
    let (check, _) = measure("Service", "ListBuckets", client.list_buckets(), |l| trn("{n} bucket", "{n} buckets", &[("n", &l.buckets.len().to_string())])).await;
    add(check);
    if bucket.is_empty() {
        drop(add);
        return checks;
    }
    let b = bucket.as_str();
    let (check, listing) = measure("Bucket", "ListObjectsV2", client.list_objects(b, "", ""), |l| trn("{n} item", "{n} items", &[("n", &l.items.len().to_string())])).await;
    add(check);
    add(measure("Bucket", "GetBucketLocation", async { Ok::<_, String>(client.bucket_settings(b).await.region) }, |r| r.clone()).await.0);
    add(measure("Bucket", "GetBucketVersioning", client.versioning(b), |v| if v.is_empty() { tr("never enabled") } else { v.clone() }).await.0);
    let settings = client.bucket_settings(b).await;
    for (name, note) in [("GetBucketEncryption", settings.encryption.note().map(str::to_string)), ("GetBucketPolicy", settings.policy.note().map(str::to_string)),
        ("GetBucketCors", settings.cors.note().map(str::to_string)), ("GetBucketLifecycle", settings.lifecycle.note().map(str::to_string)), ("GetBucketTagging", settings.tags.note().map(str::to_string))] {
        add(match note {
            None => Check { group: "Bucket", name, status: "ok", detail: String::new(), ms: 0 },
            Some(error) => Check { group: "Bucket", name, status: classify(&error), detail: error, ms: 0 },
        });
    }
    add(measure("Bucket", "GetBucketAcl", client.bucket_acl(b), |a| trn("{n} grant", "{n} grants", &[("n", &a.grants.len().to_string())])).await.0);
    add(measure("Bucket", "GetObjectLockConfiguration", client.lock_config(b), |l| if l.enabled { tr("Enabled") } else { tr("Off") }).await.0);
    add(measure("Bucket", "ListMultipartUploads", async { client.incomplete_uploads(b).await.ok_or_else(|| tr("This provider does not support this feature")) }, |u| trn("{n} upload", "{n} uploads", &[("n", &u.len().to_string())])).await.0);
    if let Some(existing) = listing.and_then(|l| l.items.into_iter().find(|i| !i.is_folder)) {
        let k = existing.key.as_str();
        add(measure("Object", "HeadObject", client.head_object(b, k), |i| format!("{} · {}", i.content_type, crate::window::format_size(i.size))).await.0);
        add(measure("Object", "GetObject (range)", client.read_bytes(b, k, 1024), |d| trn("{n} byte", "{n} bytes", &[("n", &d.len().to_string())])).await.0);
        add(measure("Object", "GetObjectTagging", client.object_tags(b, k), |t| trn("{n} tag", "{n} tags", &[("n", &t.len().to_string())])).await.0);
        add(measure("Object", "GetObjectAcl", client.object_acl(b, k), |a| trn("{n} grant", "{n} grants", &[("n", &a.grants.len().to_string())])).await.0);
        add(measure("Object", "ListObjectVersions", client.list_versions(b, k), |v| trn("{n} version", "{n} versions", &[("n", &v.len().to_string())])).await.0);
        add(measure("Object", "Presign", client.presign(b, k, 60), |_| String::new()).await.0);
    }
    if !write {
        drop(add);
        return checks;
    }
    // Write tests in a temporary folder.
    let key = format!("{TEST_DIR}{}.txt", glib::uuid_string_random());
    let copy = format!("{key}.copy");
    let big = format!("{key}.multipart");
    let temp = std::env::temp_dir().join(format!("ferry-captest-{}", glib::uuid_string_random()));
    let _ = std::fs::write(&temp, b"Ferry compatibility test\n");
    let (check, put) = measure("Write", "PutObject", client.upload_file(b, &key, &temp, &Progress::default()), |_| String::new()).await;
    add(check);
    if put.is_some() {
        add(measure("Write", "CopyObject", client.copy_object(b, &key, b, &copy), |_| String::new()).await.0);
        add(measure("Write", "PutObjectTagging", client.set_object_tags(b, &key, &[("ferry".into(), "test".into())]), |_| String::new()).await.0);
        let info = client.head_object(b, &key).await;
        if let Ok(mut info) = info {
            info.metadata = vec![("ferry".into(), "test".into())];
            add(measure("Write", "CopyObject (metadata)", client.rewrite(b, &info, None), |_| String::new()).await.0);
        }
        add(measure("Write", "DeleteObjects (batch)", client.delete_keys(b, vec![copy.clone()]), |n| trf("{n} deleted", &[("n", &n.to_string())])).await.0);
    } else {
        for name in ["CopyObject", "PutObjectTagging", "CopyObject (metadata)", "DeleteObjects (batch)"] { add(skipped("Write", name)); }
    }
    // A multipart upload of two parts; the first part has the minimum size of 5 MiB.
    let big_temp = temp.with_extension("big");
    let _ = std::fs::write(&big_temp, vec![b'x'; 17 * 1024 * 1024]);
    add(measure("Write", "Multipart upload", client.upload_file(b, &big, &big_temp, &Progress::default()), |_| crate::window::format_size(17 * 1024 * 1024)).await.0);
    let _ = std::fs::remove_file(&big_temp);
    let _ = std::fs::remove_file(&temp);
    add(measure("Write", "DeleteObject", async {
        client.delete_object(b, &key).await?;
        let _ = client.delete_object(b, &big).await;
        let _ = client.delete_object(b, &copy).await;
        Ok::<_, String>(())
    }, |_| String::new()).await.0);
    drop(add);
    checks
}

/// A status icon in a tinted circle, as GNOME Settings shows states (Device Security).
fn status_badge(icon: &str, class: &str) -> gtk::Widget {
    gtk::Image::builder().icon_name(icon).pixel_size(16).valign(gtk::Align::Center).css_classes(["status-badge", class]).build().upcast()
}

/// About how many results a run gives, for the progress bar.
const EXPECTED_READ_ONLY: usize = 15;
const EXPECTED_WITH_WRITES: usize = 24;

pub fn attach(win: &Window, bin: &adw::Bin) {
    let page = adw::PreferencesPage::new();
    let intro = adw::PreferencesGroup::builder().title(tr("Compatibility Test"))
        .description(trf("Calls every S3 operation this application uses and shows which ones the provider supports. Write tests create temporary objects under {dir} in the open bucket and remove them afterwards.", &[("dir", TEST_DIR)])).build();
    let write = adw::SwitchRow::builder().title(tr("Include write tests")).subtitle(tr("Uploads, copies and deletes temporary objects")).active(true).build();
    let run_row = adw::ButtonRow::builder().title(tr("Run Test")).start_icon_name("media-playback-start-symbolic").build();
    let copy_row = adw::ButtonRow::builder().title(tr("Copy Report")).start_icon_name("edit-copy-symbolic").visible(false).build();
    intro.add(&write);
    intro.add(&run_row);
    intro.add(&copy_row);
    page.add(&intro);
    bin.set_child(Some(&page));
    let groups: Rc<std::cell::RefCell<Vec<adw::PreferencesGroup>>> = Rc::default();
    let report: Rc<std::cell::RefCell<String>> = Rc::default();
    copy_row.connect_activated(glib::clone!(#[strong] report, move |row| {
        row.clipboard().set_text(&report.borrow());
        if let Some(win) = row.root().and_downcast::<Window>() { win.toast(&tr("Report copied")); }
    }));
    let weak = win.downgrade();
    run_row.connect_activated(move |row| {
        let Some(win) = weak.upgrade() else { return };
        let Some(client) = win.current_client() else { win.toast(&tr("Connect to a storage service first")); return };
        let bucket = win.open_bucket_name();
        let (row, page, groups, write, copy_row, report) = (row.clone(), page.clone(), groups.clone(), write.is_active(), copy_row.clone(), report.clone());
        row.set_sensitive(false);
        row.set_title(&tr("Running…"));
        copy_row.set_visible(false);
        for g in groups.borrow_mut().drain(..) { page.remove(&g); }
        // While the test runs, a group at the top says what was tested last and how far it is;
        // each result appears below as soon as it is known.
        let status = adw::PreferencesGroup::builder().title(tr("Testing")).build();
        let current = adw::ActionRow::builder().title(tr("Connecting…")).build();
        let spinner = adw::Spinner::builder().width_request(24).height_request(24).build();
        current.add_prefix(&spinner);
        let bar = gtk::ProgressBar::builder().valign(gtk::Align::Center).width_request(120).build();
        current.add_suffix(&bar);
        status.add(&current);
        page.add(&status);
        groups.borrow_mut().push(status.clone());
        let expected = if write { EXPECTED_WITH_WRITES } else { EXPECTED_READ_ONLY };
        glib::spawn_future_local(async move {
            let profile_name = client.profile.name.clone();
            let (sender, mut receiver) = futures_channel::mpsc::unbounded::<Check>();
            let task = bg(async move { Ok(run(client, bucket.clone(), write, sender).await) });
            let task = glib::spawn_future_local(task);
            let mut text = format!("# {}: {profile_name}\n", tr("S3 compatibility report"));
            let mut current_group: Option<(&str, adw::PreferencesGroup)> = None;
            let (mut done, mut counts) = (0usize, [0usize; 4]);
            use futures_util::StreamExt;
            while let Some(check) = receiver.next().await {
                done += 1;
                bar.set_fraction((done as f64 / expected as f64).min(0.95));
                current.set_title(&trf("Tested {name}", &[("name", check.name)]));
                current.set_subtitle(&trn("{n} check done", "{n} checks done", &[("n", &done.to_string())]));
                if current_group.as_ref().map(|c| c.0) != Some(check.group) {
                    let group = adw::PreferencesGroup::builder().title(check.group).build();
                    page.add(&group);
                    groups.borrow_mut().push(group.clone());
                    current_group = Some((check.group, group));
                    text.push_str(&format!("\n## {}\n", check.group));
                }
                let (icon, class, label, slot) = match check.status {
                    "ok" => ("object-select-symbolic", "success", tr("Supported"), 0),
                    "unsupported" => ("list-remove-symbolic", "neutral", tr("Unsupported"), 1),
                    "denied" => ("changes-prevent-symbolic", "warning", tr("Access denied"), 2),
                    "skipped" => ("media-skip-forward-symbolic", "neutral", tr("Skipped"), 1),
                    _ => ("window-close-symbolic", "error", tr("Error"), 3),
                };
                counts[slot] += 1;
                let result = adw::ActionRow::builder().title(check.name).subtitle(glib::markup_escape_text(&check.detail)).subtitle_lines(2).build();
                result.add_prefix(&status_badge(icon, class));
                let suffix = gtk::Box::builder().orientation(gtk::Orientation::Vertical).valign(gtk::Align::Center).build();
                suffix.append(&gtk::Label::builder().label(&label).xalign(1.0).css_classes(["dim-label", "caption-heading"]).build());
                if check.ms > 0 { suffix.append(&gtk::Label::builder().label(format!("{} ms", check.ms)).xalign(1.0).css_classes(["dim-label", "caption", "numeric"]).build()); }
                result.add_suffix(&suffix);
                if let Some((_, group)) = &current_group { group.add(&result); }
                text.push_str(&format!("- [{label}] {}{}\n", check.name, if check.detail.is_empty() { String::new() } else { format!(" — {}", check.detail) }));
            }
            let _ = task.await;
            // The status group turns into a summary of the run.
            spinner.set_visible(false);
            bar.set_visible(false);
            status.set_title(&tr("Result"));
            current.set_title(&trf("{ok} supported, {no} unsupported or skipped, {denied} denied, {error} failed", &[
                ("ok", &counts[0].to_string()), ("no", &counts[1].to_string()), ("denied", &counts[2].to_string()), ("error", &counts[3].to_string())]));
            current.set_subtitle(&trn("{n} check done", "{n} checks done", &[("n", &done.to_string())]));
            current.add_prefix(&if counts[3] == 0 { status_badge("object-select-symbolic", "success") } else { status_badge("dialog-warning-symbolic", "warning") });
            row.set_sensitive(true);
            row.set_title(&tr("Run Again"));
            report.replace(text);
            copy_row.set_visible(true);
        });
    });
}
