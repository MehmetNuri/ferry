//! Connections of other applications: Cyberduck bookmarks (.duck), rclone remotes,
//! s3cmd and AWS CLI profiles. Each becomes a profile; keys found in the files go to the
//! keyring when the profile is saved.
use std::path::{Path, PathBuf};

use crate::i18n::tr;
use crate::profile::Profile;

/// A connection found in another application's settings.
#[derive(Clone, Debug)]
pub struct Found {
    /// Where it was found, shown to the user ("rclone", "Cyberduck", …).
    pub source: String,
    pub profile: Profile,
}

/// The usual places of other applications' settings that exist on this computer.
pub fn default_files() -> Vec<PathBuf> {
    let home = gtk::glib::home_dir();
    let config = gtk::glib::user_config_dir();
    let mut files = vec![config.join("rclone/rclone.conf"), home.join(".s3cfg")];
    // Cyberduck keeps one file per bookmark (Windows and macOS; copied over by the user).
    for dir in [home.join(".duck/bookmarks"), home.join("Library/Group Containers/G69SCX94XU.duck/Library/Application Support/duck/Bookmarks")] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            files.extend(entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "duck")));
        }
    }
    files.retain(|p| p.is_file());
    files
}

/// Connections in one file, recognised by its content.
pub fn read_file(path: &Path) -> Result<Vec<Found>, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > 1024 * 1024 {
        return Err(tr("A connection file cannot be larger than 1 MB"));
    }
    let text = std::fs::read_to_string(path).map_err(|_| tr("The file is not a text file"))?;
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if text.contains("<plist") {
        return Ok(cyberduck(&text, &name).into_iter().collect());
    }
    if text.starts_with("RCLONE_ENCRYPT_") {
        return Err(tr("This rclone configuration is encrypted. Decrypt it with “rclone config show” first."));
    }
    let sections = ini(&text);
    if sections.iter().any(|(_, keys)| get(keys, "type").is_some()) {
        return Ok(rclone(&sections));
    }
    if sections.iter().any(|(_, keys)| get(keys, "host_base").is_some() || get(keys, "access_key").is_some()) {
        return Ok(s3cmd(&sections));
    }
    Err(tr("No connections were found in this file"))
}

/// The AWS CLI profiles, as connections that use them (no keys are copied).
pub fn aws_cli() -> Vec<Found> {
    crate::s3::connection::aws_profiles().into_iter().map(|name| Found {
        source: "AWS CLI".into(),
        profile: Profile { name: format!("AWS · {name}"), provider: "aws".into(), aws_profile: name, ..Default::default() },
    }).collect()
}

type Section = (String, Vec<(String, String)>);

/// INI sections with their keys, in file order.
fn ini(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') { continue; }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((name.trim().to_string(), Vec::new()));
        } else if let Some((key, value)) = line.split_once('=')
            && let Some(last) = sections.last_mut() {
            last.1.push((key.trim().to_lowercase(), value.trim().to_string()));
        }
    }
    sections
}

fn get<'a>(keys: &'a [(String, String)], name: &str) -> Option<&'a str> {
    keys.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

/// The provider preset that fits an endpoint, else a custom one.
fn provider_for(endpoint: &str) -> &'static str {
    let host = endpoint.to_lowercase();
    for (needle, id) in [("amazonaws.com", "aws"), ("supabase.co", "supabase"), ("r2.cloudflarestorage.com", "r2"), ("backblazeb2.com", "backblaze"),
        ("wasabisys.com", "wasabi"), ("digitaloceanspaces.com", "digitalocean"), ("your-objectstorage.com", "hetzner"), ("scw.cloud", "scaleway"),
        ("cloud.ovh.net", "ovh"), ("linodeobjects.com", "linode"), ("exo.io", "exoscale"), ("storjshare.io", "storj"), ("googleapis.com", "gcs")] {
        if host.contains(needle) { return id; }
    }
    if host.is_empty() { "aws" } else { "custom" }
}

fn with_scheme(endpoint: &str, https: bool) -> String {
    if endpoint.is_empty() || endpoint.contains("://") { endpoint.to_string() } else { format!("{}://{endpoint}", if https { "https" } else { "http" }) }
}

fn rclone(sections: &[Section]) -> Vec<Found> {
    sections.iter().filter(|(_, keys)| get(keys, "type") == Some("s3")).map(|(name, keys)| {
        let endpoint = with_scheme(get(keys, "endpoint").unwrap_or(""), true);
        let provider = match get(keys, "provider").unwrap_or("").to_lowercase().as_str() {
            "aws" => "aws", "minio" => "minio", "cloudflare" => "r2", "wasabi" => "wasabi", "digitalocean" => "digitalocean",
            "scaleway" => "scaleway", "storj" => "storj", "linode" => "linode", "gcs" => "gcs", _ => provider_for(&endpoint),
        };
        Found {
            source: "rclone".into(),
            profile: Profile {
                name: name.clone(),
                provider: provider.into(),
                endpoint: if provider == "aws" { String::new() } else { endpoint },
                region: get(keys, "region").unwrap_or("").to_string(),
                access_key: get(keys, "access_key_id").unwrap_or("").to_string(),
                secret_key: get(keys, "secret_access_key").unwrap_or("").to_string(),
                session_token: get(keys, "session_token").unwrap_or("").to_string(),
                path_style: get(keys, "force_path_style").is_none_or(|v| v != "false") && provider != "aws",
                // env_auth = true: credentials from the environment and ~/.aws.
                aws_profile: if get(keys, "env_auth") == Some("true") && get(keys, "access_key_id").is_none() { get(keys, "profile").unwrap_or("default").to_string() } else { String::new() },
                ..Default::default()
            },
        }
    }).collect()
}

