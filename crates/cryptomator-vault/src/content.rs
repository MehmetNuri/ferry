//! File header and file content encryption.
//!
//! # SIV_GCM (default for new vaults)
//!
//! * Header (68 bytes): `nonce(12) || AES-GCM(encKey, nonce, 0xFF*8 || contentKey(32)) || tag(16)`.
//! * Chunk `i` (up to 32 KiB cleartext): `nonce(12) || AES-GCM(contentKey, nonce,
//!   AAD = i as u64 BE || headerNonce) || tag(16)`.
//!
//! # SIV_CTRMAC (older vaults)
//!
//! * Header (88 bytes): `nonce(16) || AES-CTR(encKey, nonce, 0xFF*8 || contentKey) ||
//!   HMAC-SHA256(macKey, nonce || ciphertext)`.
//! * Chunk `i`: `nonce(16) || AES-CTR(contentKey, nonce) || HMAC-SHA256(macKey,
//!   headerNonce || i as u64 BE || nonce || ciphertext)`.
//!
//! Chunking is the same for both: the ciphertext of a file is the header
//! followed by the encrypted 32 KiB chunks; only the last chunk may be short.
//! Like Cryptomator's file system layer, this crate never writes an empty
//! trailing chunk (an empty file is just the header), but it accepts one when
//! decrypting, because Cryptomator's channel API writes one for empty
//! `dirid.c9r` / multiple-of-32-KiB payloads.
//!
//! Note that, by design of the format, dropping whole trailing chunks cannot
//! be detected (chunks carry no "last chunk" flag).

use std::io::{self, Read, Write};

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use ctr::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::error::{Error, Result, invalid};
use crate::keys::{MasterKey, random_bytes};

/// Cleartext bytes per chunk.
pub const CLEARTEXT_CHUNK_SIZE: usize = 32 * 1024;
/// Length of the content key stored in the header.
pub const CONTENT_KEY_LEN: usize = 32;
const RESERVED: [u8; 8] = [0xFF; 8];
const GCM_NONCE: usize = 12;
const GCM_TAG: usize = 16;
const CTR_NONCE: usize = 16;
const MAC_LEN: usize = 32;

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// The content cipher of a vault (`cipherCombo` claim).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CipherCombo {
    /// AES-SIV for names, AES-GCM for contents (Cryptomator 1.6+ default).
    SivGcm,
    /// AES-SIV for names, AES-CTR + HMAC-SHA256 for contents (older vaults).
    SivCtrMac,
}

impl CipherCombo {
    /// The `cipherCombo` claim value.
    pub fn name(self) -> &'static str {
        match self {
            CipherCombo::SivGcm => "SIV_GCM",
            CipherCombo::SivCtrMac => "SIV_CTRMAC",
        }
    }

    /// Parse a `cipherCombo` claim value.
    pub fn from_name(name: &str) -> Result<Self> {
        match name {
            "SIV_GCM" => Ok(CipherCombo::SivGcm),
            "SIV_CTRMAC" => Ok(CipherCombo::SivCtrMac),
            other => Err(Error::Unsupported(format!("cipher combo {other:?}"))),
        }
    }

    /// Nonce length used in headers and chunks.
    pub fn nonce_len(self) -> usize {
        match self {
            CipherCombo::SivGcm => GCM_NONCE,
            CipherCombo::SivCtrMac => CTR_NONCE,
        }
    }

    fn tag_len(self) -> usize {
        match self {
            CipherCombo::SivGcm => GCM_TAG,
            CipherCombo::SivCtrMac => MAC_LEN,
        }
    }

    /// Size of the encrypted file header (68 for GCM, 88 for CTRMAC).
    pub fn header_len(self) -> usize {
        self.nonce_len() + RESERVED.len() + CONTENT_KEY_LEN + self.tag_len()
    }

    /// Per-chunk overhead (28 for GCM, 48 for CTRMAC).
    pub fn chunk_overhead(self) -> usize {
        self.nonce_len() + self.tag_len()
    }

    /// Size of a full ciphertext chunk.
    pub fn ciphertext_chunk_len(self) -> usize {
        CLEARTEXT_CHUNK_SIZE + self.chunk_overhead()
    }
}

