//! Cryptomator vaults in a bucket. An unlocked vault is registered for its connection,
//! bucket and folder; every operation on keys below that folder then goes through it, so
//! the window, transfers, mounts and the command line all see the decrypted names and
//! contents. Keys are the cleartext paths (vault folder + path inside the vault); the
//! vault maps them to the encrypted objects (d/XX/YYY…/name.c9r).
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use cryptomator_vault::{NodeName, Vault};

use super::{Entry, Listing, ObjectInfo, Progress, Res, S3};
use crate::i18n::{tr, trf};

/// The files that make a folder a vault.
pub const CONFIG_FILE: &str = "vault.cryptomator";
pub const MASTERKEY_FILE: &str = "masterkey.cryptomator";

pub struct Unlocked {
    profile_id: String,
    bucket: String,
    /// The vault folder's key, ending in "/" (empty for a vault at the bucket's root).
    root: String,
    vault: Vault,
    /// Directory IDs of cleartext folders ("" is the vault's root), as they are found.
    dirs: Mutex<HashMap<String, String>>,
}

static UNLOCKED: Mutex<Vec<Arc<Unlocked>>> = Mutex::new(Vec::new());

fn registry() -> std::sync::MutexGuard<'static, Vec<Arc<Unlocked>>> {
    UNLOCKED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The unlocked vault a key belongs to, with the key's path inside the vault.
pub fn find(profile_id: &str, bucket: &str, key: &str) -> Option<(Arc<Unlocked>, String)> {
    registry().iter()
        .filter(|v| v.profile_id == profile_id && v.bucket == bucket && key.starts_with(&v.root))
        // A vault inside another vault's folder is not possible, but the longest root wins anyway.
        .max_by_key(|v| v.root.len())
        .map(|v| (v.clone(), key[v.root.len()..].to_string()))
}

/// Locks a vault: its keys are forgotten and its folder shows the encrypted files again.
pub fn lock(profile_id: &str, bucket: &str, root: &str) {
    registry().retain(|v| !(v.profile_id == profile_id && v.bucket == bucket && v.root == root));
}

/// Locks every vault of a connection, for example when it is edited or removed.
pub fn lock_all(profile_id: &str) {
    registry().retain(|v| v.profile_id != profile_id);
}

/// The vault root of a key's folder chain that is unlocked, for the interface.
pub fn root_of(profile_id: &str, bucket: &str, key: &str) -> Option<String> {
    find(profile_id, bucket, key).map(|(v, _)| v.root.clone())
}

fn split_parent(rel: &str) -> (&str, &str) {
    let trimmed = rel.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", trimmed),
    }
}

fn crypto(error: cryptomator_vault::Error) -> String {
    match error {
        cryptomator_vault::Error::InvalidPassword => tr("Wrong password"),
        cryptomator_vault::Error::Authentication(_) => tr("The encrypted data is damaged or was changed"),
        other => other.to_string(),
    }
}

fn inside_only() -> String {
    tr("This is not available inside a Cryptomator vault")
}

/// Unlocks the vault in a folder with its password and registers it.
pub async fn unlock(raw: &S3, bucket: &str, root: &str, password: String) -> Res<()> {
    let masterkey = raw.read_bytes_plain(bucket, &format!("{root}{MASTERKEY_FILE}"), 64 * 1024).await?;
    let config = raw.read_bytes_plain(bucket, &format!("{root}{CONFIG_FILE}"), 64 * 1024).await?;
    let config = String::from_utf8(config).map_err(|_| tr("The vault configuration is damaged"))?;
    // scrypt takes a moment and a lot of memory on purpose: off the async threads.
    let password = zeroize::Zeroizing::new(password);
    let vault = tokio::task::spawn_blocking(move || Vault::unlock(&masterkey, config.trim(), &password))
        .await.map_err(|e| e.to_string())?.map_err(crypto)?;
    let unlocked = Arc::new(Unlocked {
        profile_id: raw.profile.id.clone(), bucket: bucket.to_string(), root: root.to_string(), vault, dirs: Mutex::new(HashMap::new()),
    });
    // The root directory has to be readable, or the password fits a different vault.
    unlocked.dir_id(raw, "").await?;
    lock(&raw.profile.id, bucket, root);
    registry().push(unlocked);
    Ok(())
}

