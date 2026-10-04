//! Minimal HMAC JWT (JWS compact serialization) and the vault configuration
//! file `vault.cryptomator`.
//!
//! Only `HS256`, `HS384` and `HS512` are accepted. `none` and every
//! asymmetric algorithm are rejected, so an attacker cannot downgrade the
//! signature check.

use data_encoding::BASE64URL_NOPAD;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value, json};
use sha2::{Sha256, Sha384, Sha512};

use crate::content::CipherCombo;
use crate::error::{Error, Result, invalid};
use crate::keys::{MasterKey, random_bytes};

/// Largest accepted `vault.cryptomator` token, in bytes.
pub const MAX_TOKEN_LEN: usize = 16 * 1024;
/// The only vault format implemented by this crate.
pub const VAULT_FORMAT: i64 = 8;
/// Default `shorteningThreshold` used by Cryptomator.
pub const DEFAULT_SHORTENING_THRESHOLD: usize = 220;
/// `kid` of vaults whose key is stored in `masterkey.cryptomator`.
pub const MASTERKEY_FILE_KID: &str = "masterkeyfile:masterkey.cryptomator";

/// JWS HMAC algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtAlgorithm {
    /// HMAC-SHA256 (what Cryptomator writes).
    HS256,
    /// HMAC-SHA384.
    HS384,
    /// HMAC-SHA512.
    HS512,
}

impl JwtAlgorithm {
    /// The `alg` header value.
    pub fn name(self) -> &'static str {
        match self {
            JwtAlgorithm::HS256 => "HS256",
            JwtAlgorithm::HS384 => "HS384",
            JwtAlgorithm::HS512 => "HS512",
        }
    }

    fn from_name(name: &str) -> Result<Self> {
        match name {
            "HS256" => Ok(JwtAlgorithm::HS256),
            "HS384" => Ok(JwtAlgorithm::HS384),
            "HS512" => Ok(JwtAlgorithm::HS512),
            other => Err(Error::Unsupported(format!("JWT algorithm {other:?}"))),
        }
    }

    fn sign(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        fn mac<M: Mac + KeyInit>(key: &[u8], data: &[u8]) -> Vec<u8> {
            let mut m = <M as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
            m.update(data);
            m.finalize().into_bytes().to_vec()
        }
        match self {
            JwtAlgorithm::HS256 => mac::<Hmac<Sha256>>(key, data),
            JwtAlgorithm::HS384 => mac::<Hmac<Sha384>>(key, data),
            JwtAlgorithm::HS512 => mac::<Hmac<Sha512>>(key, data),
        }
    }

    fn verify(self, key: &[u8], data: &[u8], sig: &[u8]) -> Result<()> {
        fn check<M: Mac + KeyInit>(key: &[u8], data: &[u8], sig: &[u8]) -> bool {
            let mut m = <M as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
            m.update(data);
            m.verify_slice(sig).is_ok()
        }
        let ok = match self {
            JwtAlgorithm::HS256 => check::<Hmac<Sha256>>(key, data, sig),
            JwtAlgorithm::HS384 => check::<Hmac<Sha384>>(key, data, sig),
            JwtAlgorithm::HS512 => check::<Hmac<Sha512>>(key, data, sig),
        };
        if ok { Ok(()) } else { Err(Error::Authentication("JWT signature mismatch")) }
    }
}

/// A decoded but not yet verified JWT.
#[derive(Debug, Clone)]
pub struct UnverifiedJwt {
    /// Decoded JOSE header.
    pub header: Map<String, Value>,
    /// Decoded claims.
    pub claims: Map<String, Value>,
    algorithm: JwtAlgorithm,
    signing_input: String,
    signature: Vec<u8>,
}

fn b64url_decode(part: &str, what: &str) -> Result<Vec<u8>> {
    // Tokens are unpadded per RFC 7515; tolerate stray padding anyway.
    BASE64URL_NOPAD
        .decode(part.trim_end_matches('=').as_bytes())
        .map_err(|_| invalid(format!("JWT {what} is not valid base64url")))
}

fn json_object(bytes: &[u8], what: &str) -> Result<Map<String, Value>> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(invalid(format!("JWT {what} is not a JSON object"))),
    }
}

impl UnverifiedJwt {
    /// Split and decode a compact JWS. Nothing is trusted yet.
    pub fn decode(token: &str) -> Result<Self> {
        let token = token.trim();
        if token.len() > MAX_TOKEN_LEN {
            return Err(invalid("JWT too long"));
        }
        let mut parts = token.split('.');
        let (Some(h), Some(c), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err(invalid("JWT must have three parts"));
        };
        let header = json_object(&b64url_decode(h, "header")?, "header")?;
        let claims = json_object(&b64url_decode(c, "payload")?, "payload")?;
        let signature = b64url_decode(s, "signature")?;
        let alg = header
            .get("alg")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("JWT header lacks alg"))?;
        if header.contains_key("crit") {
            return Err(Error::Unsupported("JWT crit header".into()));
        }
        let algorithm = JwtAlgorithm::from_name(alg)?;
        Ok(Self { header, claims, algorithm, signing_input: format!("{h}.{c}"), signature })
    }

    /// The signature algorithm named in the header.
    pub fn algorithm(&self) -> JwtAlgorithm {
        self.algorithm
    }

    /// The `kid` header, if present.
    pub fn key_id(&self) -> Option<&str> {
        self.header.get("kid").and_then(Value::as_str)
    }

    /// Verify the HMAC signature with `key` (constant time).
    pub fn verify(&self, key: &[u8]) -> Result<()> {
        self.algorithm.verify(key, self.signing_input.as_bytes(), &self.signature)
    }
}

