use std::fs::File;
use std::io::{self, BufReader};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::config::{TlsCredentials, TlsListener};

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct RuntimeTlsAcceptor {
    acceptor: TlsAcceptor,
    handshake_timeout: Duration,
}

impl RuntimeTlsAcceptor {
    pub(crate) fn from_listener(listener: &TlsListener) -> Result<Self, RuntimeTlsError> {
        let (certificate_path, private_key_path, trust_store) = match &listener.credentials {
            TlsCredentials::CombinedPem(path) => (path, path, None),
            TlsCredentials::SplitPem {
                certificate,
                private_key,
                trust_store,
            } => (certificate, private_key, trust_store.as_ref()),
        };
        let certificates = read_certificates(certificate_path)?;
        let private_key = read_private_key(private_key_path)?;

        let builder =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|error| RuntimeTlsError::Configuration(error.to_string()))?;
        let config = if let Some(trust_store) = trust_store {
            let roots = Arc::new(read_trust_store(trust_store)?);
            let verifier = WebPkiClientVerifier::builder(roots)
                .build()
                .map_err(|error| RuntimeTlsError::Configuration(error.to_string()))?;
            builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(certificates, private_key)
        } else {
            builder
                .with_no_client_auth()
                .with_single_cert(certificates, private_key)
        }
        .map_err(|error| RuntimeTlsError::Configuration(error.to_string()))?;

        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            handshake_timeout: TLS_HANDSHAKE_TIMEOUT,
        })
    }

    pub(crate) async fn accept(
        &self,
        stream: TcpStream,
    ) -> Result<TlsStream<TcpStream>, RuntimeTlsError> {
        timeout(self.handshake_timeout, self.acceptor.accept(stream))
            .await
            .map_err(|_| RuntimeTlsError::HandshakeTimeout)?
            .map_err(RuntimeTlsError::Handshake)
    }
}

fn read_certificates(
    path: &std::path::Path,
) -> Result<Vec<CertificateDer<'static>>, RuntimeTlsError> {
    let mut reader = credential_reader(path, "certificate")?;
    let certificates = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| RuntimeTlsError::CredentialFile {
            kind: "certificate",
            source,
        })?;
    if certificates.is_empty() {
        return Err(RuntimeTlsError::MissingCredential("certificate"));
    }
    Ok(certificates)
}

fn read_private_key(path: &std::path::Path) -> Result<PrivateKeyDer<'static>, RuntimeTlsError> {
    let mut reader = credential_reader(path, "private key")?;
    rustls_pemfile::private_key(&mut reader)
        .map_err(|source| RuntimeTlsError::CredentialFile {
            kind: "private key",
            source,
        })?
        .ok_or(RuntimeTlsError::MissingCredential("private key"))
}

fn read_trust_store(path: &std::path::Path) -> Result<RootCertStore, RuntimeTlsError> {
    let mut roots = RootCertStore::empty();
    for certificate in read_certificates(path)? {
        roots
            .add(certificate)
            .map_err(|error| RuntimeTlsError::Configuration(error.to_string()))?;
    }
    Ok(roots)
}

fn credential_reader(
    path: &std::path::Path,
    kind: &'static str,
) -> Result<BufReader<File>, RuntimeTlsError> {
    File::open(path)
        .map(BufReader::new)
        .map_err(|source| RuntimeTlsError::CredentialFile { kind, source })
}

#[derive(Debug, Error)]
pub(crate) enum RuntimeTlsError {
    #[error("unable to read secure signaling {kind}: {source}")]
    CredentialFile {
        kind: &'static str,
        source: io::Error,
    },
    #[error("secure signaling {0} is missing")]
    MissingCredential(&'static str),
    #[error("unable to configure secure signaling: {0}")]
    Configuration(String),
    #[error("secure signaling handshake failed: {0}")]
    Handshake(io::Error),
    #[error("secure signaling handshake timed out")]
    HandshakeTimeout,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::{Ipv4Addr, SocketAddr};

    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::ClientConfig;
    use rustls::pki_types::ServerName;
    use tempfile::tempdir;
    use tokio::net::TcpListener;
    use tokio_rustls::TlsConnector;

    use super::*;

    #[tokio::test]
    async fn configured_acceptor_completes_a_secure_handshake() {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let directory = tempdir().unwrap();
        let combined = directory.path().join("listener.pem");
        fs::write(
            &combined,
            format!("{}{}", cert.pem(), signing_key.serialize_pem()),
        )
        .unwrap();
        let acceptor = RuntimeTlsAcceptor::from_listener(&TlsListener {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            credentials: TlsCredentials::CombinedPem(combined),
        })
        .unwrap();

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            acceptor.accept(stream).await.unwrap()
        });

        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let client =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client));
        let stream = TcpStream::connect(address).await.unwrap();
        connector
            .connect(ServerName::try_from("localhost").unwrap(), stream)
            .await
            .unwrap();
        accepted.await.unwrap();
    }

    #[test]
    fn missing_credentials_fail_without_exposing_the_path() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("private-value.pem");
        let error = RuntimeTlsAcceptor::from_listener(&TlsListener {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            credentials: TlsCredentials::CombinedPem(missing.clone()),
        })
        .err()
        .expect("missing credentials must fail");
        assert!(!error.to_string().contains(missing.to_str().unwrap()));
    }
}
