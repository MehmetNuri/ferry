use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::i18n::{tr, trf};
use crate::profile::Profile;
use crate::s3::{Progress, S3};

pub const COMMANDS: &[&str] = &["connections", "ls", "get", "put", "rm", "cat", "share", "help"];

fn usage() -> String {
    format!(
        "{}\n\n  ferry connections\n  ferry ls [-l] CONNECTION[:BUCKET[/PREFIX]]\n  ferry get [-r] CONNECTION:BUCKET/KEY [LOCAL]\n  ferry put [-r] LOCAL… CONNECTION:BUCKET/[PREFIX]\n  ferry rm [-r] CONNECTION:BUCKET/KEY\n  ferry cat CONNECTION:BUCKET/KEY\n  ferry share [--expires SECONDS] CONNECTION:BUCKET/KEY\n\n{}",
        tr("Usage:"),
        tr(
            "Without a command, Ferry opens its window. Connections are the ones saved in Ferry; names with spaces need quotes."
        )
    )
}

struct Remote {
    profile: Profile,
    bucket: String,
    key: String,
}

fn parse_remote(text: &str, profiles: &[Profile]) -> Option<Remote> {
    let (name, rest) = text.split_once(':').unwrap_or((text, ""));
    let profile = profiles
        .iter()
        .find(|p| p.name == name)
        .or_else(|| profiles.iter().find(|p| p.name.eq_ignore_ascii_case(name)))?;
    let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
    Some(Remote { profile: profile.clone(), bucket: bucket.to_string(), key: key.to_string() })
}

fn remote(text: &str, profiles: &[Profile]) -> Result<Remote, String> {
    parse_remote(text, profiles).ok_or_else(|| {
        trf(
            "“{name}” is not a saved connection; see “ferry connections”",
            &[("name", text.split(':').next().unwrap_or(text))],
        )
    })
}

async fn connect(profile: &Profile) -> Result<S3, String> {
    if crate::s3::connection::needs_mfa(profile) {
        eprint!("{} ", trf("MFA code for “{name}”:", &[("name", &profile.name)]));
        let _ = std::io::stderr().flush();
        let mut code = String::new();
        std::io::stdin().read_line(&mut code).map_err(|e| e.to_string())?;
        let with_keys = crate::profile::with_secrets(profile.clone()).await?;
        crate::s3::connection::start_mfa_session(&with_keys, code.trim()).await?;
    }
    crate::s3::client_for(&profile.id).await.map_err(|error| match crate::remote::sftp::UnknownHost::decode(&error) {
        Some(host) => trf(
            "The key of {host} is not known yet ({fingerprint}). Open the connection once in Ferry to check and trust it.",
            &[("host", &format!("{}:{}", host.host, host.port)), ("fingerprint", &host.fingerprint)],
        ),
        None => error,
    })
}

// File servers have one bucket, named after the connection, so the text after
// the colon is a path there; a wrong bucket must not fall back to the root.
async fn locate(place: Remote, client: &S3) -> Result<Remote, String> {
    let Some(server) = client.remote.as_ref() else { return Ok(place) };
    let buckets: Vec<String> = client.list_buckets().await?.buckets.into_iter().map(|b| b.name).collect();
    if !server.has_buckets()
        && let [only] = buckets.as_slice()
    {
        let key = match (place.bucket.as_str(), place.key.as_str()) {
            (bucket, key) if bucket == only => key.to_string(),
            ("", _) => String::new(),
            (bucket, "") => bucket.to_string(),
            (bucket, key) => format!("{bucket}/{key}"),
        };
        return Ok(Remote { bucket: only.clone(), key, ..place });
    }
    if place.bucket.is_empty() || buckets.contains(&place.bucket) {
        return Ok(place);
    }
    Err(trf(
        "“{name}” has no bucket “{bucket}”. It has: {buckets}",
        &[("name", &place.profile.name), ("bucket", &place.bucket), ("buckets", &buckets.join(", "))],
    ))
}

fn human(bytes: i64) -> String {
    gtk::glib::format_size(bytes.max(0) as u64).to_string()
}

