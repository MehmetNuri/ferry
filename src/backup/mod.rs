pub mod age;
pub mod foreign;
pub mod gpg;
pub mod password;

use std::path::Path;

use crate::i18n::tr;
use crate::profile::{self, Profile};

const MAX_SIZE: u64 = 1024 * 1024;

pub enum Protection {
    None,
    Password(String),
    Age(Vec<String>),
    Gpg(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Password,
    Age,
    AgePassphrase,
    Gpg,
}

pub fn invalid() -> String {
    tr("Invalid connection file or unsupported version")
}

pub fn read(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_SIZE {
        return Err(tr("A connection file cannot be larger than 1 MB"));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

pub fn detect(data: &[u8]) -> Result<Kind, String> {
    if data.starts_with(b"age-encryption.org/") || data.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----") {
        return Ok(if age::is_passphrase(data) { Kind::AgePassphrase } else { Kind::Age });
    }
    if gpg::looks_like(data) {
        return Ok(Kind::Gpg);
    }
    let value: serde_json::Value = serde_json::from_slice(data).map_err(|_| invalid())?;
    let format = value.get("format").and_then(|f| f.as_str());
    if format == Some(password::FORMAT) || format == Some(password::LEGACY_FORMAT) {
        return Ok(Kind::Password);
    }
    if value.is_array() { Ok(Kind::Plain) } else { Err(invalid()) }
}

async fn profiles(secrets: bool) -> Result<Vec<Profile>, String> {
    let mut out = Vec::new();
    for mut stored in profile::load() {
        if secrets {
            stored = profile::with_secrets(stored).await?;
        } else {
            stored.secret_key.clear();
            stored.session_token.clear();
        }
        out.push(stored);
    }
    Ok(out)
}

pub async fn export(path: std::path::PathBuf, protection: Protection) -> Result<usize, String> {
    let list = profiles(!matches!(protection, Protection::None)).await?;
    let plain = zeroize::Zeroizing::new(serde_json::to_vec(&list).map_err(|e| e.to_string())?);
    let data = match protection {
        Protection::None => serde_json::to_vec_pretty(&list).map_err(|e| e.to_string())?,
        Protection::Password(password) => {
            let password = zeroize::Zeroizing::new(password);
            tokio::task::spawn_blocking(move || password::seal(&plain, &password))
                .await
                .map_err(|e| e.to_string())??
        }
        Protection::Age(recipients) => {
            tokio::task::spawn_blocking(move || age::seal(&plain, &recipients)).await.map_err(|e| e.to_string())??
        }
        Protection::Gpg(fingerprint) => gpg::seal(&plain, &fingerprint).await?,
    };
    profile::write_private(&path, &data)?;
    Ok(list.len())
}

pub fn open_plain(data: &[u8]) -> Result<Vec<Profile>, String> {
    let mut list: Vec<Profile> = serde_json::from_slice(data).map_err(|_| invalid())?;
    for p in &mut list {
        p.secret_key.clear();
        p.session_token.clear();
    }
    Ok(list)
}

pub async fn restore(incoming: Vec<Profile>) -> Result<usize, String> {
    let mut added = 0;
    for mut p in incoming {
        if p.name.trim().is_empty() {
            continue;
        }
        p.id = String::new();
        profile::save(p, true).await?;
        added += 1;
    }
    Ok(added)
}
