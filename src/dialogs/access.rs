//! Permissions, Object Lock, hosting and CloudFront pages.
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use crate::i18n::{tr, trf};
use crate::runtime::bg;
use crate::s3::{ObjectInfo, Res, S3};
use crate::s3::access::{ALL_USERS, AUTHENTICATED_USERS, Acl, AclGrant, LOG_DELIVERY, PERMISSIONS};
use crate::window::Window;

type Load = Rc<dyn Fn() -> Pin<Box<dyn Future<Output = Res<Acl>> + Send>>>;
type Save = Rc<dyn Fn(Acl) -> Pin<Box<dyn Future<Output = Res<()>> + Send>>>;

fn notify(widget: &impl IsA<gtk::Widget>, text: &str) {
    // Toasts go to the closest dialog, whichever kind it is.
    let mut current = widget.as_ref().parent();
    while let Some(w) = current {
        if let Some(d) = w.downcast_ref::<adw::PreferencesDialog>() { d.add_toast(crate::window::plain_toast(text)); return; }
        if let Some(t) = w.downcast_ref::<adw::ToastOverlay>() { t.add_toast(crate::window::plain_toast(text)); return; }
        current = w.parent();
    }
}

fn saved<T>(widget: &impl IsA<gtk::Widget>, result: Res<T>) -> bool {
    match result {
        Ok(_) => { notify(widget, &tr("Settings saved")); true }
        Err(error) => { notify(widget, &error); false }
    }
}

fn group_label(uri: &str) -> Option<String> {
    match uri {
        ALL_USERS => Some(tr("Everyone")),
        AUTHENTICATED_USERS => Some(tr("Authenticated AWS users")),
        LOG_DELIVERY => Some(tr("Log delivery group")),
        _ => None,
    }
}

