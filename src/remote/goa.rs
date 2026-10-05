use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib;

use crate::i18n::tr;
use crate::s3::Res;

const BUS_NAME: &str = "org.gnome.OnlineAccounts";
const ROOT: &str = "/org/gnome/OnlineAccounts";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub identity: String,
    pub object_path: String,
}

fn bus() -> Res<gio::DBusConnection> {
    gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).map_err(|e| e.to_string())
}

fn not_running() -> String {
    tr("GNOME Online Accounts is not available. Add the account in Settings › Online Accounts.")
}

pub fn accounts(provider: &str) -> Res<Vec<Account>> {
    let reply = bus()?
        .call_sync(
            Some(BUS_NAME),
            ROOT,
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            None,
            Some(glib::VariantTy::new("(a{oa{sa{sv}}})").expect("valid type")),
            gio::DBusCallFlags::NONE,
            5000,
            gio::Cancellable::NONE,
        )
        .map_err(|_| not_running())?;
    let objects = reply.child_value(0);
    let mut found = Vec::new();
    for i in 0..objects.n_children() {
        let entry = objects.child_value(i);
        let path = entry.child_value(0).str().unwrap_or("").to_string();
        let interfaces = entry.child_value(1);
        let Some(account) = (0..interfaces.n_children())
            .map(|j| interfaces.child_value(j))
            .find(|iface| iface.child_value(0).str() == Some("org.gnome.OnlineAccounts.Account"))
        else {
            continue;
        };
        let properties = glib::VariantDict::new(Some(&account.child_value(1)));
        let property = |name: &str| -> Option<glib::Variant> { properties.lookup_value(name, None) };
        let text = |name: &str| property(name).and_then(|v| v.str().map(str::to_string)).unwrap_or_default();
        let flag = |name: &str| property(name).and_then(|v| v.get::<bool>()).unwrap_or(false);
        if text("ProviderType") != provider || flag("FilesDisabled") || flag("AttentionNeeded") {
            continue;
        }
        found.push(Account { id: text("Id"), identity: text("PresentationIdentity"), object_path: path });
    }
    found.sort_by(|a, b| a.identity.cmp(&b.identity));
    Ok(found)
}

pub fn access_token(account_id: &str) -> Res<(String, i64)> {
    let path = format!("{ROOT}/Accounts/{account_id}");
    let reply = bus()?
        .call_sync(
            Some(BUS_NAME),
            &path,
            "org.gnome.OnlineAccounts.OAuth2Based",
            "GetAccessToken",
            None,
            Some(glib::VariantTy::new("(si)").expect("valid type")),
            gio::DBusCallFlags::NONE,
            30_000,
            gio::Cancellable::NONE,
        )
        .map_err(|e| {
            let text = e.to_string();
            if text.contains("UnknownObject") || text.contains("No such") {
                tr("The online account was removed from Settings")
            } else {
                format!(
                    "{} ({text})",
                    tr("GNOME Online Accounts could not sign in. Check the account in Settings › Online Accounts.")
                )
            }
        })?;
    let token = reply.child_value(0).str().unwrap_or("").to_string();
    let expires = reply.child_value(1).get::<i32>().unwrap_or(0) as i64;
    if token.is_empty() {
        return Err(not_running());
    }
    Ok((token, expires))
}

pub fn open_settings() {
    let _ = gio::AppInfo::create_from_commandline(
        "gnome-control-center online-accounts",
        None,
        gio::AppInfoCreateFlags::NONE,
    )
    .and_then(|app| app.launch(&[], gio::AppLaunchContext::NONE));
}

#[cfg(test)]
mod tests {
    #[test]
    fn google_token() {
        if std::env::var_os("FERRY_GOA_TEST").is_none() {
            return;
        }
        let accounts = super::accounts("google").unwrap();
        println!("google accounts: {}", accounts.len());
        let (token, expires) = super::access_token(&accounts[0].id).unwrap();
        println!("token of {} characters, valid for {expires} s", token.len());
        assert!(token.len() > 20);
    }
}