/// A decrypted file header: header nonce plus the per-file content key.
#[derive(Clone)]
pub struct FileHeader {
    nonce: Vec<u8>,
    content_key: Zeroizing<[u8; CONTENT_KEY_LEN]>,
}

impl std::fmt::Debug for FileHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileHeader").field("nonce", &self.nonce).finish_non_exhaustive()
    }
}

impl FileHeader {
    /// Build a header from explicit parts. Only for deterministic tests and
    /// interop checks: real headers must use random nonces and keys
    /// ([`ContentCryptor::new_header`]).
    pub fn from_parts(nonce: Vec<u8>, content_key: [u8; CONTENT_KEY_LEN]) -> Self {
        Self { nonce, content_key: Zeroizing::new(content_key) }
    }

    /// The header nonce (bound into every chunk).
    pub fn nonce(&self) -> &[u8] {
        &self.nonce
    }

    /// The per-file content key.
    pub fn content_key(&self) -> &[u8; CONTENT_KEY_LEN] {
        &self.content_key
    }
}

/// Which ciphertext bytes to fetch for a cleartext byte range, and how to cut
/// the decrypted chunks. Produced by [`ContentCryptor::range_plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangePlan {
    /// Index of the first chunk to fetch.
    pub first_chunk: u64,
    /// Number of chunks covering the range.
    pub chunk_count: u64,
    /// First ciphertext byte to fetch (absolute offset in the object, header included).
    pub ciphertext_start: u64,
    /// One past the last ciphertext byte to fetch. May point past the end of
    /// the object for the last chunk; HTTP range requests clamp that.
    pub ciphertext_end: u64,
    /// Cleartext bytes to skip at the start of the first decrypted chunk.
    pub skip: usize,
    /// Requested cleartext length.
    pub len: u64,
}

impl RangePlan {
    /// The inclusive HTTP `Range` header value for the ciphertext chunks
    /// (`bytes=start-end`). Fetch `0..header_len` separately for the header.
    pub fn http_range(&self) -> String {
        format!("bytes={}-{}", self.ciphertext_start, self.ciphertext_end.saturating_sub(1))
    }
}

fn gcm_nonce(bytes: &[u8]) -> Result<aes_gcm::Nonce<aes_gcm::aead::consts::U12>> {
    aes_gcm::Nonce::<aes_gcm::aead::consts::U12>::try_from(bytes).map_err(|_| invalid("bad GCM nonce length"))
}

/// Encrypts and decrypts file headers and contents for one vault.
#[derive(Clone, Debug)]
pub struct ContentCryptor {
    key: MasterKey,
    combo: CipherCombo,
}

impl ContentCryptor {
    /// Create a cryptor for `combo` using the vault masterkey.
    pub fn new(key: &MasterKey, combo: CipherCombo) -> Self {
        Self { key: key.clone(), combo }
    }

    /// The cipher combo in use.
    pub fn combo(&self) -> CipherCombo {
        self.combo
    }

    /// Size of the encrypted header.
    pub fn header_len(&self) -> usize {
        self.combo.header_len()
    }

    /// A new header with random nonce and content key.
    pub fn new_header(&self) -> Result<FileHeader> {
        let mut nonce = vec![0u8; self.combo.nonce_len()];
        random_bytes(&mut nonce)?;
        let mut key = Zeroizing::new([0u8; CONTENT_KEY_LEN]);
        random_bytes(key.as_mut_slice())?;
        Ok(FileHeader { nonce, content_key: key })
    }

