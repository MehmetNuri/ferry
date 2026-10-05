//! No last-chunk flag: dropping whole trailing chunks goes unnoticed.

use std::io::{self, Read, Write};

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use ctr::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::error::{Error, Result, invalid};
use crate::keys::{MasterKey, random_bytes};

pub const CLEARTEXT_CHUNK_SIZE: usize = 32 * 1024;
pub const CONTENT_KEY_LEN: usize = 32;
const RESERVED: [u8; 8] = [0xFF; 8];
const GCM_NONCE: usize = 12;
const GCM_TAG: usize = 16;
const CTR_NONCE: usize = 16;
const MAC_LEN: usize = 32;

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;
type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CipherCombo {
    SivGcm,
    SivCtrMac,
}

impl CipherCombo {
    pub fn name(self) -> &'static str {
        match self {
            CipherCombo::SivGcm => "SIV_GCM",
            CipherCombo::SivCtrMac => "SIV_CTRMAC",
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        match name {
            "SIV_GCM" => Ok(CipherCombo::SivGcm),
            "SIV_CTRMAC" => Ok(CipherCombo::SivCtrMac),
            other => Err(Error::Unsupported(format!("cipher combo {other:?}"))),
        }
    }

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

    /// 68 for GCM, 88 for CTRMAC.
    pub fn header_len(self) -> usize {
        self.nonce_len() + RESERVED.len() + CONTENT_KEY_LEN + self.tag_len()
    }

    /// Per-chunk overhead: 28 for GCM, 48 for CTRMAC.
    pub fn chunk_overhead(self) -> usize {
        self.nonce_len() + self.tag_len()
    }

    pub fn ciphertext_chunk_len(self) -> usize {
        CLEARTEXT_CHUNK_SIZE + self.chunk_overhead()
    }
}

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
    pub fn from_parts(nonce: Vec<u8>, content_key: [u8; CONTENT_KEY_LEN]) -> Self {
        Self { nonce, content_key: Zeroizing::new(content_key) }
    }

    pub fn nonce(&self) -> &[u8] {
        &self.nonce
    }

    pub fn content_key(&self) -> &[u8; CONTENT_KEY_LEN] {
        &self.content_key
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangePlan {
    pub first_chunk: u64,
    pub chunk_count: u64,
    pub ciphertext_start: u64,
    pub ciphertext_end: u64,
    pub skip: usize,
    pub len: u64,
}

impl RangePlan {
    pub fn http_range(&self) -> String {
        format!("bytes={}-{}", self.ciphertext_start, self.ciphertext_end.saturating_sub(1))
    }
}

fn gcm_nonce(bytes: &[u8]) -> Result<aes_gcm::Nonce<aes_gcm::aead::consts::U12>> {
    aes_gcm::Nonce::<aes_gcm::aead::consts::U12>::try_from(bytes).map_err(|_| invalid("bad GCM nonce length"))
}

#[derive(Clone, Debug)]
pub struct ContentCryptor {
    key: MasterKey,
    combo: CipherCombo,
}

impl ContentCryptor {
    pub fn new(key: &MasterKey, combo: CipherCombo) -> Self {
        Self { key: key.clone(), combo }
    }

    pub fn combo(&self) -> CipherCombo {
        self.combo
    }

    pub fn header_len(&self) -> usize {
        self.combo.header_len()
    }

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
                let mut mac =
                    <HmacSha256 as hmac::KeyInit>::new_from_slice(self.key.mac_key()).map_err(|_| invalid("key"))?;
                mac.update(&out);
                out.extend_from_slice(&mac.finalize().into_bytes());
            }
        }
        Ok(out)
    }

    pub fn decrypt_header(&self, ciphertext: &[u8]) -> Result<FileHeader> {
        let n = self.combo.nonce_len();
        let ct = ciphertext.get(..self.header_len()).ok_or_else(|| invalid("ciphertext shorter than file header"))?;
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
                let mut mac =
                    <HmacSha256 as hmac::KeyInit>::new_from_slice(self.key.mac_key()).map_err(|_| invalid("key"))?;
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
        let mut mac = <HmacSha256 as hmac::KeyInit>::new_from_slice(self.key.mac_key()).map_err(|_| invalid("key"))?;
        mac.update(&header.nonce);
        mac.update(&chunk_no.to_be_bytes());
        mac.update(nonce_and_ct);
        Ok(mac)
    }

    pub fn encrypt_chunk(&self, header: &FileHeader, chunk_no: u64, cleartext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; CTR_NONCE];
        let nonce = &mut nonce[..self.combo.nonce_len()];
        random_bytes(nonce)?;
        self.encrypt_chunk_with_nonce(header, chunk_no, cleartext, nonce)
    }

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

    pub fn encrypt(&self, cleartext: &[u8]) -> Result<Vec<u8>> {
        let header = self.new_header()?;
        self.encrypt_with_header(&header, cleartext)
    }

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

    pub fn ciphertext_size(&self, cleartext_size: u64) -> u64 {
        let chunk = CLEARTEXT_CHUNK_SIZE as u64;
        let full = cleartext_size / chunk;
        let rem = cleartext_size % chunk;
        let overhead = self.combo.chunk_overhead() as u64;
        let tail = if rem == 0 { 0 } else { rem + overhead };
        self.header_len() as u64 + full * self.combo.ciphertext_chunk_len() as u64 + tail
    }

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

pub struct EncryptingWriter<W: Write> {
    inner: Option<W>,
    cryptor: ContentCryptor,
    header: FileHeader,
    buf: Vec<u8>,
    chunk_no: u64,
    header_written: bool,
}

impl<W: Write> EncryptingWriter<W> {
    pub fn new(cryptor: &ContentCryptor, inner: W) -> Result<Self> {
        let header = cryptor.new_header()?;
        Ok(Self::with_header(cryptor, header, inner))
    }

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
                self.flush_chunk()?;
            }
            let take = (CLEARTEXT_CHUNK_SIZE - self.buf.len()).min(data.len() - written);
            self.buf.extend_from_slice(&data[written..written + take]);
            written += take;
        }
        Ok(written)
    }

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

    pub fn starting_at_chunk(cryptor: &ContentCryptor, header: FileHeader, first_chunk: u64, inner: R) -> Self {
        let mut r = Self::new(cryptor, inner);
        r.header = Some(header);
        r.chunk_no = first_chunk;
        r
    }

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