async fn with_progress<F: std::future::Future<Output = Result<(), String>>>(
    label: &str,
    total: u64,
    job: impl FnOnce(Progress) -> F,
) -> Result<(), String> {
    let progress = Progress::default();
    let done = progress.done.clone();
    let show = std::io::stderr().is_terminal() && total > 0;
    let label = label.to_string();
    let ticker = show.then(|| {
        tokio::spawn(async move {
            loop {
                let now = done.load(std::sync::atomic::Ordering::Relaxed);
                eprint!("\r{label}  {}%  ", (now * 100 / total.max(1)).min(100));
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        })
    });
    let result = job(progress).await;
    if let Some(t) = ticker {
        t.abort();
        eprint!("\r\x1b[K");
    }
    result
}

async fn ls(args: &[String], profiles: &[Profile]) -> Result<(), String> {
    let long = args.iter().any(|a| a == "-l");
    let Some(target) = args.iter().find(|a| !a.starts_with('-')) else { return Err(usage()) };
    let place = remote(target, profiles)?;
    let client = connect(&place.profile).await?;
    let place = locate(place, &client).await?;
    if place.bucket.is_empty() {
        for bucket in client.list_buckets().await?.buckets {
            println!("{}/", bucket.name);
        }
        return Ok(());
    }
    let mut prefix = place.key.clone();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    let mut token = String::new();
    loop {
        let page = client.list_objects(&place.bucket, &prefix, &token).await?;
        for entry in &page.items {
            let name = if entry.is_folder { format!("{}/", entry.name) } else { entry.name.clone() };
            if long && !entry.is_folder {
                let date = gtk::glib::DateTime::from_unix_local(entry.modified)
                    .ok()
                    .and_then(|d| d.format("%Y-%m-%d %H:%M").ok())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                println!("{:>10}  {date}  {name}", human(entry.size));
            } else if long {
                println!("{:>10}  {:16}  {name}", "", "");
            } else {
                println!("{name}");
            }
        }
        if page.next_token.is_empty() {
            break;
        }
        token = page.next_token;
    }
    Ok(())
}

async fn get(args: &[String], profiles: &[Profile]) -> Result<(), String> {
    let recursive = args.iter().any(|a| a == "-r");
    let plain: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let Some(source) = plain.first() else { return Err(usage()) };
    let place = remote(source, profiles)?;
    let destination = plain.get(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let client = connect(&place.profile).await?;
    let place = locate(place, &client).await?;
    if place.bucket.is_empty() {
        return Err(tr("Name a bucket or folder to download"));
    }
    if !recursive {
        let name = place
            .key
            .rsplit('/')
            .next()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| tr("Name an object, or use -r for a folder"))?
            .to_string();
        let target = if destination.is_dir() { destination.join(&name) } else { destination };
        let size = client.head_object(&place.bucket, &place.key).await?.size.max(0) as u64;
        return with_progress(&name, size, |p| async move {
            client.download_file(&place.bucket, &place.key, None, &target, &p).await
        })
        .await;
    }
    let mut prefix = place.key.clone();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    let (items, _) = client.list_all(&place.bucket, &prefix, usize::MAX).await?;
    for entry in items.iter().filter(|e| !e.is_folder && !e.key.ends_with('/')) {
        let relative = &entry.key[prefix.len()..];
        // Don't let keys escape the destination folder.
        if relative.split('/').any(|part| part == ".." || part.is_empty()) {
            continue;
        }
        let target = destination.join(relative);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let (client, bucket, key) = (client.clone(), place.bucket.clone(), entry.key.clone());
        with_progress(relative, entry.size.max(0) as u64, |p| async move {
            client.download_file(&bucket, &key, None, &target, &p).await
        })
        .await?;
        println!("{relative}");
    }
    Ok(())
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, String)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push((path.clone(), relative.to_string_lossy().replace('\\', "/")));
        }
    }
    Ok(())
}

async fn put(args: &[String], profiles: &[Profile]) -> Result<(), String> {
    let recursive = args.iter().any(|a| a == "-r");
    let plain: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let (Some(target), true) = (plain.last(), plain.len() >= 2) else { return Err(usage()) };
    let place = remote(target, profiles)?;
    let client = connect(&place.profile).await?;
    let place = locate(place, &client).await?;
    if place.bucket.is_empty() {
        return Err(tr("Name a bucket to upload into"));
    }
    let mut prefix = place.key.clone();
    let single = plain.len() == 2 && Path::new(plain[0]).is_file();
    for local in &plain[..plain.len() - 1] {
        let path = Path::new(local.as_str());
        let mut files = Vec::new();
        if path.is_dir() {
            if !recursive {
                return Err(trf("“{name}” is a folder; use -r to upload it", &[("name", local)]));
            }
            let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut found = Vec::new();
            walk(path, path, &mut found).map_err(|e| e.to_string())?;
            files.extend(found.into_iter().map(|(p, r)| (p, format!("{base}/{r}"))));
        } else {
            files.push((
                path.to_path_buf(),
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            ));
        }
        for (file, relative) in files {
            let key = if single && !prefix.is_empty() && !prefix.ends_with('/') {
                prefix.clone()
            } else {
                if !prefix.is_empty() && !prefix.ends_with('/') {
                    prefix.push('/');
                }
                format!("{prefix}{relative}")
            };
            let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            let (client, bucket, k, f) = (client.clone(), place.bucket.clone(), key.clone(), file.clone());
            with_progress(&relative, size, |p| async move { client.upload_file(&bucket, &k, &f, &p).await }).await?;
            println!("{key}");
        }
    }
    Ok(())
}

