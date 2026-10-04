//! High level API tying the masterkey file, vault config, names and contents together.

use std::io::{Read, Write};

use unicode_normalization::UnicodeNormalization;

use crate::content::{CipherCombo, ContentCryptor, DecryptingReader, EncryptingWriter, FileHeader, RangePlan};
use crate::error::{Error, Result, invalid};
use crate::jwt::{UnverifiedVaultConfig, VaultConfig};
use crate::keys::MasterKey;
use crate::masterkey::{MasterkeyFile, ScryptParams};
use crate::names::{self, NodeName, ROOT_DIR_ID};

/// Maximum length of a symlink target in bytes (same limit as Cryptomator).
pub const MAX_SYMLINK_TARGET_LEN: usize = 32767;

/// Prefix of `kid` values that point at a masterkey file.
pub const MASTERKEY_FILE_KID_PREFIX: &str = "masterkeyfile:";

/// Everything needed to write a brand new vault to storage.
#[derive(Debug)]
pub struct CreatedVault {
    /// The unlocked vault.
    pub vault: Vault,
    /// Content of `masterkey.cryptomator`.
    pub masterkey_json: String,
    /// Content of `vault.cryptomator`.
    pub vault_config: String,
    /// Path of the root content directory, e.g. `d/AB/CDEF...` (no trailing slash).
    /// Object stores have no directories; create it (or a marker) as needed.
    pub root_dir_path: String,
    /// Optional `dirid.c9r` for the root content directory
    /// (`root_dir_path + "/dirid.c9r"`), as Cryptomator writes it.
    pub root_dir_id_backup: Vec<u8>,
}

/// An unlocked vault.
#[derive(Debug, Clone)]
pub struct Vault {
    key: MasterKey,
    config: VaultConfig,
    content: ContentCryptor,
}

impl Vault {
    /// Create a new SIV_GCM vault protected by `password` with default scrypt
    /// parameters (N = 32768, r = 8).
    pub fn create(password: &str) -> Result<CreatedVault> {
        Self::create_with(password, ScryptParams::default(), CipherCombo::SivGcm)
    }

    /// Create a new vault with explicit scrypt parameters and cipher combo.
    pub fn create_with(password: &str, scrypt: ScryptParams, combo: CipherCombo) -> Result<CreatedVault> {
        let key = MasterKey::generate()?;
        let masterkey_json = MasterkeyFile::lock(&key, password, scrypt)?.to_json();
        let mut config = VaultConfig::new()?;
        config.cipher_combo = combo;
        let vault_config = config.sign(&key);
        let vault = Self::from_parts(key, config);
        let root_dir_path = vault.dir_path(ROOT_DIR_ID)?;
        let root_dir_id_backup = vault.encrypt_dir_id_backup(ROOT_DIR_ID)?;
        Ok(CreatedVault { vault, masterkey_json, vault_config, root_dir_path, root_dir_id_backup })
    }

    /// Unlock a vault from the contents of `masterkey.cryptomator` and
    /// `vault.cryptomator` and the password.
    ///
    /// Follows the spec: decode the config without verification, check that
    /// its `kid` names a masterkey file, unlock that file, verify the JWT
    /// signature with the masterkey and finally check format and cipher combo.
    pub fn unlock(masterkey_json: &[u8], vault_config: &str, password: &str) -> Result<Self> {
        let unverified = UnverifiedVaultConfig::decode(vault_config)?;
        match unverified.key_id() {
            Some(kid) if kid.starts_with(MASTERKEY_FILE_KID_PREFIX) => {}
            Some(kid) => return Err(Error::Unsupported(format!("key loader {kid:?}"))),
            None => return Err(invalid("vault config lacks kid")),
        }
        let key = MasterkeyFile::parse(masterkey_json)?.unlock(password)?;
        let config = unverified.verify(&key)?;
        Ok(Self::from_parts(key, config))
    }

    /// Open a vault with an already known raw masterkey (e.g. from Cryptomator
    /// Hub), verifying `vault.cryptomator` with it.
    pub fn with_master_key(key: MasterKey, vault_config: &str) -> Result<Self> {
        let config = VaultConfig::verify(vault_config, &key)?;
        Ok(Self::from_parts(key, config))
    }

    fn from_parts(key: MasterKey, config: VaultConfig) -> Self {
        let content = ContentCryptor::new(&key, config.cipher_combo);
        Self { key, config, content }
    }

    /// The verified vault configuration.
    pub fn config(&self) -> &VaultConfig {
        &self.config
    }

    /// The masterkey (e.g. to re-lock it with a new password).
    pub fn master_key(&self) -> &MasterKey {
        &self.key
    }

    /// The content cryptor (header/chunk level access).
    pub fn content(&self) -> &ContentCryptor {
        &self.content
    }

    /// A new random directory ID.
    pub fn new_dir_id() -> Result<String> {
        names::new_dir_id()
    }

    /// Storage path of the content directory of `dir_id` (`""` = root).
    pub fn dir_path(&self, dir_id: &str) -> Result<String> {
        names::dir_path(&self.key, dir_id)
    }