    fn check_header_nonce(&self, header: &FileHeader) -> Result<()> {
        if header.nonce.len() != self.combo.nonce_len() {
            return Err(Error::InvalidArgument("header nonce length does not match cipher combo".into()));
        }
        Ok(())
    }

    /// Encrypt a header.
    pub fn encrypt_header(&self, header: &FileHeader) -> Result<Vec<u8>> {
        self.check_header_nonce(header)?;
        let mut payload = Zeroizing::new([0u8; RESERVED.len() + CONTENT_KEY_LEN]);
        payload[..RESERVED.len()].copy_from_slice(&RESERVED);
        payload[RESERVED.len()..].copy_from_slice(header.content_key.as_slice());
        let mut out = Vec::with_capacity(self.header_len());
        out.extend_from_slice(&header.nonce);
        match self.combo {
            CipherCombo::SivGcm => {
                let gcm = Aes256Gcm::new_from_slice(self.key.enc_key()).map_err(|_| invalid("key"))?;
                let ct = gcm
                    .encrypt(&gcm_nonce(&header.nonce)?, payload.as_slice())
                    .map_err(|_| invalid("AES-GCM encryption failed"))?;
                out.extend_from_slice(&ct);
            }
            CipherCombo::SivCtrMac => {
                let mut buf = Zeroizing::new(*payload);
                Aes256Ctr::new_from_slices(self.key.enc_key(), &header.nonce)
                    .map_err(|_| invalid("key"))?
                    .apply_keystream(buf.as_mut_slice());
                out.extend_from_slice(buf.as_slice());
                let mut mac = <HmacSha256 as hmac::KeyInit>::new_from_slice(self.key.mac_key())
                    .map_err(|_| invalid("key"))?;
                mac.update(&out);
                out.extend_from_slice(&mac.finalize().into_bytes());
            }
        }
        Ok(out)
    }

    /// Decrypt and authenticate the header at the start of `ciphertext`
    /// (only the first [`header_len`](Self::header_len) bytes are used).
    pub fn decrypt_header(&self, ciphertext: &[u8]) -> Result<FileHeader> {
        let n = self.combo.nonce_len();
        let ct = ciphertext
            .get(..self.header_len())
            .ok_or_else(|| invalid("ciphertext shorter than file header"))?;
        let nonce = ct[..n].to_vec();
        let payload: Zeroizing<Vec<u8>> = match self.combo {
            CipherCombo::SivGcm => {
                let gcm = Aes256Gcm::new_from_slice(self.key.enc_key()).map_err(|_| invalid("key"))?;
                Zeroizing::new(
                    gcm.decrypt(&gcm_nonce(&nonce)?, &ct[n..])
                        .map_err(|_| Error::Authentication("file header tag mismatch"))?,
                )
            }
            CipherCombo::SivCtrMac => {
                let (body, tag) = ct.split_at(ct.len() - MAC_LEN);
                let mut mac = <HmacSha256 as hmac::KeyInit>::new_from_slice(self.key.mac_key())
                    .map_err(|_| invalid("key"))?;
                mac.update(body);
                mac.verify_slice(tag).map_err(|_| Error::Authentication("file header MAC mismatch"))?;
                let mut buf = Zeroizing::new(body[n..].to_vec());
                Aes256Ctr::new_from_slices(self.key.enc_key(), &nonce)
                    .map_err(|_| invalid("key"))?
                    .apply_keystream(buf.as_mut_slice());
                buf
            }
        };
        let mut key = Zeroizing::new([0u8; CONTENT_KEY_LEN]);
        key.copy_from_slice(&payload[RESERVED.len()..]);
        Ok(FileHeader { nonce, content_key: key })
    }

    fn chunk_mac(&self, header: &FileHeader, chunk_no: u64, nonce_and_ct: &[u8]) -> Result<HmacSha256> {
        let mut mac = <HmacSha256 as hmac::KeyInit>::new_from_slice(self.key.mac_key())
            .map_err(|_| invalid("key"))?;
        mac.update(&header.nonce);
        mac.update(&chunk_no.to_be_bytes());
        mac.update(nonce_and_ct);
        Ok(mac)
    }

