use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region, SharedCredentialsProvider};
use aws_smithy_http_client::{Builder, ConnectorBuilder, proxy::ProxyConfig, tls};
use aws_smithy_runtime_api::client::http::SharedHttpClient;
use gio::prelude::*;

use crate::i18n::tr;
use crate::profile::Profile;

use super::Res;

pub const MFA_REQUIRED: &str = "mfa-required";

fn proxy_for(url: &str) -> ProxyConfig {
    let resolver = gio::ProxyResolver::default();
    if let Ok(proxies) = resolver.lookup(url, gio::Cancellable::NONE) {
        for proxy in proxies {
            if proxy.starts_with("direct://") {
                return ProxyConfig::disabled();
            }
            if (proxy.starts_with("http://") || proxy.starts_with("https://"))
                && let Ok(config) = ProxyConfig::all(proxy.as_str())
            {
                return config;
            }
        }
    }
    ProxyConfig::from_env()
}

pub fn http_client(profile: &Profile, endpoint: &str) -> Res<SharedHttpClient> {
    if !profile.ca_certificate.trim().is_empty() {
        return Ok(SharedHttpClient::new(super::pinned::PinnedClient::new(&profile.ca_certificate)?));
    }
    let mut store = tls::TrustStore::default().with_native_roots(true);
    if !profile.ca_certificate.trim().is_empty() {
        store.add_pem_certificate(profile.ca_certificate.as_bytes().to_vec());
    }
    let context = tls::TlsContext::builder().with_trust_store(store).build().map_err(|e| e.to_string())?;
    let target = if endpoint.is_empty() { "https://s3.amazonaws.com".to_string() } else { endpoint.to_string() };
    let proxy = proxy_for(&target);
    Ok(Builder::new().build_with_connector_fn(move |settings, components| {
        let mut builder = ConnectorBuilder::default()
            .tls_provider(tls::Provider::Rustls(tls::rustls_provider::CryptoMode::AwsLc))
            .tls_context(context.clone());
        builder.set_connector_settings(settings.cloned());
        if let Some(components) = components {
            builder.set_sleep_impl(components.sleep_impl());
        }
        builder.set_proxy_config(Some(proxy.clone()));
        builder.build()
    }))
}

async fn shared_config(profile: &Profile, region: &str, client: SharedHttpClient) -> aws_config::SdkConfig {
    let mut loader =
        aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region.to_string())).http_client(client);
    if !profile.aws_profile.trim().is_empty() {
        loader = loader.profile_name(profile.aws_profile.trim());
    }
    loader.load().await
}

static SESSIONS: Mutex<Option<HashMap<String, Credentials>>> = Mutex::new(None);

fn cached_session(id: &str) -> Option<Credentials> {
    let sessions = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner);
    let found = sessions.as_ref()?.get(id)?.clone();
    let fresh = found.expiry().is_none_or(|end| end > SystemTime::now() + Duration::from_secs(300));
    fresh.then_some(found)
}

pub fn forget_session(id: &str) {
    if let Some(map) = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
        map.remove(id);
    }
}

pub fn needs_mfa(profile: &Profile) -> bool {
    !profile.role_arn.trim().is_empty()
        && !profile.mfa_serial.trim().is_empty()
        && cached_session(&profile.id).is_none()
}

pub async fn credentials(
    profile: &Profile,
    region: &str,
    client: SharedHttpClient,
) -> Res<(aws_config::SdkConfig, Option<SharedCredentialsProvider>)> {
    let shared = shared_config(profile, region, client).await;
    let base: Option<SharedCredentialsProvider> =
        if !profile.aws_profile.trim().is_empty() || profile.access_key.trim().is_empty() {
            shared.credentials_provider()
        } else {
            let token = (!profile.session_token.is_empty()).then(|| profile.session_token.clone());
            Some(SharedCredentialsProvider::new(Credentials::new(
                profile.access_key.trim(),
                profile.secret_key.trim(),
                token,
                None,
                "profile",
            )))
        };
    let role = profile.role_arn.trim();
    if role.is_empty() {
        return Ok((shared, base));
    }
    if !profile.mfa_serial.trim().is_empty() {
        return match cached_session(&profile.id) {
            Some(session) => Ok((shared, Some(SharedCredentialsProvider::new(session)))),
            None => Err(MFA_REQUIRED.to_string()),
        };
    }
    let base = base.ok_or_else(|| tr("No credentials were found to assume the role with"))?;
    let mut builder = aws_config::sts::AssumeRoleProvider::builder(role).session_name("ferry").configure(&shared);
    if !profile.external_id.trim().is_empty() {
        builder = builder.external_id(profile.external_id.trim());
    }
    let provider = builder.build_from_provider(base).await;
    Ok((shared, Some(SharedCredentialsProvider::new(provider))))
}

