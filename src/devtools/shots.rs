// FERRY_SCREENSHOTS=<dir> FERRY_DEMO=<dir>; use separate XDG dirs.
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use std::time::Duration;

use super::smoke::{dialog_shot, names, shot, until};
use crate::window::Window;

pub fn start(win: &Window) {
    let win = win.clone();
    glib::spawn_future_local(async move {
        win.set_can_target(false);
        glib::timeout_future(Duration::from_millis(1200)).await;
        shot(&win, "01-welcome").await;
        let Some(mut home) = crate::profile::load().into_iter().find(|p| p.provider == "sftp") else {
            std::process::exit(2)
        };
        let probe = home.clone();
        let first = crate::runtime::bg(async move { crate::s3::S3::connect(probe).await.map(|_| ()) }).await;
        if let Some(unknown) = first.err().and_then(|e| crate::remote::sftp::UnknownHost::decode(&e)) {
            home.host_key = unknown.fingerprint;
        }
        let vault_setup = home.clone();
        let vault_ready = crate::runtime::bg(async move {
            let client = crate::s3::S3::connect(vault_setup).await?;
            let root = "Private Vault/";
            if client.head_object("Home Server", &format!("{root}vault.cryptomator")).await.is_err() {
                crate::s3::vault::create(&client, "Home Server", root, "demo vault password".into()).await?;
            }
            crate::s3::vault::unlock(&client, "Home Server", root, "demo vault password".into()).await?;
            for (name, data) in [
                ("Passport scan.pdf", vec![1u8; 840_000]),
                ("Tax return 2025.pdf", vec![2u8; 1_250_000]),
                ("Recovery codes.txt", b"demo".to_vec()),
            ] {
                let _ = client
                    .create_object("Home Server", &format!("{root}{name}"), data, "application/octet-stream")
                    .await;
            }
            let _ = client.create_folder("Home Server", &format!("{root}Contracts/")).await;
            Ok::<(), String>(())
        })
        .await;
        win.connect(home);
        until(20, || names(&win).iter().any(|n| n == "Photos")).await;
        shot(&win, "02-server").await;

        win.navigate("Documents/");
        until(10, || names(&win).iter().any(|n| n.starts_with("Annual"))).await;
        shot(&win, "03-documents").await;

        win.navigate("Photos/");
        until(10, || names(&win).iter().any(|n| n == "Mountain Lake.png")).await;
        win.imp().grid_toggle.set_active(true);
        glib::timeout_future(Duration::from_millis(4000)).await;
        shot(&win, "04-photos-grid").await;

        let selection = win.imp().selection.borrow().clone().unwrap();
        let index = names(&win).iter().position(|n| n == "Mountain Lake.png").unwrap_or(0) as u32;
        selection.select_item(index, true);
        win.preview_selection();
        dialog_shot(&win, "05-preview", 2500).await;
        win.imp().grid_toggle.set_active(false);
        win.imp().list_toggle.set_active(true);

        if let Err(error) = &vault_ready {
            eprintln!("vault: {error}");
        }
        win.navigate("Private Vault/");
        until(10, || !names(&win).is_empty()).await;
        if !win.in_vault()
            && let Some(client) = win.current_client()
        {
            let unlocked = crate::runtime::bg(async move {
                crate::s3::vault::unlock(&client, "Home Server", "Private Vault/", "demo vault password".into()).await
            })
            .await;
            if let Err(error) = unlocked {
                eprintln!("vault unlock: {error}");
            }
            win.navigate("");
            until(10, || names(&win).iter().any(|n| n == "Photos")).await;
            win.navigate("Private Vault/");
        }
        until(10, || names(&win).iter().any(|n| n == "Contracts")).await;
        shot(&win, "06-vault").await;

        win.navigate("Backups/");
        until(10, || !names(&win).is_empty()).await;
        let home = glib::home_dir();
        let downloads = home.join("Downloads");
        let _ = std::fs::create_dir_all(&downloads);
        crate::settings::settings().set_boolean("ask-download-folder", false).ok();
        crate::settings::settings().set_string("download-folder", &downloads.display().to_string()).ok();
        if let Ok(dir) = std::env::var("FERRY_DEMO") {
            // Leftovers from an earlier run would trigger the replace prompt.
            for name in ["phone-2026-10-03.tar.zst", "Notes export.zip"] {
                let _ = std::fs::remove_file(std::path::Path::new(&dir).join("data/Backups").join(name));
            }
            win.refresh();
            glib::timeout_future(Duration::from_millis(800)).await;
            let uploads = std::path::PathBuf::from(dir).join("upload");
            win.upload(vec![uploads.join("phone-2026-10-03.tar.zst"), uploads.join("Notes export.zip")]);
            until(30, || win.queue().summary().done >= 2).await;
        }
        crate::s3::BANDWIDTH.store(3 * 1024 * 1024, std::sync::atomic::Ordering::Relaxed);
        let selection = win.imp().selection.borrow().clone().unwrap();
        if let Some(index) = names(&win).iter().position(|n| n.starts_with("laptop")) {
            selection.select_item(index as u32, true);
        }
        let _ = gtk::prelude::WidgetExt::activate_action(&win, "win.download-selected", None);
        glib::timeout_future(Duration::from_millis(4000)).await;
        win.imp().transfers_sheet.set_open(true);
        glib::timeout_future(Duration::from_millis(1500)).await;
        shot(&win, "07-transfers").await;
        win.imp().transfers_sheet.set_open(false);
        crate::s3::BANDWIDTH.store(0, std::sync::atomic::Ordering::Relaxed);

        let editor = crate::profile::Profile {
            name: "Office Files".into(),
            provider: "sftp".into(),
            endpoint: "files.example.org".into(),
            access_key: "ada".into(),
            remote_path: "/srv/projects".into(),
            jump_host: "ops@bastion.example.org".into(),
            ..Default::default()
        };
        win.edit_profile(Some(editor));
        dialog_shot(&win, "08-connection", 800).await;
        let _ = gtk::prelude::WidgetExt::activate_action(&win, "win.export-profiles", None);
        dialog_shot(&win, "09-export", 800).await;
        std::process::exit(0);
    });
}
