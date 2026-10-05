pub mod azure;
pub mod ftp;
pub mod gdrive;
pub mod goa;
pub mod sftp;
pub mod webdav;

use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::i18n::tr;
use crate::profile::Profile;
use crate::s3::{Entry, ObjectInfo, Progress, Res};

pub const PROVIDERS: &[&str] = &["sftp", "ftp", "webdav", "nextcloud", "azure", "gdrive"];

pub fn is_remote(provider: &str) -> bool {
    PROVIDERS.contains(&provider)
}

#[derive(Clone, Debug, Default)]
pub struct Stat {
    pub size: u64,
    pub modified: i64,
    pub is_dir: bool,
}

pub enum Remote {
    Azure(azure::Azure),
    GDrive(gdrive::GDrive),
    Sftp(sftp::Sftp),
    Ftp(ftp::Ftp),
    WebDav(webdav::WebDav),
}

pub fn join(root: &str, key: &str) -> String {
    let root = root.trim_end_matches('/');
    let key = key.trim_start_matches('/');
    if key.is_empty() {
        if root.is_empty() { "/".into() } else { root.to_string() }
    } else if root.is_empty() {
        format!("/{key}")
    } else {
        format!("{root}/{key}")
    }
}

pub fn host_port(server: &str, default_port: u16) -> Res<(String, u16)> {
    let server = server.trim();
    let server = server.split_once("://").map(|(_, rest)| rest).unwrap_or(server);
    let server = server.split('/').next().unwrap_or(server);
    let server = server.rsplit_once('@').map(|(_, host)| host).unwrap_or(server);
    if server.is_empty() {
        return Err(tr("Enter the address of the server"));
    }
    if let Some(rest) = server.strip_prefix('[') {
        let (host, port) = rest.split_once(']').ok_or_else(|| tr("Invalid server address"))?;
        let port = port
            .strip_prefix(':')
            .map(|p| p.parse().map_err(|_| tr("Invalid port")))
            .transpose()?
            .unwrap_or(default_port);
        return Ok((host.to_string(), port));
    }
    match server.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            Ok((host.to_string(), port.parse().map_err(|_| tr("Invalid port"))?))
        }
        _ => Ok((server.to_string(), default_port)),
    }
}

pub async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    progress: &Progress,
) -> Res<u64> {
    let mut buffer = vec![0u8; 128 * 1024];
    let mut total = 0;
    loop {
        progress.check()?;
        let n = reader.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        writer.write_all(&buffer[..n]).await.map_err(|e| e.to_string())?;
        progress.advance(n as u64).await?;
        total += n as u64;
    }
    writer.flush().await.map_err(|e| e.to_string())?;
    Ok(total)
}

pub fn partial_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".ferry-part");
    path.with_file_name(name)
}

impl Remote {
    pub async fn connect(profile: &Profile) -> Res<Remote> {
        Ok(match profile.provider.as_str() {
            "sftp" => Remote::Sftp(sftp::Sftp::connect(profile).await?),
            "ftp" => Remote::Ftp(ftp::Ftp::connect(profile).await?),
            "azure" => Remote::Azure(azure::Azure::connect(profile).await?),
            "gdrive" => Remote::GDrive(gdrive::GDrive::connect(profile).await?),
            _ => Remote::WebDav(webdav::WebDav::connect(profile).await?),
        })
    }

    pub fn label(profile: &Profile) -> String {
        if !profile.name.trim().is_empty() {
            return profile.name.trim().to_string();
        }
        let server = if profile.provider == "nextcloud" || profile.provider == "webdav" {
            profile
                .endpoint
                .split_once("://")
                .map(|(_, r)| r)
                .unwrap_or(&profile.endpoint)
                .split('/')
                .next()
                .unwrap_or("")
                .to_string()
        } else {
            host_port(&profile.endpoint, 0).map(|(h, _)| h).unwrap_or_default()
        };
        if server.is_empty() { profile.name.clone() } else { server }
    }