/// Sign `claims` with `header` fields (`alg` is set automatically).
pub fn sign(mut header: Map<String, Value>, claims: &Map<String, Value>, alg: JwtAlgorithm, key: &[u8]) -> String {
    header.insert("alg".into(), Value::String(alg.name().into()));
    let h = BASE64URL_NOPAD.encode(Value::Object(header).to_string().as_bytes());
    let c = BASE64URL_NOPAD.encode(Value::Object(claims.clone()).to_string().as_bytes());
    let input = format!("{h}.{c}");
    let sig = BASE64URL_NOPAD.encode(&alg.sign(key, input.as_bytes()));
    format!("{input}.{sig}")
}

/// The verified contents of `vault.cryptomator`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultConfig {
    /// Random vault UUID (`jti`).
    pub jti: String,
    /// Where the key comes from (`kid`), e.g. [`MASTERKEY_FILE_KID`].
    pub kid: String,
    /// Vault format, always 8.
    pub format: i64,
    /// Content cipher.
    pub cipher_combo: CipherCombo,
    /// Encrypted names (including `.c9r`) longer than this are shortened.
    pub shortening_threshold: usize,
}

/// A decoded `vault.cryptomator` whose signature has not been checked yet.
///
/// Step 1 of the spec: decode without verification, read `kid` to find the key.
#[derive(Debug, Clone)]
pub struct UnverifiedVaultConfig {
    jwt: UnverifiedJwt,
}

impl UnverifiedVaultConfig {
    /// Decode the token (no verification).
    pub fn decode(token: &str) -> Result<Self> {
        Ok(Self { jwt: UnverifiedJwt::decode(token)? })
    }

    /// The `kid` header (where the masterkey is stored).
    pub fn key_id(&self) -> Option<&str> {
        self.jwt.key_id()
    }

    /// Verify the signature with `encKey || macKey`, then check the claims.
    pub fn verify(&self, key: &MasterKey) -> Result<VaultConfig> {
        self.jwt.verify(key.raw().as_slice())?;
        let c = &self.jwt.claims;
        let format = c
            .get("format")
            .and_then(Value::as_i64)
            .ok_or_else(|| invalid("vault config lacks format"))?;
        if format != VAULT_FORMAT {
            return Err(Error::Unsupported(format!("vault format {format}")));
        }
        let combo = c
            .get("cipherCombo")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("vault config lacks cipherCombo"))?;
        let cipher_combo = CipherCombo::from_name(combo)?;
        let threshold = c
            .get("shorteningThreshold")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("vault config lacks shorteningThreshold"))?;
        // The shortest possible encrypted name is 28 chars; anything below
        // that (or absurdly large) is not a vault Cryptomator would write.
        if !(28..=1_000_000).contains(&threshold) {
            return Err(invalid(format!("shorteningThreshold {threshold} out of range")));
        }
        let jti = c.get("jti").and_then(Value::as_str).unwrap_or_default().to_owned();
        let kid = self.jwt.key_id().unwrap_or_default().to_owned();
        Ok(VaultConfig {
            jti,
            kid,
            format,
            cipher_combo,
            shortening_threshold: threshold as usize,
        })
    }
}

/// Generate a random (version 4) UUID string.
pub fn random_uuid() -> Result<String> {
    let mut b = [0u8; 16];
    random_bytes(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = data_encoding::HEXLOWER.encode(&b);
    Ok(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

impl VaultConfig {
    /// A new SIV_GCM, format 8 configuration with a random `jti` and the
    /// masterkey file `kid`.
    pub fn new() -> Result<Self> {
        Ok(Self {
            jti: random_uuid()?,
            kid: MASTERKEY_FILE_KID.to_owned(),
            format: VAULT_FORMAT,
            cipher_combo: CipherCombo::SivGcm,
            shortening_threshold: DEFAULT_SHORTENING_THRESHOLD,
        })
    }

    /// Decode and verify `token` in one step.
    pub fn verify(token: &str, key: &MasterKey) -> Result<Self> {
        UnverifiedVaultConfig::decode(token)?.verify(key)
    }

    /// Encode and sign (HS256) with `encKey || macKey`.
    pub fn sign(&self, key: &MasterKey) -> String {
        let header = json!({ "kid": self.kid, "typ": "JWT" });
        let claims = json!({
            "format": self.format,
            "shorteningThreshold": self.shortening_threshold,
            "jti": self.jti,
            "cipherCombo": self.cipher_combo.name(),
        });
        let (Value::Object(h), Value::Object(c)) = (header, claims) else {
            unreachable!("json! object literals are objects")
        };
        sign(h, &c, JwtAlgorithm::HS256, key.raw().as_slice())
    }
}
