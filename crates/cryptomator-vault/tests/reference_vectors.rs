//! Known-answer tests against values from the reference implementation
//! (cryptomator/cryptolib unit tests) and RFC 7914.

use cryptomator_vault::masterkey::{MasterkeyFile, ScryptParams, derive_kek};
use cryptomator_vault::{CipherCombo, ContentCryptor, Error, FileHeader, MasterKey};
use data_encoding::{BASE64, HEXLOWER};

fn b64(s: &str) -> Vec<u8> {
    BASE64.decode(s.as_bytes()).unwrap()
}

fn zero_key() -> MasterKey {
    MasterKey::from_parts([0; 32], [0; 32])
}

// ---------------------------------------------------------------- scrypt

/// RFC 7914 section 12 vectors (also cryptolib ScryptTest). Our KEK is 32
/// bytes; PBKDF2 output blocks are independent of dkLen, so it must equal the
/// first 32 bytes of the 64 byte vectors.
#[test]
fn scrypt_rfc7914() {
    let k = derive_kek("", b"", ScryptParams { cost: 16, block_size: 1 }).unwrap();
    assert_eq!(
        HEXLOWER.encode(k.as_slice()),
        "77d6576238657b203b19ca42c18a0497f16b4844e3074ae8dfdffa3fede21442"
    );
    let k = derive_kek("pleaseletmein", b"SodiumChloride", ScryptParams { cost: 16384, block_size: 8 }).unwrap();
    assert_eq!(
        HEXLOWER.encode(k.as_slice()),
        "7023bdcb3afd7348461c06cd81fd38ebfda8fbba904f8e3ea9b543f6545da1f2"
    );
}

// ---------------------------------------------------------- masterkey file

/// cryptolib MasterkeyFileAccessTest: all-zero 512 bit key, password "asd",
/// 8 zero salt bytes, N=2, r=8, version 3.
const CRYPTOLIB_MASTERKEY_JSON: &str = r#"{
  "version": 3,
  "scryptSalt": "AAAAAAAAAAA=",
  "scryptCostParam": 2,
  "scryptBlockSize": 8,
  "primaryMasterKey": "mM+qoQ+o0qvPTiDAZYt+flaC3WbpNAx1sTXaUzxwpy0M9Ctj6Tih/Q==",
  "hmacMasterKey": "mM+qoQ+o0qvPTiDAZYt+flaC3WbpNAx1sTXaUzxwpy0M9Ctj6Tih/Q==",
  "versionMac": "iUmRRHITuyJsJbVNqGNw+82YQ4A3Rma7j/y1v0DCVLA="
}"#;

#[test]
fn masterkey_file_unlock_cryptolib_vector() {
    let file = MasterkeyFile::parse(CRYPTOLIB_MASTERKEY_JSON.as_bytes()).unwrap();
    let key = file.unlock("asd").unwrap();
    assert_eq!(key, zero_key());
    assert_eq!(file.unlock("qwe").unwrap_err(), Error::InvalidPassword);
}

#[test]
fn masterkey_file_lock_matches_cryptolib_vector() {
    let params = ScryptParams { cost: 2, block_size: 8 };
    let file = MasterkeyFile::lock_with_salt(&zero_key(), "asd", params, vec![0; 8], 3).unwrap();
    let expected = MasterkeyFile::parse(CRYPTOLIB_MASTERKEY_JSON.as_bytes()).unwrap();
    assert_eq!(file, expected);
    // And the JSON we write parses back to the same thing.
    assert_eq!(MasterkeyFile::parse(file.to_json().as_bytes()).unwrap(), expected);
}

// ---------------------------------------------------- SIV_GCM (cryptolib v2)

const V2_HEADER: &str = "AAAAAAAAAAAAAAAAMVi/wrKflJEHTsXTuvOdGHJgA8o3pip00aL1jnUGNY7dSrEoTUrhey+maVG6P0F2RBmZR74SjU0=";

fn gcm() -> ContentCryptor {
    ContentCryptor::new(&zero_key(), CipherCombo::SivGcm)
}

fn zero_header(combo: CipherCombo) -> FileHeader {
    FileHeader::from_parts(vec![0; combo.nonce_len()], [0; 32])
}

