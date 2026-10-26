//! Codes d'invitation, en base32 Crockford groupé par 4 pour se dicter ou se
//! coller facilement (ex. `1C8A-0R9Q-3VZG`). Un code contient :
//! - l'adresse directe de l'hôte (IP publique ouverte par UPnP, ou IP locale),
//!   avec le début de l'empreinte de son certificat : on ne parle qu'à lui ;
//! - la clé d'accès du serveur ([`AccessKey`]) : l'hôte n'accepte que ceux
//!   qui la connaissent, et elle permet de trouver dans l'annuaire
//!   décentralisé ([`super::directory`]) qui l'héberge en ce moment. Ce
//!   code-là reste valable quel que soit le membre qui héberge, et ne demande
//!   aucun port ouvert.
//!
//! Un code d'invitation se partage donc comme une clé, entre amis. Les codes
//! d'avant la clé d'accès portaient l'identifiant du serveur : la clé
//! d'origine s'en déduit, ils restent valables tant qu'elle n'a pas changé.
//!
//! Une adresse brute `IP:port` se décode encore, mais l'hôte la refuse : il
//! faut la clé.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Result, bail};

use super::{
    card::{AccessKey, ServerId, default_access_key},
    protocol::CertFingerprint,
};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const TAG_V4: u8 = 4;
const TAG_V6: u8 = 6;
/// Code « serveur » : drapeaux puis champs présents.
const TAG_SERVER: u8 = 0x20;
const HAS_DIRECT: u8 = 1;
const HAS_CERT: u8 = 2;
/// Ancien format : l'identifiant du serveur (la clé d'origine s'en déduit).
const HAS_SERVER: u8 = 4;
const HAS_KEY: u8 = 8;

/// Début de l'empreinte du certificat de l'hôte (voir [`super::tls::MIN_PIN`]).
pub type CertPin = [u8; 16];

