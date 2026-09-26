use crate::config::config_dir;
use anyhow::{Context, Result, anyhow, bail};
use hmac::{Hmac, Mac};
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::{fs, net::SocketAddr, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LABEL: &[u8] = b"airbridge/pin-auth/v1";
type HmacSha256 = Hmac<Sha256>;

pub fn validate_pin(pin: &str) -> Result<()> {
    if pin.len() != 6 || !pin.bytes().all(|digit| digit.is_ascii_digit()) {
        bail!("PIN must contain exactly six digits");
    }
    Ok(())
}

pub fn server_endpoint(address: SocketAddr) -> Result<Endpoint> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = config_dir()?;
    fs::create_dir_all(&dir)?;
    let cert_path = dir.join("host-cert.der");
    let key_path = dir.join("host-key.der");
    let (cert, key) = if cert_path.exists() && key_path.exists() {
        (fs::read(&cert_path)?, fs::read(&key_path)?)
    } else {
        let issued = rcgen::generate_simple_self_signed(vec!["airbridge.local".into()])?;
        let cert = issued.cert.der().to_vec();
        let key = issued.signing_key.serialize_der();
        fs::write(&cert_path, &cert)?;
        fs::write(&key_path, &key)?;
        (cert, key)
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
    }
    let mut crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert)],
            PrivatePkcs8KeyDer::from(key).into(),
        )?;
    crypto.alpn_protocols = vec![b"airbridge/1".to_vec()];
    let config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?,
    ));
    Endpoint::server(config, address).context("Cannot listen for QUIC connections")
}

pub fn client_endpoint(address: SocketAddr) -> Result<Endpoint> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut endpoint = Endpoint::client(address)?;
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifiedCertificate(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"airbridge/1".to_vec()];
    endpoint.set_default_client_config(ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    )));
    Ok(endpoint)
}

#[derive(Debug)]
struct PinVerifiedCertificate(Arc<rustls::crypto::CryptoProvider>);

// The certificate is authenticated by the SPAKE2 proof bound to this QUIC
// connection's TLS exporter below. No media or control stream is accepted
// before that proof succeeds.
impl rustls::client::danger::ServerCertVerifier for PinVerifiedCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn proof(key: &[u8], binding: &[u8], role: &[u8]) -> Result<[u8; 32]> {
    let mut mac = HmacSha256::new_from_slice(key)?;
    mac.update(LABEL);
    mac.update(binding);
    mac.update(role);
    Ok(mac.finalize().into_bytes().into())
}

pub async fn authenticate_host(connection: &Connection, pin: &str) -> Result<()> {
    validate_pin(pin)?;
    let (mut send, mut recv) = connection.accept_bi().await?;
    let (state, outbound) = Spake2::<Ed25519Group>::start_b(
        &Password::new(pin.as_bytes()),
        &Identity::new(b"viewer"),
        &Identity::new(b"host"),
    );
    send.write_u16(outbound.len() as u16).await?;
    send.write_all(&outbound).await?;
    let len = recv.read_u16().await? as usize;
    if len > 256 {
        bail!("Invalid PIN exchange");
    }
    let mut inbound = vec![0; len];
    recv.read_exact(&mut inbound).await?;
    let key = state
        .finish(&inbound)
        .map_err(|_| anyhow!("Invalid PIN exchange"))?;
    let mut binding = [0; 32];
    connection
        .export_keying_material(&mut binding, LABEL, b"")
        .map_err(|_| anyhow!("Cannot bind authentication to TLS"))?;
    let host_proof = proof(&key, &binding, b"host")?;
    send.write_all(&host_proof).await?;
    let mut viewer_proof = [0; 32];
    recv.read_exact(&mut viewer_proof).await?;
    let expected = proof(&key, &binding, b"viewer")?;
    if !constant_time_eq(&viewer_proof, &expected) {
        bail!("Incorrect PIN or untrusted connection");
    }
    send.finish()?;
    Ok(())
}

pub async fn authenticate_viewer(connection: &Connection, pin: &str) -> Result<()> {
    validate_pin(pin)?;
    let (mut send, mut recv) = connection.open_bi().await?;
    let (state, outbound) = Spake2::<Ed25519Group>::start_a(
        &Password::new(pin.as_bytes()),
        &Identity::new(b"viewer"),
        &Identity::new(b"host"),
    );
    send.write_u16(outbound.len() as u16).await?;
    send.write_all(&outbound).await?;
    let len = recv.read_u16().await? as usize;
    if len > 256 {
        bail!("Invalid PIN exchange");
    }
    let mut inbound = vec![0; len];
    recv.read_exact(&mut inbound).await?;
    let key = state
        .finish(&inbound)
        .map_err(|_| anyhow!("Invalid PIN exchange"))?;
    let mut binding = [0; 32];
    connection
        .export_keying_material(&mut binding, LABEL, b"")
        .map_err(|_| anyhow!("Cannot bind authentication to TLS"))?;
    let mut host_proof = [0; 32];
    recv.read_exact(&mut host_proof).await?;
    let expected = proof(&key, &binding, b"host")?;
    if !constant_time_eq(&host_proof, &expected) {
        bail!("Incorrect PIN or untrusted connection");
    }
    send.write_all(&proof(&key, &binding, b"viewer")?).await?;
    send.finish()?;
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0u8, |difference, (&x, &y)| difference | (x ^ y))
        == 0
}
