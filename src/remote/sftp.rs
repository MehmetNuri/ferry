use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use super::{Stat, host_port, join, pump};
use crate::i18n::{tr, trf};
use crate::profile::Profile;
use crate::s3::{Progress, Res};

pub const UNKNOWN_HOST: &str = "ssh-unknown-host";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownHost {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub jump: bool,
}

impl UnknownHost {
    fn encode(&self) -> String {
        format!(
            "{UNKNOWN_HOST}\n{}\n{}\n{}\n{}",
            self.host,
            self.port,
            self.fingerprint,
            if self.jump { "jump" } else { "target" }
        )
    }

    pub fn decode(error: &str) -> Option<UnknownHost> {
        let mut lines = error.strip_prefix(UNKNOWN_HOST)?.trim_start_matches('\n').lines();
        Some(UnknownHost {
            host: lines.next()?.to_string(),
            port: lines.next()?.parse().ok()?,
            fingerprint: lines.next()?.to_string(),
            jump: lines.next()? == "jump",
        })
    }
}

#[derive(Clone, Default)]
enum KeyCheck {
    #[default]
    Fine,
    Unknown(String),
    Changed,
}

struct Handler {
    host: String,
    port: u16,
    pinned: String,
    found: Arc<StdMutex<KeyCheck>>,
}

fn key_of(key: &PublicKeyOrCertificate) -> PublicKey {
    match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
        PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
    }
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, server_public_key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let key = key_of(server_public_key);
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        if !self.pinned.is_empty() && self.pinned == fingerprint {
            return Ok(true);
        }
        let verdict = match russh::keys::check_known_hosts(&self.host, self.port, &key) {
            Ok(true) => return Ok(true),
            Err(russh::keys::Error::KeyChanged { .. }) => KeyCheck::Changed,
            _ => KeyCheck::Unknown(fingerprint),
        };
        *self.found.lock().unwrap_or_else(PoisonError::into_inner) = verdict;
        Ok(false)
    }
}

fn parse_jump(text: &str, default_user: &str) -> Res<(String, String, u16)> {
    let (user, server) = text
        .trim()
        .rsplit_once('@')
        .map(|(u, s)| (u.to_string(), s))
        .unwrap_or((default_user.to_string(), text.trim()));
    let (host, port) = host_port(server, 22)?;
    Ok((user, host, port))
}

fn config() -> Arc<client::Config> {
    Arc::new(client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(600)),
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        nodelay: true,
        ..Default::default()
    })
}

fn key_failure(found: &Arc<StdMutex<KeyCheck>>, host: &str, port: u16, jump: bool, error: russh::Error) -> String {
    match found.lock().unwrap_or_else(PoisonError::into_inner).clone() {
        KeyCheck::Unknown(fingerprint) => UnknownHost { host: host.to_string(), port, fingerprint, jump }.encode(),
        KeyCheck::Changed => trf(
            "The key of {host} has changed since it was saved in ~/.ssh/known_hosts. This can mean that someone is intercepting the connection. If the server was reinstalled, remove its old line from known_hosts.",
            &[("host", host)],
        ),
        KeyCheck::Fine => describe(error),
    }
}

fn describe(error: russh::Error) -> String {
    match error {
        russh::Error::IO(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            tr("The server refused the connection; check the address and port")
        }
        russh::Error::IO(e) => e.to_string(),
        russh::Error::ConnectionTimeout | russh::Error::InactivityTimeout => tr("The server did not answer"),
        other => other.to_string(),
    }
}

async fn authenticate(handle: &mut client::Handle<Handler>, user: &str, key_file: &str, password: &str) -> Res<()> {
    let hash = handle.best_supported_rsa_hash().await.ok().flatten().flatten();
    if let Ok(mut agent) = russh::keys::agent::client::AgentClient::connect_env().await
        && let Ok(identities) = agent.request_identities().await
    {
        for identity in identities {
            let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = identity else { continue };
            if handle.authenticate_publickey_with(user, key, hash, &mut agent).await.is_ok_and(|r| r.success()) {
                return Ok(());
            }
        }
    }
    let home = gtk::glib::home_dir().join(".ssh");
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if !key_file.trim().is_empty() {
        files.push(key_file.trim().into());
    }
    files.extend(["id_ed25519", "id_ecdsa", "id_rsa"].iter().map(|n| home.join(n)));
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let text = zeroize::Zeroizing::new(text);
        let key = russh::keys::decode_secret_key(&text, None)
            .or_else(|_| russh::keys::decode_secret_key(&text, Some(password)));
        let Ok(key) = key else { continue };
        let signed = handle.authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await;
        if signed.is_ok_and(|r| r.success()) {
            return Ok(());
        }
    }
    if !password.is_empty() {
        if handle.authenticate_password(user, password).await.is_ok_and(|r| r.success()) {
            return Ok(());
        }
        // Some servers only offer keyboard-interactive for passwords.
        if let Ok(client::KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. }) =
            handle.authenticate_keyboard_interactive_start(user, None).await
        {
            let answers = prompts.iter().map(|_| password.to_string()).collect();
            if handle
                .authenticate_keyboard_interactive_respond(answers)
                .await
                .is_ok_and(|r| matches!(r, client::KeyboardInteractiveAuthResponse::Success))
            {
                return Ok(());
            }
        }
    }
    Err(trf(
        "The server did not accept the sign-in of “{user}”. Check the user name, the password or the key.",
        &[("user", user)],
    ))
}

