//! Who a device is to the others, and the encrypted link between two of them (decision D80).
//! A device has one Ed25519 key, made once and kept in a file only its owner reads. Its public
//! half is its identity: a hub's is what a pairing code names, a peer's is what the hub records
//! when it is paired. Links are TLS 1.3 with a certificate on both sides, each made from the
//! device's own key and signed by it. No authority vouches for anyone: a device checks that the
//! hub's key is the one it was paired with, and the hub looks the peer's key up in its register
//! of admitted machines. A machine joining for the first time takes any key, and the user checks
//! a six-digit code made from both keys and the session (decision D93) before it is trusted.

use std::io;
use std::path::Path;
use std::sync::Arc;

use ring::signature::{Ed25519KeyPair, KeyPair};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

/// The name in every certificate and in the client's hello. It identifies nobody: the key does.
const SUBJECT: &str = "nypi";
/// The ALPN protocol of a machine's link to its hub (decision D94: one port, chosen by ALPN).
pub const ALPN: &[u8] = b"nypi/1";
/// The label of the TLS exporter value a join's confirmation code is made from (decision D93).
const JOIN_LABEL: &[u8] = b"nypi join";

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

/// A device's key.
pub struct Identity {
    pkcs8: Vec<u8>,
    key: Ed25519KeyPair,
}

impl Identity {
    pub fn generate() -> io::Result<Identity> {
        let document = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).map_err(other)?;
        Identity::from_pkcs8(document.as_ref().to_vec())
    }

    fn from_pkcs8(pkcs8: Vec<u8>) -> io::Result<Identity> {
        let key = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| other("the device key file does not hold a key"))?;
        Ok(Identity { pkcs8, key })
    }

    /// The key in `file`, made and written there (readable by its owner only) when there is none.
    pub fn load_or_create(file: &Path) -> io::Result<Identity> {
        match std::fs::read(file) {
            Ok(bytes) => Identity::from_pkcs8(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let identity = Identity::generate()?;
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                write_private(file, &identity.pkcs8)?;
                Ok(identity)
            }
            Err(error) => Err(error),
        }
    }

    /// The public key's fingerprint: what a pairing code names and a register records.
    pub fn fingerprint(&self) -> String {
        fingerprint_of_spki(&spki(self.key.public_key().as_ref()))
    }

    /// The device's certificate: its public key, signed by its own key. The same every time, so
    /// nothing about it needs keeping but the key.
    pub fn certificate(&self) -> Vec<u8> {
        let name =
            sequence(&set(&sequence(&[&[0x06, 0x03, 0x55, 0x04, 0x03][..], &tlv(0x0c, SUBJECT.as_bytes())].concat())));
        let validity = sequence(&[tlv(0x17, b"250101000000Z"), tlv(0x18, b"99991231235959Z")].concat());
        let tbs = sequence(
            &[
                // Version 3, serial 1, signed with Ed25519.
                vec![0xa0, 0x03, 0x02, 0x01, 0x02],
                vec![0x02, 0x01, 0x01],
                ED25519.to_vec(),
                name.clone(),
                validity,
                name,
                spki(self.key.public_key().as_ref()),
            ]
            .concat(),
        );
        let signature = self.key.sign(&tbs);
        let mut bits = vec![0x00];
        bits.extend_from_slice(signature.as_ref());
        sequence(&[tbs, ED25519.to_vec(), tlv(0x03, &bits)].concat())
    }

    fn chain(&self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.pkcs8.clone()));
        (vec![CertificateDer::from(self.certificate())], key)
    }

    /// How this device takes links as a hub: every peer shows a certificate, and whose key it
    /// holds is for the caller to look up (`peer_fingerprint`) before it is given anything.
    pub fn acceptor(&self) -> io::Result<tokio_rustls::TlsAcceptor> {
        let config = self.server_config()?;
        Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
    }

    /// The hub's TLS configuration for machines' links: its own key, a certificate from every
    /// peer, `nypi/1`.
    pub fn server_config(&self) -> io::Result<rustls::ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(AnyPeer(provider.signature_verification_algorithms));
        let (chain, key) = self.chain();
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(other)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)
            .map_err(other)?;
        config.alpn_protocols = vec![ALPN.to_vec()];
        Ok(config)
    }

    /// How this device links to the hub whose key has `fingerprint`: to that key and no other.
    pub fn connector(&self, fingerprint: &str) -> io::Result<tokio_rustls::TlsConnector> {
        self.client(Some(fingerprint.to_ascii_lowercase()))
    }

    /// How a machine that knows no hub yet links to one, to ask to join (decision D93): any key
    /// is taken, and is read from the link afterwards (`server_fingerprint`). Nothing is trusted
    /// from such a link until the user has compared the confirmation code on both screens.
    pub fn first_contact(&self) -> io::Result<tokio_rustls::TlsConnector> {
        self.client(None)
    }

    fn client(&self, pin: Option<String>) -> io::Result<tokio_rustls::TlsConnector> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(Pinned { fingerprint: pin, algorithms: provider.signature_verification_algorithms });
        let (chain, key) = self.chain();
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(other)?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(chain, key)
            .map_err(other)?;
        config.alpn_protocols = vec![ALPN.to_vec()];
        Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
    }

    /// The raw Ed25519 public key (32 bytes).
    pub fn public_key(&self) -> Vec<u8> {
        self.key.public_key().as_ref().to_vec()
    }

    /// The SubjectPublicKeyInfo whose SHA-256 is the fingerprint.
    pub fn public_key_info(&self) -> Vec<u8> {
        spki(self.key.public_key().as_ref())
    }

    /// Sign `message` with this key (Ed25519).
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.key.sign(message).as_ref().to_vec()
    }

    /// Link to whatever hub listens at `address`, to ask it to let this machine join.
    pub async fn dial_any(&self, address: &str) -> io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
        let stream = tokio::net::TcpStream::connect(address).await?;
        stream.set_nodelay(true).ok();
        let name = ServerName::try_from(SUBJECT).map_err(other)?;
        self.first_contact()?.connect(name, stream).await
    }

    /// Link over `stream`, already open (through the relay, or a test's pipe), to the hub whose
    /// key has `fingerprint`, or to any hub without one.
    pub async fn dial_over<S>(
        &self,
        stream: S,
        fingerprint: Option<&str>,
    ) -> io::Result<tokio_rustls::client::TlsStream<S>>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let name = ServerName::try_from(SUBJECT).map_err(other)?;
        let connector = match fingerprint {
            Some(fingerprint) => self.connector(fingerprint)?,
            None => self.first_contact()?,
        };
        connector.connect(name, stream).await
    }

    /// Link to the hub at `address` whose key has `fingerprint`.
    pub async fn dial(
        &self,
        address: &str,
        fingerprint: &str,
    ) -> io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
        let stream = tokio::net::TcpStream::connect(address).await?;
        stream.set_nodelay(true).ok();
        let name = ServerName::try_from(SUBJECT).map_err(other)?;
        self.connector(fingerprint)?.connect(name, stream).await
    }
}