#[test]
fn gcm_header_vector() {
    let c = gcm();
    assert_eq!(c.encrypt_header(&zero_header(CipherCombo::SivGcm)).unwrap(), b64(V2_HEADER));
    let h = c.decrypt_header(&b64(V2_HEADER)).unwrap();
    assert_eq!(h.nonce(), &[0; 12]);
    assert_eq!(h.content_key(), &[0; 32]);
    // FileHeaderCryptorImplTest.testDecryptionWithInvalidTag1/2
    for bad in [
        "AAAAAAAAAAAAAAAAMVi/wrKflJEHTsXTuvOdGHJgA8o3pip00aL1jnUGNY7dSrEoTUrhey+maVG6P0F2RBmZR74SjUA=",
        "AAAAAAAAAAAAAAAAMVi/wrKflJEHTsXTuvOdGHJgA8o3pip00aL1jnUGNY7dSrEoTUrhey+maVG6P0F2RBmZR74SjUa=",
    ] {
        assert!(matches!(c.decrypt_header(&lenient_b64(bad)), Err(Error::Authentication(_))));
    }
    assert!(matches!(c.decrypt_header(&[0; 7]), Err(Error::InvalidFormat(_))));
}

/// Base64 decode ignoring non-zero trailing bits (Guava semantics).
fn lenient_b64(s: &str) -> Vec<u8> {
    let mut spec = data_encoding::BASE64.specification();
    spec.check_trailing_bits = false;
    spec.encoding().unwrap().decode(s.as_bytes()).unwrap()
}

#[test]
fn gcm_chunk_vectors() {
    let c = gcm();
    let h = zero_header(CipherCombo::SivGcm);
    // FileContentCryptorImplTest.testChunkEncryption (nonce 0x33..)
    let ct = c.encrypt_chunk_with_nonce(&h, 0, b"hello world", &[0x33; 12]).unwrap();
    assert_eq!(ct, b64("MzMzMzMzMzMzMzMzbYvL7CusRmzk70Kn1QxFA5WQg/hgKeba4bln"));
    // testChunkDecryption
    let ct = b64("VVVVVVVVVVVVVVVVnHVdh+EbedvPeiCwCdaTYpzn1CXQjhSh7PHv");
    assert_eq!(c.decrypt_chunk(&h, 0, &ct).unwrap(), b"hello world");
    // wrong chunk number = reordering
    assert!(matches!(c.decrypt_chunk(&h, 1, &ct), Err(Error::Authentication(_))));
    // testUnauthenticChunkDecryption: NONCE, CONTENT, TAG
    for bad in [
        "vVVVVVVVVVVVVVVVnHVdh+EbedvPeiCwCdaTYpzn1CXQjhSh7PHv",
        "VVVVVVVVVVVVVVVVNHVdh+EbedvPeiCwCdaTYpzn1CXQjhSh7PHv",
        "VVVVVVVVVVVVVVVVnHVdh+EbedvPeiCwCdaTYpzn1CXQjhSh7PHV",
    ] {
        assert!(matches!(c.decrypt_chunk(&h, 0, &lenient_b64(bad)), Err(Error::Authentication(_))));
    }
}

const V2_FILE: &str = "VVVVVVVVVVVVVVVVC+/OFHHE8UvKYTOPlrMO5rCRLAI7/zk8Hjoisja03+yi9ugeeMz1evZhxDExrawl93vf9DKQPx5VVVVVVVVVVVVVVVVSxe6Nf7RO8orsVTzHAmXlNSy1oJpDrg9coV0=";

#[test]
fn gcm_file_vector() {
    let c = gcm();
    let file = b64(V2_FILE);
    assert_eq!(c.decrypt(&file).unwrap(), b"hello world");
    assert_eq!(c.cleartext_size(file.len() as u64).unwrap(), 11);
    // The test's mocked RNG produced header nonce 0x55.., content key 0x77..,
    // chunk nonce 0x55..; re-encrypting with those must give identical bytes.
    let h = c.decrypt_header(&file).unwrap();
    assert_eq!(h.nonce(), &[0x55; 12]);
    assert_eq!(h.content_key(), &[0x77; 32]);
    let mut out = c.encrypt_header(&h).unwrap();
    out.extend(c.encrypt_chunk_with_nonce(&h, 0, b"hello world", &[0x55; 12]).unwrap());
    assert_eq!(out, file);
    // testDecryptionWithUnauthenticFirstChunk
    for bad in [
        "VVVVVVVVVVVVVVVVC+/OFHHE8UvKYTOPlrMO5rCRLAI7/zk8Hjoisja03+yi9ugeeMz1evZhxDExrawl93vf9DKQPx5vVVVVVVVvVVVVVVVSxe6Nf7RO8orsVTzHAmXlNSy1oJpDrg9coV0=",
        "VVVVVVVVVVVVVVVVC+/OFHHE8UvKYTOPlrMO5rCRLAI7/zk8Hjoisja03+yi9ugeeMz1evZhxDExrawl93vf9DKQPx5VVVVVVVVvVVVVVVVsxe6Nf7RO8orsVTzHAmXlNSy1oJpDrg9coV0=",
        "VVVVVVVVVVVVVVVVC+/OFHHE8UvKYTOPlrMO5rCRLAI7/zk8Hjoisja03+yi9ugeeMz1evZhxDExrawl93vf9DKQPx5VVVVVVVVVVVVVVVVSxe6Nf7RO8orsVTzHAmXlNSy1oJpDrg9coVx=",
    ] {
        assert!(matches!(c.decrypt(&lenient_b64(bad)), Err(Error::Authentication(_))));
    }
}

