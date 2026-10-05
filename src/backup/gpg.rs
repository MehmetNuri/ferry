use std::process::Stdio;
use tokio::io::AsyncWriteExt;

use super::invalid;
use crate::i18n::tr;
use crate::profile::Profile;

#[derive(Clone, Debug)]
pub struct Key {
    pub fingerprint: String,
    pub user: String,
    pub own: bool,
}

pub fn available() -> bool {
    gtk::glib::find_program_in_path("gpg").is_some()
}

pub fn looks_like(data: &[u8]) -> bool {
    data.starts_with(b"-----BEGIN PGP MESSAGE-----")
        || data.first().is_some_and(|b| matches!(b, 0x84 | 0x85 | 0x8c | 0x8d | 0xc1 | 0xc3))
}

fn gpg() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("gpg");
    command
        .args(["--batch", "--no-tty", "--quiet", "--with-colons"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn parse(text: &str, encrypt_only: bool) -> Vec<(String, String)> {
    let mut keys = Vec::new();
    let mut usable = false;
    let mut fingerprint: Option<String> = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        match fields.first().copied() {
            Some("pub" | "sec") => {
                let validity = fields.get(1).copied().unwrap_or("");
                let caps = fields.get(11).copied().unwrap_or("");
                usable = !matches!(validity, "r" | "e" | "i" | "d") && (!encrypt_only || caps.contains('E'));
                fingerprint = None;
            }
            Some("fpr") if fingerprint.is_none() => {
                fingerprint = fields.get(9).map(|s| s.to_string());
            }
            Some("uid") if usable => {
                if let Some(fpr) = fingerprint.take() {
                    let user = fields.get(9).copied().unwrap_or("").replace("\\x3a", ":");
                    keys.push((fpr, user));
                }
            }
            _ => {}
        }
    }
    keys
}

pub async fn keys() -> Result<Vec<Key>, String> {
    let public = gpg().arg("--list-keys").output().await.map_err(|e| e.to_string())?;
    let secret = gpg().arg("--list-secret-keys").output().await.map_err(|e| e.to_string())?;
    let own: Vec<String> = parse(&String::from_utf8_lossy(&secret.stdout), false).into_iter().map(|(f, _)| f).collect();
    let mut keys: Vec<Key> = parse(&String::from_utf8_lossy(&public.stdout), true)
        .into_iter()
        .map(|(fingerprint, user)| Key { own: own.contains(&fingerprint), fingerprint, user })
        .collect();
    keys.sort_by_key(|k| (!k.own, k.user.to_lowercase()));
    Ok(keys)
}

async fn run(args: &[&str], input: &[u8]) -> Result<Vec<u8>, String> {
    let mut child = gpg().args(args).spawn().map_err(|_| tr("GnuPG is not installed"))?;
    let mut stdin = child.stdin.take().ok_or_else(invalid)?;
    let data = input.to_vec();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&data).await;
    });
    let output = child.wait_with_output().await.map_err(|e| e.to_string())?;
    let _ = writer.await;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let error = String::from_utf8_lossy(&output.stderr);
    Err(if error.contains("No secret key") || error.contains("no secret key") {
        tr("None of your GnuPG keys can open this backup")
    } else if error.contains("cancel") || error.contains("Operation cancelled") {
        tr("Cancelled")
    } else {
        error.lines().last().unwrap_or("gpg").trim_start_matches("gpg: ").to_string()
    })
}

pub async fn seal(plain: &[u8], fingerprint: &str) -> Result<Vec<u8>, String> {
    if !fingerprint.chars().all(|c| c.is_ascii_hexdigit()) || fingerprint.len() < 16 {
        return Err(invalid());
    }
    run(&["--armor", "--trust-model", "always", "--encrypt", "--recipient", fingerprint, "--output", "-"], plain).await
}

pub async fn open(data: &[u8]) -> Result<Vec<Profile>, String> {
    let plain = zeroize::Zeroizing::new(run(&["--decrypt", "--output", "-"], data).await?);
    if plain.len() as u64 > super::MAX_SIZE * 4 {
        return Err(invalid());
    }
    serde_json::from_slice(&plain).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colon_listing() {
        let text = "pub:u:255:22:AAAA:1:::u:::scESC:::::ed25519:::0:\nfpr:::::::::0123456789ABCDEF0123456789ABCDEF01234567:\nuid:u::::1::X::Ada \\x3a Lovelace <ada@example.org>::::::::::0:\n\
pub:e:255:22:BBBB:1:::u:::sc:::::ed25519:::0:\nfpr:::::::::FEDCBA9876543210FEDCBA9876543210FEDCBA98:\nuid:e::::1::Y::Old <old@example.org>::::::::::0:\n";
        let keys = parse(text, true);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, "0123456789ABCDEF0123456789ABCDEF01234567");
        assert_eq!(keys[0].1, "Ada : Lovelace <ada@example.org>");
        assert!(looks_like(b"-----BEGIN PGP MESSAGE-----\n"));
        assert!(!looks_like(b"[]"));
    }

    #[test]
    fn gpg_round_trip() {
        if !available() {
            return;
        }
        let home = std::env::temp_dir().join(format!("ferry-gnupg-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        unsafe {
            std::env::set_var("GNUPGHOME", &home);
        }
        let made = std::process::Command::new("gpg")
            .args([
                "--batch",
                "--passphrase",
                "",
                "--quick-gen-key",
                "Test <test@example.invalid>",
                "default",
                "default",
                "1d",
            ])
            .env("GNUPGHOME", &home)
            .output()
            .unwrap();
        assert!(made.status.success());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let result = rt.block_on(async {
            let keys = keys().await?;
            let key = keys.first().ok_or("no key")?;
            assert!(key.own);
            let list = vec![Profile { name: "a".into(), secret_key: "s3cr3t".into(), ..Default::default() }];
            let sealed = seal(&serde_json::to_vec(&list).unwrap(), &key.fingerprint).await?;
            assert!(looks_like(&sealed));
            open(&sealed).await
        });
        let _ = std::process::Command::new("gpgconf").args(["--kill", "gpg-agent"]).env("GNUPGHOME", &home).status();
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(result.unwrap()[0].secret_key, "s3cr3t");
    }
}
