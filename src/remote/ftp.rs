use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use suppaftp::list::{File as ListFile, ListParser};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream};
use suppaftp::types::FileType;
use suppaftp::{FtpError, Mode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Stat, host_port, join, pump};
use crate::i18n::{tr, trf};
use crate::profile::Profile;
use crate::s3::{Progress, Res};

type Stream = AsyncRustlsFtpStream;

fn security(profile: &Profile) -> &str {
    match profile.ftp_security.as_str() {
        "implicit" | "none" => profile.ftp_security.as_str(),
        _ => "explicit",
    }
}

fn ftp_error(error: FtpError) -> String {
    match error {
        FtpError::ConnectionError(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            tr("The server refused the connection; check the address and port")
        }
        FtpError::ConnectionError(e) => e.to_string(),
        FtpError::SecureError(e)
            if e.contains("certificate") || e.contains("Certificate") || e.contains("UnknownIssuer") =>
        {
            format!("{} ({e})", tr("The server's certificate is not trusted"))
        }
        FtpError::SecureError(e) => e,
        FtpError::UnexpectedResponse(response) => {
            let text = String::from_utf8_lossy(&response.body).trim().to_string();
            match response.status.code() {
                530 => tr("The server did not accept the user name or password"),
                550 => {
                    format!("{} ({text})", tr("The file or folder was not found, or the server does not allow this"))
                }
                _ => text,
            }
        }
        other => other.to_string(),
    }
}

pub struct Ftp {
    profile: Profile,
    root: StdMutex<String>,
    idle: StdMutex<Vec<Stream>>,
    mlsd: std::sync::atomic::AtomicBool,
}

impl Ftp {
    pub async fn connect(profile: &Profile) -> Res<Ftp> {
        let ftp = Ftp {
            profile: profile.clone(),
            root: StdMutex::new(profile.remote_path.trim().to_string()),
            idle: StdMutex::new(Vec::new()),
            mlsd: std::sync::atomic::AtomicBool::new(false),
        };
        let first = ftp.open().await?;
        ftp.give_back(first);
        Ok(ftp)
    }

    async fn open(&self) -> Res<Stream> {
        let p = &self.profile;
        let mode = security(p);
        let (host, port) = host_port(&p.endpoint, if mode == "implicit" { 990 } else { 21 })?;
        let connector = || -> Res<AsyncRustlsConnector> {
            // TLS 1.2 only: vsftpd needs data connections to reuse the TLS session.
            let config =
                crate::s3::pinned::client_config_with(&p.ca_certificate, &[&tokio_rustls::rustls::version::TLS12])?;
            Ok(AsyncRustlsConnector::from(tokio_rustls::TlsConnector::from(Arc::new(config))))
        };
        let timeout = std::time::Duration::from_secs(20);
        let connecting = async {
            match mode {
                "implicit" => {
                    Stream::connect_secure_implicit((host.as_str(), port), connector()?, &host).await.map_err(ftp_error)
                }
                "none" => Stream::connect((host.as_str(), port)).await.map_err(ftp_error),
                _ => {
                    let plain = Stream::connect((host.as_str(), port)).await.map_err(ftp_error)?;
                    plain.into_secure(connector()?, &host).await.map_err(|e| {
                        format!(
                            "{} ({})",
                            tr("The server does not offer TLS. Choose plain FTP only on a network you trust."),
                            ftp_error(e)
                        )
                    })
                }
            }
        };
        let mut stream =
            tokio::time::timeout(timeout, connecting).await.map_err(|_| tr("The server did not answer"))??;
        let user = if p.access_key.trim().is_empty() { "anonymous" } else { p.access_key.trim() };
        let password =
            if p.access_key.trim().is_empty() && p.secret_key.is_empty() { "ferry@" } else { p.secret_key.as_str() };
        stream.login(user, password).await.map_err(ftp_error)?;
        // Implicit TLS still needs PROT P, or data goes in the clear and hangs.
        if mode == "implicit" {
            let _ = stream.custom_command("PBSZ 0", &[suppaftp::Status::CommandOk]).await;
            if stream.custom_command("PROT P", &[suppaftp::Status::CommandOk]).await.is_err() {
                return Err(tr(
                    "This server encrypts the sign-in but not the files themselves (it refuses PROT P). Choose another encryption setting.",
                ));
            }
        }
        stream.transfer_type(FileType::Binary).await.map_err(ftp_error)?;
        // MLSD/MLST only if FEAT lists MLST; an unknown command here hangs both sides.
        if let Ok(features) = stream.feat().await {
            self.mlsd
                .store(features.keys().any(|k| k.eq_ignore_ascii_case("MLST")), std::sync::atomic::Ordering::Relaxed);
        }
        stream.set_mode(Mode::ExtendedPassive);
        stream.set_passive_nat_workaround(true);
        let empty = self.root.lock().unwrap_or_else(PoisonError::into_inner).is_empty();
        if empty {
            let home = stream.pwd().await.unwrap_or_else(|_| "/".into());
            *self.root.lock().unwrap_or_else(PoisonError::into_inner) = home;
        }
        Ok(stream)
    }

