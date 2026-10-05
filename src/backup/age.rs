use std::io::{Read, Write};
use std::str::FromStr;

use age::secrecy::SecretString;
use zeroize::Zeroizing;

use super::invalid;
use crate::i18n::{tr, trf};
use crate::profile::Profile;

pub enum Prompt {
    Message(String),
    Confirm { message: String, yes: String, no: Option<String>, reply: std::sync::mpsc::Sender<Option<bool>> },
    Text { description: String, secret: bool, reply: std::sync::mpsc::Sender<Option<String>> },
}

#[derive(Clone)]
pub struct Prompts(pub tokio::sync::mpsc::UnboundedSender<Prompt>);

impl Prompts {
    fn ask_text(&self, description: &str, secret: bool) -> Option<String> {
        let (reply, answer) = std::sync::mpsc::channel();
        self.0.send(Prompt::Text { description: description.to_string(), secret, reply }).ok()?;
        answer.recv().ok().flatten()
    }
}

impl age::Callbacks for Prompts {
    fn display_message(&self, message: &str) {
        let _ = self.0.send(Prompt::Message(message.to_string()));
    }

    fn confirm(&self, message: &str, yes: &str, no: Option<&str>) -> Option<bool> {
        let (reply, answer) = std::sync::mpsc::channel();
        self.0
            .send(Prompt::Confirm {
                message: message.to_string(),
                yes: yes.to_string(),
                no: no.map(str::to_string),
                reply,
            })
            .ok()?;
        answer.recv().ok().flatten()
    }

    fn request_public_string(&self, description: &str) -> Option<String> {
        self.ask_text(description, false)
    }

    fn request_passphrase(&self, description: &str) -> Option<SecretString> {
        self.ask_text(description, true).map(SecretString::from)
    }
}

pub fn yubikey_plugin() -> bool {
    gtk::glib::find_program_in_path("age-plugin-yubikey").is_some()
}

pub async fn yubikey_recipients() -> Result<Vec<String>, String> {
    let output =
        tokio::process::Command::new("age-plugin-yubikey").arg("--list").output().await.map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let found: Vec<String> =
        text.lines().map(str::trim).filter(|l| l.starts_with("age1yubikey1")).map(str::to_string).collect();
    if found.is_empty() {
        return Err(tr("No YubiKey with an age key was found. Set one up with age-plugin-yubikey first."));
    }
    Ok(found)
}

pub fn split(text: &str) -> Vec<String> {
    text.split([',', '\n'])
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

pub fn valid(recipient: &str) -> bool {
    age::x25519::Recipient::from_str(recipient).is_ok()
        || age::ssh::Recipient::from_str(recipient).is_ok()
        || age::plugin::Recipient::from_str(recipient).is_ok()
}

pub fn seal(plain: &[u8], recipients: &[String]) -> Result<Vec<u8>, String> {
    let mut native: Vec<Box<dyn age::Recipient + Send>> = Vec::new();
    let mut plugins: Vec<age::plugin::Recipient> = Vec::new();
    for line in recipients {
        if let Ok(r) = age::x25519::Recipient::from_str(line) {
            native.push(Box::new(r));
        } else if let Ok(r) = age::ssh::Recipient::from_str(line) {
            native.push(Box::new(r));
        } else if let Ok(r) = age::plugin::Recipient::from_str(line) {
            plugins.push(r);
        } else {
            return Err(trf("“{key}” is not an age or SSH public key", &[("key", line)]));
        }
    }
    let mut names: Vec<String> = plugins.iter().map(|r| r.plugin().to_string()).collect();
    names.sort();
    names.dedup();
    for name in names {
        let plugin = age::plugin::RecipientPluginV1::new(&name, &plugins, &[], age::NoCallbacks)
            .map_err(|_| trf("The age plugin “{name}” is not installed", &[("name", &format!("age-plugin-{name}"))]))?;
        native.push(Box::new(plugin));
    }
    if native.is_empty() {
        return Err(tr("Enter at least one public key"));
    }
    let encryptor = age::Encryptor::with_recipients(native.iter().map(|r| r.as_ref() as &dyn age::Recipient))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let armored =
        age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor).map_err(|e| e.to_string())?;
    let mut writer = encryptor.wrap_output(armored).map_err(|e| e.to_string())?;
    writer.write_all(plain).map_err(|e| e.to_string())?;
    writer.finish().and_then(|armor| armor.finish()).map_err(|e| e.to_string())?;
    Ok(out)
}

