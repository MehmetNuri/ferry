//! Password-protected backups: AES-256-GCM under a key derived with Argon2id. The
//! header is authenticated too, so its parameters cannot be changed unnoticed.
//! Version 1 files (PBKDF2-SHA256) can still be opened.
use aes_gcm::aead::{Aead, KeyInit, Payload};
use base64::Engine;
use zeroize::Zeroizing;

use super::invalid;
use crate::i18n::tr;
use crate::profile::Profile;

pub const FORMAT: &str = "ferry-backup";
/// The format name of backups written by earlier versions.
pub const LEGACY_FORMAT: &str = "s3-browser-encrypted-profiles";
/// RFC 9106's second recommended setting: 64 MiB, three passes.
const MEMORY_KIB: u32 = 64 * 1024;
const PASSES: u32 = 3;
const LANES: u32 = 1;
/// The fewest characters a backup password may have.
pub const MIN_LENGTH: usize = 12;

fn argon2_key(password: &str, salt: &[u8], memory: u32, passes: u32, lanes: u32) -> Result<Zeroizing<[u8; 32]>, String> {
    let params = argon2::Params::new(memory, passes, lanes, Some(32)).map_err(|_| invalid())?;
    let mut key = Zeroizing::new([0u8; 32]);
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(|e| e.to_string())?;
    Ok(key)
}

/// The authenticated header: everything needed to derive the key.
fn header(format: &str, memory: u32, passes: u32, lanes: u32, salt: &str) -> String {
    format!("{format}|2|argon2id|{memory}|{passes}|{lanes}|{salt}")
}

pub fn seal(plain: &[u8], password: &str) -> Result<Vec<u8>, String> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    crate::profile::getrandom(&mut salt)?;
    crate::profile::getrandom(&mut nonce)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let salt_text = b64.encode(salt);
    let key = argon2_key(password, &salt, MEMORY_KIB, PASSES, LANES)?;
    let cipher = aes_gcm::Aes256Gcm::new_from_slice(key.as_ref()).map_err(|e| e.to_string())?;
    let aad = header(FORMAT, MEMORY_KIB, PASSES, LANES, &salt_text);
    let sealed = cipher.encrypt(&nonce.into(), Payload { msg: plain, aad: aad.as_bytes() }).map_err(|e| e.to_string())?;
    let document = serde_json::json!({
        "format": FORMAT, "version": 2, "kdf": "argon2id",
        "memory": MEMORY_KIB, "passes": PASSES, "lanes": LANES,
        "salt": salt_text, "nonce": b64.encode(nonce), "data": b64.encode(sealed),
    });
    serde_json::to_vec_pretty(&document).map_err(|e| e.to_string())
}

pub fn open(data: &[u8], password: &str) -> Result<Vec<Profile>, String> {
    let document: serde_json::Value = serde_json::from_slice(data).map_err(|_| invalid())?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let text = |name: &str| document.get(name).and_then(|v| v.as_str()).ok_or_else(invalid);
    let bytes = |name: &str| text(name).and_then(|v| b64.decode(v).map_err(|_| invalid()));
    let number = |name: &str| document.get(name).and_then(|v| v.as_u64()).ok_or_else(invalid);
    let (salt, sealed) = (bytes("salt")?, bytes("data")?);
    let nonce: [u8; 12] = bytes("nonce")?.try_into().map_err(|_| invalid())?;
    let wrong = || tr("Wrong password, or the file is damaged");
    let plain = Zeroizing::new(match number("version").unwrap_or(1) {
        1 => {
            // A crafted file must not keep a thread busy for hours.
            let rounds = number("iterations").unwrap_or(600_000).clamp(100_000, 10_000_000) as u32;
            let mut key = Zeroizing::new([0u8; 32]);
            pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &salt, rounds, key.as_mut());
            let cipher = aes_gcm::Aes256Gcm::new_from_slice(key.as_ref()).map_err(|e| e.to_string())?;
            cipher.decrypt(&nonce.into(), sealed.as_slice()).map_err(|_| wrong())?
        }
        2 => {
            if text("kdf")? != "argon2id" { return Err(invalid()); }
            // Limits keep a crafted file from using gigabytes of memory or minutes of work.
            let memory = number("memory")?;
            let passes = number("passes")?;
            let lanes = number("lanes")?;
            if !(8 * 1024..=1024 * 1024).contains(&memory) || !(1..=10).contains(&passes) || !(1..=8).contains(&lanes) {
                return Err(invalid());
            }
            let (memory, passes, lanes) = (memory as u32, passes as u32, lanes as u32);
            let key = argon2_key(password, &salt, memory, passes, lanes)?;
            let cipher = aes_gcm::Aes256Gcm::new_from_slice(key.as_ref()).map_err(|e| e.to_string())?;
            let aad = header(text("format")?, memory, passes, lanes, text("salt")?);
            cipher.decrypt(&nonce.into(), Payload { msg: &sealed, aad: aad.as_bytes() }).map_err(|_| wrong())?
        }
        _ => return Err(invalid()),
    });
    serde_json::from_slice(&plain).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip() {
        let list = vec![Profile { name: "a".into(), secret_key: "s3cr3t".into(), ..Default::default() }];
        let plain = serde_json::to_vec(&list).unwrap();
        let sealed = seal(&plain, "correct horse battery").unwrap();
        assert!(!String::from_utf8_lossy(&sealed).contains("s3cr3t"));
        assert_eq!(open(&sealed, "correct horse battery").unwrap()[0].secret_key, "s3cr3t");
        assert!(open(&sealed, "wrong password!").is_err());
        // The parameters are authenticated: a weaker setting in the header is refused.
        let tampered = String::from_utf8(sealed).unwrap().replace("\"passes\": 3", "\"passes\": 1");
        assert!(open(tampered.as_bytes(), "correct horse battery").is_err());
    }
}
