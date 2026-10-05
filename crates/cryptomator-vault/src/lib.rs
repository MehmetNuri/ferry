//! Cryptomator vault format 8 (read/write), without I/O.

#![forbid(unsafe_code)]

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
