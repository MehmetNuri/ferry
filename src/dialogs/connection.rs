//! The connection editor: an AdwDialog loaded from profile-dialog.ui.
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

use crate::i18n::tr;
use crate::profile::{PRESETS, Preset, Profile};
use crate::runtime::bg;
use crate::s3::S3;

/// FTP protection, in the order of the encryption choices.
const FTP_SECURITY: &[&str] = &["explicit", "implicit", "none"];

pub const STORAGE_CLASSES: &[&str] = &["STANDARD", "STANDARD_IA", "ONEZONE_IA", "INTELLIGENT_TIERING", "GLACIER_IR", "GLACIER", "DEEP_ARCHIVE"];
const ENCRYPTIONS: &[&str] = &["", "AES256", "aws:kms"];

fn preset_label(preset: &Preset) -> String {
    if preset.label.is_empty() { tr("Other S3-compatible") } else { preset.label.to_string() }
}

/// Shows the editor; `on_saved` receives the profile after it was stored.
pub fn present(parent: &impl IsA<gtk::Widget>, existing: Option<Profile>, on_saved: impl Fn(Profile, bool) + 'static) {
    let builder = gtk::Builder::from_resource("/io/github/mehmetnuri/Ferry/ui/profile-dialog.ui");
    let get = |id: &str| builder.object::<glib::Object>(id).unwrap_or_else(|| panic!("{id} missing"));
    let dialog: adw::Dialog = get("dialog").downcast().unwrap();
    let toasts: adw::ToastOverlay = get("toasts").downcast().unwrap();
    let name: adw::EntryRow = get("name_row").downcast().unwrap();
    let provider: adw::ComboRow = get("provider_row").downcast().unwrap();
    let supabase: adw::PreferencesGroup = get("supabase_group").downcast().unwrap();
    let project_ref: adw::EntryRow = get("project_ref_row").downcast().unwrap();
    let endpoint: adw::EntryRow = get("endpoint_row").downcast().unwrap();
    let region: adw::EntryRow = get("region_row").downcast().unwrap();
    let path_style: adw::SwitchRow = get("path_style_row").downcast().unwrap();
    let access_key: adw::EntryRow = get("access_key_row").downcast().unwrap();
    let secret_key: adw::PasswordEntryRow = get("secret_key_row").downcast().unwrap();
    let session_token: adw::PasswordEntryRow = get("session_token_row").downcast().unwrap();
    let buckets: adw::EntryRow = get("buckets_row").downcast().unwrap();
    let storage_class: adw::ComboRow = get("storage_class_row").downcast().unwrap();
    let encryption: adw::ComboRow = get("encryption_row").downcast().unwrap();
    let kms_key: adw::EntryRow = get("kms_key_row").downcast().unwrap();
    let test_row: adw::ButtonRow = get("test_row").downcast().unwrap();
    let test_result: adw::ActionRow = get("test_result").downcast().unwrap();
    let test_icon: gtk::Image = get("test_icon").downcast().unwrap();
    let save: gtk::Button = get("save_button").downcast().unwrap();
    let auth: adw::ComboRow = get("auth_row").downcast().unwrap();
    let aws_profile: adw::ComboRow = get("aws_profile_row").downcast().unwrap();
    let role: adw::ExpanderRow = get("role_row").downcast().unwrap();
    let role_arn: adw::EntryRow = get("role_arn_row").downcast().unwrap();
    let external_id: adw::EntryRow = get("external_id_row").downcast().unwrap();
    let mfa_serial: adw::EntryRow = get("mfa_serial_row").downcast().unwrap();
    let accelerate: adw::SwitchRow = get("accelerate_row").downcast().unwrap();
    let ca_row: adw::ActionRow = get("ca_row").downcast().unwrap();
    let ca_choose: gtk::Button = get("ca_choose").downcast().unwrap();
    let ca_clear: gtk::Button = get("ca_clear").downcast().unwrap();
    let trust: gtk::Button = get("trust_button").downcast().unwrap();
    let remote_path: adw::EntryRow = get("remote_path_row").downcast().unwrap();
    let ftp_security: adw::ComboRow = get("ftp_security_row").downcast().unwrap();
    let key_row: adw::ActionRow = get("key_row").downcast().unwrap();
    let key_choose: gtk::Button = get("key_choose").downcast().unwrap();
    let key_clear: gtk::Button = get("key_clear").downcast().unwrap();
    let jump: adw::EntryRow = get("jump_row").downcast().unwrap();
    let upload_group: adw::PreferencesGroup = get("upload_group").downcast().unwrap();
    let cancel: gtk::Button = get("cancel_button").downcast().unwrap();

    let labels: Vec<String> = PRESETS.iter().map(preset_label).collect();
    provider.set_model(Some(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())));
    let mut classes = vec![tr("Provider default")];
    classes.extend(STORAGE_CLASSES.iter().map(|c| c.to_string()));
    storage_class.set_model(Some(&gtk::StringList::new(&classes.iter().map(String::as_str).collect::<Vec<_>>())));
    let encryption_labels = [tr("Provider default"), "SSE-S3 (AES256)".to_string(), "SSE-KMS".to_string()];
    encryption.set_model(Some(&gtk::StringList::new(&encryption_labels.iter().map(String::as_str).collect::<Vec<_>>())));

    let original = existing.clone().unwrap_or_else(|| Profile { provider: "supabase".into(), path_style: true, ..Default::default() });
    let editing = existing.as_ref().is_some_and(|p| !p.id.is_empty());
    dialog.set_title(&if editing { tr("Edit Connection") } else { tr("New Connection") });
    name.set_text(&original.name);
    provider.set_selected(PRESETS.iter().position(|p| p.id == original.provider).unwrap_or(PRESETS.len() - 1) as u32);
    project_ref.set_text(&original.project_ref);
    endpoint.set_text(&original.endpoint);
    region.set_text(&original.region);
    path_style.set_active(original.path_style);
    access_key.set_text(&original.access_key);
    secret_key.set_text(&original.secret_key);
    session_token.set_text(&original.session_token);
    buckets.set_text(&original.buckets.join(", "));
    storage_class.set_selected(STORAGE_CLASSES.iter().position(|c| *c == original.storage_class).map(|i| i as u32 + 1).unwrap_or(0));
    encryption.set_selected(ENCRYPTIONS.iter().position(|e| *e == original.encryption).unwrap_or(0) as u32);
    kms_key.set_text(&original.kms_key);
    kms_key.set_visible(encryption.selected() == 2);
    supabase.set_visible(original.provider == "supabase");

    remote_path.set_text(&original.remote_path);
    jump.set_text(&original.jump_host);
    ftp_security.set_model(Some(&gtk::StringList::new(&[&tr("TLS (recommended)"), &tr("TLS on its own port (implicit, 990)"), &tr("None (password sent readably)")])));
    ftp_security.set_selected(FTP_SECURITY.iter().position(|m| *m == original.ftp_security).unwrap_or(0) as u32);
    let key_path = Rc::new(RefCell::new(original.private_key.clone()));
    // Approved server keys, changed when the user trusts an unknown server.
    let host_keys = Rc::new(RefCell::new((original.host_key.clone(), original.jump_host_key.clone())));
    let show_key = Rc::new({
        let (key_row, key_clear, key_path) = (key_row.clone(), key_clear.clone(), key_path.clone());
        move || {
            let path = key_path.borrow();
            key_clear.set_visible(!path.is_empty());
            key_row.set_subtitle(&if path.is_empty() { tr("Optional: the SSH agent and the keys in ~/.ssh are tried first") } else { glib::markup_escape_text(&path).to_string() });
        }
    });
    show_key();

    // Where the credentials come from: keys typed here, or a profile of the AWS CLI.
    auth.set_model(Some(&gtk::StringList::new(&[&tr("Access Keys"), &tr("AWS CLI Profile")])));
    let cli_profiles = crate::s3::connection::aws_profiles();
    aws_profile.set_model(Some(&gtk::StringList::new(&cli_profiles.iter().map(String::as_str).collect::<Vec<_>>())));
    if let Some(i) = cli_profiles.iter().position(|p| *p == original.aws_profile) { aws_profile.set_selected(i as u32); }
    auth.set_selected(if original.aws_profile.is_empty() { 0 } else { 1 });
    // Only offered when the AWS CLI is set up on this computer.
    auth.set_visible(!cli_profiles.is_empty() || !original.aws_profile.is_empty());
    let show_auth = {
        let (auth, aws_profile, access_key, secret_key, session_token) = (auth.clone(), aws_profile.clone(), access_key.clone(), secret_key.clone(), session_token.clone());
        move || {
            let cli = auth.selected() == 1;
            aws_profile.set_visible(cli);
            for row in [access_key.upcast_ref::<gtk::Widget>(), secret_key.upcast_ref(), session_token.upcast_ref()] { row.set_visible(!cli); }
        }
    };
    show_auth();
    auth.connect_selected_notify(move |_| show_auth());
    role.set_enable_expansion(!original.role_arn.is_empty());
    role_arn.set_text(&original.role_arn);
    external_id.set_text(&original.external_id);
    mfa_serial.set_text(&original.mfa_serial);
    accelerate.set_active(original.accelerate);
    accelerate.set_visible(original.provider == "aws");
    let ca_pem = Rc::new(RefCell::new(original.ca_certificate.clone()));
    let show_ca = Rc::new({
        let (ca_row, ca_clear, ca_pem) = (ca_row.clone(), ca_clear.clone(), ca_pem.clone());
        move || {
            let pem = ca_pem.borrow();
            let prints = crate::s3::connection::fingerprints(&pem);
            ca_clear.set_visible(!pem.trim().is_empty());
            ca_row.set_subtitle(&match prints.first() {
                None => tr("System certificates only"),
                Some(first) => format!("SHA-256 {first}"),
            });
        }
    });
    show_ca();

    // File servers and S3 services need different settings.
    let apply_kind = Rc::new({
        let (provider, region, path_style, upload_group, role, accelerate, ca_row, auth, buckets, session_token) =
            (provider.clone(), region.clone(), path_style.clone(), upload_group.clone(), role.clone(), accelerate.clone(), ca_row.clone(), auth.clone(), buckets.clone(), session_token.clone());
        let (remote_path, key_row, jump, ftp_security, access_key, secret_key, aws_profile) =
            (remote_path.clone(), key_row.clone(), jump.clone(), ftp_security.clone(), access_key.clone(), secret_key.clone(), aws_profile.clone());
        let has_cli = !cli_profiles.is_empty() || !original.aws_profile.is_empty();
        move || {
            let id = PRESETS[provider.selected() as usize].id;
            let remote = crate::remote::is_remote(id);
            for widget in [region.upcast_ref::<gtk::Widget>(), path_style.upcast_ref(), upload_group.upcast_ref(), role.upcast_ref(), buckets.upcast_ref(), session_token.upcast_ref()] {
                widget.set_visible(!remote);
            }
            auth.set_visible(!remote && has_cli);
            if remote { auth.set_selected(0); aws_profile.set_visible(false); access_key.set_visible(true); secret_key.set_visible(true); }
            accelerate.set_visible(id == "aws");
            ca_row.set_visible(id != "sftp");
            remote_path.set_visible(remote);
            key_row.set_visible(id == "sftp");
            jump.set_visible(id == "sftp");
            ftp_security.set_visible(id == "ftp");
            access_key.set_title(&if remote { tr("User name") } else { tr("Access key ID") });
            secret_key.set_title(&match id { "sftp" => tr("Password or key passphrase"), "ftp" | "webdav" | "nextcloud" => tr("Password"), _ => tr("Secret access key") });
        }
    });
    apply_kind();
    {
        let (key_path, show_key, dialog) = (key_path.clone(), show_key.clone(), dialog.clone());
        key_choose.connect_clicked(move |_| {
            let chooser = gtk::FileDialog::builder().title(tr("Choose Private Key")).modal(true).build();
            let ssh = glib::home_dir().join(".ssh");
            if ssh.is_dir() { chooser.set_initial_folder(Some(&gtk::gio::File::for_path(ssh))); }
            let (key_path, show_key, root) = (key_path.clone(), show_key.clone(), dialog.root().and_downcast::<gtk::Window>());
            glib::spawn_future_local(async move {
                let Ok(file) = chooser.open_future(root.as_ref()).await else { return };
                let Some(path) = file.path() else { return };
                key_path.replace(path.display().to_string());
                show_key();
            });
        });
    }
    {
        let (key_path, show_key) = (key_path.clone(), show_key.clone());
        key_clear.connect_clicked(move |_| { key_path.replace(String::new()); show_key(); });
    }

    // The endpoint follows the preset and region until the user types one.
    let auto_endpoint = Rc::new(RefCell::new(original.endpoint.is_empty()));
    let fill_endpoint = {
        let (provider, endpoint, region, auto_endpoint) = (provider.clone(), endpoint.clone(), region.clone(), auto_endpoint.clone());
        move || {
            let preset = &PRESETS[provider.selected() as usize];
            if !*auto_endpoint.borrow() || preset.endpoint.is_empty() {
                return;
            }
            let value = if preset.endpoint.contains("{region}") {
                if region.text().is_empty() { String::new() } else { preset.endpoint.replace("{region}", &region.text()) }
            } else {
                preset.endpoint.to_string()
            };
            endpoint.set_text(&value);
            *auto_endpoint.borrow_mut() = true;
        }
    };
    let update_hints = {
        let (provider, endpoint, region) = (provider.clone(), endpoint.clone(), region.clone());
        move || {
            let preset = &PRESETS[provider.selected() as usize];
            region.set_tooltip_text(Some(&format!("{} {}", tr("For example"), preset.region_hint)));
            endpoint.set_title(&match preset.id {
                "aws" => tr("Endpoint (optional)"),
                "supabase" => tr("Endpoint (derived from the project ref if empty)"),
                "sftp" | "ftp" => tr("Server (host or host:port)"),
                "webdav" => tr("Address (https://…)"),
                "nextcloud" => tr("Server address (https://…)"),
                _ => tr("Endpoint"),
            });
        }
    };
    update_hints();
    // A new connection started from a provider tile gets that provider's endpoint.
    if !editing { fill_endpoint(); }
    {
        let (supabase, path_style, region, fill_endpoint, update_hints, accelerate, apply_kind) = (supabase.clone(), path_style.clone(), region.clone(), fill_endpoint.clone(), update_hints.clone(), accelerate.clone(), apply_kind.clone());
        provider.connect_selected_notify(move |row| {
            let preset = &PRESETS[row.selected() as usize];
            supabase.set_visible(preset.id == "supabase");
            accelerate.set_visible(preset.id == "aws");
            path_style.set_active(preset.path_style);
            if !preset.region.is_empty() && region.text().is_empty() {
                region.set_text(preset.region);
            }
            fill_endpoint();
            update_hints();
            apply_kind();
        });
    }
    {
        let fill_endpoint = fill_endpoint.clone();
        region.connect_changed(move |_| fill_endpoint());
    }
    {
        let auto_endpoint = auto_endpoint.clone();
        endpoint.connect_changed(move |row| {
            if row.has_focus() || row.focus_child().is_some() {
                *auto_endpoint.borrow_mut() = row.text().is_empty();
            }
        });
    }
    {
        let kms_key = kms_key.clone();
        encryption.connect_selected_notify(move |row| kms_key.set_visible(row.selected() == 2));
    }

    let collect = Rc::new({
        let original = original.clone();
        let (name, provider, project_ref, endpoint, region, path_style) = (name.clone(), provider.clone(), project_ref.clone(), endpoint.clone(), region.clone(), path_style.clone());
        let (access_key, secret_key, session_token, buckets, storage_class, encryption, kms_key) =
            (access_key.clone(), secret_key.clone(), session_token.clone(), buckets.clone(), storage_class.clone(), encryption.clone(), kms_key.clone());
        let (auth, aws_profile, role, role_arn, external_id, mfa_serial, accelerate, ca_pem) =
            (auth.clone(), aws_profile.clone(), role.clone(), role_arn.clone(), external_id.clone(), mfa_serial.clone(), accelerate.clone(), ca_pem.clone());
        let (remote_path, key_path, host_keys, jump, ftp_security) = (remote_path.clone(), key_path.clone(), host_keys.clone(), jump.clone(), ftp_security.clone());
        let remote_kind = { let provider = provider.clone(); move || crate::remote::is_remote(PRESETS[provider.selected() as usize].id) };
        let sftp_kind = { let provider = provider.clone(); move || PRESETS[provider.selected() as usize].id == "sftp" };
        move || Profile {
            id: original.id.clone(),
            name: name.text().trim().to_string(),
            provider: PRESETS[provider.selected() as usize].id.to_string(),
            endpoint: endpoint.text().trim().to_string(),
            region: region.text().trim().to_string(),
            access_key: access_key.text().trim().to_string(),
            secret_key: secret_key.text().trim().to_string(),
            session_token: session_token.text().trim().to_string(),
            path_style: path_style.is_active(),
            project_ref: project_ref.text().trim().to_string(),
            buckets: buckets.text().split([',', ' ', '\n']).filter(|b| !b.is_empty()).map(str::to_string).collect(),
            storage_class: match storage_class.selected() { 0 => String::new(), i => STORAGE_CLASSES[i as usize - 1].to_string() },
            encryption: ENCRYPTIONS[encryption.selected() as usize].to_string(),
            kms_key: if encryption.selected() == 2 { kms_key.text().trim().to_string() } else { String::new() },
            aws_profile: if auth.selected() == 1 { aws_profile.selected_item().and_downcast::<gtk::StringObject>().map(|s| s.string().to_string()).unwrap_or_default() } else { String::new() },
            role_arn: if role.enables_expansion() { role_arn.text().trim().to_string() } else { String::new() },
            external_id: if role.enables_expansion() { external_id.text().trim().to_string() } else { String::new() },
            mfa_serial: if role.enables_expansion() { mfa_serial.text().trim().to_string() } else { String::new() },
            accelerate: accelerate.is_active() && PRESETS[provider.selected() as usize].id == "aws",
            ca_certificate: ca_pem.borrow().trim().to_string(),
            remote_path: if remote_kind() { remote_path.text().trim().to_string() } else { String::new() },
            private_key: if sftp_kind() { key_path.borrow().clone() } else { String::new() },
            host_key: if sftp_kind() { host_keys.borrow().0.clone() } else { String::new() },
            jump_host: if sftp_kind() { jump.text().trim().to_string() } else { String::new() },
            jump_host_key: if sftp_kind() && !jump.text().trim().is_empty() { host_keys.borrow().1.clone() } else { String::new() },
            ftp_security: if PRESETS[provider.selected() as usize].id == "ftp" { FTP_SECURITY[ftp_security.selected() as usize].to_string() } else { String::new() },
        }
    });

    {
        let save = save.clone();
        save.set_sensitive(!name.text().trim().is_empty());
        name.connect_changed(move |row| save.set_sensitive(!row.text().trim().is_empty()));
    }
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| { dialog.close(); });
    }
    {
        let (ca_pem, show_ca, toasts, dialog) = (ca_pem.clone(), show_ca.clone(), toasts.clone(), dialog.clone());
        ca_choose.connect_clicked(move |_| {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some(&tr("Certificates")));
            for suffix in ["pem", "crt", "cer"] { filter.add_suffix(suffix); }
            filter.add_mime_type("application/x-x509-ca-cert");
            let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let chooser = gtk::FileDialog::builder().title(tr("Choose Certificate")).filters(&filters).modal(true).build();
            let (ca_pem, show_ca, toasts, root) = (ca_pem.clone(), show_ca.clone(), toasts.clone(), dialog.root().and_downcast::<gtk::Window>());
            glib::spawn_future_local(async move {
                let Ok(file) = chooser.open_future(root.as_ref()).await else { return };
                let text = file.path().and_then(|p| std::fs::read(p).ok()).filter(|d| d.len() < 256 * 1024).and_then(|d| String::from_utf8(d).ok()).unwrap_or_default();
                if text.contains("PRIVATE KEY") || crate::s3::connection::fingerprints(&text).is_empty() {
                    toasts.add_toast(crate::window::plain_toast(&tr("The file is not a PEM certificate")));
                    return;
                }
                ca_pem.replace(text);
                show_ca();
            });
        });
    }
    {
        let (ca_pem, show_ca) = (ca_pem.clone(), show_ca.clone());
        ca_clear.connect_clicked(move |_| { ca_pem.replace(String::new()); show_ca(); });
    }
    {
        // The result stays next to the button, and a toast reports it wherever the page is scrolled.
        let (collect, test_result, test_icon, toasts) = (collect.clone(), test_result.clone(), test_icon.clone(), toasts.clone());
        let (dialog, trust, host_keys) = (dialog.clone(), trust.clone(), host_keys.clone());
        test_row.connect_activated(move |row| {
            let profile = collect();
            let (row, test_result, test_icon, toasts) = (row.clone(), test_result.clone(), test_icon.clone(), toasts.clone());
            row.set_sensitive(false);
            row.set_title(&tr("Testing…"));
            test_result.set_visible(false);
            let (dialog, trust, host_keys) = (dialog.clone(), trust.clone(), host_keys.clone());
            glib::spawn_future_local(async move {
                trust.set_visible(false);
                let mut profile = profile;
                let mut result = bg(S3::test(profile.clone())).await;
                // An SFTP server (or its jump host) seen for the first time: the user checks its key.
                for _ in 0..2 {
                    let Some(unknown) = result.as_ref().err().and_then(|e| crate::remote::sftp::UnknownHost::decode(e)) else { break };
                    if !trust_host_key(&dialog, &unknown).await { break; }
                    if unknown.jump { host_keys.borrow_mut().1 = unknown.fingerprint.clone(); profile.jump_host_key = unknown.fingerprint; }
                    else { host_keys.borrow_mut().0 = unknown.fingerprint.clone(); profile.host_key = unknown.fingerprint; }
                    result = bg(S3::test(profile.clone())).await;
                }
                if result.as_ref().err().is_some_and(|e| e == crate::s3::connection::MFA_REQUIRED) {
                    result = if unlock_mfa(&dialog, &profile).await { bg(S3::test(profile.clone())).await } else { Err(tr("An MFA code is needed to test this connection")) };
                }
                let certificate = result.as_ref().err().is_some_and(|e| crate::s3::connection::is_certificate_error(e)) && profile.endpoint.trim().starts_with("https://");
                trust.set_visible(certificate);
                row.set_sensitive(true);
                row.set_title(&tr("Test Connection"));
                let ok = result.is_ok();
                let (title, detail) = match result {
                    Ok(message) => (tr("Connection works"), message),
                    Err(error) => (tr("Connection failed"), error),
                };
                test_icon.set_icon_name(Some(if ok { "emblem-ok-symbolic" } else { "dialog-error-symbolic" }));
                test_icon.set_css_classes(if ok { &["success"] } else { &["error"] });
                test_result.set_title(&title);
                test_result.set_subtitle(&glib::markup_escape_text(&detail));
                test_result.set_visible(true);
                let toast = crate::window::plain_toast(&if ok { detail } else { title });
                toast.set_timeout(if ok { 3 } else { 5 });
                toasts.add_toast(toast);
            });
        });
    }
    {
        // A self-signed or private-CA server: the user compares the fingerprint, then trusts it.
        let (collect, ca_pem, show_ca, dialog, toasts, test_row) = (collect.clone(), ca_pem.clone(), show_ca.clone(), dialog.clone(), toasts.clone(), test_row.clone());
        trust.connect_clicked(move |button| {
            let endpoint = crate::s3::endpoint_of(&collect());
            let (ca_pem, show_ca, dialog, toasts, button, test_row) = (ca_pem.clone(), show_ca.clone(), dialog.clone(), toasts.clone(), button.clone(), test_row.clone());
            glib::spawn_future_local(async move {
                let fetched = bg(async move { crate::s3::connection::fetch_certificate(&endpoint).await }).await;
                let certificate = match fetched {
                    Ok(c) => c,
                    Err(error) => { toasts.add_toast(crate::window::plain_toast(&error)); return; }
                };
                let alert = adw::AlertDialog::new(Some(&tr("Trust This Certificate?")),
                    Some(&crate::i18n::trf("The system does not know the certificate of {host}. Trust it only if this fingerprint matches the one of your server, for example as shown by its administrator:", &[("host", &certificate.host)])));
                let print = gtk::Label::builder().label(&certificate.fingerprint).wrap(true).wrap_mode(gtk::pango::WrapMode::Char).selectable(true).css_classes(["monospace"]).build();
                alert.set_extra_child(Some(&print));
                alert.add_responses(&[("cancel", &tr("Cancel")), ("trust", &tr("_Trust"))]);
                alert.set_response_appearance("trust", adw::ResponseAppearance::Destructive);
                alert.set_close_response("cancel");
                if alert.choose_future(Some(&dialog)).await != "trust" { return; }
                let mut pem = ca_pem.borrow().clone();
                if !pem.contains(certificate.pem.trim()) { pem.push_str(&certificate.pem); }
                ca_pem.replace(pem);
                show_ca();
                button.set_visible(false);
                test_row.emit_activate();
            });
        });
    }
    let on_saved = Rc::new(on_saved);
    {
        let (dialog, collect, toasts) = (dialog.clone(), collect.clone(), toasts.clone());
        save.connect_clicked(move |button| {
            let profile = collect();
            let (dialog, toasts, button, on_saved) = (dialog.clone(), toasts.clone(), button.clone(), on_saved.clone());
            button.set_sensitive(false);
            glib::spawn_future_local(async move {
                // Plain HTTP to a host on the internet, or FTP without TLS, sends data and passwords readably.
                if crate::s3::S3::insecure_endpoint(&profile.endpoint) || profile.ftp_security == "none" {
                    let alert = adw::AlertDialog::new(Some(&tr("Unencrypted Connection?")),
                        Some(&tr("This endpoint uses http:// instead of https://. Files, session tokens and share links can be read or changed on the way. Use https:// unless the server is in a network you trust.")));
                    alert.add_responses(&[("cancel", &tr("Cancel")), ("save", &tr("Save Anyway"))]);
                    alert.set_response_appearance("save", adw::ResponseAppearance::Destructive);
                    alert.set_close_response("cancel");
                    if alert.choose_future(Some(&dialog)).await != "save" { button.set_sensitive(true); return; }
                }
                let mut result = bg(crate::profile::save(profile.clone(), false)).await;
                // Without a keyring the keys would be stored readable on disk: the user decides.
                if result.as_ref().err().is_some_and(|e| e == crate::profile::KEYRING_UNAVAILABLE) {
                    let alert = adw::AlertDialog::new(Some(&tr("Keyring Unavailable")),
                        Some(&tr("The system keyring could not be opened, so the secret key cannot be stored safely. It can be saved in a file only you can read, which is less safe. It moves to the keyring as soon as one works.")));
                    alert.add_responses(&[("cancel", &tr("Cancel")), ("save", &tr("Save in File"))]);
                    alert.set_response_appearance("save", adw::ResponseAppearance::Destructive);
                    alert.set_close_response("cancel");
                    if alert.choose_future(Some(&dialog)).await != "save" { button.set_sensitive(true); return; }
                    result = bg(crate::profile::save(profile, true)).await;
                }
                match result {
                    Ok((saved, in_keyring)) => { dialog.close(); on_saved(saved, in_keyring); }
                    Err(error) => { button.set_sensitive(true); toasts.add_toast(crate::window::plain_toast(&error)); }
                }
            });
        });
    }
    dialog.present(Some(parent));
}