    pub async fn buckets(&self, profile: &Profile) -> Res<Vec<String>> {
        match self {
            Remote::Azure(r) => r.containers().await,
            Remote::GDrive(r) => r.buckets().await,
            _ => Ok(vec![Remote::label(profile)]),
        }
    }

    pub async fn create_bucket(&self, name: &str) -> Res<()> {
        match self {
            Remote::Azure(r) => r.create_container(name).await,
            _ => Err(tr("File servers do not offer this")),
        }
    }

    pub async fn delete_bucket(&self, name: &str) -> Res<()> {
        match self {
            Remote::Azure(r) => r.delete_container(name).await,
            _ => Err(tr("File servers do not offer this")),
        }
    }

    pub fn has_buckets(&self) -> bool {
        matches!(self, Remote::Azure(_) | Remote::GDrive(_))
    }

    pub async fn list(&self, bucket: &str, dir: &str) -> Res<Vec<Entry>> {
        let found = match self {
            Remote::Azure(r) => r.list(bucket, dir).await?,
            Remote::GDrive(r) => r.list(bucket, dir).await?,
            Remote::Sftp(r) => r.list(dir).await?,
            Remote::Ftp(r) => r.list(dir).await?,
            Remote::WebDav(r) => r.list(dir).await?,
        };
        let prefix = if dir.is_empty() || dir.ends_with('/') { dir.to_string() } else { format!("{dir}/") };
        Ok(found
            .into_iter()
            .filter(|(name, _)| name != "." && name != ".." && !name.is_empty())
            .map(|(name, stat)| Entry {
                key: if stat.is_dir { format!("{prefix}{name}/") } else { format!("{prefix}{name}") },
                name,
                size: stat.size as i64,
                modified: stat.modified,
                is_folder: stat.is_dir,
                ..Default::default()
            })
            .collect())
    }

    pub async fn list_all(&self, bucket: &str, dir: &str, limit: usize) -> Res<(Vec<Entry>, bool)> {
        let mut out = Vec::new();
        let mut folders = vec![dir.to_string()];
        while let Some(folder) = folders.pop() {
            for entry in self.list(bucket, &folder).await? {
                if entry.is_folder {
                    folders.push(entry.key.clone());
                }
                out.push(entry);
                if out.len() >= limit {
                    return Ok((out, true));
                }
            }
        }
        Ok((out, false))
    }

    pub async fn stat(&self, bucket: &str, key: &str) -> Res<Stat> {
        match self {
            Remote::Azure(r) => r.stat(bucket, key).await,
            Remote::GDrive(r) => r.stat(bucket, key).await,
            Remote::Sftp(r) => r.stat(key).await,
            Remote::Ftp(r) => r.stat(key).await,
            Remote::WebDav(r) => r.stat(key).await,
        }
    }

    pub async fn head(&self, bucket: &str, key: &str) -> Res<ObjectInfo> {
        let stat = self.stat(bucket, key).await?;
        if stat.is_dir {
            return Err(tr("This is a folder"));
        }
        Ok(ObjectInfo {
            key: key.to_string(),
            size: stat.size as i64,
            modified: stat.modified,
            content_type: crate::s3::content_type_of(Path::new(key)),
            ..Default::default()
        })
    }

    pub async fn read(&self, bucket: &str, key: &str, limit: u64) -> Res<Vec<u8>> {
        match self {
            Remote::Azure(r) => r.read(bucket, key, limit).await,
            Remote::GDrive(r) => r.read(bucket, key, limit).await,
            Remote::Sftp(r) => r.read(key, limit).await,
            Remote::Ftp(r) => r.read(key, limit).await,
            Remote::WebDav(r) => r.read(key, limit).await,
        }
    }