/// Creates a new, empty vault in a folder (it must not be a vault yet).
pub async fn create(raw: &S3, bucket: &str, root: &str, password: String) -> Res<()> {
    if raw.head_object_plain(bucket, &format!("{root}{CONFIG_FILE}")).await.is_ok() {
        return Err(tr("This folder is a vault already"));
    }
    let password = zeroize::Zeroizing::new(password);
    let created = tokio::task::spawn_blocking(move || Vault::create(&password)).await.map_err(|e| e.to_string())?.map_err(crypto)?;
    // Object stores have no folders: the root directory's ID backup marks it, as Cryptomator writes it.
    raw.create_object_plain(bucket, &format!("{root}{}/dirid.c9r", created.root_dir_path), created.root_dir_id_backup.clone(), "application/octet-stream").await?;
    raw.create_object_plain(bucket, &format!("{root}{MASTERKEY_FILE}"), created.masterkey_json.clone().into_bytes(), "application/json").await?;
    raw.create_object_plain(bucket, &format!("{root}{CONFIG_FILE}"), created.vault_config.clone().into_bytes(), "application/jwt").await?;
    Ok(())
}

impl Unlocked {
    /// The raw key prefix of a directory's contents.
    fn content_prefix(&self, dir_id: &str) -> Res<String> {
        Ok(format!("{}{}/", self.root, self.vault.dir_path(dir_id).map_err(crypto)?))
    }

    /// The directory ID of a cleartext folder path ("" for the root, no trailing "/").
    async fn dir_id(&self, raw: &S3, rel_dir: &str) -> Res<String> {
        let rel_dir = rel_dir.trim_end_matches('/');
        if rel_dir.is_empty() {
            return Ok(String::new());
        }
        if let Some(id) = self.dirs.lock().unwrap_or_else(PoisonError::into_inner).get(rel_dir) {
            return Ok(id.clone());
        }
        // Resolved one level at a time from the deepest known parent.
        let mut id = String::new();
        let mut path = String::new();
        for part in rel_dir.split('/') {
            let next = if path.is_empty() { part.to_string() } else { format!("{path}/{part}") };
            let known = self.dirs.lock().unwrap_or_else(PoisonError::into_inner).get(&next).cloned();
            id = match known {
                Some(found) => found,
                None => {
                    let node = self.vault.node_name(part, &id).map_err(crypto)?;
                    let key = format!("{}{}", self.content_prefix(&id)?, node.dir_file_path());
                    let bytes = raw.read_bytes_plain(&self.bucket, &key, 64).await
                        .map_err(|_| trf("The folder “{name}” was not found in the vault", &[("name", part)]))?;
                    let found = self.vault.parse_dir_file(&bytes).map_err(crypto)?;
                    self.dirs.lock().unwrap_or_else(PoisonError::into_inner).insert(next.clone(), found.clone());
                    found
                }
            };
            path = next;
        }
        Ok(id)
    }

    /// Makes sure a cleartext folder and its parents exist, as `mkdir -p` does, and
    /// returns its ID. Uploading a folder writes into folders that are new.
    async fn ensure_dir(&self, raw: &S3, rel_dir: &str) -> Res<String> {
        let rel_dir = rel_dir.trim_end_matches('/');
        if let Ok(id) = self.dir_id(raw, rel_dir).await {
            return Ok(id);
        }
        let mut path = String::new();
        let mut id = String::new();
        for part in rel_dir.split('/') {
            let next = if path.is_empty() { part.to_string() } else { format!("{path}/{part}") };
            id = match self.dir_id(raw, &next).await {
                Ok(found) => found,
                Err(_) => self.make_dir(raw, &id, &next, part).await?,
            };
            path = next;
        }
        Ok(id)
    }