    /// Encrypt one chunk (at most 32 KiB) with a random chunk nonce.
    pub fn encrypt_chunk(&self, header: &FileHeader, chunk_no: u64, cleartext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; CTR_NONCE];
        let nonce = &mut nonce[..self.combo.nonce_len()];
        random_bytes(nonce)?;
        self.encrypt_chunk_with_nonce(header, chunk_no, cleartext, nonce)
    }

    /// Encrypt one chunk with a caller supplied nonce. Reusing a nonce with
    /// the same content key breaks AES-GCM/CTR; use only for test vectors.
    pub fn encrypt_chunk_with_nonce(
        &self,
        header: &FileHeader,
        chunk_no: u64,
        cleartext: &[u8],
        nonce: &[u8],
    ) -> Result<Vec<u8>> {
        self.check_header_nonce(header)?;
        if cleartext.len() > CLEARTEXT_CHUNK_SIZE {
            return Err(Error::InvalidArgument("chunk larger than 32 KiB".into()));
        }
        if nonce.len() != self.combo.nonce_len() {
            return Err(Error::InvalidArgument("bad chunk nonce length".into()));
        }
        let mut out = Vec::with_capacity(cleartext.len() + self.combo.chunk_overhead());
        out.extend_from_slice(nonce);
        match self.combo {
            CipherCombo::SivGcm => {
                let mut aad = [0u8; 8 + GCM_NONCE];
                aad[..8].copy_from_slice(&chunk_no.to_be_bytes());
                aad[8..].copy_from_slice(&header.nonce);
                let gcm = Aes256Gcm::new_from_slice(header.content_key.as_slice()).map_err(|_| invalid("key"))?;
                let ct = gcm
                    .encrypt(&gcm_nonce(nonce)?, Payload { msg: cleartext, aad: &aad })
                    .map_err(|_| invalid("AES-GCM encryption failed"))?;
                out.extend_from_slice(&ct);
            }
            CipherCombo::SivCtrMac => {
                let start = out.len();
                out.extend_from_slice(cleartext);
                Aes256Ctr::new_from_slices(header.content_key.as_slice(), nonce)
                    .map_err(|_| invalid("key"))?
                    .apply_keystream(&mut out[start..]);
                let tag = self.chunk_mac(header, chunk_no, &out)?.finalize().into_bytes();
                out.extend_from_slice(&tag);
            }
        }
        Ok(out)
    }

    /// Decrypt and authenticate one chunk.
    pub fn decrypt_chunk(&self, header: &FileHeader, chunk_no: u64, chunk: &[u8]) -> Result<Vec<u8>> {
        self.check_header_nonce(header)?;
        let overhead = self.combo.chunk_overhead();
        if chunk.len() < overhead || chunk.len() > self.combo.ciphertext_chunk_len() {
            return Err(invalid(format!("invalid chunk length {}", chunk.len())));
        }
        let n = self.combo.nonce_len();
        match self.combo {
            CipherCombo::SivGcm => {
                let mut aad = [0u8; 8 + GCM_NONCE];
                aad[..8].copy_from_slice(&chunk_no.to_be_bytes());
                aad[8..].copy_from_slice(&header.nonce);
                let gcm = Aes256Gcm::new_from_slice(header.content_key.as_slice()).map_err(|_| invalid("key"))?;
                gcm.decrypt(&gcm_nonce(&chunk[..n])?, Payload { msg: &chunk[n..], aad: &aad })
                    .map_err(|_| Error::Authentication("chunk tag mismatch"))
            }
            CipherCombo::SivCtrMac => {
                let (body, tag) = chunk.split_at(chunk.len() - MAC_LEN);
                self.chunk_mac(header, chunk_no, body)?
                    .verify_slice(tag)
                    .map_err(|_| Error::Authentication("chunk MAC mismatch"))?;
                let mut pt = body[n..].to_vec();
                Aes256Ctr::new_from_slices(header.content_key.as_slice(), &body[..n])
                    .map_err(|_| invalid("key"))?
                    .apply_keystream(&mut pt);
                Ok(pt)
            }
        }
    }

    /// Encrypt a whole file: header plus chunks.
    pub fn encrypt(&self, cleartext: &[u8]) -> Result<Vec<u8>> {
        let header = self.new_header()?;
        self.encrypt_with_header(&header, cleartext)
    }

    /// Encrypt a whole file with an explicit header (chunk nonces are random).
    pub fn encrypt_with_header(&self, header: &FileHeader, cleartext: &[u8]) -> Result<Vec<u8>> {
        let size = usize::try_from(self.ciphertext_size(cleartext.len() as u64))
            .map_err(|_| Error::InvalidArgument("file too large".into()))?;
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&self.encrypt_header(header)?);
        for (i, chunk) in cleartext.chunks(CLEARTEXT_CHUNK_SIZE).enumerate() {
            out.extend_from_slice(&self.encrypt_chunk(header, i as u64, chunk)?);
        }
        Ok(out)
    }

    /// Decrypt a whole file. Fails if the header or any chunk does not
    /// authenticate, or if the size is impossible.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let size = self.cleartext_size(ciphertext.len() as u64)?;
        let header = self.decrypt_header(ciphertext)?;
        let mut out = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        let body = &ciphertext[self.header_len()..];
        for (i, chunk) in body.chunks(self.combo.ciphertext_chunk_len()).enumerate() {
            out.extend_from_slice(&self.decrypt_chunk(&header, i as u64, chunk)?);
        }
        Ok(out)
    }

    /// Total ciphertext size (header included) for a cleartext of `cleartext_size` bytes.
    pub fn ciphertext_size(&self, cleartext_size: u64) -> u64 {
        let chunk = CLEARTEXT_CHUNK_SIZE as u64;
        let full = cleartext_size / chunk;
        let rem = cleartext_size % chunk;
        let overhead = self.combo.chunk_overhead() as u64;
        let tail = if rem == 0 { 0 } else { rem + overhead };
        self.header_len() as u64 + full * self.combo.ciphertext_chunk_len() as u64 + tail
    }

    /// Cleartext size for a ciphertext object of `ciphertext_size` bytes
    /// (header included). A trailing empty chunk counts as zero bytes.
    /// Errors for sizes no valid file can have.
    pub fn cleartext_size(&self, ciphertext_size: u64) -> Result<u64> {
        let body = ciphertext_size
            .checked_sub(self.header_len() as u64)
            .ok_or_else(|| invalid("ciphertext shorter than file header"))?;
        let full_len = self.combo.ciphertext_chunk_len() as u64;
        let overhead = self.combo.chunk_overhead() as u64;
        let full = body / full_len;
        let rem = body % full_len;
        if rem > 0 && rem < overhead {
            return Err(invalid(format!("impossible ciphertext size {ciphertext_size}")));
        }
        let tail = if rem == 0 { 0 } else { rem - overhead };
        Ok(full * CLEARTEXT_CHUNK_SIZE as u64 + tail)
    }

    /// Plan a ranged read of `len` cleartext bytes starting at `offset`.
    ///
    /// Fetch the header (`0..header_len`) and `ciphertext_start..ciphertext_end`,
    /// then call [`decrypt_range`](Self::decrypt_range). `len == 0` yields an
    /// empty plan (`chunk_count == 0`).
    pub fn range_plan(&self, offset: u64, len: u64) -> RangePlan {
        let chunk = CLEARTEXT_CHUNK_SIZE as u64;
        let first_chunk = offset / chunk;
        let skip = (offset % chunk) as usize;
        let chunk_count = if len == 0 {
            0
        } else {
            let last = offset.saturating_add(len - 1) / chunk;
            last - first_chunk + 1
        };
        let full_len = self.combo.ciphertext_chunk_len() as u64;
        let start = self.header_len() as u64 + first_chunk.saturating_mul(full_len);
        let end = start.saturating_add(chunk_count.saturating_mul(full_len));
        RangePlan { first_chunk, chunk_count, ciphertext_start: start, ciphertext_end: end, skip, len }
    }

    /// Decrypt the chunks fetched for `plan` (`chunks` starts at
    /// `plan.ciphertext_start` and may be shorter than planned if the file
    /// ends earlier) and return the requested cleartext bytes. The result is
    /// shorter than `plan.len` when the range extends past the end of file.
    pub fn decrypt_range(&self, header: &FileHeader, plan: &RangePlan, chunks: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let full_len = self.combo.ciphertext_chunk_len();
        for (i, chunk) in chunks.chunks(full_len).take(plan.chunk_count as usize).enumerate() {
            out.extend_from_slice(&self.decrypt_chunk(header, plan.first_chunk + i as u64, chunk)?);
        }
        let start = plan.skip.min(out.len());
        let end = start.saturating_add(usize::try_from(plan.len).unwrap_or(usize::MAX)).min(out.len());
        out.truncate(end);
        out.drain(..start);
        Ok(out)
    }
}

