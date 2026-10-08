use crate::config::config_dir;
use anyhow::{Context, Result, anyhow, bail};
use hmac::{Hmac, Mac};
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::{fs, net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LABEL: &[u8] = b"airbridge/pin-auth/v1";
type HmacSha256 = Hmac<Sha256>;

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut config = quinn::TransportConfig::default();
    // macOS can take longer than QUIC's idle timeout to initialize capture.
    // Keep the authenticated connection alive while there are no media frames.
    config.keep_alive_interval(Some(Duration::from_secs(5)));
    config.max_concurrent_uni_streams(24u32.into());
    config.max_concurrent_bidi_streams(4u32.into());
    config.stream_receive_window((8u32 * 1024 * 1024).into());
    config.receive_window((16u32 * 1024 * 1024).into());
    Arc::new(config)
}

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
    let mut config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?,
    ));
    config.transport_config(transport_config());
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
    let mut config = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    ));
    config.transport_config(transport_config());
    endpoint.set_default_client_config(config);
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
    authenticate_host_scoped(connection, pin, b"legacy").await
}
async fn authenticate_host_scoped(connection: &Connection, pin: &str, scope: &[u8]) -> Result<()> {
    let (mut send, mut recv) = connection.accept_bi().await?;
    let (state, outbound) = Spake2::<Ed25519Group>::start_b(
        &Password::new(pin.as_bytes()),
        &Identity::new(&[b"viewer/".as_slice(), scope].concat()),
        &Identity::new(&[b"host/".as_slice(), scope].concat()),
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
    authenticate_viewer_scoped(connection, pin, b"legacy").await
}
async fn authenticate_viewer_scoped(
    connection: &Connection,
    pin: &str,
    scope: &[u8],
) -> Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;
    let (state, outbound) = Spake2::<Ed25519Group>::start_a(
        &Password::new(pin.as_bytes()),
        &Identity::new(&[b"viewer/".as_slice(), scope].concat()),
        &Identity::new(&[b"host/".as_slice(), scope].concat()),
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

#[derive(serde::Serialize, serde::Deserialize)]
struct AuthTarget {
    version: u16,
    screen_id: u32,
    #[serde(default)]
    account: Option<SessionLogin>,
}
fn screen_scope(id: u32) -> Vec<u8> {
    format!("airbridge/v{}/screen/{id}", crate::protocol::VERSION).into_bytes()
}
/// Target selection is untrusted; protected content is sent only after proof.
pub async fn authenticate_host_screen(
    connection: &Connection,
    pin_for: impl Fn(u32) -> Option<String>,
) -> Result<u32> {
    let (mut send, mut recv) = connection.accept_bi().await?;
    let target: AuthTarget = crate::protocol::receive_json(&mut recv).await?;
    if target.account.is_some() {
        bail!("Session authentication requires a session-aware host");
    }
    let pin = if target.version == crate::protocol::VERSION {
        pin_for(target.screen_id)
    } else {
        None
    };
    let rejection = if target.version != crate::protocol::VERSION {
        Some(crate::failure::FailureCode::VersionMismatch)
    } else if pin.is_none() {
        Some(crate::failure::FailureCode::ScreenUnavailable)
    } else {
        None
    };
    crate::protocol::send_json(&mut send, &rejection).await?;
    send.finish()?;
    let pin = pin.ok_or_else(|| anyhow!("Screen unavailable or protocol version mismatch"))?;
    validate_pin(&pin)?;
    authenticate_host_scoped(connection, &pin, &screen_scope(target.screen_id)).await?;
    Ok(target.screen_id)
}

pub async fn authenticate_viewer_screen(
    connection: &Connection,
    pin: &str,
    screen_id: u32,
) -> Result<()> {
    validate_pin(pin)?;
    let (mut send, mut recv) = connection.open_bi().await?;
    crate::protocol::send_json(
        &mut send,
        &AuthTarget {
            version: crate::protocol::VERSION,
            screen_id,
            account: None,
        },
    )
    .await?;
    send.finish()?;
    let rejection: Option<crate::failure::FailureCode> =
        crate::protocol::receive_json(&mut recv).await?;
    if let Some(reason) = rejection {
        bail!("Host rejected screen selection: {reason:?}");
    }
    authenticate_viewer_scoped(connection, pin, &screen_scope(screen_id)).await
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionLogin {
    pub username: String,
    pub session_id: Option<String>,
    pub host_id: Option<String>,
    pub viewer_id: String,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionSelection {
    host_id: String,
    session_id: String,
    salt: [u8; 16],
}
pub enum AuthenticatedTarget {
    Screen(u32),
    Session {
        session_id: String,
        viewer_id: String,
    },
}
fn session_scope(host: &str, session: &str, viewer: &str) -> Vec<u8> {
    format!(
        "airbridge/v{}/host/{host}/session/{session}/viewer/{viewer}",
        crate::protocol::VERSION
    )
    .into_bytes()
}
pub async fn authenticate_host_target(
    connection: &Connection,
    pin_for: impl Fn(u32) -> Option<String>,
    session_for: impl Fn(&SessionLogin) -> Option<crate::sessions::HostSession>,
) -> Result<AuthenticatedTarget> {
    let (mut send, mut recv) = connection.accept_bi().await?;
    let target: AuthTarget = crate::protocol::receive_json(&mut recv).await?;
    if target.version != crate::protocol::VERSION {
        bail!("Protocol version mismatch");
    }
    if let Some(login) = target.account {
        if login.username.len() > 64 || !crate::sessions::valid_id(&login.viewer_id) {
            bail!("Invalid session login");
        }
        let session = session_for(&login).filter(|s| {
            s.contract.username == login.username
                && login
                    .host_id
                    .as_ref()
                    .is_none_or(|host| *host == s.contract.host_id)
                && login
                    .session_id
                    .as_ref()
                    .is_none_or(|session| *session == s.contract.session_id)
        });
        let selection = session.as_ref().map(|s| SessionSelection {
            host_id: s.contract.host_id.clone(),
            session_id: s.contract.session_id.clone(),
            salt: s.salt,
        });
        crate::protocol::send_json(&mut send, &selection).await?;
        send.finish()?;
        let session = session.ok_or_else(|| anyhow!("Account or session unavailable"))?;
        let scope = session_scope(
            &session.contract.host_id,
            &session.contract.session_id,
            &login.viewer_id,
        );
        authenticate_host_scoped(connection, &session.key.0, &scope).await?;
        Ok(AuthenticatedTarget::Session {
            session_id: session.contract.session_id,
            viewer_id: login.viewer_id,
        })
    } else {
        let pin = pin_for(target.screen_id);
        let rejection = pin
            .is_none()
            .then_some(crate::failure::FailureCode::ScreenUnavailable);
        crate::protocol::send_json(&mut send, &rejection).await?;
        send.finish()?;
        let pin = pin.ok_or_else(|| anyhow!("Screen unavailable"))?;
        validate_pin(&pin)?;
        authenticate_host_scoped(connection, &pin, &screen_scope(target.screen_id)).await?;
        Ok(AuthenticatedTarget::Screen(target.screen_id))
    }
}
pub async fn authenticate_viewer_session(
    connection: &Connection,
    login: &SessionLogin,
    password: &str,
    remembered: Option<&crate::sessions::Secret>,
) -> Result<(String, crate::sessions::Secret)> {
    let (mut send, mut recv) = connection.open_bi().await?;
    crate::protocol::send_json(
        &mut send,
        &AuthTarget {
            version: crate::protocol::VERSION,
            screen_id: 0,
            account: Some(login.clone()),
        },
    )
    .await?;
    send.finish()?;
    let selection: Option<SessionSelection> = crate::protocol::receive_json(&mut recv).await?;
    let selection = selection.ok_or_else(|| anyhow!("Account or session unavailable"))?;
    if !crate::sessions::valid_id(&selection.host_id)
        || !crate::sessions::valid_id(&selection.session_id)
        || login
            .host_id
            .as_ref()
            .is_some_and(|h| *h != selection.host_id)
        || login
            .session_id
            .as_ref()
            .is_some_and(|s| *s != selection.session_id)
    {
        bail!("Session identity mismatch");
    }
    let key = if let Some(key) = remembered {
        key.clone()
    } else {
        crate::sessions::validate_login(&login.username, password)?;
        let password = crate::sessions::Secret(password.to_owned());
        let salt = selection.salt;
        tokio::task::spawn_blocking(move || crate::sessions::password_key(&password.0, &salt))
            .await??
    };
    let scope = session_scope(&selection.host_id, &selection.session_id, &login.viewer_id);
    authenticate_viewer_scoped(connection, &key.0, &scope).await?;
    Ok((selection.session_id, key))
}