/// Write `bytes` to `file` so that only its owner can read it, from the first byte on.
pub fn write_private(file: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let draft = file.with_extension("tmp");
    let _ = std::fs::remove_file(&draft);
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&draft)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    std::fs::rename(&draft, file)
}

/// The fingerprint of the key in a peer's certificate, read as the handshake read it. None for
/// a certificate that does not parse.
pub fn fingerprint_of(certificate: &[u8]) -> Option<String> {
    let der = CertificateDer::from(certificate);
    let parsed = webpki::EndEntityCert::try_from(&der).ok()?;
    Some(fingerprint_of_spki(parsed.subject_public_key_info().as_ref()))
}

/// The fingerprint of the peer on an accepted link.
pub fn peer_fingerprint<S>(stream: &tokio_rustls::server::TlsStream<S>) -> Option<String> {
    let (_, connection) = stream.get_ref();
    fingerprint_of(connection.peer_certificates()?.first()?.as_ref())
}

/// The fingerprint of the hub's key on a link this machine opened.
pub fn server_fingerprint<S>(stream: &tokio_rustls::client::TlsStream<S>) -> Option<String> {
    let (_, connection) = stream.get_ref();
    fingerprint_of(connection.peer_certificates()?.first()?.as_ref())
}

/// The session's join value (RFC 5705 keying material, label `nypi join`), on the hub's side.
pub fn join_value_server<S>(stream: &tokio_rustls::server::TlsStream<S>) -> Option<[u8; 32]> {
    let (_, connection) = stream.get_ref();
    connection.export_keying_material([0u8; 32], JOIN_LABEL, None).ok()
}

/// The same value on the joining machine's side: equal to the hub's only when nobody sits
/// between them.
pub fn join_value_client<S>(stream: &tokio_rustls::client::TlsStream<S>) -> Option<[u8; 32]> {
    let (_, connection) = stream.get_ref();
    connection.export_keying_material([0u8; 32], JOIN_LABEL, None).ok()
}

/// The six-digit confirmation code of a join (decision D93): from both keys and the session, so
/// a machine in the middle that swaps either key, or holds two sessions, makes the two screens
/// differ.
pub fn confirmation_code(hub: &str, device: &str, join_value: &[u8]) -> String {
    let mut input = b"nypi-join".to_vec();
    input.extend_from_slice(hub.to_ascii_lowercase().as_bytes());
    input.extend_from_slice(device.to_ascii_lowercase().as_bytes());
    input.extend_from_slice(join_value);
    let digest = ring::digest::digest(&ring::digest::SHA256, &input);
    let bytes = digest.as_ref();
    let number = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % 1_000_000;
    format!("{number:06}")
}

/// A code as people read it: `482 913`.
pub fn spaced(code: &str) -> String {
    match code.len() {
        6 => format!("{} {}", &code[..3], &code[3..]),
        _ => code.to_string(),
    }
}

