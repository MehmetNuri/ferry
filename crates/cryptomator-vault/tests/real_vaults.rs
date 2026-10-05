//! Vaults made by other implementations, see tests/fixtures/SOURCES.txt.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use cryptomator_vault::names::{self, EntryKind, classify_entry};
use cryptomator_vault::{CipherCombo, Error, Vault};

#[derive(Debug, PartialEq)]
enum Node {
    File(Vec<u8>),
    Dir,
    Symlink(String),
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn open(name: &str, password: &str) -> (Vault, PathBuf) {
    let root = fixture(name);
    let mk = fs::read(root.join("masterkey.cryptomator")).unwrap();
    let cfg = fs::read_to_string(root.join("vault.cryptomator")).unwrap();
    (Vault::unlock(&mk, &cfg, password).unwrap(), root)
}

fn walk(vault: &Vault, root: &Path, dir_id: &str, prefix: &str, out: &mut BTreeMap<String, Node>) {
    let content_dir = root.join(vault.dir_path(dir_id).unwrap());
    assert!(content_dir.is_dir(), "missing content dir for {prefix:?}");
    for entry in fs::read_dir(&content_dir).unwrap() {
        let entry = entry.unwrap();
        let raw = entry.file_name().into_string().unwrap();
        let path = entry.path();
        let (name, node_dir) = match classify_entry(&raw) {
            EntryKind::DirIdBackup => {
                let id = vault.decrypt_dir_id_backup(&fs::read(&path).unwrap()).unwrap();
                assert_eq!(id, dir_id, "dirid.c9r must hold the directory's own ID");
                continue;
            }
            EntryKind::Regular => (vault.decrypt_name(&raw, dir_id).unwrap(), path.clone()),
            EntryKind::Shortened => {
                let name_file = fs::read(path.join(names::NAME_FILE)).unwrap();
                (vault.decrypt_shortened_name(&raw, &name_file, dir_id).unwrap(), path.clone())
            }
            EntryKind::Other => panic!("unexpected entry {raw}"),
        };
        let full = format!("{prefix}/{name}");
        if path.is_file() {
            out.insert(full, Node::File(vault.decrypt_file(&fs::read(&path).unwrap()).unwrap()));
        } else if node_dir.join(names::DIR_FILE).is_file() {
            let child = vault.parse_dir_file(&fs::read(node_dir.join(names::DIR_FILE)).unwrap()).unwrap();
            out.insert(full.clone(), Node::Dir);
            walk(vault, root, &child, &full, out);
        } else if node_dir.join(names::SYMLINK_FILE).is_file() {
            let t = vault.decrypt_symlink_target(&fs::read(node_dir.join(names::SYMLINK_FILE)).unwrap()).unwrap();
            out.insert(full, Node::Symlink(t));
        } else if node_dir.join(names::CONTENTS_FILE).is_file() {
            let data = vault.decrypt_file(&fs::read(node_dir.join(names::CONTENTS_FILE)).unwrap()).unwrap();
            out.insert(full, Node::File(data));
        } else {
            panic!("unknown node type at {raw}");
        }
    }
}

fn listing(vault: &Vault, root: &Path) -> BTreeMap<String, Node> {
    let mut out = BTreeMap::new();
    walk(vault, root, "", "", &mut out);
    for (k, v) in &out {
        let desc = match v {
            Node::File(d) => format!("file {} bytes", d.len()),
            Node::Dir => "dir".into(),
            Node::Symlink(t) => format!("symlink -> {t}"),
        };
        println!("{k:?}: {desc}");
    }
    out
}

#[test]
fn siv_gcm_vault_created_by_cryptomator() {
    let (vault, root) = open("vault-siv-gcm", "cryptomator-vault-sync");
    assert_eq!(vault.config().cipher_combo, CipherCombo::SivGcm);
    assert_eq!(vault.config().shortening_threshold, 220);
    assert_eq!(vault.config().jti, "3d591f10-6e66-4f5d-ac16-167297870b50");
    assert_eq!(vault.dir_path("").unwrap(), "d/WN/WA7DFYE4SWMC3FAXOQKQUOZZ2YN2K5");
    let all = listing(&vault, &root);
    assert_eq!(all.len(), 2);
    let welcome = all.iter().find(|(k, _)| k.ends_with(".rtf")).expect("an .rtf file");
    let Node::File(data) = welcome.1 else { panic!() };
    assert!(data.starts_with(b"{\\rtf"), "decrypted file is RTF");
    let mk = fs::read(root.join("masterkey.cryptomator")).unwrap();
    let cfg = fs::read_to_string(root.join("vault.cryptomator")).unwrap();
    assert_eq!(Vault::unlock(&mk, &cfg, "wrong").unwrap_err(), Error::InvalidPassword);
    assert_eq!(cfg, fs::read_to_string(root.join("vault.cryptomator.AD47C184.bkup")).unwrap());
}

#[test]
fn siv_gcm_vault_names_round_trip() {
    let (vault, root) = open("vault-siv-gcm", "cryptomator-vault-sync");
    let dir = root.join(vault.dir_path("").unwrap());
    for e in fs::read_dir(dir).unwrap() {
        let raw = e.unwrap().file_name().into_string().unwrap();
        if classify_entry(&raw) == EntryKind::Regular {
            let clear = vault.decrypt_name(&raw, "").unwrap();
            assert_eq!(vault.encrypt_name(&clear, "").unwrap(), raw);
        }
    }
}

#[test]
fn ctrmac_vault_with_nested_dirs() {
    let (vault, root) = open("vault-ctrmac-1", "qq11@@11");
    assert_eq!(vault.config().cipher_combo, CipherCombo::SivCtrMac);
    let all = listing(&vault, &root);
    let Some(Node::File(welcome)) = all.get("/WELCOME.rtf") else { panic!("WELCOME.rtf missing") };
    assert!(String::from_utf8_lossy(welcome).contains("Cryptomator"));
    let root_items: Vec<_> = all.keys().filter(|k| k.matches('/').count() == 1).collect();
    let dirs = root_items.iter().filter(|k| all[k.as_str()] == Node::Dir).count();
    assert_eq!((dirs, root_items.len() - dirs), (4, 1));
    assert!(all.keys().any(|k| k.matches('/').count() >= 3));
}

#[test]
fn ctrmac_corrupted_config_is_rejected() {
    let root = fixture("vault-ctrmac-1");
    let mk = fs::read(root.join("masterkey.cryptomator")).unwrap();
    let cfg = fs::read_to_string(root.join("vault-corrupted.cryptomator")).unwrap();
    assert!(matches!(Vault::unlock(&mk, &cfg, "qq11@@11"), Err(Error::Authentication(_))));
}

#[test]
fn ctrmac_vault_with_long_names() {
    let (vault, root) = open("vault-ctrmac-2", "12341234");
    let all = listing(&vault, &root);
    let long_dir = all.iter().find(|(k, v)| **v == Node::Dir && k.contains(&"A".repeat(220)));
    assert!(long_dir.is_some(), "shortened directory");
    let (_, file) =
        all.iter().find(|(k, _)| k.contains(&"B".repeat(220)) && k.ends_with(".txt")).expect("shortened file");
    let Node::File(data) = file else { panic!() };
    assert!(String::from_utf8_lossy(data).contains("Hello world"));
    let dir = root.join(vault.dir_path("").unwrap());
    for e in fs::read_dir(dir).unwrap() {
        let raw = e.unwrap().file_name().into_string().unwrap();
        if classify_entry(&raw) == EntryKind::Shortened {
            let nf = fs::read(fixture("vault-ctrmac-2").join(vault.dir_path("").unwrap()).join(&raw).join("name.c9s"))
                .unwrap();
            let clear = vault.decrypt_shortened_name(&raw, &nf, "").unwrap();
            assert_eq!(vault.node_name(&clear, "").unwrap().storage_name(), raw);
        }
    }
}