/// Asks for the code of the connection's MFA device and starts a role session with it.
/// False when the user cancelled.
pub async fn unlock_mfa(parent: &impl IsA<gtk::Widget>, profile: &Profile) -> bool {
    loop {
        let alert = adw::AlertDialog::new(Some(&tr("Enter MFA Code")),
            Some(&crate::i18n::trf("“{name}” assumes a role that needs a code from your MFA device.", &[("name", &profile.name)])));
        let group = adw::PreferencesGroup::new();
        let entry = adw::EntryRow::builder().title(tr("Code")).input_purpose(gtk::InputPurpose::Digits).activates_default(true).build();
        group.add(&entry);
        alert.set_extra_child(Some(&group));
        alert.add_responses(&[("cancel", &tr("Cancel")), ("ok", &tr("Connect"))]);
        alert.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        alert.set_default_response(Some("ok"));
        alert.set_close_response("cancel");
        alert.set_response_enabled("ok", false);
        entry.connect_changed(glib::clone!(#[weak] alert, move |e| {
            let code = e.text();
            alert.set_response_enabled("ok", code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()));
        }));
        entry.grab_focus();
        if alert.choose_future(Some(parent)).await != "ok" { return false; }
        let (profile, code) = (profile.clone(), entry.text().to_string());
        let started = bg(async move {
            let profile = crate::profile::with_secrets(profile).await?;
            crate::s3::connection::start_mfa_session(&profile, &code).await
        }).await;
        match started {
            Ok(()) => return true,
            Err(error) => {
                let failed = adw::AlertDialog::new(Some(&tr("MFA Failed")), Some(&error));
                failed.add_response("ok", &tr("_Try Again"));
                failed.choose_future(Some(parent)).await;
            }
        }
    }
}

