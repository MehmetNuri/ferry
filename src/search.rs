//! GNOME Shell search provider. Objects seen while browsing are kept in a small
//! local index, so the Activities search answers instantly and works offline;
//! activating a result opens the object in the window.
use gtk::{gio, glib};
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;

use crate::config;

pub const OBJECT_PATH: &str = "/io/github/mehmetnuri/Ferry/SearchProvider";
const INDEX_LIMIT: usize = 30_000;
const SEP: char = '\u{1f}';

const INTERFACE: &str = r#"
<node>
  <interface name="org.gnome.Shell.SearchProvider2">
    <method name="GetInitialResultSet"><arg type="as" name="terms" direction="in"/><arg type="as" name="results" direction="out"/></method>
    <method name="GetSubsearchResultSet"><arg type="as" name="previous_results" direction="in"/><arg type="as" name="terms" direction="in"/><arg type="as" name="results" direction="out"/></method>
    <method name="GetResultMetas"><arg type="as" name="identifiers" direction="in"/><arg type="aa{sv}" name="metas" direction="out"/></method>
    <method name="ActivateResult"><arg type="s" name="identifier" direction="in"/><arg type="as" name="terms" direction="in"/><arg type="u" name="timestamp" direction="in"/></method>
    <method name="LaunchSearch"><arg type="as" name="terms" direction="in"/><arg type="u" name="timestamp" direction="in"/></method>
  </interface>
</node>"#;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Item {
    profile: String,
    profile_name: String,
    bucket: String,
    key: String,
    folder: bool,
    seen: i64,
}

thread_local! {
    static INDEX: RefCell<Option<HashMap<String, Item>>> = const { RefCell::new(None) };
    static SAVE_PENDING: RefCell<bool> = const { RefCell::new(false) };
}

fn index_path() -> std::path::PathBuf {
    let dir = glib::user_cache_dir().join("ferry");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("search-index.json")
}

fn with_index<R>(f: impl FnOnce(&mut HashMap<String, Item>) -> R) -> R {
    INDEX.with(|cell| {
        let mut cell = cell.borrow_mut();
        let index = cell.get_or_insert_with(|| {
            std::fs::read(index_path()).ok()
                .and_then(|d| serde_json::from_slice::<Vec<Item>>(&d).ok())
                .map(|items| items.into_iter().map(|i| (id_of(&i.profile, &i.bucket, &i.key), i)).collect())
                .unwrap_or_default()
        });
        f(index)
    })
}

fn id_of(profile: &str, bucket: &str, key: &str) -> String {
    format!("{profile}{SEP}{bucket}{SEP}{key}")
}

/// Records objects of a listing; called whenever a folder was listed. With
/// `complete_folder`, the listing is the whole content of that folder and
/// entries of it that are gone (deleted or moved elsewhere) are forgotten.
pub fn record(profile: &str, profile_name: &str, bucket: &str, keys: impl Iterator<Item = (String, bool)>, complete_folder: Option<&str>) {
    // Like Recent, the index follows GNOME Settings › Privacy › File History.
    if !crate::pages::recent::remembering() { return; }
    let now = glib::real_time() / 1_000_000;
    // Names inside an unlocked vault are secret: they are never indexed.
    let keys: Vec<(String, bool)> = keys.filter(|(k, _)| crate::s3::vault::find(profile, bucket, k).is_none()).collect();
    if complete_folder.is_some_and(|f| crate::s3::vault::find(profile, bucket, f).is_some()) { return; }
    with_index(|index| {
        if let Some(folder) = complete_folder {
            let present: std::collections::HashSet<&str> = keys.iter().map(|(k, _)| k.as_str()).collect();
            let is_child = |item: &Item| item.profile == profile && item.bucket == bucket && item.key.starts_with(folder)
                && item.key.len() > folder.len() && !item.key[folder.len()..].trim_end_matches('/').contains('/');
            // A vanished folder takes everything recorded below it along.
            let gone: Vec<String> = index.values().filter(|i| is_child(i) && i.folder && !present.contains(i.key.as_str())).map(|i| i.key.clone()).collect();
            index.retain(|_, item| {
                let below_gone = item.profile == profile && item.bucket == bucket && gone.iter().any(|g| item.key.starts_with(g.as_str()));
                !below_gone && (!is_child(item) || present.contains(item.key.as_str()))
            });
        }
        for (key, folder) in keys.into_iter() {
            index.insert(id_of(profile, bucket, &key), Item { profile: profile.into(), profile_name: profile_name.into(), bucket: bucket.into(), key, folder, seen: now });
        }
        // The oldest entries leave when the index grows too large.
        if index.len() > INDEX_LIMIT {
            let mut items: Vec<(String, i64)> = index.iter().map(|(k, v)| (k.clone(), v.seen)).collect();
            items.sort_by_key(|(_, seen)| *seen);
            for (k, _) in items.into_iter().take(index.len() - INDEX_LIMIT) { index.remove(&k); }
        }
    });
    schedule_save();
}

