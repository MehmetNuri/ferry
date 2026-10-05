use aes_kw::{KeyInit, KwAes256};
use data_encoding::BASE64;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::error::{Error, Result, invalid};
use crate::keys::{MasterKey, SUBKEY_LEN, random_bytes};

pub const MASTERKEY_FILE_VERSION: u32 = 999;

pub const MAX_SCRYPT_COST: u32 = 1 << 20;
pub const MAX_SCRYPT_BLOCK_SIZE: u32 = 32;
pub const MAX_SCRYPT_MEMORY: u64 = 256 * 1024 * 1024;
pub const MAX_SALT_LEN: usize = 1024;
pub const SALT_LEN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScryptParams {
    pub cost: u32,
    pub block_size: u32,
}

impl Default for ScryptParams {
    fn default() -> Self {
        Self { cost: 32768, block_size: 8 }
    }
}

impl ScryptParams {
    /// Reject instead of clamping: a clamped N/r would derive a wrong KEK anyway.
    pub fn validate(&self) -> Result<()> {
        let n = self.cost;
        if n < 2 || !n.is_power_of_two() {
            return Err(invalid(format!("scryptCostParam {n} is not a power of two > 1")));
        }
        if n > MAX_SCRYPT_COST {
            return Err(Error::Unsupported(format!("scryptCostParam {n} exceeds limit {MAX_SCRYPT_COST}")));
        }
        let r = self.block_size;
        if r == 0 {
            return Err(invalid("scryptBlockSize must be positive"));
        }
        if r > MAX_SCRYPT_BLOCK_SIZE {
            return Err(Error::Unsupported(format!("scryptBlockSize {r} exceeds limit {MAX_SCRYPT_BLOCK_SIZE}")));
        }
        let mem = 128 * u64::from(n) * u64::from(r);
        if mem > MAX_SCRYPT_MEMORY {
            return Err(Error::Unsupported(format!(
                "scrypt would need {mem} bytes of memory (limit {MAX_SCRYPT_MEMORY})"
            )));
        }
        Ok(())
    }
}

pub fn derive_kek(password: &str, salt: &[u8], params: ScryptParams) -> Result<Zeroizing<[u8; 32]>> {
    params.validate()?;
    if salt.len() > MAX_SALT_LEN {
        return Err(invalid("scrypt salt too long"));
    }
    let log_n = params.cost.trailing_zeros() as u8;
    let p = scrypt::Params::new(log_n, params.block_size, 1).map_err(|_| invalid("invalid scrypt parameters"))?;
    let mut kek = Zeroizing::new([0u8; 32]);
    scrypt::scrypt(password.as_bytes(), salt, &p, kek.as_mut_slice())
        .map_err(|_| invalid("invalid scrypt output length"))?;
    Ok(kek)
}

pub fn wrap_key(kek: &[u8; 32], key: &[u8; SUBKEY_LEN]) -> Result<[u8; SUBKEY_LEN + 8]> {
    let kw = KwAes256::new_from_slice(kek).map_err(|_| invalid("bad KEK length"))?;
    let mut out = [0u8; SUBKEY_LEN + 8];
    kw.wrap_key(key, &mut out).map_err(|_| invalid("AES key wrap failed"))?;
    Ok(out)
}

pub fn unwrap_key(kek: &[u8; 32], wrapped: &[u8]) -> Result<Zeroizing<[u8; SUBKEY_LEN]>> {
    if wrapped.len() != SUBKEY_LEN + 8 {
        return Err(invalid(format!("wrapped key must be 40 bytes, got {}", wrapped.len())));
    }
    let kw = KwAes256::new_from_slice(kek).map_err(|_| invalid("bad KEK length"))?;
    let mut out = Zeroizing::new([0u8; SUBKEY_LEN]);
    kw.unwrap_key(wrapped, out.as_mut_slice()).map_err(|e| match e {
        aes_kw::Error::IntegrityCheckFailed => Error::InvalidPassword,
        _ => invalid("AES key unwrap failed"),
    })?;
    Ok(out)
}