// ------------------------------------------------- SIV_CTRMAC (cryptolib v1)

const V1_HEADER: &str = "AAAAAAAAAAAAAAAAAAAAACNqP4ddv3Z2rUiiFJKEIIdTD4r7x0U2ualjtPHEy3OLzqdAPU1ga24VjC86+zlHN49BfMdzvHF3f9EE0LSnRLSsu6ps3IRcJg==";
const V1_CHUNK: &str = "AAAAAAAAAAAAAAAAAAAAALTwrBTNYP7m3yTGKlhka9WPvX1Lpn5EYfVxlyX1ISgRXtdRnivM7r6F3Og=";

fn ctrmac() -> ContentCryptor {
    ContentCryptor::new(&zero_key(), CipherCombo::SivCtrMac)
}

#[test]
fn ctrmac_header_vector() {
    let c = ctrmac();
    assert_eq!(c.encrypt_header(&zero_header(CipherCombo::SivCtrMac)).unwrap(), b64(V1_HEADER));
    let h = c.decrypt_header(&b64(V1_HEADER)).unwrap();
    assert_eq!(h.content_key(), &[0; 32]);
    // v1 FileHeaderCryptorImplTest invalid header vectors
    for bad in [
        "AAAAAAAAAAAAAAAAAAAAANyVwHiiQImjrUiiFJKEIIdTD4r7x0U2ualjtPHEy3OLzqdAPU1ga26lJzstK9RUv1hj5zDC4wC9FgMfoVE1mD0HnuENuYXkJa==",
        "aAAAAAAAAAAAAAAAAAAAANyVwHiiQImjrUiiFJKEIIdTD4r7x0U2ualjtPHEy3OLzqdAPU1ga26lJzstK9RUv1hj5zDC4wC9FgMfoVE1mD0HnuENuYXkJA==",
    ] {
        assert!(matches!(c.decrypt_header(&lenient_b64(bad)), Err(Error::Authentication(_))));
    }
}

#[test]
fn ctrmac_chunk_and_file_vectors() {
    let c = ctrmac();
    let h = zero_header(CipherCombo::SivCtrMac);
    let ct = c.encrypt_chunk_with_nonce(&h, 0, b"hello world", &[0; 16]).unwrap();
    assert_eq!(ct, b64(V1_CHUNK));
    assert_eq!(c.decrypt_chunk(&h, 0, &b64(V1_CHUNK)).unwrap(), b"hello world");
    let tampered = lenient_b64("AAAAAAAAAAAAAAAAAAAAALTwrBTNYP7m3yTGKlhka9WPvX1Lpn5EYfVxlyX1ISgRXtdRnivM7r6F3OG=");
    assert!(matches!(c.decrypt_chunk(&h, 0, &tampered), Err(Error::Authentication(_))));

    // testFileEncryption with the all-zero RNG: header || chunk.
    let file = [b64(V1_HEADER), b64(V1_CHUNK)].concat();
    assert_eq!(file.len(), 147);
    assert_eq!(c.decrypt(&file).unwrap(), b"hello world");

    // testFileDecryption
    let file = b64(concat!(
        "AAAAAAAAAAAAAAAAAAAAANyVwHiiQImCrUiiFJKEIIdTD4r7x0U2ualjtPHEy3OLzqdAPU1ga27XjlTjFxC1VCqZa+",
        "L2eH+xWVgrSLX+JkG35ZJxk5xXswAAAAAAAAAAAAAAAAAAAAC08KwUzWD+5t8kxipYZGvVj719S6Z+RGH1cZcl9SEoEV7XUZ4rzO6+hdzo"
    ));
    assert_eq!(c.decrypt(&file).unwrap(), b"hello world");
    let tampered = lenient_b64(concat!(
        "AAAAAAAAAAAAAAAAAAAAANyVwHiiQImCrUiiFJKEIIdTD4r7x0U2ualjtPHEy3OLzqdAPU1ga27XjlTjFxC1VCqZa+",
        "L2eH+xWVgrSLX+JkG35ZJxk5xXswAAAAAAAAAAAAAAAAAAAAC08KwUzWD+5t8kxipYZGvVj719S6Z+RGH1cZcl9SEoEV7XUZ4rzO6+hdzO"
    ));
    assert!(matches!(c.decrypt(&tampered), Err(Error::Authentication(_))));
}