/// Forgets every object of a connection, for example when it is deleted.
pub fn forget_profile(profile: &str) {
    with_index(|index| index.retain(|_, item| item.profile != profile));
    schedule_save();
}

fn schedule_save() {
    if SAVE_PENDING.with(|p| p.replace(true)) { return; }
    glib::timeout_add_seconds_local_once(5, save);
}

/// Writes the index now; also called when the application quits.
pub fn save() {
    SAVE_PENDING.with(|p| p.replace(false));
    if INDEX.with(|cell| cell.borrow().is_none()) { return; }
    let items: Vec<Item> = with_index(|index| index.values().cloned().collect());
    if let Ok(data) = serde_json::to_vec(&items) {
        let _ = crate::profile::write_private(&index_path(), &data);
    }
}

fn name_of(item: &Item) -> String {
    item.key.trim_end_matches('/').rsplit('/').next().unwrap_or(&item.key).to_string()
}

/// Every term must appear in the object's name, or in its bucket and path.
fn search(terms: &[String], within: Option<&[String]>) -> Vec<String> {
    let terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).filter(|t| !t.is_empty()).collect();
    if terms.is_empty() { return Vec::new(); }
    with_index(|index| {
        let mut hits: Vec<(i32, i64, String)> = Vec::new();
        let candidates: Box<dyn Iterator<Item = (&String, &Item)>> = match within {
            Some(ids) => Box::new(ids.iter().filter_map(|id| index.get_key_value(id))),
            None => Box::new(index.iter()),
        };
        for (id, item) in candidates {
            let name = name_of(item).to_lowercase();
            let path = format!("{}/{}", item.bucket, item.key).to_lowercase();
            if !terms.iter().all(|t| path.contains(t)) { continue; }
            // Names that start with the term rank first, then names containing it.
            let score = if terms.iter().all(|t| name.starts_with(t)) { 0 } else if terms.iter().all(|t| name.contains(t)) { 1 } else { 2 };
            hits.push((score, -item.seen, id.clone()));
        }
        hits.sort();
        hits.into_iter().take(50).map(|(_, _, id)| id).collect()
    })
}

/// A found object for the in-application search of every connection.
pub struct Found {
    pub profile: String,
    pub profile_name: String,
    pub bucket: String,
    pub key: String,
    pub folder: bool,
}

/// Searches the objects seen in any connection, best matches first.
pub fn find(text: &str) -> Vec<Found> {
    let terms: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    let ids = search(&terms, None);
    with_index(|index| ids.iter().filter_map(|id| index.get(id)).map(|i| Found {
        profile: i.profile.clone(), profile_name: i.profile_name.clone(), bucket: i.bucket.clone(), key: i.key.clone(), folder: i.folder,
    }).collect())
}

fn metas(ids: &[String]) -> Vec<HashMap<String, glib::Variant>> {
    with_index(|index| ids.iter().filter_map(|id| index.get(id).map(|item| {
        let icon: gio::Icon = if item.folder {
            gio::ThemedIcon::new("folder").upcast()
        } else {
            let (guess, _) = gio::content_type_guess(Some(name_of(item).as_str()), None::<&[u8]>);
            gio::content_type_get_icon(&guess)
        };
        let folder = &item.key[..item.key.trim_end_matches('/').rfind('/').map(|i| i + 1).unwrap_or(0)];
        let mut meta = HashMap::new();
        meta.insert("id".to_string(), id.to_variant());
        meta.insert("name".to_string(), name_of(item).to_variant());
        meta.insert("description".to_string(), format!("{} · {}/{}", item.profile_name, item.bucket, folder).to_variant());
        meta.insert("gicon".to_string(), icon.to_string().unwrap_or_default().to_variant());
        meta
    })).collect())
}