    /// Encrypt `name` (NFC-normalized) for parent `parent_dir_id`; returns `xxx.c9r`
    /// without shortening.
    pub fn encrypt_name(&self, name: &str, parent_dir_id: &str) -> Result<String> {
        names::encrypt_name(&self.key, name, parent_dir_id)
    }

    /// Encrypt `name` and apply the vault's shortening threshold.
    pub fn node_name(&self, name: &str, parent_dir_id: &str) -> Result<NodeName> {
        names::node_name(&self.key, name, parent_dir_id, self.config.shortening_threshold)
    }

    /// Decrypt a `xxx.c9r` name from directory `parent_dir_id`.
    pub fn decrypt_name(&self, encrypted: &str, parent_dir_id: &str) -> Result<String> {
        names::decrypt_name(&self.key, encrypted, parent_dir_id)
    }

    /// Decrypt a shortened node: `short_name` is the `xxx.c9s` entry and
    /// `name_file` the content of its `name.c9s`.
    pub fn decrypt_shortened_name(&self, short_name: &str, name_file: &[u8], parent_dir_id: &str) -> Result<String> {
        let full = names::parse_name_file(short_name, name_file)?;
        self.decrypt_name(&full, parent_dir_id)
    }

    /// Encrypt a whole file.
    pub fn encrypt_file(&self, cleartext: &[u8]) -> Result<Vec<u8>> {
        self.content.encrypt(cleartext)
    }

    /// Decrypt a whole file.
    pub fn decrypt_file(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        self.content.decrypt(ciphertext)
    }

    /// Decrypt the header at the start of `ciphertext`.
    pub fn decrypt_header(&self, ciphertext: &[u8]) -> Result<FileHeader> {
        self.content.decrypt_header(ciphertext)
    }

    /// Size of the encrypted file header.
    pub fn header_len(&self) -> usize {
        self.content.header_len()
    }

    /// Cleartext size of a ciphertext object of `ciphertext_size` bytes.
    pub fn cleartext_size(&self, ciphertext_size: u64) -> Result<u64> {
        self.content.cleartext_size(ciphertext_size)
    }

    /// Ciphertext object size for `cleartext_size` bytes of content.
    pub fn ciphertext_size(&self, cleartext_size: u64) -> u64 {
        self.content.ciphertext_size(cleartext_size)
    }

    /// Plan a ranged read; see [`ContentCryptor::range_plan`].
    pub fn range_plan(&self, offset: u64, len: u64) -> RangePlan {
        self.content.range_plan(offset, len)
    }

    /// Decrypt the chunks fetched for `plan`; see [`ContentCryptor::decrypt_range`].
    pub fn decrypt_range(&self, header: &FileHeader, plan: &RangePlan, chunks: &[u8]) -> Result<Vec<u8>> {
        self.content.decrypt_range(header, plan, chunks)
    }

    /// Streaming encryption into `inner`.
    pub fn encrypting_writer<W: Write>(&self, inner: W) -> Result<EncryptingWriter<W>> {
        EncryptingWriter::new(&self.content, inner)
    }

    /// Streaming decryption from `inner`.
    pub fn decrypting_reader<R: Read>(&self, inner: R) -> DecryptingReader<R> {
        DecryptingReader::new(&self.content, inner)
    }

    /// Parse the content of a `dir.c9r` file.
    pub fn parse_dir_file(&self, content: &[u8]) -> Result<String> {
        names::parse_dir_id(content)
    }

    /// Encrypted content for the `dirid.c9r` backup file of directory `dir_id`.
    pub fn encrypt_dir_id_backup(&self, dir_id: &str) -> Result<Vec<u8>> {
        if dir_id.len() > names::MAX_DIR_ID_LEN {
            return Err(Error::InvalidArgument("directory ID too long".into()));
        }
        self.content.encrypt(dir_id.as_bytes())
    }

    /// Decrypt a `dirid.c9r` backup file.
    pub fn decrypt_dir_id_backup(&self, ciphertext: &[u8]) -> Result<String> {
        names::parse_dir_id(&self.content.decrypt(ciphertext)?)
    }

    /// Encrypted content of a `symlink.c9r` file for `target` (UTF-8, NFC).
    pub fn encrypt_symlink_target(&self, target: &str) -> Result<Vec<u8>> {
        let nfc: String = target.nfc().collect();
        if nfc.is_empty() || nfc.len() > MAX_SYMLINK_TARGET_LEN {
            return Err(Error::InvalidArgument("invalid symlink target length".into()));
        }
        self.content.encrypt(nfc.as_bytes())
    }

    /// Decrypt a `symlink.c9r` file. The target is returned verbatim; callers
    /// must not follow it outside the vault without their own checks.
    pub fn decrypt_symlink_target(&self, ciphertext: &[u8]) -> Result<String> {
        let size = self.content.cleartext_size(ciphertext.len() as u64)?;
        if size > MAX_SYMLINK_TARGET_LEN as u64 {
            return Err(invalid("symlink target too long"));
        }
        String::from_utf8(self.content.decrypt(ciphertext)?).map_err(|_| invalid("symlink target is not UTF-8"))
    }
}
