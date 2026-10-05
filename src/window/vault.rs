use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::i18n::{tr, trf};
use crate::runtime::bg;
use crate::s3::vault;
use crate::window::Window;

const OFF_IN_VAULT: &[&str] = &[
    "copy-link",
    "headers-selected",
    "storage-class-selected",
    "upload-link",
    "deleted-objects",
    "object-versions",
    "object-headers",
    "object-tags",
    "object-permissions",
    "copy-cli",
];

const MIN_PASSWORD: usize = 8;

impl Window {
    pub(crate) fn in_vault(&self) -> bool {
        self.imp().vault_here.borrow().as_ref().is_some_and(|(_, unlocked)| *unlocked)
    }

    pub(crate) fn update_vault_state(&self) {
        let imp = self.imp();
        let (bucket, prefix) = (imp.bucket.borrow().clone(), imp.prefix.borrow().clone());
        let Some(profile) = imp.client.borrow().as_ref().map(|c| c.profile.id.clone()) else {
            imp.vault_here.replace(None);
            imp.vault_banner.set_revealed(false);
            return;
        };
        let state = if let Some(root) = vault::root_of(&profile, &bucket, &prefix) {
            Some((root, true))
        } else {
            let names: Vec<String> = imp
                .store
                .borrow()
                .as_ref()
                .map(|s| {
                    (0..s.n_items())
                        .filter_map(|i| s.item(i))
                        .map(|o| super::entry_of(&o))
                        .filter(|e| !e.is_folder)
                        .map(|e| e.name)
                        .collect()
                })
                .unwrap_or_default();
            let is_vault =
                names.iter().any(|n| n == vault::CONFIG_FILE) && names.iter().any(|n| n == vault::MASTERKEY_FILE);
            (is_vault && imp.search.borrow().is_none()).then(|| (prefix.clone(), false))
        };
        match &state {
            Some((_, true)) => {
                imp.vault_banner.set_title(&tr(
                    "Cryptomator vault, unlocked. Names and files are decrypted on this computer only.",
                ));
                imp.vault_banner.set_button_label(Some(&tr("_Lock")));
                imp.vault_banner.set_revealed(true);
            }
            Some((_, false)) => {
                imp.vault_banner.set_title(&tr("This folder is a Cryptomator vault"));
                imp.vault_banner.set_button_label(Some(&tr("_Unlock…")));
                imp.vault_banner.set_revealed(true);
            }
            None => imp.vault_banner.set_revealed(false),
        }
        imp.vault_here.replace(state);
        if self.in_vault() {
            for name in OFF_IN_VAULT {
                self.set_action_enabled(name, false);
            }
        }
    }

