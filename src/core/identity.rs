//! Identité cryptographique d'un utilisateur : une paire de clés Ed25519 créée
//! au premier lancement et gardée sur la machine. La clé publique ([`UserId`])
//! sert d'identifiant stable : elle signe les messages et les réglages des
//! serveurs, ce qui permet à n'importe quel membre de ré-héberger un serveur
//! sans pouvoir falsifier ce que les autres ont écrit ou décidé.

use std::fmt;

use anyhow::{Result, anyhow};
use ring::{
    rand::SystemRandom,
    signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey},
};
use serde::{Deserialize, Serialize};

/// Clé publique Ed25519 d'un utilisateur.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UserId(pub [u8; 32]);

impl UserId {
    /// Empreinte courte, lisible (8 caractères hexadécimaux).
    pub fn short(&self) -> String {
        hex(&self.0[..4])
    }

    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        UnparsedPublicKey::new(&ED25519, self.0)
            .verify(message, signature)
            .is_ok()
    }
}

impl fmt::Debug for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UserId({})", self.short())
    }
}

pub struct Identity {
    pkcs8: Vec<u8>,
    pair: Ed25519KeyPair,
}

impl Identity {
    pub fn generate() -> Self {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .expect("générateur aléatoire du système indisponible")
            .as_ref()
            .to_vec();
        Self::from_pkcs8(&pkcs8).expect("clé tout juste générée")
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|e| anyhow!("clé d'identité illisible : {e}"))?;
        Ok(Self {
            pkcs8: pkcs8.to_vec(),
            pair,
        })
    }

    /// Forme sérialisée de la clé privée (à garder sur la machine uniquement).
    pub fn to_pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    pub fn id(&self) -> UserId {
        UserId(
            self.pair
                .public_key()
                .as_ref()
                .try_into()
                .expect("clé publique Ed25519 de 32 octets"),
        )
    }

    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.pair.sign(message).as_ref().to_vec()
    }
}

impl Clone for Identity {
    fn clone(&self) -> Self {
        Self::from_pkcs8(&self.pkcs8).expect("clé déjà valide")
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signe_et_verifie() {
        let me = Identity::generate();
        let sig = me.sign(b"bonjour");
        assert!(me.id().verify(b"bonjour", &sig));
        assert!(!me.id().verify(b"bonsoir", &sig));
        assert!(!Identity::generate().id().verify(b"bonjour", &sig));

        let again = Identity::from_pkcs8(me.to_pkcs8()).unwrap();
        assert_eq!(again.id(), me.id());
    }
}
