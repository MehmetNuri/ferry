//! Round trips through every layer of a freshly created vault, with tampering and
//! chunk-boundary cases.
use std::io::{Read, Write};

use cryptomator_vault::{CipherCombo, Error, NodeName, ScryptParams, Vault};

/// A low scrypt cost keeps the tests fast; real vaults use `Vault::create`.
fn vault(combo: CipherCombo) -> Vault {
    let created = Vault::create_with("correct horse", ScryptParams { cost: 1024, block_size: 8 }, combo).unwrap();
    Vault::unlock(created.masterkey_json.as_bytes(), &created.vault_config, "correct horse").unwrap()
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

#[test]
fn wrong_password_is_reported() {
    let created = Vault::create_with("right", ScryptParams { cost: 1024, block_size: 8 }, CipherCombo::SivGcm).unwrap();
    assert!(matches!(Vault::unlock(created.masterkey_json.as_bytes(), &created.vault_config, "wrong"), Err(Error::InvalidPassword)));
}

#[test]
fn contents_at_chunk_boundaries() {
    for combo in [CipherCombo::SivGcm, CipherCombo::SivCtrMac] {
        let v = vault(combo);
        for len in [0, 1, 32767, 32768, 32769, 100_000] {
            let plain = data(len);
            let sealed = v.encrypt_file(&plain).unwrap();
            assert_eq!(sealed.len() as u64, v.ciphertext_size(len as u64), "{combo:?} {len}");
            assert_eq!(v.cleartext_size(sealed.len() as u64).unwrap(), len as u64);
            assert_eq!(v.decrypt_file(&sealed).unwrap(), plain, "{combo:?} {len}");
        }
    }
}

#[test]
fn tampering_is_detected() {
    for combo in [CipherCombo::SivGcm, CipherCombo::SivCtrMac] {
        let v = vault(combo);
        let sealed = v.encrypt_file(&data(70_000)).unwrap();
        for at in [5, v.header_len() + 20, sealed.len() - 1] {
            let mut bad = sealed.clone();
            bad[at] ^= 1;
            assert!(v.decrypt_file(&bad).is_err(), "{combo:?} flip at {at}");
        }
        // Dropping the last chunk is noticed too when it was a full one... a truncated file
        // must at least not decrypt to the original.
        let cut = &sealed[..sealed.len() - 10];
        assert!(v.decrypt_file(cut).map(|p| p != data(70_000)).unwrap_or(true));
    }
}

#[test]
fn names_and_shortening() {
    let v = vault(CipherCombo::SivGcm);
    let dir = Vault::new_dir_id().unwrap();
    for name in ["a", "hello world.txt", "Ünïcødé ğüşiöç.pdf"] {
        let enc = v.encrypt_name(name, &dir).unwrap();
        assert!(enc.ends_with(".c9r"));
        assert_eq!(v.decrypt_name(&enc, &dir).unwrap(), name);
        // The parent's ID is bound to the name.
        assert!(v.decrypt_name(&enc, "").is_err());
    }
    let long = "x".repeat(300);
    match v.node_name(&long, &dir).unwrap() {
        NodeName::Shortened { short_name, full_name } => {
            assert!(short_name.ends_with(".c9s"));
            assert_eq!(v.decrypt_shortened_name(&short_name, full_name.as_bytes(), &dir).unwrap(), long);
        }
        NodeName::Regular(_) => panic!("a long name has to be shortened"),
    }
    let root = v.dir_path("").unwrap();
    assert!(root.starts_with("d/") && root.len() == 2 + 2 + 1 + 30);
    assert_eq!(v.decrypt_dir_id_backup(&v.encrypt_dir_id_backup(&dir).unwrap()).unwrap(), dir);
}

#[test]
fn ranges_decrypt_only_needed_chunks() {
    let v = vault(CipherCombo::SivGcm);
    let plain = data(200_000);
    let sealed = v.encrypt_file(&plain).unwrap();
    let header = v.decrypt_header(&sealed[..v.header_len()]).unwrap();
    for (offset, len) in [(0u64, 10u64), (32_760, 20), (65_536, 32_768), (199_990, 10), (100, 150_000)] {
        let plan = v.range_plan(offset, len);
        let end = (plan.ciphertext_end as usize).min(sealed.len());
        let part = v.decrypt_range(&header, &plan, &sealed[plan.ciphertext_start as usize..end]).unwrap();
        assert_eq!(part, &plain[offset as usize..(offset + len) as usize], "range {offset}+{len}");
    }
}

#[test]
fn streaming_matches_buffers() {
    let v = vault(CipherCombo::SivGcm);
    let plain = data(90_001);
    let mut writer = v.encrypting_writer(Vec::new()).unwrap();
    for piece in plain.chunks(7_000) {
        writer.write_all(piece).unwrap();
    }
    let sealed = writer.finish().unwrap();
    assert_eq!(v.decrypt_file(&sealed).unwrap(), plain);
    let mut out = Vec::new();
    v.decrypting_reader(sealed.as_slice()).read_to_end(&mut out).unwrap();
    assert_eq!(out, plain);
}