    /// Creates one folder in the directory `parent_id`.
    async fn make_dir(&self, raw: &S3, parent_id: &str, rel_dir: &str, name: &str) -> Res<String> {
        let base = self.content_prefix(parent_id)?;
        let node = self.vault.node_name(name, parent_id).map_err(crypto)?;
        let id = Vault::new_dir_id().map_err(crypto)?;
        // The new directory's own folder, marked by the backup of its ID.
        let backup = self.vault.encrypt_dir_id_backup(&id).map_err(crypto)?;
        raw.put_bytes_plain(&self.bucket, &format!("{}dirid.c9r", self.content_prefix(&id)?), backup).await?;
        if let NodeName::Shortened { full_name, .. } = &node
            && let Some(p) = node.name_file_path() {
            raw.put_bytes_plain(&self.bucket, &format!("{base}{p}"), full_name.clone().into_bytes()).await?;
        }
        raw.put_bytes_plain(&self.bucket, &format!("{base}{}", node.dir_file_path()), id.clone().into_bytes()).await?;
        self.dirs.lock().unwrap_or_else(PoisonError::into_inner).insert(rel_dir.to_string(), id.clone());
        Ok(id)
    }

    /// The raw keys of a file to be written; missing parent folders are created.
    async fn file_keys_for_write(&self, raw: &S3, rel: &str) -> Res<(String, Option<(String, Vec<u8>)>)> {
        let (parent, _) = split_parent(rel);
        self.ensure_dir(raw, parent).await?;
        self.file_keys(raw, rel).await
    }

    /// The raw keys of a file: its contents, and the name file of a shortened name.
    async fn file_keys(&self, raw: &S3, rel: &str) -> Res<(String, Option<(String, Vec<u8>)>)> {
        let (parent, name) = split_parent(rel);
        let id = self.dir_id(raw, parent).await?;
        let base = self.content_prefix(&id)?;
        let node = self.vault.node_name(name, &id).map_err(crypto)?;
        let name_file = match &node {
            NodeName::Shortened { full_name, .. } => node.name_file_path().map(|p| (format!("{base}{p}"), full_name.clone().into_bytes())),
            NodeName::Regular(_) => None,
        };
        Ok((format!("{base}{}", node.file_contents_path()), name_file))
    }

    /// The raw key or prefix of a folder's entry in its parent (dir.c9r, or the .c9s folder).
    async fn dir_node(&self, raw: &S3, rel_dir: &str) -> Res<(String, String, Option<(String, Vec<u8>)>)> {
        let (parent, name) = split_parent(rel_dir);
        let id = self.dir_id(raw, parent).await?;
        let base = self.content_prefix(&id)?;
        let node = self.vault.node_name(name, &id).map_err(crypto)?;
        let name_file = match &node {
            NodeName::Shortened { full_name, .. } => node.name_file_path().map(|p| (format!("{base}{p}"), full_name.clone().into_bytes())),
            NodeName::Regular(_) => None,
        };
        // The whole entry folder (name.c9r/ or hash.c9s/) goes when the folder is removed.
        Ok((format!("{base}{}", node.dir_file_path()), format!("{base}{}/", node.storage_name()), name_file))
    }

