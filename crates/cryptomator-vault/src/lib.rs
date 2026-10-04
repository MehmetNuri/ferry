//! Storage agnostic implementation of the [Cryptomator] vault format 8.
//!
//! The crate performs no I/O of its own: callers pass in the bytes of
//! `masterkey.cryptomator`, `vault.cryptomator`, `dir.c9r`, `name.c9s` and
//! encrypted files, and write the bytes this crate returns, so vaults can live
//! on any storage (local disk, S3, WebDAV, ...).
//!
//! Supported: masterkey files (scrypt + AES key wrap), the HS256/384/512
//! signed vault config, AES-SIV directory IDs and names with name shortening,
//! and file contents for both `SIV_GCM` (current default) and `SIV_CTRMAC`
//! (older format 8 vaults), including streaming and ranged decryption.
//! Not supported: vault formats other than 8, Cryptomator Hub key loading
//! (a raw masterkey from Hub can be passed to [`Vault::with_master_key`]).
//!
//! ```
//! use cryptomator_vault::{CipherCombo, ScryptParams, Vault};
//!
//! # fn main() -> Result<(), cryptomator_vault::Error> {
//! // Tiny scrypt cost only to keep the doc test fast; use Vault::create in real code.
//! let params = ScryptParams { cost: 1024, block_size: 8 };
//! let created = Vault::create_with("secret", params, CipherCombo::SivGcm)?;
//! let vault = Vault::unlock(created.masterkey_json.as_bytes(), &created.vault_config, "secret")?;
//!
//! let root = vault.dir_path("")?;                       // "d/XX/YYYYYYYY..."
//! let name = vault.node_name("hello.txt", "")?;         // name inside `root`
//! let object = vault.encrypt_file(b"hello world")?;
//! let key = format!("{root}/{}", name.file_contents_path());
//! # let _ = key;
//! assert_eq!(vault.decrypt_file(&object)?, b"hello world");
//! # Ok(())
//! # }
//! ```
//!
//! [Cryptomator]: https://docs.cryptomator.org/en/latest/security/architecture/

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod content;
pub mod error;
pub mod jwt;
pub mod keys;
pub mod masterkey;
pub mod names;
pub mod vault;

pub use content::{CipherCombo, ContentCryptor, DecryptingReader, EncryptingWriter, FileHeader, RangePlan};
pub use error::{Error, Result};
pub use jwt::{UnverifiedVaultConfig, VaultConfig};
pub use keys::MasterKey;
pub use masterkey::{MasterkeyFile, ScryptParams};
pub use names::{EntryKind, NodeName, ROOT_DIR_ID};
pub use vault::{CreatedVault, Vault};
