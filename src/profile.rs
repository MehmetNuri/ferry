use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::config;
use crate::i18n::tr;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub endpoint: String,
    pub region: String,
    pub access_key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub secret_key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session_token: String,
    pub path_style: bool,
    pub project_ref: String,
    pub buckets: Vec<String>,
    pub storage_class: String,
    pub encryption: String,
    pub kms_key: String,
    pub aws_profile: String,
    pub role_arn: String,
    pub external_id: String,
    pub mfa_serial: String,
    pub accelerate: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ca_certificate: String,
    pub remote_path: String,
    pub private_key: String,
    pub host_key: String,
    pub jump_host: String,
    pub jump_host_key: String,
    pub ftp_security: String,
    pub online_account: String,
}

pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub endpoint: &'static str,
    pub region: &'static str,
    pub region_hint: &'static str,
    pub path_style: bool,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "supabase",
        label: "Supabase Storage",
        endpoint: "",
        region: "",
        region_hint: "eu-central-1",
        path_style: true,
    },
    Preset { id: "aws", label: "Amazon S3", endpoint: "", region: "", region_hint: "us-east-1", path_style: false },
    Preset { id: "minio", label: "MinIO", endpoint: "", region: "", region_hint: "us-east-1", path_style: true },
    Preset { id: "r2", label: "Cloudflare R2", endpoint: "", region: "auto", region_hint: "auto", path_style: false },
    Preset {
        id: "backblaze",
        label: "Backblaze B2",
        endpoint: "https://s3.{region}.backblazeb2.com",
        region: "",
        region_hint: "us-west-004",
        path_style: true,
    },
    Preset {
        id: "wasabi",
        label: "Wasabi",
        endpoint: "https://s3.{region}.wasabisys.com",
        region: "us-east-1",
        region_hint: "us-east-1",
        path_style: false,
    },
    Preset {
        id: "digitalocean",
        label: "DigitalOcean Spaces",
        endpoint: "https://{region}.digitaloceanspaces.com",
        region: "",
        region_hint: "nyc3",
        path_style: false,
    },
    Preset {
        id: "hetzner",
        label: "Hetzner Object Storage",
        endpoint: "https://{region}.your-objectstorage.com",
        region: "",
        region_hint: "fsn1",
        path_style: false,
    },
    Preset {
        id: "scaleway",
        label: "Scaleway Object Storage",
        endpoint: "https://s3.{region}.scw.cloud",
        region: "",
        region_hint: "fr-par",
        path_style: false,
    },
    Preset {
        id: "ovh",
        label: "OVHcloud Object Storage",
        endpoint: "https://s3.{region}.io.cloud.ovh.net",
        region: "",
        region_hint: "gra",
        path_style: false,
    },
    Preset {
        id: "linode",
        label: "Akamai (Linode) Object Storage",
        endpoint: "https://{region}.linodeobjects.com",
        region: "",
        region_hint: "us-east-1",
        path_style: false,
    },
    Preset {
        id: "exoscale",
        label: "Exoscale SOS",
        endpoint: "https://sos-{region}.exo.io",
        region: "",
        region_hint: "ch-gva-2",
        path_style: false,
    },
    Preset {
        id: "storj",
        label: "Storj",
        endpoint: "https://gateway.storjshare.io",
        region: "us-1",
        region_hint: "us-1",
        path_style: true,
    },
    Preset {
        id: "gcs",
        label: "Google Cloud Storage (HMAC)",
        endpoint: "https://storage.googleapis.com",
        region: "auto",
        region_hint: "auto",
        path_style: true,
    },
    Preset { id: "gdrive", label: "Google Drive", endpoint: "", region: "", region_hint: "", path_style: false },
    Preset { id: "azure", label: "Azure Blob Storage", endpoint: "", region: "", region_hint: "", path_style: false },
    Preset { id: "sftp", label: "SFTP (SSH)", endpoint: "", region: "", region_hint: "", path_style: false },
    Preset { id: "ftp", label: "FTP", endpoint: "", region: "", region_hint: "", path_style: false },
    Preset { id: "webdav", label: "WebDAV", endpoint: "", region: "", region_hint: "", path_style: false },
    Preset {
        id: "nextcloud",
        label: "Nextcloud / ownCloud",
        endpoint: "",
        region: "",
        region_hint: "",
        path_style: false,
    },
    Preset { id: "custom", label: "", endpoint: "", region: "", region_hint: "us-east-1", path_style: true },
];

pub fn config_dir() -> PathBuf {
    let base = gtk::glib::user_config_dir();
    let dir = base.join(config::APP_ID);
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
        for name in ["profiles.json", "backups.json"] {
            let old = base.join("s3-browser").join(name);
            if old.exists() && std::fs::rename(&old, dir.join(name)).is_err() {
                let _ = std::fs::copy(&old, dir.join(name));
            }
        }
    }
    let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    dir
}

fn profiles_path() -> PathBuf {
    config_dir().join("profiles.json")
}

pub fn write_private(path: &std::path::Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // Fresh random name so an existing file or symlink can't lend us its permissions.
    let tmp = path.with_extension(format!("{}.tmp", gtk::glib::uuid_string_random()));
    let mut file =
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).map_err(|e| e.to_string())?;
    file.write_all(data).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn sorted(mut profiles: Vec<Profile>) -> Vec<Profile> {
    profiles.sort_by_key(|p| p.name.to_lowercase());
    profiles
}

