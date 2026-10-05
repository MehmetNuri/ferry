// Self-signed certs are often flagged as CA and rejected, hence the pin fallback.
use std::sync::Arc;
use std::time::Duration;

use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::body::SdkBody;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::Digest;
use tokio_rustls::rustls::{self, client::WebPkiServerVerifier, client::danger, crypto::CryptoProvider};

#[derive(Debug)]
struct Verifier {
    usual: Arc<WebPkiServerVerifier>,
    pins: Vec<[u8; 32]>,
    provider: Arc<CryptoProvider>,
}

impl danger::ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        middle: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<danger::ServerCertVerified, rustls::Error> {
        match self.usual.verify_server_cert(end, middle, name, ocsp, now) {
            Ok(verified) => Ok(verified),
            Err(error) => {
                let print: [u8; 32] = sha2::Sha256::digest(end.as_ref()).into();
                if self.pins.contains(&print) { Ok(danger::ServerCertVerified::assertion()) } else { Err(error) }
            }
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

type Inner = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    SdkBody,
>;

#[derive(Clone, Debug)]
pub struct PinnedClient(Inner);

pub fn client_config(pem: &str) -> Result<rustls::ClientConfig, String> {
    client_config_with(pem, rustls::DEFAULT_VERSIONS)
}

pub fn client_config_with(
    pem: &str,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let mut pins = Vec::new();
    for cert in CertificateDer::pem_slice_iter(pem.as_bytes()).flatten() {
        pins.push(sha2::Sha256::digest(cert.as_ref()).into());
        // Also as a trust anchor, for private CAs.
        let _ = roots.add(cert);
    }
    let usual = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| e.to_string())?;
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(versions)
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier { usual, pins, provider }))
        .with_no_client_auth();
    Ok(config)
}

impl PinnedClient {
    pub fn new(pem: &str) -> Result<Self, String> {
        let config = client_config(pem)?;
        let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(Duration::from_secs(10)));
        http.set_nodelay(true);
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(config)
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);
        let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
        Ok(PinnedClient(client))
    }
}

#[derive(Debug)]
struct Connector(Inner);

impl HttpConnector for Connector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let client = self.0.clone();
        HttpConnectorFuture::new(async move {
            let request = request.try_into_http1x().map_err(|e| ConnectorError::other(e.into(), None))?;
            let response = client.request(request).await.map_err(|e| ConnectorError::io(e.into()))?;
            HttpResponse::try_from(response.map(SdkBody::from_body_1_x))
                .map_err(|e| ConnectorError::other(e.into(), None))
        })
    }
}

impl HttpClient for PinnedClient {
    fn http_connector(&self, _: &HttpConnectorSettings, _: &RuntimeComponents) -> SharedHttpConnector {
        SharedHttpConnector::new(Connector(self.0.clone()))
    }
}