/// Streaming encryptor: a [`Write`] adaptor that writes the encrypted file to
/// `inner`. Call [`finish`](Self::finish) to write the last chunk; dropping
/// the writer without `finish` attempts it but ignores errors.
pub struct EncryptingWriter<W: Write> {
    inner: Option<W>,
    cryptor: ContentCryptor,
    header: FileHeader,
    buf: Vec<u8>,
    chunk_no: u64,
    header_written: bool,
}

impl<W: Write> EncryptingWriter<W> {
    /// Start a new encrypted file with a random header.
    pub fn new(cryptor: &ContentCryptor, inner: W) -> Result<Self> {
        let header = cryptor.new_header()?;
        Ok(Self::with_header(cryptor, header, inner))
    }

    /// Start a new encrypted file with an explicit header.
    pub fn with_header(cryptor: &ContentCryptor, header: FileHeader, inner: W) -> Self {
        Self {
            inner: Some(inner),
            cryptor: cryptor.clone(),
            header,
            buf: Vec::with_capacity(CLEARTEXT_CHUNK_SIZE),
            chunk_no: 0,
            header_written: false,
        }
    }

    fn inner(&mut self) -> io::Result<&mut W> {
        self.inner.as_mut().ok_or_else(|| io::Error::other("writer already finished"))
    }

