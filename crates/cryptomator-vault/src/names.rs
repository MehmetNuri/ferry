use aes_siv::KeyInit;
use aes_siv::siv::Aes256Siv;
use data_encoding::{BASE32, BASE64URL, BASE64URL_NOPAD};
use sha1::{Digest, Sha1};
use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, Result, invalid};
use crate::keys::MasterKey;

pub const ROOT_DIR_ID: &str = "";
pub const MAX_DIR_ID_LEN: usize = 36;
pub const DATA_DIR: &str = "d";
pub const C9R_SUFFIX: &str = ".c9r";
pub const C9S_SUFFIX: &str = ".c9s";
pub const DIR_FILE: &str = "dir.c9r";
pub const SYMLINK_FILE: &str = "symlink.c9r";
pub const CONTENTS_FILE: &str = "contents.c9r";
pub const NAME_FILE: &str = "name.c9s";
pub const DIR_ID_BACKUP_FILE: &str = "dirid.c9r";

pub const MAX_ENCRYPTED_NAME_LEN: usize = 64 * 1024;

fn siv(key: &MasterKey) -> Aes256Siv {
    Aes256Siv::new_from_slice(key.siv_key().as_slice()).expect("AES-SIV-512 key is 64 bytes")
}

pub(crate) fn siv_encrypt(key: &MasterKey, plaintext: &[u8], ad: &[&[u8]]) -> Result<Vec<u8>> {
    siv(key).encrypt(ad.iter(), plaintext).map_err(|_| Error::InvalidArgument("AES-SIV encryption failed".into()))
}

pub(crate) fn siv_decrypt(key: &MasterKey, ciphertext: &[u8], ad: &[&[u8]]) -> Result<Vec<u8>> {
    if ciphertext.len() < 16 {
        return Err(Error::Authentication("SIV ciphertext too short"));
    }
    siv(key).decrypt(ad.iter(), ciphertext).map_err(|_| Error::Authentication("AES-SIV tag mismatch"))
}

pub fn hash_dir_id(key: &MasterKey, dir_id: &str) -> Result<String> {
    let enc = siv_encrypt(key, dir_id.as_bytes(), &[])?;
    Ok(BASE32.encode(&Sha1::digest(&enc)))
}

pub fn dir_path(key: &MasterKey, dir_id: &str) -> Result<String> {
    let h = hash_dir_id(key, dir_id)?;
    Ok(format!("{DATA_DIR}/{}/{}", &h[..2], &h[2..]))
}

pub fn parse_dir_id(bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAX_DIR_ID_LEN {
        return Err(invalid(format!("directory ID longer than {MAX_DIR_ID_LEN} bytes")));
    }
    let s = std::str::from_utf8(bytes).map_err(|_| invalid("directory ID is not UTF-8"))?;
    if !s.is_ascii() || s.contains('/') {
        return Err(invalid("directory ID contains invalid characters"));
    }
    Ok(s.to_owned())
}

fn check_cleartext_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(Error::InvalidArgument(format!("invalid file name {name:?}")));
    }
    if name.contains('/') || name.contains('\0') {
        return Err(Error::InvalidArgument("file name contains '/' or NUL".into()));
    }
    Ok(())
}

pub fn encrypt_name(key: &MasterKey, name: &str, parent_dir_id: &str) -> Result<String> {
    check_cleartext_name(name)?;
    let nfc: String = name.nfc().collect();
    let ct = siv_encrypt(key, nfc.as_bytes(), &[parent_dir_id.as_bytes()])?;
    Ok(format!("{}{C9R_SUFFIX}", BASE64URL.encode(&ct)))
}

/// Rejects ., .., /, NUL and empty names: path traversal from hostile vaults.
pub fn decrypt_name(key: &MasterKey, encrypted: &str, parent_dir_id: &str) -> Result<String> {
    let base = encrypted.strip_suffix(C9R_SUFFIX).unwrap_or(encrypted);
    if base.len() > MAX_ENCRYPTED_NAME_LEN {
        return Err(invalid("encrypted name too long"));
    }
    let bytes = BASE64URL_NOPAD
        .decode(base.trim_end_matches('=').as_bytes())
        .map_err(|_| invalid("encrypted name is not valid base64url"))?;
    let pt = siv_decrypt(key, &bytes, &[parent_dir_id.as_bytes()])?;
    let name = String::from_utf8(pt).map_err(|_| invalid("decrypted name is not UTF-8"))?;
    check_cleartext_name(&name).map_err(|_| invalid("decrypted name is not a valid file name"))?;
    Ok(name)
}

