// GOA tokens only have the legacy "docs" scope: Drive v2 accepts it, v3 doesn't.
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::Stat;
use crate::i18n::{tr, trf};
use crate::profile::Profile;
use crate::s3::{Progress, Res};

const API: &str = "https://www.googleapis.com/drive/v2";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v2";
const FOLDER: &str = "application/vnd.google-apps.folder";
pub const MY_DRIVE: &str = "My Drive";
const FIELDS: &str = "id,title,mimeType,fileSize,modifiedDate";

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct File {
    id: String,
    #[serde(rename = "title")]
    name: String,
    mime_type: String,
    #[serde(default, rename = "fileSize")]
    size: Option<String>,
    #[serde(default, rename = "modifiedDate")]
    modified_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileList {
    #[serde(default, rename = "items")]
    files: Vec<File>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct Drive {
    id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveList {
    #[serde(default, rename = "items")]
    drives: Vec<Drive>,
    #[serde(default)]
    next_page_token: Option<String>,
}

fn export_type(mime: &str) -> Option<&'static str> {
    match mime {
        "application/vnd.google-apps.document" => Some("application/vnd.oasis.opendocument.text"),
        "application/vnd.google-apps.spreadsheet" => Some("application/vnd.oasis.opendocument.spreadsheet"),
        "application/vnd.google-apps.presentation" => Some("application/vnd.oasis.opendocument.presentation"),
        "application/vnd.google-apps.drawing" => Some("image/png"),
        _ => None,
    }
}

fn rfc3339(text: &str) -> i64 {
    gtk::glib::DateTime::from_iso8601(text, None).map(|d| d.to_unix()).unwrap_or(0)
}

fn stat_of(file: &File) -> Stat {
    Stat {
        size: file.size.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0),
        modified: file.modified_time.as_deref().map(rfc3339).unwrap_or(0),
        is_dir: file.mime_type == FOLDER,
    }
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub struct GDrive {
    client: reqwest::Client,
    account: String,
    token: Mutex<Option<(String, std::time::Instant)>>,
    drives: Mutex<Vec<(String, String)>>,
    ids: Mutex<HashMap<(String, String), String>>,
}

impl GDrive {
    pub async fn connect(profile: &Profile) -> Res<GDrive> {
        let account = profile.online_account.trim().to_string();
        if account.is_empty() {
            return Err(tr("Choose a Google account from Settings › Online Accounts"));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .user_agent(format!("Ferry/{}", crate::config::VERSION))
            .build()
            .map_err(|e| e.to_string())?;
        let drive = GDrive {
            client,
            account,
            token: Mutex::new(None),
            drives: Mutex::new(Vec::new()),
            ids: Mutex::new(HashMap::new()),
        };
        drive.buckets().await?;
        Ok(drive)
    }

    async fn token(&self, renew: bool) -> Res<String> {
        if !renew
            && let Some((token, until)) = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone()
            && until > std::time::Instant::now()
        {
            return Ok(token);
        }
        let account = self.account.clone();
        let (token, expires) = tokio::task::spawn_blocking(move || super::goa::access_token(&account))
            .await
            .map_err(|e| e.to_string())??;
        let until =
            std::time::Instant::now() + std::time::Duration::from_secs(expires.saturating_sub(60).max(30) as u64);
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some((token.clone(), until));
        Ok(token)
    }

    async fn send(&self, build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder) -> Res<reqwest::Response> {
        let mut renew = false;
        loop {
            let token = self.token(renew).await?;
            let response = build(&self.client).bearer_auth(token).send().await.map_err(|e| {
                if e.is_connect() {
                    tr("Google Drive could not be reached; check the network")
                } else if e.is_timeout() {
                    tr("The server did not answer")
                } else {
                    e.to_string()
                }
            })?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && !renew {
                renew = true;
                continue;
            }
            if status.is_success() {
                return Ok(response);
            }
            let body = response.text().await.unwrap_or_default();
            let reason = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_default();
            return Err(match status {
                StatusCode::UNAUTHORIZED => {
                    tr("Google did not accept the sign-in. Check the account in Settings › Online Accounts.")
                }
                StatusCode::FORBIDDEN if reason.contains("insufficient") || reason.contains("scope") => tr(
                    "This Google account does not give access to files. Turn on Files for it in Settings › Online Accounts.",
                ),
                StatusCode::FORBIDDEN if reason.contains("quota") || reason.contains("storage") => {
                    tr("The storage on the server is full")
                }
                StatusCode::FORBIDDEN => tr("The server does not allow this"),
                StatusCode::NOT_FOUND => tr("The file or folder was not found"),
                _ => trf("The server answered {status}", &[("status", format!("{status} {reason}").trim())]),
            });
        }
    }

    async fn json<T: for<'de> Deserialize<'de>>(
        &self,
        build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> Res<T> {
        self.send(build).await?.json::<T>().await.map_err(|e| e.to_string())
    }

    pub async fn buckets(&self) -> Res<Vec<String>> {
        let mut drives = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page_token = token.clone();
            let listed: Res<DriveList> = self
                .json(|c| {
                    let mut q = vec![("maxResults", "100".to_string())];
                    if let Some(t) = &page_token {
                        q.push(("pageToken", t.clone()));
                    }
                    c.get(format!("{API}/drives")).query(&q)
                })
                .await;
            // "docs"-scope accounts can't list shared drives; My Drive still works.
            let Ok(page) = listed else { break };
            drives.extend(page.drives.into_iter().map(|d| (d.name, d.id)));
            token = page.next_page_token;
            if token.is_none() {
                break;
            }
        }
        self.json::<serde_json::Value>(|c| c.get(format!("{API}/about")).query(&[("fields", "kind")])).await?;
        let mut names = vec![MY_DRIVE.to_string()];
        names.extend(drives.iter().map(|(n, _)| n.clone()));
        *self.drives.lock().unwrap_or_else(|e| e.into_inner()) = drives;
        Ok(names)
    }

    fn root_of(&self, bucket: &str) -> Res<String> {
        if bucket == MY_DRIVE {
            return Ok("root".into());
        }
        self.drives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(n, _)| n == bucket)
            .map(|(_, id)| id.clone())
            .ok_or_else(|| tr("The shared drive was not found"))
    }

    async fn children(&self, parent: &str, name: Option<&str>) -> Res<Vec<File>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        let mut query = format!("{} in parents and trashed = false", quoted(parent));
        if let Some(name) = name {
            query.push_str(&format!(" and title = {}", quoted(name)));
        }
        loop {
            let (q, page_token) = (query.clone(), token.clone());
            let page: FileList = self
                .json(|c| {
                    let mut params = vec![
                        ("q", q.clone()),
                        ("fields", format!("nextPageToken,items({FIELDS})")),
                        ("maxResults", "1000".into()),
                        ("supportsAllDrives", "true".into()),
                        ("includeItemsFromAllDrives", "true".into()),
                    ];
                    if let Some(t) = &page_token {
                        params.push(("pageToken", t.clone()));
                    }
                    c.get(format!("{API}/files")).query(&params)
                })
                .await?;
            out.extend(page.files);
            token = page.next_page_token;
            if token.is_none() {
                return Ok(out);
            }
        }
    }

    async fn find(&self, bucket: &str, key: &str) -> Res<File> {
        let key = key.trim_matches('/');
        let root = self.root_of(bucket)?;
        if key.is_empty() {
            return Ok(File {
                id: root,
                name: String::new(),
                mime_type: FOLDER.into(),
                size: None,
                modified_time: None,
            });
        }
        let cached =
            self.ids.lock().unwrap_or_else(|e| e.into_inner()).get(&(bucket.to_string(), key.to_string())).cloned();
        if let Some(id) = cached
            && let Ok(file) = self
                .json::<File>(|c| {
                    c.get(format!("{API}/files/{id}")).query(&[("fields", FIELDS), ("supportsAllDrives", "true")])
                })
                .await
        {
            return Ok(file);
        }
        let mut parent = root;
        let mut path = String::new();
        let mut found = None;
        for part in key.split('/') {
            path = if path.is_empty() { part.to_string() } else { format!("{path}/{part}") };
            let mut matches = self.children(&parent, Some(part)).await?;
            matches.sort_by_key(|f| f.mime_type != FOLDER);
            let file = matches.into_iter().next().ok_or_else(|| tr("The file or folder was not found"))?;
            self.ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((bucket.to_string(), path.clone()), file.id.clone());
            parent = file.id.clone();
            found = Some(file);
        }
        found.ok_or_else(|| tr("The file or folder was not found"))
    }

    fn forget(&self, bucket: &str, key: &str) {
        let key = key.trim_matches('/').to_string();
        let below = format!("{key}/");
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(b, k), _| !(b == bucket && (*k == key || k.starts_with(&below))));
    }

    pub async fn list(&self, bucket: &str, dir: &str) -> Res<Vec<(String, Stat)>> {
        let folder = self.find(bucket, dir).await?;
        let files = self.children(&folder.id, None).await?;
        let base = dir.trim_matches('/');
        let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
        Ok(files
            .into_iter()
            .map(|f| {
                let path = if base.is_empty() { f.name.clone() } else { format!("{base}/{}", f.name) };
                ids.insert((bucket.to_string(), path), f.id.clone());
                (f.name.clone(), stat_of(&f))
            })
            .collect())
    }

    pub async fn stat(&self, bucket: &str, key: &str) -> Res<Stat> {
        Ok(stat_of(&self.find(bucket, key).await?))
    }

    fn content(&self, file: &File) -> Res<impl Fn(&reqwest::Client) -> reqwest::RequestBuilder> {
        let (id, export) = (file.id.clone(), export_type(&file.mime_type));
        if export.is_none() && file.mime_type.starts_with("application/vnd.google-apps.") {
            return Err(tr("This kind of Google file cannot be downloaded"));
        }
        Ok(move |c: &reqwest::Client| match export {
            Some(mime) => c.get(format!("{API}/files/{id}/export")).query(&[("mimeType", mime)]),
            None => c.get(format!("{API}/files/{id}")).query(&[("alt", "media"), ("supportsAllDrives", "true")]),
        })
    }

    pub async fn read(&self, bucket: &str, key: &str, limit: u64) -> Res<Vec<u8>> {
        let file = self.find(bucket, key).await?;
        let request = self.content(&file)?;
        let range = format!("bytes=0-{}", limit.saturating_sub(1));
        let response = self.send(|c| request(c).header("Range", &range)).await?;
        let mut stream = response.bytes_stream();
        let mut data = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            let room = (limit as usize).saturating_sub(data.len());
            data.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if data.len() as u64 >= limit {
                break;
            }
        }
        Ok(data)
    }

    pub async fn download(&self, bucket: &str, key: &str, partial: &Path, progress: &Progress) -> Res<()> {
        let file = self.find(bucket, key).await?;
        let request = self.content(&file)?;
        let response = self.send(request).await?;
        let mut stream = response.bytes_stream();
        let mut local = tokio::fs::File::create(partial).await.map_err(|e| e.to_string())?;
        while let Some(chunk) = stream.next().await {
            progress.check()?;
            let chunk = chunk.map_err(|e| e.to_string())?;
            local.write_all(&chunk).await.map_err(|e| e.to_string())?;
            progress.advance(chunk.len() as u64).await?;
        }
        local.flush().await.map_err(|e| e.to_string())
    }

    async fn parent_of(&self, bucket: &str, key: &str) -> Res<(String, String)> {
        let key = key.trim_matches('/');
        let (parent, name) = key.rsplit_once('/').unwrap_or(("", key));
        self.mkdir_p(bucket, parent).await?;
        Ok((self.find(bucket, parent).await?.id, name.to_string()))
    }

    async fn mkdir_p(&self, bucket: &str, dir: &str) -> Res<()> {
        let mut path = String::new();
        for part in dir.trim_matches('/').split('/').filter(|p| !p.is_empty()) {
            let parent = self.find(bucket, &path).await?.id;
            path = if path.is_empty() { part.to_string() } else { format!("{path}/{part}") };
            if self.find(bucket, &path).await.is_ok() {
                continue;
            }
            let body = serde_json::json!({ "title": part, "mimeType": FOLDER, "parents": [{ "id": parent }] });
            let made: File = self
                .json(|c| {
                    c.post(format!("{API}/files"))
                        .query(&[("fields", FIELDS), ("supportsAllDrives", "true")])
                        .json(&body)
                })
                .await?;
            self.ids.lock().unwrap_or_else(|e| e.into_inner()).insert((bucket.to_string(), path.clone()), made.id);
        }
        Ok(())
    }

    pub async fn mkdir(&self, bucket: &str, key: &str) -> Res<()> {
        self.mkdir_p(bucket, key).await
    }

    async fn put(&self, bucket: &str, key: &str, size: u64, mut body: impl FnMut() -> reqwest::Body) -> Res<()> {
        let mime = crate::s3::content_type_of(Path::new(key));
        let existing = self.find(bucket, key).await.ok().filter(|f| f.mime_type != FOLDER);
        let start = match &existing {
            Some(file) => {
                let id = file.id.clone();
                self.send(|c| {
                    c.put(format!("{UPLOAD}/files/{id}"))
                        .query(&[("uploadType", "resumable"), ("supportsAllDrives", "true")])
                        .header("X-Upload-Content-Type", &mime)
                        .header("X-Upload-Content-Length", size)
                        .json(&serde_json::json!({}))
                })
                .await?
            }
            None => {
                let (parent, name) = self.parent_of(bucket, key).await?;
                let meta = serde_json::json!({ "title": name, "parents": [{ "id": parent }] });
                self.send(|c| {
                    c.post(format!("{UPLOAD}/files"))
                        .query(&[("uploadType", "resumable"), ("supportsAllDrives", "true")])
                        .header("X-Upload-Content-Type", &mime)
                        .header("X-Upload-Content-Length", size)
                        .json(&meta)
                })
                .await?
            }
        };
        let location = start
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| tr("Google Drive did not start the upload"))?
            .to_string();
        let response = self
            .client
            .put(&location)
            .header("Content-Length", size)
            .body(body())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(trf("The server answered {status}", &[("status", &response.status().to_string())]));
        }
        let made: File = response.json().await.map_err(|e| e.to_string())?;
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((bucket.to_string(), key.trim_matches('/').to_string()), made.id);
        Ok(())
    }

    pub async fn upload(&self, bucket: &str, path: &Path, key: &str, progress: &Progress) -> Res<()> {
        let size = tokio::fs::metadata(path).await.map_err(|e| e.to_string())?.len();
        let file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
        let shared = std::sync::Arc::new(tokio::sync::Mutex::new(Some(file)));
        let progress = progress.clone();

        self.put(bucket, key, size, || {
            let (shared, progress) = (shared.clone(), progress.clone());
            let stream = futures_util::stream::unfold((shared, progress), |(shared, progress)| async move {
                let mut guard = shared.lock().await;
                let file = guard.as_mut()?;
                let mut buffer = vec![0u8; 256 * 1024];
                match file.read(&mut buffer).await {
                    Ok(0) => None,
                    Ok(n) => {
                        drop(guard);
                        if let Err(e) = progress.advance(n as u64).await {
                            return Some((Err(std::io::Error::other(e)), (shared, progress)));
                        }
                        buffer.truncate(n);
                        Some((Ok(bytes::Bytes::from(buffer)), (shared, progress)))
                    }
                    Err(e) => {
                        drop(guard);
                        Some((Err(e), (shared, progress)))
                    }
                }
            });
            reqwest::Body::wrap_stream(stream)
        })
        .await
    }

    pub async fn write(&self, bucket: &str, key: &str, data: Vec<u8>) -> Res<()> {
        let size = data.len() as u64;
        self.put(bucket, key, size, || data.clone().into()).await
    }

    pub async fn remove(&self, bucket: &str, key: &str) -> Res<()> {
        let file = self.find(bucket, key).await?;
        if self.root_of(bucket).is_ok_and(|root| root == file.id) {
            return Err(tr("The connection's folder itself cannot be deleted"));
        }
        let id = file.id.clone();
        self.send(|c| c.post(format!("{API}/files/{id}/trash")).query(&[("supportsAllDrives", "true")])).await?;
        self.forget(bucket, key);
        Ok(())
    }

    pub async fn rename(&self, bucket: &str, from: &str, to: &str) -> Res<()> {
        let file = self.find(bucket, from).await?;
        let (old_parent, _) = from.trim_matches('/').rsplit_once('/').unwrap_or(("", ""));
        let old_parent_id = self.find(bucket, old_parent).await?.id;
        let (new_parent_id, name) = self.parent_of(bucket, to).await?;
        if let Ok(existing) = self.find(bucket, to).await
            && existing.id != file.id
            && existing.mime_type != FOLDER
        {
            self.remove(bucket, to).await?;
        }
        let id = file.id.clone();
        let mut query = vec![("supportsAllDrives", "true".to_string())];
        if new_parent_id != old_parent_id {
            query.push(("addParents", new_parent_id.clone()));
            query.push(("removeParents", old_parent_id.clone()));
        }
        self.send(|c| c.patch(format!("{API}/files/{id}")).query(&query).json(&serde_json::json!({ "title": name })))
            .await?;
        self.forget(bucket, from);
        self.forget(bucket, to);
        Ok(())
    }

    pub async fn copy(&self, bucket: &str, from: &str, to: &str) -> Res<()> {
        let file = self.find(bucket, from).await?;
        if file.mime_type == FOLDER {
            return Err(tr("Google Drive copies files, not folders"));
        }
        let (parent, name) = self.parent_of(bucket, to).await?;
        if let Ok(existing) = self.find(bucket, to).await
            && existing.mime_type != FOLDER
        {
            self.remove(bucket, to).await?;
        }
        let id = file.id.clone();
        let body = serde_json::json!({ "title": name, "parents": [{ "id": parent }] });
        let made: File = self
            .json(|c| {
                c.post(format!("{API}/files/{id}/copy"))
                    .query(&[("fields", FIELDS), ("supportsAllDrives", "true")])
                    .json(&body)
            })
            .await?;
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((bucket.to_string(), to.trim_matches('/').to_string()), made.id);
        Ok(())
    }
}

#[cfg(test)]
impl GDrive {
    pub async fn purge(&self, bucket: &str, key: &str) -> Res<()> {
        let id = self.find(bucket, key).await?.id;
        self.send(|c| c.delete(format!("{API}/files/{id}")).query(&[("supportsAllDrives", "true")])).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_and_types() {
        assert_eq!(quoted("Ada's \\ files"), "'Ada\\'s \\\\ files'");
        assert_eq!(
            export_type("application/vnd.google-apps.spreadsheet"),
            Some("application/vnd.oasis.opendocument.spreadsheet")
        );
        assert_eq!(rfc3339("1994-11-06T08:49:37.000Z"), 784111777);
        let folder =
            File { id: "x".into(), name: "a".into(), mime_type: FOLDER.into(), size: None, modified_time: None };
        assert!(stat_of(&folder).is_dir);
    }
}
