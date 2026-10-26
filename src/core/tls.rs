//! Configuration TLS/QUIC.
//!
//! L'hôte génère un certificat auto-signé à chaque lancement. Lors d'une
//! relève d'hôte, les membres connaissent d'avance l'empreinte du certificat
//! du successeur (`Control::Succession`) et refusent tout autre certificat
//! ([`client_config_pinned`]). Le premier contact aussi : le code
//! d'invitation porte le début de l'empreinte du certificat de l'hôte, et
//! l'annuaire (lisible seulement avec le code) son empreinte entière. Seule
//! une adresse brute `IP:port` tapée à la main accepte encore n'importe quel
//! certificat ; le serveur reste alors authentifié par sa carte signée, et
//! l'identité du membre par `Hello::proof`, lié à la session TLS.
//!
//! En plus, chacun prouve à l'autre qu'il connaît la clé d'accès du serveur
//! ([`access_proof`]) : sans le code d'invitation, on ne peut ni entrer chez
//! l'hôte, ni se faire passer pour lui.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, ring},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};

use super::{ALPN, MESH_ALPN, card::AccessKey, protocol::CertFingerprint};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// Un hôte muet depuis ce délai est considéré parti : la relève démarre.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Réglages QUIC pensés pour la voix :
/// - keep-alive court : un hôte disparu est détecté en quelques secondes, ce
///   qui déclenche la relève d'hôte ;
/// - petite file d'envoi des datagrammes : quand le lien sature (partage
///   d'écran, Wi-Fi faible), mieux vaut perdre une vieille trame de voix que la
///   jouer avec une seconde de retard. Quinn jette alors les plus anciennes.
fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().unwrap()));
    // ≈ 200 ms de voix pour une dizaine de personnes qui parlent en même temps.
    t.datagram_send_buffer_size(24 * 1024);
    Arc::new(t)
}

/// Empreinte SHA-256 d'un certificat.
pub fn fingerprint(cert: &CertificateDer) -> CertFingerprint {
    ::ring::digest::digest(&::ring::digest::SHA256, cert)
        .as_ref()
        .try_into()
        .expect("SHA-256 fait 32 octets")
}

/// Certificat prêt à servir, généré d'avance par chaque membre pour pouvoir
/// prendre le relais de l'hôte sans délai.
#[derive(Clone)]
pub struct HostCert {
    pub config: quinn::ServerConfig,
    pub fingerprint: CertFingerprint,
}

impl HostCert {
    pub fn generate() -> Result<Self> {
        let (config, cert) = server_config()?;
        Ok(Self {
            config,
            fingerprint: fingerprint(&cert),
        })
    }
}

/// Config serveur + certificat DER (à partager aux clients pour l'épinglage).
pub fn server_config() -> Result<(quinn::ServerConfig, CertificateDer<'static>)> {
    server_config_for(&[ALPN, MESH_ALPN])
}

fn server_config_for(alpns: &[&[u8]]) -> Result<(quinn::ServerConfig, CertificateDer<'static>)> {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let cert = generated.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()));

    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)?;
    tls.alpn_protocols = alpns.iter().map(|a| a.to_vec()).collect();

    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    cfg.transport_config(transport());
    Ok((cfg, cert))
}

pub fn client_config() -> Result<quinn::ClientConfig> {
    client_config_for(ALPN, None)
}

/// Longueur minimale d'une empreinte épinglée (début du SHA-256) : celle
/// des codes d'invitation.
pub const MIN_PIN: usize = 16;

/// N'accepte que le certificat dont l'empreinte commence par `pin`
/// (successeur annoncé, code d'invitation, annuaire).
pub fn client_config_pinned(pin: &[u8]) -> Result<quinn::ClientConfig> {
    client_config_for(ALPN, Some(pin))
}

/// Connexion directe vers un autre membre du salon vocal, dont on connaît
/// le certificat.
pub fn mesh_client_config(pin: CertFingerprint) -> Result<quinn::ClientConfig> {
    client_config_for(MESH_ALPN, Some(&pin))
}

/// Protocole négocié sur une connexion entrante.
pub fn alpn(conn: &quinn::Connection) -> Option<Vec<u8>> {
    conn.handshake_data()?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .ok()?
        .protocol
}

fn client_config_for(alpn: &[u8], pin: Option<&[u8]>) -> Result<quinn::ClientConfig> {
    anyhow::ensure!(pin.is_none_or(|p| p.len() >= MIN_PIN), "empreinte trop courte");
    let pin = pin.map(|p| p.to_vec().into_boxed_slice());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider(), pin)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];

    let mut cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cfg.transport_config(transport());
    Ok(cfg)
}

/// Secret dérivé de la session TLS, identique des deux côtés d'une même
/// connexion et différent pour chaque connexion.
pub fn session_binding(conn: &quinn::Connection) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, b"rip-discord hello", b"")
        .map_err(|_| anyhow::anyhow!("dérivation de la clé de session impossible"))?;
    Ok(out)
}

/// Qui prouve connaître la clé d'accès : le membre qui arrive, ou l'hôte qui
/// l'accueille. Les deux preuves diffèrent : on ne peut pas renvoyer à
/// l'autre celle qu'il vient d'envoyer.
#[derive(Debug, Clone, Copy)]
pub enum Side {
    Member,
    Host,
}

/// Preuve qu'on connaît `key`, liée à cette connexion (pas de rejeu ailleurs)
/// et qui ne révèle pas la clé.
pub fn access_proof(conn: &quinn::Connection, key: &AccessKey, side: Side) -> Result<[u8; 32]> {
    let message = access_message(conn, side)?;
    let tag = ::ring::hmac::sign(&hmac_key(key), &message);
    Ok(tag.as_ref().try_into().expect("HMAC-SHA256 fait 32 octets"))
}

/// Vérifie une preuve reçue (comparaison en temps constant).
pub fn check_access(conn: &quinn::Connection, key: &AccessKey, side: Side, proof: &[u8; 32]) -> bool {
    access_message(conn, side).is_ok_and(|m| ::ring::hmac::verify(&hmac_key(key), &m, proof).is_ok())
}

fn hmac_key(key: &AccessKey) -> ::ring::hmac::Key {
    ::ring::hmac::Key::new(::ring::hmac::HMAC_SHA256, key)
}

fn access_message(conn: &quinn::Connection, side: Side) -> Result<Vec<u8>> {
    let label: &[u8] = match side {
        Side::Member => b"rip-discord/acces/membre",
        Side::Host => b"rip-discord/acces/hote",
    };
    Ok([label, &session_binding(conn)?].concat())
}

/// Accepte n'importe quel certificat (ou seulement celui épinglé), et vérifie
/// les signatures de la poignée de main.
#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>, Option<Box<[u8]>>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match &self.1 {
            Some(pin) if !fingerprint(end_entity).starts_with(pin) => Err(rustls::Error::General(
                "certificat différent de celui annoncé : ce n'est pas l'hôte attendu".into(),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