fn decryptor(data: &[u8]) -> Result<age::Decryptor<age::armor::ArmoredReader<std::io::BufReader<&[u8]>>>, String> {
    age::Decryptor::new_buffered(age::armor::ArmoredReader::new(data)).map_err(|_| invalid())
}

pub fn is_passphrase(data: &[u8]) -> bool {
    decryptor(data).is_ok_and(|d| d.is_scrypt())
}

fn finish(
    decryptor: age::Decryptor<age::armor::ArmoredReader<std::io::BufReader<&[u8]>>>,
    identities: &[Box<dyn age::Identity>],
) -> Result<Vec<Profile>, String> {
    let mut reader =
        decryptor.decrypt(identities.iter().map(|i| i.as_ref() as &dyn age::Identity)).map_err(|e| match e {
            age::DecryptError::NoMatchingKeys => tr("None of your keys can open this backup"),
            age::DecryptError::DecryptionFailed | age::DecryptError::KeyDecryptionFailed => {
                tr("Wrong password, or the file is damaged")
            }
            other => other.to_string(),
        })?;
    let mut plain = Zeroizing::new(Vec::new());
    reader.read_to_end(&mut plain).map_err(|_| tr("Wrong password, or the file is damaged"))?;
    serde_json::from_slice(&plain).map_err(|_| invalid())
}

pub fn open_passphrase(data: &[u8], passphrase: &str) -> Result<Vec<Profile>, String> {
    let identity = age::scrypt::Identity::new(SecretString::from(passphrase.to_string()));
    finish(decryptor(data)?, &[Box::new(identity)])
}

fn identities_from(path: &std::path::Path, prompts: &Prompts) -> Vec<Box<dyn age::Identity>> {
    let Ok(text) = std::fs::read(path) else { return Vec::new() };
    let text = Zeroizing::new(text);
    if let Ok(file) = age::IdentityFile::from_buffer(text.as_slice())
        && let Ok(found) = file.with_callbacks(prompts.clone()).into_identities()
        && !found.is_empty()
    {
        return found;
    }
    match age::ssh::Identity::from_buffer(text.as_slice(), Some(path.display().to_string())) {
        Ok(identity) if !matches!(identity, age::ssh::Identity::Unsupported(_)) => {
            vec![Box::new(identity.with_callbacks(prompts.clone()))]
        }
        _ => Vec::new(),
    }
}

fn default_identities(prompts: &Prompts) -> Vec<Box<dyn age::Identity>> {
    let home = gtk::glib::home_dir();
    let config = gtk::glib::user_config_dir();
    let mut found = Vec::new();
    for path in [
        config.join("age/keys.txt"),
        config.join("sops/age/keys.txt"),
        home.join(".ssh/id_ed25519"),
        home.join(".ssh/id_rsa"),
    ] {
        if path.is_file() {
            found.extend(identities_from(&path, prompts));
        }
    }
    if yubikey_plugin()
        && let identity = age::plugin::Identity::default_for_plugin("yubikey")
        && let Ok(plugin) = age::plugin::IdentityPluginV1::new("yubikey", &[identity], prompts.clone())
    {
        found.push(Box::new(plugin));
    }
    found
}

pub fn open(data: &[u8], identity_file: Option<&std::path::Path>, prompts: &Prompts) -> Result<Vec<Profile>, String> {
    let identities = match identity_file {
        Some(path) => identities_from(path, prompts),
        None => default_identities(prompts),
    };
    if identities.is_empty() {
        return Err(if identity_file.is_some() {
            tr("The file is not an age identity or SSH private key")
        } else {
            tr("None of your keys can open this backup")
        });
    }
    finish(decryptor(data)?, &identities)
}

#[cfg(test)]
mod tests {
    use super::*;
    use age::secrecy::ExposeSecret;

    #[test]
    fn age_round_trip() {
        let key = age::x25519::Identity::generate();
        let list = vec![Profile { name: "a".into(), secret_key: "s3cr3t".into(), ..Default::default() }];
        let sealed = seal(&serde_json::to_vec(&list).unwrap(), &[key.to_public().to_string()]).unwrap();
        assert!(sealed.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----"));
        assert!(!is_passphrase(&sealed));
        let dir = std::env::temp_dir().join(format!("ferry-age-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("key.txt");
        std::fs::write(&file, key.to_string().expose_secret()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let opened = open(&sealed, Some(&file), &Prompts(tx));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(opened.unwrap()[0].secret_key, "s3cr3t");
        assert!(!valid("age1notakey"));
        assert!(valid(&key.to_public().to_string()));
    }
}