pub fn shorten_name(encrypted_name: &str) -> String {
    format!("{}{C9S_SUFFIX}", BASE64URL.encode(&Sha1::digest(encrypted_name.as_bytes())))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeName {
    Regular(String),
    Shortened { short_name: String, full_name: String },
}

impl NodeName {
    pub fn storage_name(&self) -> &str {
        match self {
            NodeName::Regular(n) => n,
            NodeName::Shortened { short_name, .. } => short_name,
        }
    }

    pub fn file_contents_path(&self) -> String {
        match self {
            NodeName::Regular(n) => n.clone(),
            NodeName::Shortened { short_name, .. } => format!("{short_name}/{CONTENTS_FILE}"),
        }
    }

    pub fn dir_file_path(&self) -> String {
        format!("{}/{DIR_FILE}", self.storage_name())
    }

    pub fn symlink_file_path(&self) -> String {
        format!("{}/{SYMLINK_FILE}", self.storage_name())
    }

    pub fn name_file_path(&self) -> Option<String> {
        match self {
            NodeName::Regular(_) => None,
            NodeName::Shortened { short_name, .. } => Some(format!("{short_name}/{NAME_FILE}")),
        }
    }
}

pub fn node_name(key: &MasterKey, name: &str, parent_dir_id: &str, threshold: usize) -> Result<NodeName> {
    let full = encrypt_name(key, name, parent_dir_id)?;
    if full.len() > threshold {
        Ok(NodeName::Shortened { short_name: shorten_name(&full), full_name: full })
    } else {
        Ok(NodeName::Regular(full))
    }
}

pub fn parse_name_file(short_name: &str, name_file: &[u8]) -> Result<String> {
    if name_file.len() > MAX_ENCRYPTED_NAME_LEN {
        return Err(invalid("name.c9s too long"));
    }
    let full = std::str::from_utf8(name_file).map_err(|_| invalid("name.c9s is not UTF-8"))?.trim();
    if shorten_name(full) != short_name {
        return Err(Error::Authentication("name.c9s does not match its .c9s directory name"));
    }
    Ok(full.to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Regular,
    Shortened,
    DirIdBackup,
    Other,
}

pub fn classify_entry(name: &str) -> EntryKind {
    if name == DIR_ID_BACKUP_FILE {
        EntryKind::DirIdBackup
    } else if name.ends_with(C9S_SUFFIX) {
        EntryKind::Shortened
    } else if name.ends_with(C9R_SUFFIX) {
        EntryKind::Regular
    } else {
        EntryKind::Other
    }
}

pub fn new_dir_id() -> Result<String> {
    crate::jwt::random_uuid()
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_encoding::HEXLOWER_PERMISSIVE as HEX;

    // Vectors from cryptomator/siv-mode; separate keys also check siv_key() order.
    #[test]
    fn siv_mode_reference_vectors() {
        let data = include_str!("../tests/fixtures/siv-mode-testcases.txt");
        let mut count = 0;
        for line in data.lines().filter(|l| !l.is_empty()) {
            let f: Vec<&str> = line.split(';').collect();
            let ctr: [u8; 32] = HEX.decode(f[0].as_bytes()).unwrap().try_into().unwrap();
            let mac: [u8; 32] = HEX.decode(f[1].as_bytes()).unwrap().try_into().unwrap();
            let pt = HEX.decode(f[2].as_bytes()).unwrap();
            let n: usize = f[3].parse().unwrap();
            let ad: Vec<Vec<u8>> = (0..n).map(|i| HEX.decode(f[4 + i].as_bytes()).unwrap()).collect();
            let expected = HEX.decode(f[4 + n].as_bytes()).unwrap();
            let key = MasterKey::from_parts(ctr, mac);
            let ad_refs: Vec<&[u8]> = ad.iter().map(Vec::as_slice).collect();
            assert_eq!(siv_encrypt(&key, &pt, &ad_refs).unwrap(), expected, "line {line}");
            assert_eq!(siv_decrypt(&key, &expected, &ad_refs).unwrap(), pt);
            count += 1;
        }
        assert_eq!(count, 400);
    }

    #[test]
    fn decrypt_rejects_unsafe_names() {
        let key = MasterKey::from_parts([7; 32], [9; 32]);
        for bad in ["a/b", "..", ".", "", "x\0y"] {
            let ct = siv_encrypt(&key, bad.as_bytes(), &[b"parent"]).unwrap();
            let enc = format!("{}.c9r", BASE64URL.encode(&ct));
            assert!(matches!(decrypt_name(&key, &enc, "parent"), Err(Error::InvalidFormat(_))), "{bad:?}");
        }
        let ct = siv_encrypt(&key, &[0xff, 0xfe], &[b"parent"]).unwrap();
        let enc = format!("{}.c9r", BASE64URL.encode(&ct));
        assert!(matches!(decrypt_name(&key, &enc, "parent"), Err(Error::InvalidFormat(_))));
    }
}