/// Exports the provider on the application's bus connection.
pub fn register(connection: &gio::DBusConnection) {
    let Ok(node) = gio::DBusNodeInfo::for_xml(INTERFACE) else { return };
    let Some(interface) = node.lookup_interface("org.gnome.Shell.SearchProvider2") else { return };
    let result = connection.register_object(OBJECT_PATH, &interface)
        .method_call(|_, _, _, _, method, params, invocation| {
            let strings = |v: Option<glib::Variant>| v.and_then(|v| v.get::<Vec<String>>()).unwrap_or_default();
            match method {
                "GetInitialResultSet" => {
                    let terms = strings(params.try_child_value(0));
                    invocation.return_value(Some(&(search(&terms, None),).to_variant()));
                }
                "GetSubsearchResultSet" => {
                    let previous = strings(params.try_child_value(0));
                    let terms = strings(params.try_child_value(1));
                    invocation.return_value(Some(&(search(&terms, Some(&previous)),).to_variant()));
                }
                "GetResultMetas" => {
                    let ids = strings(params.try_child_value(0));
                    invocation.return_value(Some(&(metas(&ids),).to_variant()));
                }
                "ActivateResult" => {
                    let id = params.try_child_value(0).and_then(|v| v.get::<String>()).unwrap_or_default();
                    invocation.return_value(None);
                    activate(&id);
                }
                "LaunchSearch" => {
                    let terms = strings(params.try_child_value(0));
                    invocation.return_value(None);
                    launch(&terms.join(" "));
                }
                _ => invocation.return_error(gio::DBusError::UnknownMethod, method),
            }
        })
        .build();
    if let Err(error) = result {
        eprintln!("Search provider could not be registered: {error}");
    }
}

fn window() -> Option<crate::window::Window> {
    let app = gio::Application::default()?.downcast::<gtk::Application>().ok()?;
    app.activate();
    app.windows().into_iter().find_map(|w| w.downcast::<crate::window::Window>().ok())
}

fn activate(id: &str) {
    crate::debug!("search result activated");
    let mut parts = id.splitn(3, SEP);
    let (Some(profile), Some(bucket), Some(key)) = (parts.next(), parts.next(), parts.next()) else { crate::debug!("malformed result id {id:?}"); return };
    let found = window();
    crate::debug!("result {profile} {bucket} {key}, window found: {}", found.is_some());
    if let Some(win) = found {
        win.open_object(profile.to_string(), bucket.to_string(), key.to_string());
    }
}

fn launch(text: &str) {
    if let Some(win) = window() {
        win.start_search(text);
    }
}

#[allow(dead_code)]
pub fn app_id() -> &'static str {
    config::APP_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_and_filters() {
        INDEX.with(|cell| cell.replace(Some(HashMap::new())));
        SAVE_PENDING.with(|p| p.replace(true));
        record("p", "Main", "photos", vec![("2024/beach.jpg".to_string(), false), ("2024/".to_string(), true), ("docs/beach-report.pdf".to_string(), false), ("misc/other.txt".to_string(), false)].into_iter(), None);
        let hits = search(&["beach".into()], None);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].ends_with("2024/beach.jpg"));
        let narrowed = search(&["beach".into(), "pdf".into()], Some(&hits));
        assert_eq!(narrowed.len(), 1);
        let meta = &metas(&narrowed)[0];
        assert_eq!(meta["name"].get::<String>().unwrap(), "beach-report.pdf");
        assert_eq!(meta["description"].get::<String>().unwrap(), "Main · photos/docs/");
        assert!(search(&["photos".into(), "2024".into()], None).len() >= 2);
        // A complete listing of "2024/" without beach.jpg forgets it.
        record("p", "Main", "photos", std::iter::empty(), Some("2024/"));
        assert_eq!(search(&["beach".into()], None).len(), 1);
    }
}
