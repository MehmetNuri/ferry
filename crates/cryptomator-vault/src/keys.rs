//! The 512 bit vault masterkey (256 bit encryption key + 256 bit MAC key).

use std::fmt;

use zeroize::Zeroizing;

use crate::error::{Error, Result};

/// Length of each of the two subkeys in bytes.
pub const SUBKEY_LEN: usize = 32;

/// Fill `buf` with bytes from the operating system CSPRNG.
pub(crate) fn random_bytes(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf).map_err(|_| Error::Random)
}

/// The vault masterkey: an AES-256 encryption key and a 256 bit MAC key.
///
/// Both halves live in [`Zeroizing`] buffers and are wiped on drop. `Debug`
/// output never contains key material.
#[derive(Clone)]
pub struct MasterKey {
    enc: Zeroizing<[u8; SUBKEY_LEN]>,
    mac: Zeroizing<[u8; SUBKEY_LEN]>,
}

impl MasterKey {
    /// Generate a new random masterkey from the OS CSPRNG.
    pub fn generate() -> Result<Self> {
        let mut key = Self::from_parts([0; SUBKEY_LEN], [0; SUBKEY_LEN]);
        random_bytes(key.enc.as_mut_slice())?;
        random_bytes(key.mac.as_mut_slice())?;
        Ok(key)
    }

    /// Build a masterkey from its encryption and MAC halves.
    pub fn from_parts(enc: [u8; SUBKEY_LEN], mac: [u8; SUBKEY_LEN]) -> Self {
        Self { enc: Zeroizing::new(enc), mac: Zeroizing::new(mac) }
    }

    /// Build a masterkey from the 64 byte raw form `encKey || macKey`
    /// (the form used by Cryptomator Hub and for signing `vault.cryptomator`).
    pub fn from_raw(raw: &[u8]) -> Result<Self> {
        if raw.len() != 2 * SUBKEY_LEN {
            return Err(Error::InvalidArgument(format!(
                "raw masterkey must be 64 bytes, got {}",
                raw.len()
            )));
        }
        let mut key = Self::from_parts([0; SUBKEY_LEN], [0; SUBKEY_LEN]);
        key.enc.copy_from_slice(&raw[..SUBKEY_LEN]);
        key.mac.copy_from_slice(&raw[SUBKEY_LEN..]);
        Ok(key)
    }

    /// The AES-256 encryption masterkey.
    pub fn enc_key(&self) -> &[u8; SUBKEY_LEN] {
        &self.enc
    }

    /// The MAC masterkey.
    pub fn mac_key(&self) -> &[u8; SUBKEY_LEN] {
        &self.mac
    }

    /// `encKey || macKey`, the key used to sign `vault.cryptomator`.
    pub fn raw(&self) -> Zeroizing<[u8; 2 * SUBKEY_LEN]> {
        let mut out = Zeroizing::new([0u8; 2 * SUBKEY_LEN]);
        out[..SUBKEY_LEN].copy_from_slice(self.enc.as_slice());
        out[SUBKEY_LEN..].copy_from_slice(self.mac.as_slice());
        out
    }

    /// `macKey || encKey`, the 512 bit AES-SIV key (RFC 5297 orders the
    /// S2V/CMAC key first, then the CTR key; Cryptomator's `SivMode` passes
    /// the MAC masterkey as the S2V key and the encryption masterkey as the
    /// CTR key).
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
    /// Constant-time comparison.
    fn eq(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq;
        self.raw().as_slice().ct_eq(other.raw().as_slice()).into()
    }
}

impl Eq for MasterKey {}
