//! Directory IDs, directory paths and file name encryption.
//!
//! * `dirPath(dirId) = "d/" + h[0..2] + "/" + h[2..32]` with
//!   `h = BASE32(SHA1(AES-SIV(dirId, no associated data)))`.
//! * `encName = BASE64URL(AES-SIV(NFC(name), AD = parent dirId)) + ".c9r"`.
//! * If `encName` is longer than the shortening threshold it is stored as
//!   `BASE64URL(SHA1(encName)) + ".c9s"`, a directory holding `name.c9s`
//!   (the full `encName`) plus `contents.c9r`, `dir.c9r` or `symlink.c9r`.

use aes_siv::KeyInit;
use aes_siv::siv::Aes256Siv;
use data_encoding::{BASE32, BASE64URL, BASE64URL_NOPAD};
use sha1::{Digest, Sha1};
use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, Result, invalid};
use crate::keys::MasterKey;

/// Directory ID of the vault root.
pub const ROOT_DIR_ID: &str = "";
/// Maximum length of a directory ID (a UUID is 36 ASCII chars).
pub const MAX_DIR_ID_LEN: usize = 36;
/// Name of the vault data directory.
pub const DATA_DIR: &str = "d";
/// Suffix of regular encrypted names.
pub const C9R_SUFFIX: &str = ".c9r";
/// Suffix of shortened names.
pub const C9S_SUFFIX: &str = ".c9s";
/// File inside a directory node that holds its directory ID.
pub const DIR_FILE: &str = "dir.c9r";
/// File inside a symlink node that holds the encrypted target.
pub const SYMLINK_FILE: &str = "symlink.c9r";
/// File inside a shortened file node that holds the file contents.
pub const CONTENTS_FILE: &str = "contents.c9r";
/// File inside a shortened node that holds the full encrypted name.
pub const NAME_FILE: &str = "name.c9s";
/// Optional backup of a directory's own ID inside its content directory
/// (encrypted like file contents).
pub const DIR_ID_BACKUP_FILE: &str = "dirid.c9r";

/// Maximum accepted length of a `name.c9s` file / encrypted name.
pub const MAX_ENCRYPTED_NAME_LEN: usize = 64 * 1024;

fn siv(key: &MasterKey) -> Aes256Siv {
    // Aes256Siv takes a 512 bit key: macKey || encKey (see MasterKey::siv_key).
    Aes256Siv::new_from_slice(key.siv_key().as_slice()).expect("AES-SIV-512 key is 64 bytes")
}

/// AES-SIV encryption with Cryptomator's key order and a list of associated data.
pub(crate) fn siv_encrypt(key: &MasterKey, plaintext: &[u8], ad: &[&[u8]]) -> Result<Vec<u8>> {
    siv(key)
        .encrypt(ad.iter(), plaintext)
        .map_err(|_| Error::InvalidArgument("AES-SIV encryption failed".into()))
}

/// AES-SIV decryption with Cryptomator's key order.
pub(crate) fn siv_decrypt(key: &MasterKey, ciphertext: &[u8], ad: &[&[u8]]) -> Result<Vec<u8>> {
    if ciphertext.len() < 16 {
        return Err(Error::Authentication("SIV ciphertext too short"));
    }
    siv(key)
        .decrypt(ad.iter(), ciphertext)
        .map_err(|_| Error::Authentication("AES-SIV tag mismatch"))
}

/// `BASE32(SHA1(AES-SIV(dirId)))`, 32 characters.
pub fn hash_dir_id(key: &MasterKey, dir_id: &str) -> Result<String> {
    let enc = siv_encrypt(key, dir_id.as_bytes(), &[])?;
    Ok(BASE32.encode(&Sha1::digest(&enc)))
}

/// Storage path of a directory's contents, relative to the vault root,
/// e.g. `d/BZ/R4VZSS5PEF7TU3PMFIMON5GJRNBDWA` (no trailing slash).
pub fn dir_path(key: &MasterKey, dir_id: &str) -> Result<String> {
    let h = hash_dir_id(key, dir_id)?;
    Ok(format!("{DATA_DIR}/{}/{}", &h[..2], &h[2..]))
}

/// Validate and decode the content of a `dir.c9r` file (or a decrypted
/// `dirid.c9r`) into a directory ID.
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

/// Encrypt a cleartext name for the directory `parent_dir_id`.
///
/// The name is NFC-normalized first. Returns the full encrypted name with
/// the `.c9r` suffix (not yet shortened; see [`node_name`]).
pub fn encrypt_name(key: &MasterKey, name: &str, parent_dir_id: &str) -> Result<String> {
    check_cleartext_name(name)?;
    let nfc: String = name.nfc().collect();
    let ct = siv_encrypt(key, nfc.as_bytes(), &[parent_dir_id.as_bytes()])?;
    Ok(format!("{}{C9R_SUFFIX}", BASE64URL.encode(&ct)))
}