    pub async fn list_objects(&self, raw: &S3, rel_prefix: &str, token: &str) -> Res<Listing> {
        let id = self.dir_id(raw, rel_prefix).await?;
        let prefix = self.content_prefix(&id)?;
        let page = raw.list_objects_plain(&self.bucket, &prefix, token).await?;
        let clear_prefix = format!("{}{rel_prefix}", self.root);
        let mut listing = Listing { items: Vec::new(), next_token: page.next_token };
        for item in page.items {
            let raw_name = item.name.trim_end_matches('/');
            let entry = if raw_name.ends_with(".c9s") {
                // A shortened name: the full encrypted name and the kind are inside.
                let full = raw.read_bytes_plain(&self.bucket, &format!("{prefix}{raw_name}/name.c9s"), 64 * 1024).await.ok()
                    .and_then(|b| self.vault.decrypt_shortened_name(raw_name, &b, &id).ok());
                let Some(name) = full else { continue };
                match raw.head_object_plain(&self.bucket, &format!("{prefix}{raw_name}/contents.c9r")).await {
                    Ok(head) => Entry { key: format!("{clear_prefix}{name}"), name, size: self.vault.cleartext_size(head.size.max(0) as u64).unwrap_or(0) as i64,
                        modified: head.modified, etag: head.etag, storage_class: head.storage_class, is_folder: false },
                    Err(_) => Entry { key: format!("{clear_prefix}{name}/"), name, is_folder: true, ..Default::default() },
                }
            } else if raw_name.ends_with(".c9r") {
                let Ok(name) = self.vault.decrypt_name(raw_name, &id) else { continue };
                if item.is_folder {
                    Entry { key: format!("{clear_prefix}{name}/"), name, is_folder: true, ..Default::default() }
                } else {
                    Entry { key: format!("{clear_prefix}{name}"), name, size: self.vault.cleartext_size(item.size.max(0) as u64).unwrap_or(0) as i64,
                        modified: item.modified, etag: item.etag, storage_class: item.storage_class, is_folder: false }
                }
            } else {
                // dirid.c9r and anything that is not part of the vault format.
                continue;
            };
            listing.items.push(entry);
        }
        Ok(listing)
    }

    /// Every file below a cleartext folder, walking the directory tree.
    pub async fn list_all(&self, raw: &S3, rel_prefix: &str, limit: usize) -> Res<(Vec<Entry>, bool)> {
        let mut out = Vec::new();
        let mut folders = vec![rel_prefix.to_string()];
        while let Some(folder) = folders.pop() {
            let mut token = String::new();
            loop {
                let page = self.list_objects(raw, &folder, &token).await?;
                for entry in page.items {
                    if entry.is_folder {
                        folders.push(entry.key[self.root.len()..].to_string());
                        // Folders show up as markers, as with plain S3 listings.
                        out.push(Entry { size: 0, ..entry });
                    } else {
                        out.push(entry);
                    }
                    if out.len() >= limit { return Ok((out, true)); }
                }
                if page.next_token.is_empty() { break; }
                token = page.next_token;
            }
        }
        Ok((out, false))
    }

    pub async fn head_object(&self, raw: &S3, rel: &str) -> Res<ObjectInfo> {
        let (key, _) = self.file_keys(raw, rel).await?;
        let mut info = raw.head_object_plain(&self.bucket, &key).await?;
        info.size = self.vault.cleartext_size(info.size.max(0) as u64).map_err(crypto)? as i64;
        info.key = format!("{}{rel}", self.root);
        info.content_type = super::content_type_of(Path::new(rel));
        Ok(info)
    }

    pub async fn read_bytes(&self, raw: &S3, rel: &str, limit: u64) -> Res<Vec<u8>> {
        let (key, _) = self.file_keys(raw, rel).await?;
        let size = self.vault.cleartext_size(raw.head_object_plain(&self.bucket, &key).await?.size.max(0) as u64).map_err(crypto)?;
        let wanted = limit.min(size);
        if wanted == 0 {
            return Ok(Vec::new());
        }
        let plan = self.vault.range_plan(0, wanted);
        let data = raw.read_bytes_plain(&self.bucket, &key, plan.ciphertext_end).await?;
        let header = self.vault.decrypt_header(&data[..self.vault.header_len().min(data.len())]).map_err(crypto)?;
        let start = (plan.ciphertext_start as usize).min(data.len());
        self.vault.decrypt_range(&header, &plan, &data[start..]).map_err(crypto)
    }

    pub async fn download_file(&self, raw: &S3, rel: &str, path: &Path, progress: &Progress) -> Res<()> {
        let (key, _) = self.file_keys(raw, rel).await?;
        let sealed = path.with_extension("ferry-vault-part");
        raw.download_file_plain(&self.bucket, &key, None, &sealed, progress).await?;
        let (vault, target) = (self.vault.clone(), path.to_path_buf());
        let result = tokio::task::spawn_blocking(move || -> Res<()> {
            let input = std::io::BufReader::new(std::fs::File::open(&sealed).map_err(|e| e.to_string())?);
            let mut reader = vault.decrypting_reader(input);
            let temporary = target.with_extension("ferry-vault-new");
            let mut output = std::fs::File::create(&temporary).map_err(|e| e.to_string())?;
            let copied = std::io::copy(&mut reader, &mut output).map_err(|_| tr("The encrypted data is damaged or was changed"));
            let _ = std::fs::remove_file(&sealed);
            if let Err(error) = copied {
                let _ = std::fs::remove_file(&temporary);
                return Err(error);
            }
            output.flush().map_err(|e| e.to_string())?;
            std::fs::rename(&temporary, &target).map_err(|e| e.to_string())
        }).await.map_err(|e| e.to_string())?;
        result
    }