pub async fn start_mfa_session(profile: &Profile, code: &str) -> Res<()> {
    let region = if profile.region.trim().is_empty() { "us-east-1" } else { profile.region.trim() };
    let client = http_client(profile, "")?;
    let mut base_profile = profile.clone();
    base_profile.role_arn.clear();
    let (shared, base) = credentials(&base_profile, region, client).await?;
    let mut config = aws_sdk_sts::config::Builder::from(&shared);
    if let Some(base) = base {
        config = config.credentials_provider(base);
    }
    let sts = aws_sdk_sts::Client::from_conf(config.build());
    let mut request = sts
        .assume_role()
        .role_arn(profile.role_arn.trim())
        .role_session_name("ferry")
        .serial_number(profile.mfa_serial.trim())
        .token_code(code.trim())
        .duration_seconds(3600);
    if !profile.external_id.trim().is_empty() {
        request = request.external_id(profile.external_id.trim());
    }
    let out = request.send().await.map_err(|e| match aws_sdk_sts::error::ProvideErrorMetadata::code(&e) {
        Some("AccessDenied") => tr("The MFA code is wrong or expired, or the role does not allow you"),
        _ => aws_sdk_sts::error::DisplayErrorContext(&e).to_string(),
    })?;
    let c = out.credentials().ok_or_else(|| tr("The role gave no credentials"))?;
    let expiry = SystemTime::try_from(*c.expiration()).ok();
    let session =
        Credentials::new(c.access_key_id(), c.secret_access_key(), Some(c.session_token().to_string()), expiry, "mfa");
    SESSIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(Default::default)
        .insert(profile.id.clone(), session);
    Ok(())
}

pub fn aws_profiles() -> Vec<String> {
    let home = gtk::glib::home_dir();
    let config = std::env::var_os("AWS_CONFIG_FILE").map(Into::into).unwrap_or_else(|| home.join(".aws/config"));
    let credentials = std::env::var_os("AWS_SHARED_CREDENTIALS_FILE")
        .map(Into::into)
        .unwrap_or_else(|| home.join(".aws/credentials"));
    let mut names = Vec::new();
    for (path, prefixed) in [(config, true), (credentials, false)] {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        names.extend(parse_profile_names(&text, prefixed));
    }
    names.sort();
    names.dedup();
    names
}

fn parse_profile_names(text: &str, prefixed: bool) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let section = line.trim().strip_prefix('[')?.strip_suffix(']')?.trim();
            if !prefixed {
                return Some(section.to_string());
            }
            if section == "default" {
                return Some(section.to_string());
            }
            section.strip_prefix("profile ").map(|n| n.trim().to_string())
        })
        .filter(|n| !n.is_empty())
        .collect()
}

pub struct ServerCertificate {
    pub host: String,
    pub fingerprint: String,
    pub pem: String,
}

#[derive(Debug)]
struct Inspect(std::sync::Arc<tokio_rustls::rustls::crypto::CryptoProvider>);

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for Inspect {
    fn verify_server_cert(
        &self,
        _: &rustls_pki_types::CertificateDer<'_>,
        _: &[rustls_pki_types::CertificateDer<'_>],
        _: &rustls_pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls_pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub async fn fetch_certificate(endpoint: &str) -> Res<ServerCertificate> {
    use base64::Engine;
    use sha2::Digest;
    let url = endpoint.trim();
    let rest = url.strip_prefix("https://").ok_or_else(|| tr("Only https:// endpoints have certificates"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => {
            (h.trim_matches(['[', ']']).to_string(), p.parse::<u16>().map_err(|_| tr("Invalid endpoint"))?)
        }
        _ => (authority.trim_matches(['[', ']']).to_string(), 443),
    };
    let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let config = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(Inspect(provider)))
        .with_no_client_auth();
    let name = rustls_pki_types::ServerName::try_from(host.clone()).map_err(|_| tr("Invalid endpoint"))?;
    let tcp = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect((host.as_str(), port)))
        .await
        .map_err(|_| tr("The server did not answer"))?
        .map_err(|e| e.to_string())?;
    let stream = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_rustls::TlsConnector::from(std::sync::Arc::new(config)).connect(name, tcp),
    )
    .await
    .map_err(|_| tr("The server did not answer"))?
    .map_err(|e| e.to_string())?;
    let chain = stream.get_ref().1.peer_certificates().ok_or_else(|| tr("The server presented no certificate"))?;
    let top = chain.last().ok_or_else(|| tr("The server presented no certificate"))?;
    let digest = sha2::Sha256::digest(top.as_ref());
    let fingerprint = digest.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":");
    let body = base64::engine::general_purpose::STANDARD.encode(top.as_ref());
    let lines: Vec<&str> = body.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap_or("")).collect();
    let pem = format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"));
    Ok(ServerCertificate { host, fingerprint, pem })
}

pub fn fingerprints(pem: &str) -> Vec<String> {
    use base64::Engine;
    use sha2::Digest;
    let mut found = Vec::new();
    let mut body = String::new();
    let mut inside = false;
    for line in pem.lines() {
        let line = line.trim();
        if line == "-----BEGIN CERTIFICATE-----" {
            inside = true;
            body.clear();
            continue;
        }
        if line == "-----END CERTIFICATE-----" {
            inside = false;
            if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(&body) {
                found.push(sha2::Sha256::digest(&der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":"));
            }
            continue;
        }
        if inside {
            body.push_str(line);
        }
    }
    found
}

pub fn is_certificate_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "unknownissuer",
        "unknown issuer",
        "invalidcertificate",
        "invalid peer certificate",
        "certificate",
        "self signed",
        "self-signed",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_ini_sections() {
        let config =
            "[default]\nregion=eu-west-1\n[profile work]\nsso_session = corp\n[sso-session corp]\n[services s3]\n";
        assert_eq!(parse_profile_names(config, true), vec!["default", "work"]);
        assert_eq!(parse_profile_names("[default]\n[ci]\n", false), vec!["default", "ci"]);
    }
}