/// Decrypt an encrypted name (with or without `.c9r` suffix) that lives in
/// directory `parent_dir_id`.
///
/// Names that decrypt to something unusable as a path component (empty,
/// `.`/`..`, containing `/` or NUL) are rejected, so a malicious vault cannot
/// cause path traversal in a client.
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

/// `BASE64URL(SHA1(encrypted_name)) + ".c9s"` for a full encrypted name
/// (including its `.c9r` suffix).
pub fn shorten_name(encrypted_name: &str) -> String {
    format!("{}{C9S_SUFFIX}", BASE64URL.encode(&Sha1::digest(encrypted_name.as_bytes())))
}

/// How a node is stored inside its parent's content directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeName {
    /// The encrypted name (`xxx.c9r`) is used directly.
    Regular(String),
    /// The name is too long: a `.c9s` directory named `short_name` holds a
    /// `name.c9s` file whose content is `full_name`.
    Shortened {
        /// The `xxx.c9s` directory name.
        short_name: String,
        /// The full `xxx.c9r` encrypted name to write into `name.c9s`.
        full_name: String,
    },
}

impl NodeName {
    /// The name of the entry inside the parent content directory.
    pub fn storage_name(&self) -> &str {
        match self {
            NodeName::Regular(n) => n,
            NodeName::Shortened { short_name, .. } => short_name,
        }
    }

    /// Path (relative to the parent content directory) of the object holding
    /// a regular file's contents: `xxx.c9r` or `xxx.c9s/contents.c9r`.
    pub fn file_contents_path(&self) -> String {
        match self {
            NodeName::Regular(n) => n.clone(),
            NodeName::Shortened { short_name, .. } => format!("{short_name}/{CONTENTS_FILE}"),
        }
    }

    /// Path of the `dir.c9r` file for a directory node.
    pub fn dir_file_path(&self) -> String {
        format!("{}/{DIR_FILE}", self.storage_name())
    }

    /// Path of the `symlink.c9r` file for a symlink node.
    pub fn symlink_file_path(&self) -> String {
        format!("{}/{SYMLINK_FILE}", self.storage_name())
    }

    /// Path of the `name.c9s` file, if the name is shortened.
    pub fn name_file_path(&self) -> Option<String> {
        match self {
            NodeName::Regular(_) => None,
            NodeName::Shortened { short_name, .. } => Some(format!("{short_name}/{NAME_FILE}")),
        }
    }
}

/// Encrypt `name` and apply name shortening with `threshold`
/// (shorten when the encrypted name incl. `.c9r` is longer than `threshold`).
pub fn node_name(key: &MasterKey, name: &str, parent_dir_id: &str, threshold: usize) -> Result<NodeName> {
    let full = encrypt_name(key, name, parent_dir_id)?;
    if full.len() > threshold {
        Ok(NodeName::Shortened { short_name: shorten_name(&full), full_name: full })
    } else {
        Ok(NodeName::Regular(full))
    }
}

/// Decode a `name.c9s` file of the `.c9s` entry `short_name` and check that it
/// really hashes to `short_name`. Returns the full encrypted name.
pub fn parse_name_file(short_name: &str, name_file: &[u8]) -> Result<String> {
    if name_file.len() > MAX_ENCRYPTED_NAME_LEN {
        return Err(invalid("name.c9s too long"));
    }
    let full = std::str::from_utf8(name_file)
        .map_err(|_| invalid("name.c9s is not UTF-8"))?
        .trim();
    if shorten_name(full) != short_name {
        return Err(Error::Authentication("name.c9s does not match its .c9s directory name"));
    }
    Ok(full.to_owned())
}

/// Kind of a raw entry found in a ciphertext content directory listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// `xxx.c9r`: a file object (regular file) or a prefix (directory or
    /// symlink node, distinguished by containing `dir.c9r`/`symlink.c9r`).
    Regular,
    /// `xxx.c9s`: a shortened node; read its `name.c9s`.
    Shortened,
    /// `dirid.c9r`: the directory ID backup; not a child node.
    DirIdBackup,
    /// Anything else (sync conflict copies, `.DS_Store`, ...).
    Other,
}

/// Classify an entry name of a ciphertext content directory.
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

/// A fresh random directory ID (UUID v4, 36 chars).
pub fn new_dir_id() -> Result<String> {
    crate::jwt::random_uuid()
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_encoding::HEXLOWER_PERMISSIVE as HEX;

    /// Cryptomator's own AES-SIV test vectors (siv-mode). Each line gives the
    /// CTR key and the MAC key separately; we build `MasterKey { enc: ctrKey,
    /// mac: macKey }`, which checks that `siv_key()` uses Cryptomator's order.
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