pub fn load() -> Vec<Profile> {
    let Ok(data) = std::fs::read(profiles_path()) else { return Vec::new() };
    sorted(serde_json::from_slice(&data).unwrap_or_default())
}

fn store(profiles: &[Profile]) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(profiles).map_err(|e| e.to_string())?;
    write_private(&profiles_path(), &data)
}

fn attributes<'a>(id: &'a str, field: &'a str) -> HashMap<&'a str, &'a str> {
    HashMap::from([("application", config::APP_ID), ("profile", id), ("field", field)])
}

async fn keyring() -> Result<oo7::Keyring, String> {
    let keyring = oo7::Keyring::new().await.map_err(|e| e.to_string())?;
    keyring.unlock().await.map_err(|e| e.to_string())?;
    Ok(keyring)
}

async fn read_secret(keyring: &oo7::Keyring, id: &str, field: &str) -> Result<String, String> {
    let items = keyring.search_items(&attributes(id, field)).await.map_err(|e| e.to_string())?;
    if let Some(item) = items.first() {
        let secret = item.secret().await.map_err(|e| e.to_string())?;
        return Ok(String::from_utf8_lossy(secret.as_bytes()).into_owned());
    }
    Ok(String::new())
}

async fn write_secret(keyring: &oo7::Keyring, profile: &Profile, field: &str, value: &str) -> Result<(), String> {
    let attrs = attributes(&profile.id, field);
    if value.is_empty() {
        return keyring.delete(&attrs).await.map_err(|e| e.to_string());
    }
    let label = format!("Ferry: {} ({field})", profile.name);
    keyring.create_item(&label, &attrs, value, true).await.map_err(|e| e.to_string())
}

pub async fn with_secrets(mut profile: Profile) -> Result<Profile, String> {
    if !profile.secret_key.is_empty() {
        if let Ok(keyring) = keyring().await
            && write_secret(&keyring, &profile, "secret", &profile.secret_key).await.is_ok()
            && write_secret(&keyring, &profile, "token", &profile.session_token).await.is_ok()
        {
            let mut profiles = load();
            if let Some(stored) = profiles.iter_mut().find(|p| p.id == profile.id) {
                stored.secret_key.clear();
                stored.session_token.clear();
            }
            let _ = store(&profiles);
        }
        return Ok(profile);
    }
    if profile.access_key.is_empty() {
        return Ok(profile);
    }
    let keyring = keyring()
        .await
        .map_err(|e| format!("{} ({e})", tr("The saved access key could not be read from the keyring")))?;
    profile.secret_key = read_secret(&keyring, &profile.id, "secret").await?;
    profile.session_token = read_secret(&keyring, &profile.id, "token").await?;
    Ok(profile)
}

fn new_id() -> String {
    gtk::glib::uuid_string_random().to_string()
}

pub const KEYRING_UNAVAILABLE: &str = "keyring-unavailable";

pub async fn save(mut profile: Profile, allow_plain: bool) -> Result<(Profile, bool), String> {
    if profile.name.trim().is_empty() {
        return Err(tr("The connection needs a name"));
    }
    if profile.id.is_empty() {
        profile.id = new_id();
    }
    let mut on_disk = profile.clone();
    let mut in_keyring = false;
    if let Ok(keyring) = keyring().await {
        let secret = write_secret(&keyring, &profile, "secret", &profile.secret_key).await;
        let token = write_secret(&keyring, &profile, "token", &profile.session_token).await;
        if secret.is_ok() && token.is_ok() {
            on_disk.secret_key.clear();
            on_disk.session_token.clear();
            in_keyring = true;
        } else {
            let _ = keyring.delete(&attributes(&profile.id, "secret")).await;
            let _ = keyring.delete(&attributes(&profile.id, "token")).await;
        }
    }
    let has_secrets = !profile.secret_key.is_empty() || !profile.session_token.is_empty();
    if !in_keyring && has_secrets && !allow_plain {
        return Err(KEYRING_UNAVAILABLE.to_string());
    }
    let mut profiles = load();
    profiles.retain(|p| p.id != profile.id);
    profiles.push(on_disk);
    store(&sorted(profiles))?;
    crate::s3::forget_client(&profile.id);
    Ok((profile, in_keyring))
}

pub async fn delete(id: String) -> Result<(), String> {
    let mut profiles = load();
    profiles.retain(|p| p.id != id);
    store(&profiles)?;
    crate::s3::forget_client(&id);
    if let Ok(keyring) = keyring().await {
        let _ = keyring.delete(&attributes(&id, "secret")).await;
        let _ = keyring.delete(&attributes(&id, "token")).await;
    }
    Ok(())
}

pub fn set_linked(id: &str, bucket: &str, linked: bool) -> Result<(), String> {
    let mut profiles = load();
    if let Some(profile) = profiles.iter_mut().find(|p| p.id == id) {
        profile.buckets.retain(|b| b != bucket);
        if linked {
            profile.buckets.push(bucket.to_string());
        }
    }
    store(&profiles)
}

pub(crate) fn getrandom(buffer: &mut [u8]) -> Result<(), String> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut r| r.read_exact(buffer))
        .map_err(|e| format!("{} ({e})", tr("No random numbers are available")))
}