    async fn take(&self) -> Res<Stream> {
        let idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner).pop();
        if let Some(mut stream) = idle
            && stream.noop().await.is_ok()
        {
            return Ok(stream);
        }
        self.open().await
    }

    fn give_back(&self, stream: Stream) {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < 4 {
            idle.push(stream);
        }
    }

    fn path(&self, key: &str) -> String {
        join(&self.root.lock().unwrap_or_else(PoisonError::into_inner), key).trim_end_matches('/').to_string()
    }

    fn stat_of(file: &ListFile) -> Stat {
        let modified = file.modified().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        Stat { size: file.size() as u64, modified, is_dir: file.is_directory() }
    }

    pub async fn list(&self, dir: &str) -> Res<Vec<(String, Stat)>> {
        let mut stream = self.take().await?;
        let path = self.path(dir);
        let path = if path.is_empty() { "/".to_string() } else { path };
        let mut files = Vec::new();
        let machine = if self.mlsd.load(std::sync::atomic::Ordering::Relaxed) {
            stream.mlsd(Some(&path)).await.ok()
        } else {
            None
        };
        match machine {
            Some(lines) => {
                for line in lines {
                    if let Ok(file) = ListParser::parse_mlsd(&line) {
                        files.push(file);
                    }
                }
            }
            None => {
                let lines = stream.list(Some(&path)).await.map_err(ftp_error)?;
                for line in lines {
                    if let Ok(file) = ListParser::parse_posix(&line).or_else(|_| ListParser::parse_dos(&line)) {
                        files.push(file);
                    }
                }
            }
        }
        self.give_back(stream);
        Ok(files.iter().map(|f| (f.name().to_string(), Self::stat_of(f))).collect())
    }

    pub async fn stat(&self, key: &str) -> Res<Stat> {
        let key = key.trim_end_matches('/');
        if key.is_empty() {
            return Ok(Stat { is_dir: true, ..Default::default() });
        }
        let mut stream = self.take().await?;
        let path = self.path(key);
        if self.mlsd.load(std::sync::atomic::Ordering::Relaxed)
            && let Ok(line) = stream.mlst(Some(&path)).await
            && let Ok(file) = ListParser::parse_mlst(&line)
        {
            self.give_back(stream);
            return Ok(Self::stat_of(&file));
        }
        self.give_back(stream);
        let (parent, name) = key.rsplit_once('/').unwrap_or(("", key));
        self.list(parent)
            .await?
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| s)
            .ok_or_else(|| tr("The file or folder was not found"))
    }

    pub async fn read(&self, key: &str, limit: u64) -> Res<Vec<u8>> {
        let mut stream = self.take().await?;
        let mut transfer = stream.retr_as_stream(self.path(key)).await.map_err(ftp_error)?;
        let mut data = Vec::new();
        (&mut transfer).take(limit).read_to_end(&mut data).await.map_err(|e| e.to_string())?;
        // Drop the connection instead of ABOR, which not every server handles.
        if (data.len() as u64) < limit {
            transfer.finish().await.map_err(ftp_error)?;
            self.give_back(stream);
        } else {
            drop(transfer);
            drop(stream);
        }
        Ok(data)
    }

    pub async fn download(&self, key: &str, partial: &Path, progress: &Progress) -> Res<()> {
        let mut stream = self.take().await?;
        let mut transfer = stream.retr_as_stream(self.path(key)).await.map_err(ftp_error)?;
        let mut local = tokio::fs::File::create(partial).await.map_err(|e| e.to_string())?;
        pump(&mut transfer, &mut local, progress).await?;
        transfer.finish().await.map_err(ftp_error)?;
        self.give_back(stream);
        Ok(())
    }

    pub async fn upload(&self, path: &Path, key: &str, progress: &Progress) -> Res<()> {
        let mut stream = self.take().await?;
        let target = self.path(key);
        let mut transfer = stream.put_with_stream(&target).await.map_err(ftp_error)?;
        let mut local = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
        pump(&mut local, &mut transfer, progress).await?;
        transfer.finish().await.map_err(ftp_error)?;
        if let Some(mtime) = std::fs::metadata(path).ok().and_then(|m| m.modified().ok()) {
            let stamp = gtk::glib::DateTime::from_unix_utc(
                mtime.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
            )
            .ok()
            .and_then(|d| d.format("%Y%m%d%H%M%S").ok());
            if let Some(stamp) = stamp {
                let _ = stream.custom_command(format!("MFMT {stamp} {target}"), &[suppaftp::Status::File]).await;
            }
        }
        self.give_back(stream);
        Ok(())
    }

    pub async fn write(&self, key: &str, data: Vec<u8>) -> Res<()> {
        let mut stream = self.take().await?;
        let mut transfer = stream.put_with_stream(self.path(key)).await.map_err(ftp_error)?;
        transfer.write_all(&data).await.map_err(|e| e.to_string())?;
        transfer.finish().await.map_err(ftp_error)?;
        self.give_back(stream);
        Ok(())
    }

    async fn simple<F>(&self, op: F) -> Res<()>
    where
        F: for<'a> FnOnce(
            &'a mut Stream,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), FtpError>> + Send + 'a>>,
    {
        let mut stream = self.take().await?;
        let result = op(&mut stream).await;
        self.give_back(stream);
        result.map_err(ftp_error)
    }

    pub async fn mkdir(&self, key: &str) -> Res<()> {
        let path = self.path(key);
        self.simple(move |s| Box::pin(async move { s.mkdir(&path).await })).await
    }

    pub async fn remove_file(&self, key: &str) -> Res<()> {
        let path = self.path(key);
        self.simple(move |s| Box::pin(async move { s.rm(&path).await })).await
    }

    pub async fn remove_dir(&self, key: &str) -> Res<()> {
        let path = self.path(key);
        self.simple(move |s| Box::pin(async move { s.rmdir(&path).await })).await
    }

    pub async fn rename(&self, from: &str, to: &str) -> Res<()> {
        let (from, to) = (self.path(from), self.path(to));
        self.simple(move |s| Box::pin(async move { s.rename(&from, &to).await }))
            .await
            .map_err(|e| trf("Renaming failed: {error}", &[("error", &e)]))
    }
}