pub fn pin(cert: &CertFingerprint) -> CertPin {
    cert[..16].try_into().unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Invite {
    pub direct: Option<SocketAddr>,
    /// Certificat attendu à l'adresse directe.
    pub cert: Option<CertPin>,
    pub key: Option<AccessKey>,
}

impl Invite {
    /// Adresse brute, sans certificat (tapée à la main).
    pub fn direct(addr: SocketAddr) -> Self {
        Self {
            direct: Some(addr),
            ..Self::default()
        }
    }

    /// Adresse de l'hôte, son certificat et la clé d'accès.
    pub fn host(addr: SocketAddr, cert: &CertFingerprint, key: AccessKey) -> Self {
        Self {
            direct: Some(addr),
            cert: Some(pin(cert)),
            key: Some(key),
        }
    }

    /// Clé seule : l'hôte se trouve par l'annuaire.
    pub fn key(key: AccessKey) -> Self {
        Self {
            key: Some(key),
            ..Self::default()
        }
    }
}

fn push_addr(bytes: &mut Vec<u8>, addr: SocketAddr) {
    match addr.ip() {
        IpAddr::V4(ip) => {
            bytes.push(TAG_V4);
            bytes.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            bytes.push(TAG_V6);
            bytes.extend_from_slice(&ip.octets());
        }
    }
    bytes.extend_from_slice(&addr.port().to_be_bytes());
}

fn take_addr(bytes: &mut &[u8]) -> Result<SocketAddr> {
    let (ip, rest): (IpAddr, &[u8]) = match bytes.split_first() {
        Some((&TAG_V4, rest)) if rest.len() >= 6 => (
            Ipv4Addr::from(<[u8; 4]>::try_from(&rest[..4]).unwrap()).into(),
            &rest[4..],
        ),
        Some((&TAG_V6, rest)) if rest.len() >= 18 => (
            Ipv6Addr::from(<[u8; 16]>::try_from(&rest[..16]).unwrap()).into(),
            &rest[16..],
        ),
        _ => bail!("code d'invitation invalide"),
    };
    let port = u16::from_be_bytes([rest[0], rest[1]]);
    if port == 0 {
        bail!("code d'invitation invalide");
    }
    *bytes = &rest[2..];
    Ok(SocketAddr::new(ip, port))
}

pub fn encode(invite: &Invite) -> String {
    let mut bytes = Vec::with_capacity(40);
    match (invite.direct, invite.cert, invite.key) {
        // Format court historique : une adresse seule.
        (Some(addr), None, None) => push_addr(&mut bytes, addr),
        _ => {
            let mut flags = 0;
            if invite.direct.is_some() {
                flags |= HAS_DIRECT;
            }
            let cert = invite.cert.filter(|_| invite.direct.is_some());
            if cert.is_some() {
                flags |= HAS_CERT;
            }
            if invite.key.is_some() {
                flags |= HAS_KEY;
            }
            bytes.push(TAG_SERVER | flags);
            if let Some(addr) = invite.direct {
                push_addr(&mut bytes, addr);
            }
            if let Some(cert) = cert {
                bytes.extend_from_slice(&cert);
            }
            if let Some(key) = invite.key {
                bytes.extend_from_slice(&key);
            }
        }
    }

    let chars = base32_encode(&bytes);
    chars
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn decode(code: &str) -> Result<Invite> {
    // Collé depuis une messagerie : guillemets, retours à la ligne, espaces insécables…
    let code = code.trim_matches(|c: char| c.is_whitespace() || "\"'`«»“”‘’<>".contains(c));
    if code.is_empty() {
        bail!("code vide");
    }
    if let Ok(addr) = code.parse::<SocketAddr>() {
        return Ok(Invite::direct(addr));
    }
    let bytes = base32_decode(code)?;
    let mut rest = bytes.as_slice();
    match rest.first() {
        Some(&TAG_V4 | &TAG_V6) => Ok(Invite::direct(take_addr(&mut rest)?)),
        Some(&b) if b & 0xf0 == TAG_SERVER => {
            rest = &rest[1..];
            let mut invite = Invite::default();
            if b & HAS_DIRECT != 0 {
                invite.direct = Some(take_addr(&mut rest)?);
            }
            if b & HAS_CERT != 0 {
                let Some((cert, tail)) = rest.split_first_chunk::<16>() else {
                    bail!("code d'invitation invalide");
                };
                invite.cert = Some(*cert);
                rest = tail;
            }
            if b & (HAS_SERVER | HAS_KEY) != 0 {
                let Some((bytes, _)) = rest.split_first_chunk::<16>() else {
                    bail!("code d'invitation invalide");
                };
                invite.key = Some(if b & HAS_KEY != 0 {
                    *bytes
                } else {
                    default_access_key(ServerId(*bytes))
                });
            }
            if invite.direct.is_none() && invite.key.is_none() {
                bail!("code d'invitation invalide");
            }
            Ok(invite)
        }
        _ => bail!("code d'invitation invalide"),
    }
}

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.chars() {
        let v = match c.to_ascii_uppercase() {
            '-' | '_' | '.' => continue,
            c if c.is_whitespace() => continue,
            'O' => 0,
            'I' | 'L' => 1,
            c => match ALPHABET.iter().position(|&a| a as char == c) {
                Some(v) => v as u32,
                None => bail!("caractère « {c} » inattendu dans le code"),
            },
        };
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aller_retour() {
        for a in ["203.0.113.7:7878", "1.2.3.4:1", "[2a01:e0a::1]:7878"] {
            let invite = Invite::direct(a.parse().unwrap());
            let code = encode(&invite);
            assert_eq!(decode(&code).unwrap(), invite, "{code}");
            assert_eq!(decode(&code.to_lowercase().replace('-', " ")).unwrap(), invite);
        }
    }

    #[test]
    fn format_court() {
        let code = encode(&Invite::direct("203.0.113.7:7878".parse().unwrap()));
        assert_eq!(code.len(), 14, "{code}");
    }

    #[test]
    fn code_serveur() {
        let key = Some([7; 16]);
        for direct in [None, Some("203.0.113.7:7878".parse().unwrap())] {
            for cert in [None, Some([3; 16])] {
                let cert = cert.filter(|_| direct.is_some());
                let invite = Invite { direct, cert, key };
                let code = encode(&invite);
                assert_eq!(decode(&code).unwrap(), invite, "{code}");
            }
        }
        let court = encode(&Invite::key([9; 16]));
        assert_eq!(court.len(), 34, "{court}");
    }

    #[test]
    fn ancien_code_avec_identifiant() {
        // Codes d'avant la clé d'accès : drapeau « serveur » + identifiant.
        let id = ServerId([4; 16]);
        let mut bytes = vec![TAG_SERVER | HAS_SERVER];
        bytes.extend_from_slice(&id.0);
        let invite = decode(&base32_encode(&bytes)).unwrap();
        assert_eq!(invite.key, Some(default_access_key(id)));
    }

    #[test]
    fn adresse_et_certificat() {
        let invite = Invite::host("203.0.113.7:7878".parse().unwrap(), &[9; 32], [1; 16]);
        let code = encode(&invite);
        assert_eq!(decode(&code).unwrap(), invite);
        assert_eq!(decode(&code).unwrap().cert, Some([9; 16]));
        // Tronqué au milieu de l'empreinte : refusé.
        let bytes = base32_decode(&code).unwrap();
        assert!(decode(&base32_encode(&bytes[..12])).is_err());
    }

    #[test]
    fn adresse_brute_et_erreurs() {
        assert_eq!(
            decode(" 192.168.1.20:7878 ").unwrap(),
            Invite::direct("192.168.1.20:7878".parse().unwrap())
        );
        assert!(decode("").is_err());
        let code = encode(&Invite::key([5; 16]));
        let colle = format!("« {}\n{} »\u{a0}", &code[..10], &code[10..]);
        assert_eq!(decode(&colle).unwrap(), Invite::key([5; 16]));
        assert!(decode("ABCD").is_err());
        assert!(decode("UUUU").is_err());
    }
}
