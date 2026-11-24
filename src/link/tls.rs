//! TLS for the relay. The coordinator keeps a private CA of its own and serves the relay with a
//! certificate it signs for the relay URL's host. Switches trust that CA alone: it comes with
//! each stackbox's uplink configuration, over the same authenticated channel the coordinator
//! already reaches the host's agent through, so no public CA and no extra exchange is needed.
//!
//! Files under `<state dir>/relay-tls/`: `ca.key` and `ca.pem`, made once and kept, since the
//! switches of existing stackboxes hold the CA; and `server.key`, `server.pem`, signed anew at every
//! start for the current relay URL.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

/// How long a server certificate lasts; the coordinator signs a new one at every start.
const SERVER_DAYS: i64 = 397;

pub struct RelayTls {
    /// The CA switches trust, in PEM.
    pub ca_pem: String,
    pub server: Arc<rustls::ServerConfig>,
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "stackbox relay CA");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    let staging = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(&staging)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&staging, path)?;
    Ok(())
}

/// The host part of a relay URL, as the server certificate names it.
pub fn url_host(url: &str) -> Result<String> {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .context("the relay URL needs a scheme")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split_once(']').context("invalid IPv6 relay host")?.0
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    ensure!(!host.is_empty(), "the relay URL has no host");
    Ok(host.to_owned())
}

impl RelayTls {
    /// Load the CA from `state_dir`, making it the first time, and sign a server certificate
    /// for `host`.
    pub fn open(state_dir: &Path, host: &str) -> Result<Self> {
        let dir: PathBuf = state_dir.join("relay-tls");
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let (ca_key, ca_pem) = match (
            std::fs::read_to_string(dir.join("ca.key")),
            std::fs::read_to_string(dir.join("ca.pem")),
        ) {
            (Ok(key), Ok(pem)) => (KeyPair::from_pem(&key)?, pem),
            _ => {
                let key = KeyPair::generate()?;
                let pem = ca_params().self_signed(&key)?.pem();
                write_private(&dir.join("ca.key"), &key.serialize_pem())?;
                write_private(&dir.join("ca.pem"), &pem)?;
                (key, pem)
            }
        };
        // The same name and key sign as the stored CA certificate does.
        let issuer = Issuer::new(ca_params(), &ca_key);
        let server_key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![host.to_owned()])?;
        params.distinguished_name.push(DnType::CommonName, host);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(SERVER_DAYS);
        let server_pem = params.signed_by(&server_key, &issuer)?.pem();
        write_private(&dir.join("server.key"), &server_key.serialize_pem())?;
        write_private(&dir.join("server.pem"), &server_pem)?;
        let chain = vec![
            CertificateDer::from_pem_slice(server_pem.as_bytes())?,
            CertificateDer::from_pem_slice(ca_pem.as_bytes())?,
        ];
        let key = PrivateKeyDer::from_pem_slice(server_key.serialize_pem().as_bytes())?;
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
        Ok(Self {
            ca_pem,
            server: Arc::new(server),
        })
    }
}

/// A client that trusts `ca_pem` and nothing else.
pub fn client(ca_pem: &str) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from_pem_slice(ca_pem.as_bytes()).context("invalid relay CA")?)?;
    Ok(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth(),
    ))
}

/// TLS connections on a TCP listener, for `axum::serve`. Handshakes run on their own tasks,
/// so a slow client holds up no other.
pub struct TlsListener {
    accepted: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    addr: SocketAddr,
}

impl TlsListener {
    pub fn new(listener: TcpListener, config: Arc<rustls::ServerConfig>) -> Result<Self> {
        let addr = listener.local_addr()?;
        let acceptor = TlsAcceptor::from(config);
        let (tx, accepted) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let (tcp, peer) = match listener.accept().await {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        eprintln!("relay: accept: {error}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let _ = tcp.set_nodelay(true);
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await
                    {
                        Ok(Ok(stream)) => {
                            let _ = tx.send((stream, peer)).await;
                        }
                        Ok(Err(error)) => eprintln!("relay: TLS handshake with {peer}: {error}"),
                        Err(_) => eprintln!("relay: TLS handshake with {peer} timed out"),
                    }
                });
            }
        });
        Ok(Self { accepted, addr })
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.accepted.recv().await {
            Some(accepted) => accepted,
            // The acceptor task never ends while the listener lives.
            None => std::future::pending().await,
        }
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.addr)
    }
}

/// Check that `pem` holds one certificate.
pub fn validate_ca(pem: &str) -> Result<()> {
    CertificateDer::from_pem_slice(pem.as_bytes())
        .map(|_| ())
        .map_err(|e| anyhow!("invalid relay CA: {e}"))
}