fn version_mac(mac_key: &[u8; SUBKEY_LEN], version: u32) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(mac_key).expect("HMAC accepts keys of any length");
    mac.update(&version.to_be_bytes());
    mac
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMasterkeyFile {
    version: u32,
    scrypt_salt: String,
    scrypt_cost_param: u32,
    scrypt_block_size: u32,
    primary_master_key: String,
    hmac_master_key: String,
    version_mac: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterkeyFile {
    pub version: u32,
    pub scrypt_salt: Vec<u8>,
    pub scrypt: ScryptParams,
    pub wrapped_enc_key: Vec<u8>,
    pub wrapped_mac_key: Vec<u8>,
    pub version_mac: Vec<u8>,
}

fn b64(field: &str, s: &str) -> Result<Vec<u8>> {
    BASE64.decode(s.as_bytes()).map_err(|_| invalid(format!("{field} is not valid Base64")))
}

impl MasterkeyFile {
    pub fn parse(json: &[u8]) -> Result<Self> {
        let raw: RawMasterkeyFile =
            serde_json::from_slice(json).map_err(|e| invalid(format!("masterkey file: {e}")))?;
        let file = MasterkeyFile {
            version: raw.version,
            scrypt_salt: b64("scryptSalt", &raw.scrypt_salt)?,
            scrypt: ScryptParams { cost: raw.scrypt_cost_param, block_size: raw.scrypt_block_size },
            wrapped_enc_key: b64("primaryMasterKey", &raw.primary_master_key)?,
            wrapped_mac_key: b64("hmacMasterKey", &raw.hmac_master_key)?,
            version_mac: b64("versionMac", &raw.version_mac)?,
        };
        file.scrypt.validate()?;
        if file.scrypt_salt.len() > MAX_SALT_LEN {
            return Err(invalid("scrypt salt too long"));
        }
        for (name, k) in [("primaryMasterKey", &file.wrapped_enc_key), ("hmacMasterKey", &file.wrapped_mac_key)] {
            if k.len() != SUBKEY_LEN + 8 {
                return Err(invalid(format!("{name} must be 40 bytes, got {}", k.len())));
            }
        }
        if file.version_mac.len() != 32 {
            return Err(invalid("versionMac must be 32 bytes"));
        }
        Ok(file)
    }

    pub fn to_json(&self) -> String {
        let raw = RawMasterkeyFile {
            version: self.version,
            scrypt_salt: BASE64.encode(&self.scrypt_salt),
            scrypt_cost_param: self.scrypt.cost,
            scrypt_block_size: self.scrypt.block_size,
            primary_master_key: BASE64.encode(&self.wrapped_enc_key),
            hmac_master_key: BASE64.encode(&self.wrapped_mac_key),
            version_mac: BASE64.encode(&self.version_mac),
        };
        serde_json::to_string_pretty(&raw).expect("serializing plain struct cannot fail")
    }

    pub fn unlock(&self, password: &str) -> Result<MasterKey> {
        let kek = derive_kek(password, &self.scrypt_salt, self.scrypt)?;
        let enc = unwrap_key(&kek, &self.wrapped_enc_key)?;
        let mac = unwrap_key(&kek, &self.wrapped_mac_key)?;
        version_mac(&mac, self.version)
            .verify_slice(&self.version_mac)
            .map_err(|_| Error::Authentication("masterkey file versionMac mismatch"))?;
        Ok(MasterKey::from_parts(*enc, *mac))
    }

    pub fn lock(key: &MasterKey, password: &str, params: ScryptParams) -> Result<Self> {
        let mut salt = vec![0u8; SALT_LEN];
        random_bytes(&mut salt)?;
        Self::lock_with_salt(key, password, params, salt, MASTERKEY_FILE_VERSION)
    }

    pub fn lock_with_salt(
        key: &MasterKey,
        password: &str,
        params: ScryptParams,
        salt: Vec<u8>,
        version: u32,
    ) -> Result<Self> {
        let kek = derive_kek(password, &salt, params)?;
        Ok(MasterkeyFile {
            version,
            scrypt_salt: salt,
            scrypt: params,
            wrapped_enc_key: wrap_key(&kek, key.enc_key())?.to_vec(),
            wrapped_mac_key: wrap_key(&kek, key.mac_key())?.to_vec(),
            version_mac: version_mac(key.mac_key(), version).finalize().into_bytes().to_vec(),
        })
    }

    pub fn change_password(&self, old_password: &str, new_password: &str) -> Result<Self> {
        let key = self.unlock(old_password)?;
        let mut salt = vec![0u8; SALT_LEN];
        random_bytes(&mut salt)?;
        Self::lock_with_salt(&key, new_password, self.scrypt, salt, self.version)
    }
}