async fn rm(args: &[String], profiles: &[Profile]) -> Result<(), String> {
    let recursive = args.iter().any(|a| a == "-r");
    let Some(target) = args.iter().find(|a| !a.starts_with('-')) else { return Err(usage()) };
    let place = remote(target, profiles)?;
    let client = connect(&place.profile).await?;
    let place = locate(place, &client).await?;
    if place.bucket.is_empty() || place.key.trim_matches('/').is_empty() {
        return Err(tr("Name an object or folder to remove; buckets are not removed from the command line"));
    }
    let keys = if recursive {
        let mut prefix = place.key.clone();
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        let (items, _) = client.list_all(&place.bucket, &prefix, usize::MAX).await?;
        let mut keys: Vec<String> = items.into_iter().map(|e| e.key).collect();
        keys.push(prefix);
        keys
    } else {
        if place.key.ends_with('/') {
            return Err(tr("Use -r to remove a folder"));
        }
        if let Err(error) = client.head_object(&place.bucket, &place.key).await {
            let (inside, _) = client.list_all(&place.bucket, &format!("{}/", place.key), 1).await.unwrap_or_default();
            return Err(if inside.is_empty() { error } else { tr("Use -r to remove a folder") });
        }
        vec![place.key.clone()]
    };
    let removed = client.delete_keys(&place.bucket, keys).await?;
    eprintln!("{}", trf("{n} removed", &[("n", &removed.to_string())]));
    Ok(())
}

async fn cat(args: &[String], profiles: &[Profile]) -> Result<(), String> {
    let Some(target) = args.first() else { return Err(usage()) };
    let place = remote(target, profiles)?;
    let client = connect(&place.profile).await?;
    let place = locate(place, &client).await?;
    let size = client.head_object(&place.bucket, &place.key).await?.size.max(0) as u64;
    if size <= 16 * 1024 * 1024 {
        let data = client.read_bytes(&place.bucket, &place.key, size).await?;
        std::io::stdout().write_all(&data).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let temp = std::env::temp_dir().join(format!("ferry-cat-{}", gtk::glib::uuid_string_random()));
    client.download_file(&place.bucket, &place.key, None, &temp, &Progress::default()).await?;
    let mut file = std::fs::File::open(&temp).map_err(|e| e.to_string())?;
    let copied = std::io::copy(&mut file, &mut std::io::stdout());
    let _ = std::fs::remove_file(&temp);
    copied.map(|_| ()).map_err(|e| e.to_string())
}

async fn share(args: &[String], profiles: &[Profile]) -> Result<(), String> {
    let mut seconds = 3600u64;
    let mut target = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--expires" {
            seconds = iter.next().and_then(|s| s.parse().ok()).ok_or_else(usage)?;
        } else {
            target = Some(arg);
        }
    }
    let place = remote(target.ok_or_else(usage)?, profiles)?;
    let client = connect(&place.profile).await?;
    let place = locate(place, &client).await?;
    println!("{}", client.presign(&place.bucket, &place.key, seconds.clamp(1, 604_800)).await?);
    Ok(())
}

pub fn run(args: &[String]) -> i32 {
    let command = args[0].as_str();
    let rest = &args[1..];
    let profiles = crate::profile::load();
    let result = crate::runtime::runtime().block_on(async {
        match command {
            "connections" => {
                for p in &profiles {
                    println!("{}", p.name);
                }
                Ok(())
            }
            "ls" => ls(rest, &profiles).await,
            "get" => get(rest, &profiles).await,
            "put" => put(rest, &profiles).await,
            "rm" => rm(rest, &profiles).await,
            "cat" => cat(rest, &profiles).await,
            "share" => share(rest, &profiles).await,
            _ => {
                println!("{}", usage());
                Ok(())
            }
        }
    });
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("ferry: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes() {
        let profiles = vec![Profile { name: "My Work".into(), ..Default::default() }];
        let r = parse_remote("my work:photos/2026/a.jpg", &profiles).unwrap();
        assert_eq!((r.bucket.as_str(), r.key.as_str()), ("photos", "2026/a.jpg"));
        let r = parse_remote("My Work", &profiles).unwrap();
        assert!(r.bucket.is_empty());
        assert!(parse_remote("Other:b/k", &profiles).is_none());
    }
}