    /// Encrypts bytes or a file into a temporary file for uploading.
    fn seal_file(&self, source: &Path) -> Res<tempfile_path::TempPath> {
        let target = tempfile_path::TempPath::new()?;
        let mut input = std::io::BufReader::new(std::fs::File::open(source).map_err(|e| e.to_string())?);
        let output = std::io::BufWriter::new(std::fs::File::create(target.path()).map_err(|e| e.to_string())?);
        let mut writer = self.vault.encrypting_writer(output).map_err(crypto)?;
        let mut buffer = vec![0u8; 256 * 1024];
        loop {
            let n = input.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 { break; }
            writer.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        }
        writer.finish().map_err(|e| e.to_string())?.flush().map_err(|e| e.to_string())?;
        Ok(target)
    }

    pub async fn upload_file(self: &Arc<Self>, raw: &S3, rel: &str, path: &Path, progress: &Progress) -> Res<()> {
        let (key, name_file) = self.file_keys_for_write(raw, rel).await?;
        let (me, source) = (self.clone(), path.to_path_buf());
        let sealed = tokio::task::spawn_blocking(move || me.seal_file(&source)).await.map_err(|e| e.to_string())??;
        raw.upload_file_plain(&self.bucket, &key, sealed.path(), progress).await?;
        if let Some((name_key, full)) = name_file {
            raw.put_bytes_plain(&self.bucket, &name_key, full).await?;
        }
        Ok(())
    }

    pub async fn put_bytes(&self, raw: &S3, rel: &str, data: Vec<u8>, replace: bool) -> Res<()> {
        let (key, name_file) = self.file_keys_for_write(raw, rel).await?;
        if !replace && raw.head_object_plain(&self.bucket, &key).await.is_ok() {
            return Err(trf("“{name}” already exists", &[("name", split_parent(rel).1)]));
        }
        let sealed = self.vault.encrypt_file(&data).map_err(crypto)?;
        raw.put_bytes_plain(&self.bucket, &key, sealed).await?;
        if let Some((name_key, full)) = name_file {
            raw.put_bytes_plain(&self.bucket, &name_key, full).await?;
        }
        Ok(())
    }

    /// Creates a folder with its missing parents; an existing folder is fine, as with
    /// folder markers in plain buckets.
    pub async fn create_folder(&self, raw: &S3, rel_dir: &str) -> Res<()> {
        self.ensure_dir(raw, rel_dir).await.map(|_| ())
    }

    /// Deletes files and folders (keys ending in "/") with everything in them.
    pub async fn delete(&self, raw: &S3, rels: Vec<String>) -> Res<usize> {
        let mut count = 0;
        for rel in rels {
            if rel.ends_with('/') || rel.is_empty() {
                count += self.delete_folder(raw, rel.trim_end_matches('/')).await?;
            } else {
                let (key, name_file) = self.file_keys(raw, &rel).await?;
                match name_file {
                    // The whole hash.c9s/ folder holds the contents and the name.
                    Some((name_key, _)) => { raw.delete_keys_plain(&self.bucket, vec![name_key.trim_end_matches("name.c9s").to_string()]).await?; }
                    None => { raw.delete_keys_plain(&self.bucket, vec![key]).await?; }
                }
                count += 1;
            }
        }
        Ok(count)
    }