    pub(crate) fn setup_vault_banner(&self) {
        self.imp().vault_banner.connect_button_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                let Some((root, unlocked)) = win.imp().vault_here.borrow().clone() else { return };
                if unlocked { win.lock_vault(root) } else { win.unlock_vault(root) }
            }
        ));
    }

    fn forget_vault_listings(&self, root: &str) {
        let profile = self.imp().client.borrow().as_ref().map(|c| c.profile.id.clone()).unwrap_or_default();
        let bucket = self.imp().bucket.borrow().clone();
        let start = format!("{profile}\u{0}{bucket}\u{0}{root}");
        self.imp().listing_cache.borrow_mut().retain(|k, _| !k.starts_with(&start));
        self.imp().thumbnails.borrow_mut().clear();
    }

    fn lock_vault(&self, root: String) {
        let imp = self.imp();
        let Some(profile) = imp.client.borrow().as_ref().map(|c| c.profile.id.clone()) else { return };
        let bucket = imp.bucket.borrow().clone();
        self.forget_vault_listings(&root);
        vault::lock(&profile, &bucket, &root);
        self.go_to(&bucket, &root);
        self.toast(&tr("Vault locked"));
    }

    async fn ask_vault_password(
        &self,
        heading: &str,
        body: &str,
        confirm: bool,
        error: Option<&str>,
    ) -> Option<String> {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        let group = adw::PreferencesGroup::new();
        if let Some(error) = error {
            group.set_description(Some(&glib::markup_escape_text(error)));
        }
        let first = adw::PasswordEntryRow::builder().title(tr("Password")).activates_default(!confirm).build();
        let second = adw::PasswordEntryRow::builder()
            .title(tr("Repeat Password"))
            .activates_default(true)
            .visible(confirm)
            .build();
        group.add(&first);
        group.add(&second);
        dialog.set_extra_child(Some(&group));
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("ok", &if confirm { tr("Create") } else { tr("Unlock") })]);
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("cancel");
        dialog.set_response_enabled("ok", false);
        let check = glib::clone!(
            #[weak]
            dialog,
            #[weak]
            first,
            #[weak]
            second,
            move || {
                let (a, b) = (first.text(), second.text());
                let short = confirm && !a.is_empty() && a.chars().count() < MIN_PASSWORD;
                if short {
                    first.add_css_class("warning");
                } else {
                    first.remove_css_class("warning");
                }
                if confirm && !b.is_empty() && a != b {
                    second.add_css_class("error");
                } else {
                    second.remove_css_class("error");
                }
                dialog.set_response_enabled("ok", !a.is_empty() && !short && (!confirm || a == b));
            }
        );
        let c = check.clone();
        first.connect_changed(move |_| c());
        second.connect_changed(move |_| check());
        first.grab_focus();
        (dialog.choose_future(Some(self)).await == "ok").then(|| first.text().to_string())
    }

    fn unlock_vault(&self, root: String) {
        let Some(client) = self.client() else { return };
        let bucket = self.imp().bucket.borrow().clone();
        let win = self.clone();
        let name =
            root.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(&bucket).to_string();
        glib::spawn_future_local(async move {
            let mut error: Option<String> = None;
            loop {
                let Some(password) = win
                    .ask_vault_password(
                        &tr("Unlock Vault"),
                        &trf("Enter the password of the vault “{name}”.", &[("name", &name)]),
                        false,
                        error.as_deref(),
                    )
                    .await
                else {
                    return;
                };
                let (c, b, r) = (client.clone(), bucket.clone(), root.clone());
                win.imp().vault_banner.set_sensitive(false);
                let result = bg(async move { vault::unlock(&c, &b, &r, password).await }).await;
                win.imp().vault_banner.set_sensitive(true);
                match result {
                    Ok(()) => break,
                    Err(e) => error = Some(e),
                }
            }
            win.forget_vault_listings(&root);
            win.go_to(&bucket, &root);
            win.toast(&tr("Vault unlocked"));
        });
    }

    pub(crate) fn new_vault(&self) {
        let Some(client) = self.client() else { return };
        if self.in_vault() {
            self.toast(&tr("A vault cannot be made inside another vault"));
            return;
        }
        let (bucket, prefix) = (self.imp().bucket.borrow().clone(), self.imp().prefix.borrow().clone());
        let win = self.clone();
        glib::spawn_future_local(async move {
            let Some(name) = win.ask_text(&tr("New Vault"), &tr("A Cryptomator vault encrypts names and contents on this computer, before they are uploaded. The Cryptomator apps open it too."), &tr("Vault"), &tr("Next"), false).await else { return };
            let name = name.trim().trim_matches('/').to_string();
            if name.is_empty() || name.contains('/') {
                win.toast(&tr("A vault name cannot contain “/”"));
                return;
            }
            let Some(password) = win.ask_vault_password(&tr("Vault Password"),
                &tr("Use at least 8 characters. Nobody can open the vault without this password, and it cannot be recovered."), true, None).await else { return };
            let root = format!("{prefix}{name}/");
            let (c, b, r) = (client.clone(), bucket.clone(), root.clone());
            let result = bg(async move {
                vault::create(&c, &b, &r, password.clone()).await?;
                vault::unlock(&c, &b, &r, password).await
            })
            .await;
            match result {
                Ok(()) => {
                    win.go_to(&bucket, &root);
                    win.toast(&tr("Vault created"));
                }
                Err(error) => win.toast(&error),
            }
        });
    }
}