/// Adds an editable ACL to a page: the grant list, then rows to add grants and save.
fn add_acl(page: &adw::PreferencesPage, load: Load, save: Save) {
    let list = adw::PreferencesGroup::builder().title(tr("Access Control List"))
        .description(tr("The owner always keeps full control. Public grants are refused while the public access block is on.")).build();
    let editing = adw::PreferencesGroup::new();
    let acl: Rc<RefCell<Option<Acl>>> = Rc::default();
    let rows: Rc<RefCell<Vec<gtk::Widget>>> = Rc::default();
    let kinds = [tr("Everyone"), tr("Authenticated AWS users"), tr("Log delivery group"), tr("Canonical user ID"), tr("AWS account e-mail")];
    let kind = adw::ComboRow::builder().title(tr("Grantee")).model(&gtk::StringList::new(&kinds.iter().map(String::as_str).collect::<Vec<_>>())).build();
    let who = adw::EntryRow::builder().title(tr("Canonical ID or e-mail address")).visible(false).build();
    let permission = adw::ComboRow::builder().title(tr("Permission")).model(&gtk::StringList::new(&PERMISSIONS)).build();
    let add = adw::ButtonRow::builder().title(tr("Add Permission")).start_icon_name("list-add-symbolic").build();
    let apply = adw::ButtonRow::builder().title(tr("Save Permissions")).build();
    for row in [kind.upcast_ref::<gtk::Widget>(), who.upcast_ref(), permission.upcast_ref(), add.upcast_ref(), apply.upcast_ref()] { editing.add(row); }
    editing.set_sensitive(false);
    kind.connect_selected_notify(glib::clone!(#[weak] who, move |k| who.set_visible(k.selected() >= 3)));

    // The renderer refers to itself so remove buttons can redraw the list.
    let render: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::default();
    let render_fn: Rc<dyn Fn()> = {
        let (list, acl, rows, render) = (list.clone(), acl.clone(), rows.clone(), Rc::downgrade(&render));
        Rc::new(move || {
            for row in rows.borrow_mut().drain(..) { list.remove(&row); }
            let Some(current) = acl.borrow().clone() else { return };
            let owner = adw::ActionRow::builder().title(tr("Owner")).subtitle(glib::markup_escape_text(if current.owner_name.is_empty() { &current.owner_id } else { &current.owner_name })).build();
            owner.add_suffix(&gtk::Label::builder().label("FULL_CONTROL").css_classes(["dim-label", "caption"]).build());
            list.add(&owner);
            rows.borrow_mut().push(owner.upcast());
            for (index, grant) in current.grants.iter().enumerate() {
                let title = group_label(&grant.grantee).unwrap_or_else(|| if grant.name.is_empty() { grant.grantee.clone() } else { grant.name.clone() });
                let row = adw::ActionRow::builder().title(glib::markup_escape_text(&title)).subtitle(&grant.permission).tooltip_text(&grant.grantee).build();
                let remove = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text(tr("Remove")).valign(gtk::Align::Center).css_classes(["flat"]).build();
                let (acl, render) = (acl.clone(), render.clone());
                remove.connect_clicked(move |_| {
                    if let Some(a) = acl.borrow_mut().as_mut() && index < a.grants.len() { a.grants.remove(index); }
                    if let Some(cell) = render.upgrade() && let Some(f) = cell.borrow().clone() { f(); }
                });
                row.add_suffix(&remove);
                list.add(&row);
                rows.borrow_mut().push(row.upcast());
            }
        })
    };
    render.replace(Some(render_fn.clone()));

    {
        let (acl, render_fn, who) = (acl.clone(), render_fn.clone(), who.clone());
        let (kind, permission) = (kind.clone(), permission.clone());
        add.connect_activated(move |row| {
            let (kind_name, grantee) = match kind.selected() {
                0 => ("Group", ALL_USERS.to_string()),
                1 => ("Group", AUTHENTICATED_USERS.to_string()),
                2 => ("Group", LOG_DELIVERY.to_string()),
                3 => ("CanonicalUser", who.text().trim().to_string()),
                _ => ("AmazonCustomerByEmail", who.text().trim().to_string()),
            };
            if grantee.is_empty() { notify(row, &tr("Enter the canonical ID or e-mail address")); return; }
            let grant = AclGrant { kind: kind_name.into(), grantee, name: String::new(), permission: PERMISSIONS[permission.selected() as usize].into() };
            if let Some(a) = acl.borrow_mut().as_mut() && !a.grants.contains(&grant) { a.grants.push(grant); }
            who.set_text("");
            render_fn();
        });
    }
    {
        let (acl, load, render_fn) = (acl.clone(), load.clone(), render_fn.clone());
        apply.connect_activated(move |row| {
            let Some(current) = acl.borrow().clone() else { return };
            let (row, future, load, acl, render_fn) = (row.clone(), save(current), load.clone(), acl.clone(), render_fn.clone());
            row.set_sensitive(false);
            glib::spawn_future_local(async move {
                let result = bg(future).await;
                row.set_sensitive(true);
                if saved(&row, result) && let Ok(fresh) = bg(load()).await {
                    acl.replace(Some(fresh));
                    render_fn();
                }
            });
        });
    }
    {
        let (list, editing, render) = (list.clone(), editing.clone(), render.clone());
        glib::spawn_future_local(async move {
            match bg(load()).await {
                Ok(current) => {
                    acl.replace(Some(current));
                    editing.set_sensitive(true);
                    if let Some(f) = render.borrow().clone() { f(); }
                }
                Err(error) => { list.set_description(Some(&glib::markup_escape_text(&error))); editing.set_visible(false); }
            }
        });
    }
    page.add(&list);
    page.add(&editing);
}

/// Permission, Object Lock, hosting and CloudFront pages of the bucket settings.
pub fn add_bucket_pages(dialog: &adw::PreferencesDialog, client: S3, bucket: String) {
    // Permissions
    let page = adw::PreferencesPage::builder().title(tr("Permissions")).icon_name("system-users-symbolic").build();
    let block = adw::PreferencesGroup::builder().title(tr("Block Public Access")).build();
    let labels = [tr("Block new public ACLs"), tr("Ignore existing public ACLs"), tr("Block public bucket policies"), tr("Restrict access to buckets with public policies")];
    let switches: Vec<adw::SwitchRow> = labels.iter().map(|l| adw::SwitchRow::builder().title(l).sensitive(false).build()).collect();
    for s in &switches { block.add(s); }
    let block_save = adw::ButtonRow::builder().title(tr("Save")).sensitive(false).build();
    block.add(&block_save);
    page.add(&block);
    let (c, b) = (client.clone(), bucket.clone());
    add_acl(&page, Rc::new(move || { let (c, b) = (c.clone(), b.clone()); Box::pin(async move { c.bucket_acl(&b).await }) }),
        { let (c, b) = (client.clone(), bucket.clone()); Rc::new(move |acl| { let (c, b) = (c.clone(), b.clone()); Box::pin(async move { c.set_bucket_acl(&b, &acl).await }) }) });
    dialog.add(&page);

    // Object Lock
    let lock_page = adw::PreferencesPage::builder().title(tr("Object Lock")).icon_name("changes-prevent-symbolic").build();
    let lock = adw::PreferencesGroup::builder().title(tr("Object Lock"))
        .description(tr("Locked objects cannot be deleted or overwritten before their retention ends. Enabling Object Lock requires versioning and cannot be undone.")).build();
    let state = adw::ActionRow::builder().title(tr("State")).subtitle(tr("Loading…")).build();
    let mode = adw::ComboRow::builder().title(tr("Default retention")).model(&gtk::StringList::new(&[&tr("None"), "GOVERNANCE", "COMPLIANCE"])).build();
    let period = adw::SpinRow::builder().title(tr("Retention period")).adjustment(&gtk::Adjustment::new(30.0, 1.0, 36500.0, 1.0, 10.0, 0.0)).build();
    let unit = adw::ComboRow::builder().title(tr("Unit")).model(&gtk::StringList::new(&[&tr("Days"), &tr("Years")])).build();
    let acknowledge = adw::SwitchRow::builder().title(tr("I understand that Object Lock cannot be turned off for this bucket")).build();
    let lock_save = adw::ButtonRow::builder().title(tr("Save")).sensitive(false).build();
    for row in [state.upcast_ref::<gtk::Widget>(), mode.upcast_ref(), period.upcast_ref(), unit.upcast_ref(), acknowledge.upcast_ref(), lock_save.upcast_ref()] { lock.add(row); }
    mode.connect_selected_notify(glib::clone!(#[weak] period, #[weak] unit, move |m| { period.set_visible(m.selected() > 0); unit.set_visible(m.selected() > 0); }));
    period.set_visible(false);
    unit.set_visible(false);
    lock_page.add(&lock);
    dialog.add(&lock_page);

    // Hosting
    let hosting_page = adw::PreferencesPage::builder().title(tr("Hosting")).icon_name("web-browser-symbolic").build();
    let website = adw::PreferencesGroup::builder().title(tr("Static Website")).description(tr("With an index document the bucket is served as a static website; saving an empty one turns it off.")).build();
    let index = adw::EntryRow::builder().title(tr("Index document")).build();
    let error_doc = adw::EntryRow::builder().title(tr("Error document")).build();
    let website_save = adw::ButtonRow::builder().title(tr("Save")).build();
    website.add(&index);
    website.add(&error_doc);
    website.add(&website_save);
    let logging = adw::PreferencesGroup::builder().title(tr("Access Logging")).description(tr("Access logs are written to another bucket; saving an empty target turns logging off.")).build();
    let target = adw::EntryRow::builder().title(tr("Target bucket")).build();
    let prefix = adw::EntryRow::builder().title(tr("Prefix")).text("logs/").build();
    let logging_save = adw::ButtonRow::builder().title(tr("Save")).build();
    logging.add(&target);
    logging.add(&prefix);
    logging.add(&logging_save);
    let payment = adw::PreferencesGroup::new();
    let requester = adw::SwitchRow::builder().title(tr("Requester pays")).subtitle(tr("The requester pays for requests and data transfer instead of the bucket owner")).sensitive(false).build();
    payment.add(&requester);
    hosting_page.add(&website);
    hosting_page.add(&logging);
    hosting_page.add(&payment);
    dialog.add(&hosting_page);

    // Values arrive in the background; rows become editable once loaded.
    {
        let (client, bucket) = (client.clone(), bucket.clone());
        let (switches, block_save, state, mode, period, unit, acknowledge, lock_save) = (switches.clone(), block_save.clone(), state.clone(), mode.clone(), period.clone(), unit.clone(), acknowledge.clone(), lock_save.clone());
        let (index, error_doc, target, prefix, requester) = (index.clone(), error_doc.clone(), target.clone(), prefix.clone(), requester.clone());
        glib::spawn_future_local(async move {
            let (c, b) = (client.clone(), bucket.clone());
            let (hosting, lock) = bg(async move { Ok((c.hosting(&b).await, c.lock_config(&b).await)) }).await.unwrap_or_else(|e| (Default::default(), Err(e)));
            if let Some(flags) = hosting.public_block {
                for (s, f) in switches.iter().zip(flags) { s.set_active(f); s.set_sensitive(true); }
                block_save.set_sensitive(true);
            }
            index.set_text(&hosting.index);
            error_doc.set_text(&hosting.error);
            target.set_text(&hosting.logging_bucket);
            if !hosting.logging_prefix.is_empty() { prefix.set_text(&hosting.logging_prefix); }
            if let Some(pays) = hosting.requester_pays { requester.set_active(pays); requester.set_sensitive(true); }
            match lock {
                Ok(config) => {
                    state.set_subtitle(&if config.enabled { tr("Enabled") } else { tr("Off") });
                    acknowledge.set_visible(!config.enabled);
                    mode.set_selected(match config.mode.as_str() { "GOVERNANCE" => 1, "COMPLIANCE" => 2, _ => 0 });
                    if config.years > 0 { unit.set_selected(1); period.set_value(config.years as f64); } else if config.days > 0 { period.set_value(config.days as f64); }
                    lock_save.set_sensitive(true);
                }
                Err(error) => { state.set_subtitle(&glib::markup_escape_text(&error)); mode.set_sensitive(false); acknowledge.set_visible(false); }
            }
            // Saving each section.
            {
                let (client, bucket, switches) = (client.clone(), bucket.clone(), switches.clone());
                block_save.connect_activated(move |row| {
                    let flags = [switches[0].is_active(), switches[1].is_active(), switches[2].is_active(), switches[3].is_active()];
                    let (c, b, row) = (client.clone(), bucket.clone(), row.clone());
                    glib::spawn_future_local(async move { let r = bg(async move { c.set_public_block(&b, flags).await }).await; saved(&row, r); });
                });
            }
            {
                let (client, bucket) = (client.clone(), bucket.clone());
                lock_save.connect_activated(move |row| {
                    if acknowledge.is_visible() && !acknowledge.is_active() {
                        notify(row, &tr("Confirm that Object Lock cannot be turned off before enabling it"));
                        return;
                    }
                    let mode_name = ["", "GOVERNANCE", "COMPLIANCE"][mode.selected() as usize].to_string();
                    let value = period.value() as i32;
                    let (days, years) = if mode_name.is_empty() { (0, 0) } else if unit.selected() == 1 { (0, value) } else { (value, 0) };
                    let (c, b, row, state, acknowledge) = (client.clone(), bucket.clone(), row.clone(), state.clone(), acknowledge.clone());
                    glib::spawn_future_local(async move {
                        let r = bg(async move { c.set_lock_config(&b, &mode_name, days, years).await }).await;
                        if saved(&row, r) { state.set_subtitle(&tr("Enabled")); acknowledge.set_visible(false); }
                    });
                });
            }
            {
                let (client, bucket) = (client.clone(), bucket.clone());
                website_save.connect_activated(move |row| {
                    let (c, b, i, e, row) = (client.clone(), bucket.clone(), index.text().to_string(), error_doc.text().to_string(), row.clone());
                    glib::spawn_future_local(async move { let r = bg(async move { c.set_website(&b, &i, &e).await }).await; saved(&row, r); });
                });
            }
            {
                let (client, bucket) = (client.clone(), bucket.clone());
                logging_save.connect_activated(move |row| {
                    let (c, b, t, p, row) = (client.clone(), bucket.clone(), target.text().to_string(), prefix.text().to_string(), row.clone());
                    glib::spawn_future_local(async move { let r = bg(async move { c.set_logging(&b, &t, &p).await }).await; saved(&row, r); });
                });
            }
            requester.connect_active_notify(move |row| {
                let (c, b, on, row) = (client.clone(), bucket.clone(), row.is_active(), row.clone());
                glib::spawn_future_local(async move { let r = bg(async move { c.set_requester_pays(&b, on).await }).await; saved(&row, r); });
            });
        });
    }

    // CloudFront (Amazon S3 only)
    if client.profile.provider == "aws" {
        let page = adw::PreferencesPage::builder().title(tr("CloudFront")).icon_name("network-wireless-symbolic").build();
        let group = adw::PreferencesGroup::builder().title(tr("Distributions"))
            .description(tr("CloudFront distributions that use this bucket as their origin. To purge the cache, enter paths separated by spaces (for example /index.html /images/*).")).build();
        let paths = adw::EntryRow::builder().title(tr("Paths to invalidate")).text("/*").build();
        let paths_group = adw::PreferencesGroup::new();
        paths_group.add(&paths);
        page.add(&paths_group);
        page.add(&group);
        dialog.add(&page);
        glib::spawn_future_local(async move {
            let (c, b) = (client.clone(), bucket.clone());
            match bg(async move { c.distributions(&b).await }).await {
                Ok(list) if list.is_empty() => group.add(&adw::ActionRow::builder().title(tr("No distribution uses this bucket.")).build()),
                Ok(list) => for d in list {
                    let mut subtitle = format!("{} · {}", d.id, d.status);
                    if !d.enabled { subtitle.push_str(&format!(" · {}", tr("disabled"))); }
                    if !d.aliases.is_empty() { subtitle.push_str(&format!("\n{}", d.aliases.join(", "))); }
                    let row = adw::ActionRow::builder().title(&d.domain).subtitle(glib::markup_escape_text(&subtitle)).subtitle_lines(2).tooltip_text(&d.origin).build();
                    let button = gtk::Button::builder().label(tr("Invalidate")).valign(gtk::Align::Center).build();
                    let (client, paths, id) = (client.clone(), paths.clone(), d.id.clone());
                    button.connect_clicked(move |button| {
                        let list: Vec<String> = paths.text().split_whitespace().map(str::to_string).collect();
                        let (c, id, button) = (client.clone(), id.clone(), button.clone());
                        button.set_sensitive(false);
                        glib::spawn_future_local(async move {
                            let r = bg(async move { c.invalidate(&id, list).await }).await;
                            button.set_sensitive(true);
                            match r { Ok(reference) => notify(&button, &trf("Invalidation created ({id})", &[("id", &reference)])), Err(e) => notify(&button, &e) }
                        });
                    });
                    row.add_suffix(&button);
                    group.add(&row);
                },
                Err(error) => group.set_description(Some(&glib::markup_escape_text(&error))),
            }
        });
    }
}

/// Storage class, archive restore, public access, ACL and Object Lock of one object.
pub fn object_permissions(win: &Window, client: S3, bucket: String, info: ObjectInfo) {
    let name = info.key.rsplit('/').next().unwrap_or(&info.key).to_string();
    let dialog = adw::PreferencesDialog::builder().title(trf("Permissions of {name}", &[("name", &name)])).search_enabled(false).content_width(640).content_height(640).build();
    let page = adw::PreferencesPage::new();
    let key = info.key.clone();

    // Storage class and archive restore
    let storage = adw::PreferencesGroup::builder().title(tr("Storage")).build();
    let classes = ["STANDARD", "STANDARD_IA", "ONEZONE_IA", "INTELLIGENT_TIERING", "GLACIER_IR", "GLACIER", "DEEP_ARCHIVE", "REDUCED_REDUNDANCY"];
    let current = if info.storage_class.is_empty() { "STANDARD" } else { info.storage_class.as_str() };
    let class = adw::ComboRow::builder().title(tr("Storage class")).model(&gtk::StringList::new(&classes)).selected(classes.iter().position(|c| *c == current).unwrap_or(0) as u32).build();
    let class_apply = gtk::Button::builder().label(tr("Apply")).valign(gtk::Align::Center).build();
    class.add_suffix(&class_apply);
    storage.add(&class);
    if !info.encryption.is_empty() {
        storage.add(&adw::ActionRow::builder().title(tr("Server-side encryption")).subtitle(&info.encryption).css_classes(["property"]).build());
    }
    {
        let (client, bucket, key) = (client.clone(), bucket.clone(), key.clone());
        let win = win.clone();
        class_apply.connect_clicked(move |button| {
            let chosen = classes[class.selected() as usize].to_string();
            let (c, b, k, button, win) = (client.clone(), bucket.clone(), key.clone(), button.clone(), win.clone());
            glib::spawn_future_local(async move {
                let r = bg(async move { c.set_storage_class(&b, vec![k], &chosen).await }).await;
                if saved(&button, r) { win.refresh(); }
            });
        });
    }
    if matches!(current, "GLACIER" | "DEEP_ARCHIVE") {
        let restore = adw::ExpanderRow::builder().title(tr("Restore From Archive")).subtitle(tr("Archived objects need a temporary copy before they can be downloaded")).build();
        let days = adw::SpinRow::builder().title(tr("Days the copy stays available")).adjustment(&gtk::Adjustment::new(7.0, 1.0, 365.0, 1.0, 7.0, 0.0)).build();
        let tier = adw::ComboRow::builder().title(tr("Speed")).model(&gtk::StringList::new(&["Standard", "Bulk", "Expedited"])).build();
        let request = adw::ButtonRow::builder().title(tr("Request Restore")).build();
        restore.add_row(&days);
        restore.add_row(&tier);
        restore.add_row(&request);
        storage.add(&restore);
        let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
        glib::spawn_future_local(glib::clone!(#[weak] restore, async move {
            if let Ok(header) = bg(async move { c.restore_state(&b, &k).await }).await && !header.is_empty() {
                restore.set_subtitle(&if header.contains("ongoing-request=\"true\"") { tr("Restore in progress…") } else { tr("A restored copy is available") });
            }
        }));
        let (client, bucket, key) = (client.clone(), bucket.clone(), key.clone());
        request.connect_activated(move |row| {
            let (c, b, k, d, t, row) = (client.clone(), bucket.clone(), key.clone(), days.value() as i32, ["Standard", "Bulk", "Expedited"][tier.selected() as usize].to_string(), row.clone());
            glib::spawn_future_local(async move {
                match bg(async move { c.restore_archived(&b, &k, d, &t).await }).await {
                    Ok(()) => notify(&row, &tr("Restore requested")),
                    Err(e) => notify(&row, &e),
                }
            });
        });
    }
    page.add(&storage);

    // Public access: a shortcut for the "Everyone can read" grant.
    let access = adw::PreferencesGroup::builder().title(tr("Public Access")).build();
    let public = adw::SwitchRow::builder().title(tr("Anyone can read this object")).subtitle(tr("Adds or removes the public read grant")).sensitive(false).build();
    access.add(&public);
    page.add(&access);
    {
        let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
        let (client, bucket, key) = (client.clone(), bucket.clone(), key.clone());
        glib::spawn_future_local(glib::clone!(#[weak] public, async move {
            let Ok(acl) = bg(async move { c.object_acl(&b, &k).await }).await else { public.set_subtitle(&tr("This provider does not report object permissions")); return };
            public.set_active(acl.is_public());
            public.set_sensitive(true);
            public.connect_active_notify(move |row| {
                let (c, b, k, on, row) = (client.clone(), bucket.clone(), key.clone(), row.is_active(), row.clone());
                glib::spawn_future_local(async move {
                    let r = bg(async move {
                        let mut acl = c.object_acl(&b, &k).await?;
                        acl.grants.retain(|g| g.grantee != ALL_USERS);
                        if on { acl.grants.push(AclGrant { kind: "Group".into(), grantee: ALL_USERS.into(), name: String::new(), permission: "READ".into() }); }
                        c.set_object_acl(&b, &k, &acl).await
                    }).await;
                    saved(&row, r);
                });
            });
        }));
    }
    let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
    let (c2, b2, k2) = (client.clone(), bucket.clone(), key.clone());
    add_acl(&page,
        Rc::new(move || { let (c, b, k) = (c.clone(), b.clone(), k.clone()); Box::pin(async move { c.object_acl(&b, &k).await }) }),
        Rc::new(move |acl| { let (c, b, k) = (c2.clone(), b2.clone(), k2.clone()); Box::pin(async move { c.set_object_acl(&b, &k, &acl).await }) }));

    // Object Lock
    let lock = adw::PreferencesGroup::builder().title(tr("Object Lock")).build();
    let summary = adw::ActionRow::builder().title(tr("Retention")).subtitle(tr("Loading…")).build();
    let mode = adw::ComboRow::builder().title(tr("Mode")).model(&gtk::StringList::new(&["GOVERNANCE", "COMPLIANCE"])).build();
    let days = adw::SpinRow::builder().title(tr("Retain for days")).adjustment(&gtk::Adjustment::new(30.0, 1.0, 36500.0, 1.0, 10.0, 0.0)).build();
    let bypass = adw::SwitchRow::builder().title(tr("Bypass GOVERNANCE retention")).subtitle(tr("Needs the permission to shorten a retention")).build();
    let apply = adw::ButtonRow::builder().title(tr("Apply Retention")).build();
    let hold = adw::SwitchRow::builder().title(tr("Legal hold")).subtitle(tr("While on, the object cannot be deleted")).build();
    for row in [summary.upcast_ref::<gtk::Widget>(), mode.upcast_ref(), days.upcast_ref(), bypass.upcast_ref(), apply.upcast_ref(), hold.upcast_ref()] { lock.add(row); }
    lock.set_sensitive(false);
    page.add(&lock);
    {
        let (client, bucket, key) = (client.clone(), bucket.clone(), key.clone());
        glib::spawn_future_local(async move {
            let (c, b, k) = (client.clone(), bucket.clone(), key.clone());
            match bg(async move { c.retention(&b, &k).await }).await {
                Ok(r) => {
                    summary.set_subtitle(&if r.mode.is_empty() { tr("No retention lock") } else {
                        trf("{mode} until {date}", &[("mode", &r.mode), ("date", &crate::window::format_time(r.until))])
                    });
                    if r.mode == "COMPLIANCE" { mode.set_selected(1); }
                    hold.set_active(r.legal_hold);
                    lock.set_sensitive(true);
                    {
                        let (client, bucket, key) = (client.clone(), bucket.clone(), key.clone());
                        apply.connect_activated(move |row| {
                            let until = glib::real_time() / 1_000_000 + days.value() as i64 * 86_400;
                            let (c, b, k, m, by, row, summary) = (client.clone(), bucket.clone(), key.clone(), ["GOVERNANCE", "COMPLIANCE"][mode.selected() as usize].to_string(), bypass.is_active(), row.clone(), summary.clone());
                            glib::spawn_future_local(async move {
                                let mode_label = m.clone();
                                let r = bg(async move { c.set_retention(&b, &k, &m, until, by).await }).await;
                                if saved(&row, r) { summary.set_subtitle(&trf("{mode} until {date}", &[("mode", &mode_label), ("date", &crate::window::format_time(until))])); }
                            });
                        });
                    }
                    hold.connect_active_notify(move |row| {
                        let (c, b, k, on, row) = (client.clone(), bucket.clone(), key.clone(), row.is_active(), row.clone());
                        glib::spawn_future_local(async move { let r = bg(async move { c.set_legal_hold(&b, &k, on).await }).await; saved(&row, r); });
                    });
                }
                Err(error) => { summary.set_subtitle(&glib::markup_escape_text(&error)); lock.set_sensitive(true); for w in [mode.upcast_ref::<gtk::Widget>(), days.upcast_ref(), bypass.upcast_ref(), apply.upcast_ref(), hold.upcast_ref()] { w.set_visible(false); } }
            }
        });
    }
    dialog.add(&page);
    dialog.present(Some(win));
}