/// Whether the Ed25519 `signature` of `message` was made by the key whose public half is
/// `public_key`, and that key has `fingerprint`.
pub fn verify(fingerprint: &str, public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    if !same(&fingerprint_of_spki(&spki(public_key)), fingerprint) {
        return false;
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key).verify(message, signature).is_ok()
}

/// The fingerprint of the key whose raw Ed25519 public half is `public_key`.
pub fn key_fingerprint(public_key: &[u8]) -> String {
    fingerprint_of_spki(&spki(public_key))
}

fn fingerprint_of_spki(spki: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, spki).as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// AlgorithmIdentifier for Ed25519 (1.3.101.112), which has no parameters.
const ED25519: [u8; 7] = [0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];

fn length(len: usize) -> Vec<u8> {
    match len {
        0..=0x7f => vec![len as u8],
        0x80..=0xff => vec![0x81, len as u8],
        _ => vec![0x82, (len >> 8) as u8, len as u8],
    }
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(length(content.len()));
    out.extend_from_slice(content);
    out
}

fn sequence(content: &[u8]) -> Vec<u8> {
    tlv(0x30, content)
}

fn set(content: &[u8]) -> Vec<u8> {
    tlv(0x31, content)
}

/// SubjectPublicKeyInfo for an Ed25519 public key.
fn spki(public: &[u8]) -> Vec<u8> {
    let mut bits = vec![0x00];
    bits.extend_from_slice(public);
    sequence(&[ED25519.to_vec(), tlv(0x03, &bits)].concat())
}

fn same(a: &str, b: &str) -> bool {
    // Compared in full whatever differs: how long it takes says nothing about where.
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The hub's check of a peer's certificate: that the peer holds the key in it. Who that key
/// belongs to is looked up afterwards.
#[derive(Debug)]
struct AnyPeer(WebPkiSupportedAlgorithms);

impl ClientCertVerifier for AnyPeer {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        match fingerprint_of(end_entity.as_ref()) {
            Some(_) => Ok(ClientCertVerified::assertion()),
            None => Err(rustls::Error::General("the peer's certificate does not hold a key".into())),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("only TLS 1.3 is spoken".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// A device's check of the hub's certificate: the key it was paired with, held by the peer.
#[derive(Debug)]
struct Pinned {
    /// None on a first contact: any key, read afterwards.
    fingerprint: Option<String>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match (fingerprint_of(end_entity.as_ref()), &self.fingerprint) {
            (Some(shown), Some(pinned)) if same(&shown, pinned) => Ok(ServerCertVerified::assertion()),
            (Some(_), None) => Ok(ServerCertVerified::assertion()),
            _ => Err(rustls::Error::General("this is not the hub this machine trusts".into())),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("only TLS 1.3 is spoken".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn two_devices_link_by_each_others_keys_and_by_nothing_else() {
        let dir = std::env::temp_dir().join(format!("nypi-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Identity::load_or_create(&dir.join("hub.key")).unwrap();
        let device = Identity::generate().unwrap();
        // The key is kept, readable by its owner only, and is the same key when read again.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join("hub.key")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(Identity::load_or_create(&dir.join("hub.key")).unwrap().fingerprint(), hub.fingerprint());
        assert_eq!(hub.fingerprint().len(), 64);
        assert_ne!(hub.fingerprint(), device.fingerprint());
        // A certificate says its own key, to whoever reads it as the handshake does.
        assert_eq!(fingerprint_of(&hub.certificate()), Some(hub.fingerprint()));
        assert_eq!(hub.certificate(), hub.certificate(), "the same every time");
        assert_eq!(fingerprint_of(b"not a certificate"), None);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let acceptor = hub.acceptor().unwrap();
        let serving = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                match acceptor.accept(stream).await {
                    Ok(mut stream) => {
                        seen.push(peer_fingerprint(&stream));
                        let mut said = [0u8; 5];
                        stream.read_exact(&mut said).await.unwrap();
                        stream.write_all(&said).await.unwrap();
                        stream.shutdown().await.ok();
                    }
                    Err(_) => seen.push(None),
                }
            }
            seen
        });

        // Paired with this hub: the link comes up, and the hub knows whose key is on it.
        let mut stream = device.dial(&address, &hub.fingerprint()).await.unwrap();
        stream.write_all(b"hello").await.unwrap();
        let mut back = [0u8; 5];
        stream.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"hello");
        assert_eq!(stream.get_ref().1.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_3));
        drop(stream);

        // Pinned to another hub: this one is refused before a byte is said to it.
        let other_hub = Identity::generate().unwrap();
        let Err(refused) = device.dial(&address, &other_hub.fingerprint()).await else { panic!("refused") };
        assert!(refused.to_string().contains("not the hub this machine trusts"), "{refused}");

        let seen = serving.await.unwrap();
        assert_eq!(seen[0], Some(device.fingerprint()));
        assert_eq!(seen[1], None, "the link that was refused never came up");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