fn s3cmd(sections: &[Section]) -> Vec<Found> {
    sections.iter().filter(|(_, keys)| get(keys, "access_key").is_some() || get(keys, "host_base").is_some()).map(|(name, keys)| {
        let https = get(keys, "use_https").is_none_or(|v| v.eq_ignore_ascii_case("true"));
        let host = get(keys, "host_base").unwrap_or("s3.amazonaws.com");
        let endpoint = with_scheme(host, https);
        let provider = provider_for(&endpoint);
        Found {
            source: "s3cmd".into(),
            profile: Profile {
                name: if name == "default" { format!("s3cmd · {host}") } else { name.clone() },
                provider: provider.into(),
                endpoint: if provider == "aws" { String::new() } else { endpoint },
                region: get(keys, "bucket_location").filter(|l| *l != "US").unwrap_or("").to_string(),
                access_key: get(keys, "access_key").unwrap_or("").to_string(),
                secret_key: get(keys, "secret_key").unwrap_or("").to_string(),
                session_token: get(keys, "access_token").unwrap_or("").to_string(),
                // host_bucket without %(bucket)s means the bucket goes into the path.
                path_style: !get(keys, "host_bucket").unwrap_or("%(bucket)s").contains("%(bucket)s"),
                ..Default::default()
            },
        }
    }).collect()
}

/// The string values of a property list's top-level dictionary.
fn plist_strings(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<key>") {
        rest = &rest[start + 5..];
        let Some(end) = rest.find("</key>") else { break };
        let key = unescape(&rest[..end]);
        rest = &rest[end + 6..];
        let trimmed = rest.trim_start();
        if let Some(value) = trimmed.strip_prefix("<string>")
            && let Some(end) = value.find("</string>") {
            out.push((key, unescape(&value[..end])));
        }
    }
    out
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn cyberduck(text: &str, file_name: &str) -> Option<Found> {
    let values = plist_strings(text);
    let value = |name: &str| values.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("");
    let protocol = value("Protocol");
    if !protocol.starts_with("s3") {
        return None;
    }
    let host = value("Hostname");
    let port = value("Port");
    let https = !value("Protocol").contains("http-") && !value("Provider").contains("http-");
    let mut endpoint = if host.is_empty() || host == "s3.amazonaws.com" { String::new() } else { with_scheme(host, https) };
    if !endpoint.is_empty() && !port.is_empty() && port != "443" && port != "80" {
        endpoint = format!("{endpoint}:{port}");
    }
    let provider = provider_for(&endpoint);
    let nickname = value("Nickname");
    let path = value("Path").trim_matches('/');
    let bucket = path.split('/').next().unwrap_or("").to_string();
    Some(Found {
        source: "Cyberduck".into(),
        profile: Profile {
            name: if nickname.is_empty() { if host.is_empty() { file_name.to_string() } else { host.to_string() } } else { nickname.to_string() },
            provider: provider.into(),
            endpoint,
            region: value("Region").to_string(),
            access_key: value("Username").to_string(),
            path_style: provider == "custom" || provider == "minio",
            buckets: if bucket.is_empty() { Vec::new() } else { vec![bucket] },
            ..Default::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rclone_remotes() {
        let conf = "[r2]\ntype = s3\nprovider = Cloudflare\naccess_key_id = AK\nsecret_access_key = SK\nendpoint = https://acc.r2.cloudflarestorage.com\n\n[local]\ntype = local\n\n[aws]\ntype = s3\nprovider = AWS\nenv_auth = true\nregion = eu-west-1\n";
        let found = rclone(&ini(conf));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].profile.provider, "r2");
        assert_eq!(found[0].profile.secret_key, "SK");
        assert_eq!(found[1].profile.aws_profile, "default");
        assert!(found[1].profile.endpoint.is_empty());
    }

    #[test]
    fn s3cmd_config() {
        let conf = "[default]\naccess_key = AK\nsecret_key = SK\nhost_base = minio.example.org:9000\nhost_bucket = minio.example.org:9000\nuse_https = False\n";
        let found = s3cmd(&ini(conf));
        assert_eq!(found[0].profile.endpoint, "http://minio.example.org:9000");
        assert!(found[0].profile.path_style);
        assert_eq!(found[0].profile.provider, "custom");
    }

    #[test]
    fn cyberduck_bookmark() {
        let plist = r#"<?xml version="1.0"?><plist version="1.0"><dict>
<key>Protocol</key><string>s3</string><key>Nickname</key><string>Work &amp; Co</string>
<key>Hostname</key><string>s3.amazonaws.com</string><key>Port</key><string>443</string>
<key>Username</key><string>AKIAEXAMPLE</string><key>Path</key><string>/my-bucket/docs</string></dict></plist>"#;
        let found = cyberduck(plist, "x").unwrap();
        assert_eq!(found.profile.name, "Work & Co");
        assert_eq!(found.profile.provider, "aws");
        assert_eq!(found.profile.buckets, vec!["my-bucket"]);
        assert!(found.profile.secret_key.is_empty());
        let sftp = plist.replace("<string>s3</string>", "<string>sftp</string>");
        assert!(cyberduck(&sftp, "x").is_none());
    }
}
