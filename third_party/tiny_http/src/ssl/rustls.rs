//! PLIPPA PATCH (SECFIX_03): rustls 0.23 with the `ring` provider.
//!
//! Two things changed from upstream's rustls 0.20 module:
//!
//! * The certificate and key arrive as **DER**, not PEM, because they are generated in-process
//!   by the server and never leave it as text. `rustls-pemfile` is therefore not needed.
//! * The handshake is **not** started here. [`crate::ssl::SslContextImpl::accept`] only builds
//!   the connection object; rustls performs the handshake on the first read, which happens in
//!   the worker thread that parses the request. A client that connects and says nothing
//!   occupies one worker (bounded by the head deadline), never the accept thread.
use crate::connection::Connection;
use crate::util::refined_tcp_stream::Stream as RefinedStream;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::error::Error;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zeroize::Zeroizing;

/// A wrapper around an owned Rustls connection and corresponding stream.
///
/// Uses an internal Mutex to permit disparate reader & writer threads to access the stream
/// independently.
pub(crate) struct RustlsStream(
    Arc<Mutex<rustls::StreamOwned<rustls::ServerConnection, Connection>>>,
);

impl RustlsStream {
    pub(crate) fn peer_addr(&mut self) -> std::io::Result<Option<SocketAddr>> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .sock
            .peer_addr()
    }

    pub(crate) fn shutdown(&mut self, how: Shutdown) -> std::io::Result<()> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .sock
            .shutdown(how)
    }

    /// PLIPPA PATCH: the read timeout lives on the socket underneath TLS.
    pub(crate) fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .sock
            .set_read_timeout(d)
    }
}

impl Clone for RustlsStream {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Read for RustlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .read(buf)
    }
}

impl Write for RustlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .flush()
    }
}

pub(crate) struct RustlsContext(Arc<rustls::ServerConfig>);

impl RustlsContext {
    /// Build the TLS configuration from a DER certificate and its DER PKCS#8 key.
    ///
    /// Refuses anything but a single self-signed certificate chain and a PKCS#8 key: this
    /// server never has a CA-issued chain, so a PEM "bundle" is a sign that the caller is
    /// passing the wrong thing. `ring` is named explicitly: the process has exactly one
    /// provider, so the choice cannot depend on a global default being installed.
    pub(crate) fn from_der(
        certificate: Vec<u8>,
        private_key: Zeroizing<Vec<u8>>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if certificate.is_empty() {
            return Err("Couldn't extract certificate chain from config.".into());
        }
        let certificate_chain: Vec<CertificateDer<'static>> = vec![CertificateDer::from(certificate)];
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key.to_vec()));

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls_conf = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()? // TLS 1.2 and 1.3, nothing older
            .with_no_client_auth()
            .with_single_cert(certificate_chain, private_key)?;
        // The portal speaks HTTP/1.1 only; without ALPN a browser would not know that.
        tls_conf.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(Self(Arc::new(tls_conf)))
    }

    pub(crate) fn accept(
        &self,
        stream: Connection,
    ) -> Result<RustlsStream, Box<dyn Error + Send + Sync + 'static>> {
        let connection = rustls::ServerConnection::new(self.0.clone())?;
        Ok(RustlsStream(Arc::new(Mutex::new(
            rustls::StreamOwned::new(connection, stream),
        ))))
    }
}

impl From<RustlsStream> for RefinedStream {
    fn from(stream: RustlsStream) -> Self {
        Self::Https(stream)
    }
}