    fn write_header(&mut self) -> io::Result<()> {
        if !self.header_written {
            let h = self.cryptor.encrypt_header(&self.header)?;
            self.inner()?.write_all(&h)?;
            self.header_written = true;
        }
        Ok(())
    }

    fn flush_chunk(&mut self) -> io::Result<()> {
        let ct = self.cryptor.encrypt_chunk(&self.header, self.chunk_no, &self.buf)?;
        self.inner()?.write_all(&ct)?;
        self.chunk_no += 1;
        self.buf.clear();
        Ok(())
    }

    fn finish_inner(&mut self) -> io::Result<()> {
        self.write_header()?;
        if !self.buf.is_empty() {
            self.flush_chunk()?;
        }
        self.inner()?.flush()
    }

    /// Write the header (if not yet written) and the final chunk, flush, and
    /// return the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.finish_inner()?;
        self.inner.take().ok_or_else(|| io::Error::other("writer already finished"))
    }
}

impl<W: Write> Write for EncryptingWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.write_header()?;
        let mut written = 0;
        while written < data.len() {
            if self.buf.len() == CLEARTEXT_CHUNK_SIZE {
                // Only emit a full chunk once more data follows, so the file
                // never ends with an empty chunk.
                self.flush_chunk()?;
            }
            let take = (CLEARTEXT_CHUNK_SIZE - self.buf.len()).min(data.len() - written);
            self.buf.extend_from_slice(&data[written..written + take]);
            written += take;
        }
        Ok(written)
    }

    /// Flushes the inner writer. Buffered cleartext (< 1 chunk) stays
    /// buffered until more data arrives or [`finish`](EncryptingWriter::finish).
    fn flush(&mut self) -> io::Result<()> {
        self.inner()?.flush()
    }
}