/// Shows the key of an SFTP server (or jump host) seen for the first time; true when
/// the user trusts it.
pub async fn trust_host_key(parent: &impl IsA<gtk::Widget>, unknown: &crate::remote::sftp::UnknownHost) -> bool {
    let place = if unknown.port == 22 { unknown.host.clone() } else { format!("{}:{}", unknown.host, unknown.port) };
    let body = if unknown.jump {
        crate::i18n::trf("Ferry has not connected to the jump host {host} before. Connect only if this key fingerprint is the server's, for example as its administrator or “ssh-keyscan” shows it:", &[("host", &place)])
    } else {
        crate::i18n::trf("Ferry has not connected to {host} before. Connect only if this key fingerprint is the server's, for example as its administrator or “ssh-keyscan” shows it:", &[("host", &place)])
    };
    let alert = adw::AlertDialog::new(Some(&tr("Unknown Server")), Some(&body));
    let print = gtk::Label::builder().label(&unknown.fingerprint).wrap(true).wrap_mode(gtk::pango::WrapMode::Char).selectable(true).css_classes(["monospace"]).build();
    alert.set_extra_child(Some(&print));
    alert.add_responses(&[("cancel", &tr("Cancel")), ("trust", &tr("_Trust and Connect"))]);
    alert.set_response_appearance("trust", adw::ResponseAppearance::Suggested);
    alert.set_close_response("cancel");
    alert.choose_future(Some(parent)).await == "trust"
}