    async fn delete_folder(&self, raw: &S3, rel_dir: &str) -> Res<usize> {
        if rel_dir.is_empty() {
            return Err(tr("The vault itself cannot be emptied this way; delete its folder instead"));
        }
        // Every directory below, deepest first, then their entries.
        let mut dirs = vec![rel_dir.to_string()];
        let mut index = 0;
        let mut files = 0;
        while index < dirs.len() {
            let folder = format!("{}/", dirs[index]);
            let (entries, _) = self.list_all_one(raw, &folder).await?;
            for entry in entries {
                if entry.is_folder { dirs.push(entry.key[self.root.len()..].trim_end_matches('/').to_string()); } else { files += 1; }
            }
            index += 1;
        }
        for dir in dirs.iter().rev() {
            let id = self.dir_id(raw, dir).await?;
            raw.delete_keys_plain(&self.bucket, vec![self.content_prefix(&id)?]).await?;
            let (_, entry_prefix, _) = self.dir_node(raw, dir).await?;
            raw.delete_keys_plain(&self.bucket, vec![entry_prefix]).await?;
            let prefix = format!("{dir}/");
            self.dirs.lock().unwrap_or_else(PoisonError::into_inner).retain(|k, _| k != dir && !k.starts_with(&prefix));
        }
        Ok(files + dirs.len())
    }

    /// One folder's entries, all pages.
    async fn list_all_one(&self, raw: &S3, rel_prefix: &str) -> Res<(Vec<Entry>, bool)> {
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let page = self.list_objects(raw, rel_prefix, &token).await?;
            out.extend(page.items);
            if page.next_token.is_empty() { return Ok((out, false)); }
            token = page.next_token;
        }
    }

    /// Renames or moves a file or folder inside the vault. Folder contents stay where
    /// they are: only the folder's entry (its ID) moves.
    pub async fn rename(&self, raw: &S3, from: &str, to: &str) -> Res<()> {
        if from.ends_with('/') {
            let (from_file, from_entry, _) = self.dir_node(raw, from.trim_end_matches('/')).await?;
            self.ensure_dir(raw, split_parent(to).0).await?;
            let (to_file, _, to_name) = self.dir_node(raw, to.trim_end_matches('/')).await?;
            if raw.head_object_plain(&self.bucket, &to_file).await.is_ok() {
                return Err(trf("“{name}” already exists", &[("name", split_parent(to).1)]));
            }
            let id = raw.read_bytes_plain(&self.bucket, &from_file, 64).await?;
            if let Some((name_key, full)) = to_name { raw.put_bytes_plain(&self.bucket, &name_key, full).await?; }
            raw.put_bytes_plain(&self.bucket, &to_file, id).await?;
            raw.delete_keys_plain(&self.bucket, vec![from_entry]).await?;
            let old = from.trim_end_matches('/').to_string();
            let prefix = format!("{old}/");
            self.dirs.lock().unwrap_or_else(PoisonError::into_inner).retain(|k, _| *k != old && !k.starts_with(&prefix));
            return Ok(());
        }
        self.copy_file(raw, from, to).await?;
        self.delete(raw, vec![from.to_string()]).await.map(|_| ())
    }

    /// Copies a file inside the vault. The contents do not depend on the name, so the
    /// encrypted object is copied on the server as it is.
    pub async fn copy_file(&self, raw: &S3, from: &str, to: &str) -> Res<()> {
        let (from_key, _) = self.file_keys(raw, from).await?;
        let (to_key, name_file) = self.file_keys_for_write(raw, to).await?;
        raw.copy_object_plain(&self.bucket, &from_key, &self.bucket, &to_key).await?;
        if let Some((name_key, full)) = name_file { raw.put_bytes_plain(&self.bucket, &name_key, full).await?; }
        Ok(())
    }
}

/// The error for operations that make no sense on encrypted objects.
pub fn unsupported() -> String {
    inside_only()
}

/// A temporary file that is removed when dropped.
mod tempfile_path {
    use std::path::{Path, PathBuf};

    pub struct TempPath(PathBuf);

    impl TempPath {
        pub fn new() -> Result<Self, String> {
            let dir = gtk::glib::user_cache_dir().join("ferry").join("vault");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            Ok(TempPath(dir.join(gtk::glib::uuid_string_random().as_str())))
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}