impl<W: Write> Drop for EncryptingWriter<W> {
    fn drop(&mut self) {
        if self.inner.is_some() {
            let _ = self.finish_inner();
        }
    }
}

/// Streaming decryptor: a [`Read`] adaptor over an encrypted file.
///
/// Authentication errors surface as [`io::ErrorKind::InvalidData`] errors
/// wrapping [`Error`]. No cleartext of a chunk is returned before the whole
/// chunk has been authenticated.
pub struct DecryptingReader<R: Read> {
    inner: R,
    cryptor: ContentCryptor,
    header: Option<FileHeader>,
    chunk_no: u64,
    out: Vec<u8>,
    pos: usize,
    eof: bool,
    cbuf: Vec<u8>,
}

impl<R: Read> DecryptingReader<R> {
    /// Read a whole encrypted file (header first) from `inner`.
    pub fn new(cryptor: &ContentCryptor, inner: R) -> Self {
        Self {
            inner,
            cryptor: cryptor.clone(),
            header: None,
            chunk_no: 0,
            out: Vec::new(),
            pos: 0,
            eof: false,
            cbuf: vec![0u8; cryptor.combo().ciphertext_chunk_len()],
        }
    }

    /// Read ciphertext chunks starting at chunk `first_chunk` (for example the
    /// body of a ranged GET planned with [`ContentCryptor::range_plan`]),
    /// using an already decrypted header.
    pub fn starting_at_chunk(cryptor: &ContentCryptor, header: FileHeader, first_chunk: u64, inner: R) -> Self {
        let mut r = Self::new(cryptor, inner);
        r.header = Some(header);
        r.chunk_no = first_chunk;
        r
    }

    /// The decrypted header (available after the first successful read).
    pub fn header(&self) -> Option<&FileHeader> {
        self.header.as_ref()
    }

    fn read_full(&mut self, len: usize) -> io::Result<usize> {
        let mut n = 0;
        while n < len {
            match self.inner.read(&mut self.cbuf[n..len]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }

    fn next_chunk(&mut self) -> io::Result<()> {
        if self.header.is_none() {
            let hl = self.cryptor.header_len();
            let n = self.read_full(hl)?;
            if n < hl {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated file header"));
            }
            self.header = Some(self.cryptor.decrypt_header(&self.cbuf[..hl])?);
        }
        let full = self.cryptor.combo().ciphertext_chunk_len();
        let n = self.read_full(full)?;
        if n == 0 {
            self.eof = true;
            return Ok(());
        }
        let header = self.header.as_ref().ok_or_else(|| io::Error::other("missing header"))?;
        self.out = self.cryptor.decrypt_chunk(header, self.chunk_no, &self.cbuf[..n])?;
        self.pos = 0;
        self.chunk_no += 1;
        if n < full {
            // A short chunk must be the last one.
            let mut probe = [0u8; 1];
            if self.inner.read(&mut probe)? != 0 {
                return Err(Error::InvalidFormat("data after short final chunk".into()).into());
            }
            self.eof = true;
        }
        Ok(())
    }
}

impl<R: Read> Read for DecryptingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.out.len() {
            if self.eof {
                return Ok(0);
            }
            self.out.clear();
            self.pos = 0;
            self.next_chunk()?;
        }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}
