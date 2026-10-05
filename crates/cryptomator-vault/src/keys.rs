use std::fmt;

use zeroize::Zeroizing;

use crate::error::{Error, Result};

pub const SUBKEY_LEN: usize = 32;

pub(crate) fn random_bytes(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf).map_err(|_| Error::Random)
}

#[derive(Clone)]
pub struct MasterKey {
    enc: Zeroizing<[u8; SUBKEY_LEN]>,
    mac: Zeroizing<[u8; SUBKEY_LEN]>,
}

impl MasterKey {
    pub fn generate() -> Result<Self> {
        let mut key = Self::from_parts([0; SUBKEY_LEN], [0; SUBKEY_LEN]);
        random_bytes(key.enc.as_mut_slice())?;
        random_bytes(key.mac.as_mut_slice())?;
        Ok(key)
    }

    pub fn from_parts(enc: [u8; SUBKEY_LEN], mac: [u8; SUBKEY_LEN]) -> Self {
        Self { enc: Zeroizing::new(enc), mac: Zeroizing::new(mac) }
    }

    pub fn from_raw(raw: &[u8]) -> Result<Self> {
        if raw.len() != 2 * SUBKEY_LEN {
            return Err(Error::InvalidArgument(format!("raw masterkey must be 64 bytes, got {}", raw.len())));
        }
        let mut key = Self::from_parts([0; SUBKEY_LEN], [0; SUBKEY_LEN]);
        key.enc.copy_from_slice(&raw[..SUBKEY_LEN]);
        key.mac.copy_from_slice(&raw[SUBKEY_LEN..]);
        Ok(key)
    }

    pub fn enc_key(&self) -> &[u8; SUBKEY_LEN] {
        &self.enc
    }

    pub fn mac_key(&self) -> &[u8; SUBKEY_LEN] {
        &self.mac
    }

    pub fn raw(&self) -> Zeroizing<[u8; 2 * SUBKEY_LEN]> {
        let mut out = Zeroizing::new([0u8; 2 * SUBKEY_LEN]);
        out[..SUBKEY_LEN].copy_from_slice(self.enc.as_slice());
        out[SUBKEY_LEN..].copy_from_slice(self.mac.as_slice());
        out
    }

    // macKey || encKey: RFC 5297 puts the S2V key (Cryptomator's macKey) first.
    pub(crate) fn siv_key(&self) -> Zeroizing<[u8; 2 * SUBKEY_LEN]> {
        let mut out = Zeroizing::new([0u8; 2 * SUBKEY_LEN]);
        out[..SUBKEY_LEN].copy_from_slice(self.mac.as_slice());
        out[SUBKEY_LEN..].copy_from_slice(self.enc.as_slice());
        out
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

impl PartialEq for MasterKey {
    // Constant time to avoid leaking key bytes via timing.
    fn eq(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq;
        self.raw().as_slice().ct_eq(other.raw().as_slice()).into()
    }
}

impl Eq for MasterKey {}