struct Connected {
    sftp: Arc<SftpSession>,
    // Must stay alive, dropping them closes the tunnel.
    _handles: Vec<client::Handle<Handler>>,
}

pub struct Sftp {
    profile: Profile,
    root: StdMutex<String>,
    session: Mutex<Option<Connected>>,
}

impl Sftp {
    pub async fn connect(profile: &Profile) -> Res<Sftp> {
        let sftp = Sftp {
            profile: profile.clone(),
            root: StdMutex::new(profile.remote_path.trim().to_string()),
            session: Mutex::new(None),
        };
        sftp.session().await?;
        Ok(sftp)
    }

    async fn open(&self) -> Res<Connected> {
        let p = &self.profile;
        let (host, port) = host_port(&p.endpoint, 22)?;
        let user = if p.access_key.trim().is_empty() {
            std::env::var("USER").unwrap_or_default()
        } else {
            p.access_key.trim().to_string()
        };
        let mut handles = Vec::new();
        let found = Arc::new(StdMutex::new(KeyCheck::Fine));
        let handler = Handler { host: host.clone(), port, pinned: p.host_key.clone(), found: found.clone() };
        let mut target = if p.jump_host.trim().is_empty() {
            let connecting = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                client::connect(config(), (host.as_str(), port), handler),
            )
            .await
            .map_err(|_| tr("The server did not answer"))?;
            connecting.map_err(|e| key_failure(&found, &host, port, false, e))?
        } else {
            let (jump_user, jump_host, jump_port) = parse_jump(&p.jump_host, &user)?;
            let jump_found = Arc::new(StdMutex::new(KeyCheck::Fine));
            let jump_handler = Handler {
                host: jump_host.clone(),
                port: jump_port,
                pinned: p.jump_host_key.clone(),
                found: jump_found.clone(),
            };
            let connecting = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                client::connect(config(), (jump_host.as_str(), jump_port), jump_handler),
            )
            .await
            .map_err(|_| trf("The jump host {host} did not answer", &[("host", &jump_host)]))?;
            let mut jump = connecting.map_err(|e| key_failure(&jump_found, &jump_host, jump_port, true, e))?;
            authenticate(&mut jump, &jump_user, &p.private_key, "").await.map_err(|e| {
                format!("{} ({e})", trf("Signing in to the jump host {host} failed", &[("host", &jump_host)]))
            })?;
            let channel =
                jump.channel_open_direct_tcpip(host.clone(), port as u32, "127.0.0.1", 0).await.map_err(|e| {
                    trf("The jump host could not reach {host}: {error}", &[("host", &host), ("error", &e.to_string())])
                })?;
            handles.push(jump);
            client::connect_stream(config(), channel.into_stream(), handler)
                .await
                .map_err(|e| key_failure(&found, &host, port, false, e))?
        };
        authenticate(&mut target, &user, &p.private_key, &p.secret_key).await?;
        let channel = target.channel_open_session().await.map_err(describe)?;
        channel.request_subsystem(true, "sftp").await.map_err(describe)?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| trf("The server offers no SFTP: {error}", &[("error", &e.to_string())]))?;
        handles.push(target);
        let root_empty = self.root.lock().unwrap_or_else(PoisonError::into_inner).is_empty();
        if root_empty {
            let home = sftp.canonicalize(".").await.unwrap_or_else(|_| "/".into());
            *self.root.lock().unwrap_or_else(PoisonError::into_inner) = home;
        }
        Ok(Connected { sftp: Arc::new(sftp), _handles: handles })
    }

    async fn session(&self) -> Res<Arc<SftpSession>> {
        let mut guard = self.session.lock().await;
        if let Some(connected) = guard.as_ref()
            && !connected._handles.last().is_some_and(|h| h.is_closed())
        {
            return Ok(connected.sftp.clone());
        }
        let connected = self.open().await?;
        let sftp = connected.sftp.clone();
        *guard = Some(connected);
        Ok(sftp)
    }

    fn path(&self, key: &str) -> String {
        join(&self.root.lock().unwrap_or_else(PoisonError::into_inner), key)
    }

    pub async fn list(&self, dir: &str) -> Res<Vec<(String, Stat)>> {
        let sftp = self.session().await?;
        let base = self.path(dir);
        let mut out = Vec::new();
        for entry in sftp.read_dir(base.clone()).await.map_err(sftp_error)? {
            let name = entry.file_name();
            let mut meta = entry.metadata();
            if meta.is_symlink()
                && let Ok(target) = sftp.metadata(format!("{}/{name}", base.trim_end_matches('/'))).await
            {
                meta = target;
            }
            out.push((
                name,
                Stat { size: meta.size.unwrap_or(0), modified: meta.mtime.unwrap_or(0) as i64, is_dir: meta.is_dir() },
            ));
        }
        Ok(out)
    }

    pub async fn stat(&self, key: &str) -> Res<Stat> {
        let meta = self.session().await?.metadata(self.path(key)).await.map_err(sftp_error)?;
        Ok(Stat { size: meta.size.unwrap_or(0), modified: meta.mtime.unwrap_or(0) as i64, is_dir: meta.is_dir() })
    }

    pub async fn read(&self, key: &str, limit: u64) -> Res<Vec<u8>> {
        let mut file = self.session().await?.open(self.path(key)).await.map_err(sftp_error)?;
        let mut data = Vec::new();
        (&mut file).take(limit).read_to_end(&mut data).await.map_err(|e| e.to_string())?;
        let _ = file.shutdown().await;
        Ok(data)
    }

    pub async fn download(&self, key: &str, partial: &Path, progress: &Progress) -> Res<()> {
        let mut remote = self.session().await?.open(self.path(key)).await.map_err(sftp_error)?;
        let mut local = tokio::fs::File::create(partial).await.map_err(|e| e.to_string())?;
        pump(&mut remote, &mut local, progress).await?;
        let _ = remote.shutdown().await;
        Ok(())
    }

    pub async fn upload(&self, path: &Path, key: &str, progress: &Progress) -> Res<()> {
        let sftp = self.session().await?;
        let target = self.path(key);
        let mut remote = sftp
            .open_with_flags(target.clone(), OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE)
            .await
            .map_err(sftp_error)?;
        let mut local = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
        pump(&mut local, &mut remote, progress).await?;
        remote.shutdown().await.map_err(|e| e.to_string())?;
        if let Some(mtime) = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        {
            let attrs = russh_sftp::protocol::FileAttributes {
                mtime: Some(mtime.as_secs() as u32),
                atime: Some(mtime.as_secs() as u32),
                ..Default::default()
            };
            let _ = sftp.set_metadata(target, attrs).await;
        }
        Ok(())
    }

    pub async fn write(&self, key: &str, data: Vec<u8>) -> Res<()> {
        let mut remote = self
            .session()
            .await?
            .open_with_flags(self.path(key), OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE)
            .await
            .map_err(sftp_error)?;
        remote.write_all(&data).await.map_err(|e| e.to_string())?;
        remote.shutdown().await.map_err(|e| e.to_string())
    }

    pub async fn mkdir(&self, key: &str) -> Res<()> {
        self.session().await?.create_dir(self.path(key)).await.map_err(sftp_error)
    }

    pub async fn remove_file(&self, key: &str) -> Res<()> {
        self.session().await?.remove_file(self.path(key)).await.map_err(sftp_error)
    }

    pub async fn remove_dir(&self, key: &str) -> Res<()> {
        self.session().await?.remove_dir(self.path(key).trim_end_matches('/').to_string()).await.map_err(sftp_error)
    }

    pub async fn rename(&self, from: &str, to: &str) -> Res<()> {
        let sftp = self.session().await?;
        let (from, to) =
            (self.path(from).trim_end_matches('/').to_string(), self.path(to).trim_end_matches('/').to_string());
        if sftp.rename(from.clone(), to.clone()).await.is_ok() {
            return Ok(());
        }
        // SFTPv3 rename won't overwrite, so remove the target first.
        if sftp.metadata(to.clone()).await.is_ok_and(|m| !m.is_dir()) {
            sftp.remove_file(to.clone()).await.map_err(sftp_error)?;
        }
        sftp.rename(from, to).await.map_err(sftp_error)
    }
}

fn sftp_error(error: russh_sftp::client::error::Error) -> String {
    let text = error.to_string();
    if text.contains("No such file") || text.contains("NoSuchFile") {
        tr("The file or folder was not found")
    } else if text.contains("Permission denied") || text.contains("PermissionDenied") {
        tr("The server does not allow this")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_host_round_trip() {
        let found =
            UnknownHost { host: "example.org".into(), port: 2222, fingerprint: "SHA256:abc".into(), jump: true };
        assert_eq!(UnknownHost::decode(&found.encode()), Some(found));
        assert!(UnknownHost::decode("some other error").is_none());
        assert_eq!(parse_jump("bastion.example.org", "ada").unwrap(), ("ada".into(), "bastion.example.org".into(), 22));
        assert_eq!(parse_jump("ops@10.0.0.1:2200", "ada").unwrap(), ("ops".into(), "10.0.0.1".into(), 2200));
    }
}