    pub async fn download(&self, bucket: &str, key: &str, path: &Path, progress: &Progress) -> Res<()> {
        let partial = partial_path(path);
        let result = match self {
            Remote::Azure(r) => r.download(bucket, key, &partial, progress).await,
            Remote::GDrive(r) => r.download(bucket, key, &partial, progress).await,
            Remote::Sftp(r) => r.download(key, &partial, progress).await,
            Remote::Ftp(r) => r.download(key, &partial, progress).await,
            Remote::WebDav(r) => r.download(key, &partial, progress).await,
        };
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(error);
        }
        tokio::fs::rename(&partial, path).await.map_err(|e| e.to_string())
    }

    pub async fn upload(&self, bucket: &str, path: &Path, key: &str, progress: &Progress) -> Res<()> {
        if !self.has_buckets()
            && let Some((parent, _)) = key.trim_end_matches('/').rsplit_once('/')
        {
            self.mkdir_p(bucket, parent).await?;
        }
        match self {
            Remote::Azure(r) => r.upload(bucket, path, key, progress).await,
            Remote::GDrive(r) => r.upload(bucket, path, key, progress).await,
            Remote::Sftp(r) => r.upload(path, key, progress).await,
            Remote::Ftp(r) => r.upload(path, key, progress).await,
            Remote::WebDav(r) => r.upload(path, key, progress).await,
        }
    }

    pub async fn write(&self, bucket: &str, key: &str, data: Vec<u8>) -> Res<()> {
        if !self.has_buckets()
            && let Some((parent, _)) = key.trim_end_matches('/').rsplit_once('/')
        {
            self.mkdir_p(bucket, parent).await?;
        }
        match self {
            Remote::Azure(r) => r.write(bucket, key, data).await,
            Remote::GDrive(r) => r.write(bucket, key, data).await,
            Remote::Sftp(r) => r.write(key, data).await,
            Remote::Ftp(r) => r.write(key, data).await,
            Remote::WebDav(r) => r.write(key, data).await,
        }
    }

    pub async fn mkdir_p(&self, bucket: &str, dir: &str) -> Res<()> {
        let mut path = String::new();
        for part in dir.trim_matches('/').split('/').filter(|p| !p.is_empty()) {
            path = if path.is_empty() { part.to_string() } else { format!("{path}/{part}") };
            match self.stat(bucket, &path).await {
                Ok(stat) if stat.is_dir => continue,
                Ok(_) => return Err(crate::i18n::trf("“{name}” is a file, not a folder", &[("name", part)])),
                Err(_) => {}
            }
            let made = match self {
                Remote::Azure(r) => r.mkdir(bucket, &path).await,
                Remote::GDrive(r) => r.mkdir(bucket, &path).await,
                Remote::Sftp(r) => r.mkdir(&path).await,
                Remote::Ftp(r) => r.mkdir(&path).await,
                Remote::WebDav(r) => r.mkdir(&path).await,
            };
            if made.is_err() && !self.stat(bucket, &path).await.is_ok_and(|s| s.is_dir) {
                return made;
            }
        }
        Ok(())
    }

    pub async fn delete(&self, bucket: &str, keys: Vec<String>) -> Res<usize> {
        let mut count = 0;
        for key in keys {
            if key.ends_with('/') {
                if key.trim_matches('/').is_empty() {
                    return Err(tr("The connection's folder itself cannot be deleted"));
                }
                if let Remote::GDrive(r) = self {
                    r.remove(bucket, &key).await?;
                    count += 1;
                    continue;
                }
                let (entries, _) = self.list_all(bucket, &key, usize::MAX).await?;
                let (mut folders, files): (Vec<Entry>, Vec<Entry>) = entries.into_iter().partition(|e| e.is_folder);
                for file in files {
                    self.remove_file(bucket, &file.key).await?;
                    count += 1;
                }
                folders.sort_by_key(|f| std::cmp::Reverse(f.key.matches('/').count()));
                for folder in folders {
                    self.remove_dir(bucket, &folder.key).await?;
                    count += 1;
                }
                self.remove_dir(bucket, &key).await?;
                count += 1;
            } else {
                self.remove_file(bucket, &key).await?;
                count += 1;
            }
        }
        Ok(count)
    }

    async fn remove_file(&self, bucket: &str, key: &str) -> Res<()> {
        match self {
            Remote::Azure(r) => r.remove(bucket, key).await,
            Remote::GDrive(r) => r.remove(bucket, key).await,
            Remote::Sftp(r) => r.remove_file(key).await,
            Remote::Ftp(r) => r.remove_file(key).await,
            Remote::WebDav(r) => r.remove(key).await,
        }
    }

    async fn remove_dir(&self, bucket: &str, key: &str) -> Res<()> {
        match self {
            Remote::Azure(r) => r.remove_dir(bucket, key).await,
            Remote::GDrive(r) => r.remove(bucket, key).await,
            Remote::Sftp(r) => r.remove_dir(key).await,
            Remote::Ftp(r) => r.remove_dir(key).await,
            Remote::WebDav(r) => r.remove(key).await,
        }
    }

    pub async fn rename(&self, bucket: &str, from: &str, to: &str) -> Res<()> {
        if !self.has_buckets()
            && let Some((parent, _)) = to.trim_end_matches('/').rsplit_once('/')
        {
            self.mkdir_p(bucket, parent).await?;
        }
        match self {
            Remote::GDrive(r) => r.rename(bucket, from, to).await,
            Remote::Azure(r) => {
                if from.ends_with('/') {
                    let (entries, _) = self.list_all(bucket, from, usize::MAX).await?;
                    for entry in entries.iter().filter(|e| !e.is_folder) {
                        let target = format!("{to}{}", &entry.key[from.len()..]);
                        r.copy(bucket, &entry.key, &target).await?;
                        r.remove(bucket, &entry.key).await?;
                    }
                    return r.remove_dir(bucket, from).await;
                }
                r.copy(bucket, from, to).await?;
                r.remove(bucket, from).await
            }
            Remote::Sftp(r) => r.rename(from, to).await,
            Remote::Ftp(r) => r.rename(from, to).await,
            Remote::WebDav(r) => r.rename(from, to).await,
        }
    }

    pub async fn copy(&self, bucket: &str, from: &str, to: &str) -> Option<Res<()>> {
        match self {
            Remote::Azure(r) => Some(r.copy(bucket, from, to).await),
            Remote::GDrive(r) => Some(r.copy(bucket, from, to).await),
            Remote::WebDav(r) => {
                if !self.has_buckets()
                    && let Some((parent, _)) = to.trim_end_matches('/').rsplit_once('/')
                    && let Err(e) = self.mkdir_p(bucket, parent).await
                {
                    return Some(Err(e));
                }
                Some(r.copy(from, to).await)
            }
            _ => None,
        }
    }

    pub async fn test(&self, profile: &Profile) -> Res<String> {
        let entries = self.list(&self.buckets(profile).await?.into_iter().next().unwrap_or_default(), "").await?;
        Ok(crate::i18n::trn(
            "Connection successful: {n} item in the folder",
            "Connection successful: {n} items in the folder",
            &[("n", &entries.len().to_string())],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(join("/home/ada", "a/b.txt"), "/home/ada/a/b.txt");
        assert_eq!(join("/", "a/"), "/a/");
        assert_eq!(join("", ""), "/");
        assert_eq!(join("/srv/", ""), "/srv");
        assert_eq!(host_port("example.org", 22).unwrap(), ("example.org".into(), 22));
        assert_eq!(host_port("sftp://ada@example.org:2222/home", 22).unwrap(), ("example.org".into(), 2222));
        assert_eq!(host_port("[::1]:21", 990).unwrap(), ("::1".into(), 21));
        assert!(host_port("example.org:x", 22).is_err());
    }
}
