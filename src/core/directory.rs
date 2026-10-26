//! Annuaire décentralisé : « où est hébergé ce serveur en ce moment ? ».
//!
//! Aucun serveur à installer : l'annuaire vit dans le réseau DHT de BitTorrent
//! (« Mainline », des millions de PC), qui stocke des petites valeurs signées
//! (BEP 44). Chaque serveur RIP Discord y a quelques cases dont l'adresse et
//! la clé de chiffrement découlent de sa clé d'accès : seuls ceux qui ont le
//! code d'invitation (ou le serveur enregistré) savent où regarder et lire.
//! Les nœuds du DHT ne voient que des octets chiffrés. Quand la clé d'accès
//! change, les cases changent aussi : un ancien code ne mène plus à l'hôte.
//!
//! Quelqu'un qui a le code peut y écrire une fausse adresse ; le membre qui
//! s'y connecte le démasque à la poignée de main (l'hôte doit prouver qu'il
//! connaît la clé, voir [`super::tls::access_proof`]).
//!
//! - case `host` : le membre qui héberge y écrit son adresse publique (vue par
//!   STUN, voir [`super::nat`]) et la rafraîchit tant qu'il héberge ;
//! - cases `knock-N` : un membre qui arrive y écrit sa propre adresse publique ;
//!   l'hôte, qui les surveille, lui envoie alors un paquet pour que sa box le
//!   laisse entrer (perforation UDP).

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use mainline::{Dht, MutableItem, SigningKey, async_dht::AsyncDht};
use ring::{
    aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey},
    digest::{SHA256, digest},
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::debug;

use super::{
    card::{AccessKey, now_ms},
    identity::UserId,
    protocol::CertFingerprint,
};

/// L'hôte rafraîchit sa case à ce rythme…
pub const REFRESH: Duration = Duration::from_secs(15);
/// … et une case plus vieille que ça est considérée abandonnée (hôte parti
/// sans prévenir, PC éteint). Large devant [`REFRESH`] : une écriture dans
/// le DHT peut prendre quelques secondes, et les anciennes versions
/// rafraîchissaient toutes les 20 s.
pub const STALE: Duration = Duration::from_secs(45);
/// Nombre de cases « on frappe à la porte » : plusieurs membres peuvent
/// arriver en même temps sans s'écraser.
pub const KNOCK_SLOTS: u8 = 3;

/// Adresse d'un hôte telle que publiée dans l'annuaire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAddr {
    pub public: SocketAddr,
    pub lan: Option<SocketAddr>,
}

impl HostAddr {
    /// Adresses à essayer depuis un membre dont l'adresse publique est `me`,
    /// dans l'ordre : l'adresse locale d'abord si on est derrière la même box
    /// (beaucoup de box ne savent pas renvoyer vers l'intérieur leur propre IP
    /// publique).
    pub fn candidates(&self, me: Option<SocketAddr>) -> Vec<SocketAddr> {
        let mut v = Vec::new();
        if let Some(lan) = self.lan
            && me.is_none_or(|me| me.ip() == self.public.ip())
        {
            v.push(lan);
        }
        v.push(self.public);
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    pub user: UserId,
    pub addr: HostAddr,
    /// Certificat que l'hôte présente : le membre qui arrive n'accepte que lui.
    pub cert: CertFingerprint,
    pub at_ms: u64,
}

impl HostRecord {
    pub fn is_fresh(&self) -> bool {
        self.age() < STALE
    }

    /// Temps écoulé depuis le dernier rafraîchissement.
    pub fn age(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.at_ms))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Knock {
    pub public: Option<SocketAddr>,
    pub lan: Option<SocketAddr>,
    pub at_ms: u64,
}

/// Les cases d'un serveur : clé de signature et de chiffrement partagées par
/// tous ceux qui connaissent sa clé d'accès.
#[derive(Clone)]
pub struct Slots {
    signer: SigningKey,
    seal: [u8; 32],
}

impl Slots {
    pub fn new(key: AccessKey) -> Self {
        let derive = |label: &str| -> [u8; 32] {
            let mut input = b"rip-discord/annuaire/v2/".to_vec();
            input.extend_from_slice(label.as_bytes());
            input.extend_from_slice(&key);
            digest(&SHA256, &input).as_ref().try_into().unwrap()
        };
        Self {
            signer: SigningKey::from_bytes(&derive("signature")),
            seal: derive("chiffrement"),
        }
    }

    fn key(&self) -> [u8; 32] {
        self.signer.verifying_key().to_bytes()
    }

    fn encrypt<T: Serialize>(&self, value: &T) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow!("aléa indisponible"))?;
        let mut data = postcard::to_stdvec(value)?;
        self.cipher()
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut data)
            .map_err(|_| anyhow!("chiffrement impossible"))?;
        Ok([nonce.as_slice(), &data].concat())
    }

    fn decrypt<T: DeserializeOwned>(&self, sealed: &[u8]) -> Option<T> {
        let (nonce, data) = sealed.split_at_checked(12)?;
        let mut data = data.to_vec();
        let plain = self
            .cipher()
            .open_in_place(Nonce::try_assume_unique_for_key(nonce).ok()?, Aad::empty(), &mut data)
            .ok()?;
        postcard::from_bytes(plain).ok()
    }

    fn cipher(&self) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &self.seal).unwrap())
    }
}

fn knock_salt(slot: u8) -> Vec<u8> {
    format!("knock-{slot}").into_bytes()
}

const HOST_SALT: &[u8] = b"host";

/// Nœuds d'entrée du DHT. Par défaut ceux de BitTorrent ;
/// `RIP_DISCORD_DHT_BOOTSTRAP` (liste séparée par des virgules) les remplace
/// (tests, réseau de développement).
fn bootstrap() -> Option<Vec<String>> {
    let list = std::env::var("RIP_DISCORD_DHT_BOOTSTRAP").ok()?;
    Some(
        list.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// Accès à l'annuaire. Peu coûteux à cloner.
#[derive(Clone)]
pub struct Directory {
    dht: AsyncDht,
}

impl Directory {
    /// Rejoint le réseau DHT (quelques secondes la première fois).
    pub async fn start() -> Result<Self> {
        let mut builder = Dht::builder();
        if let Some(nodes) = bootstrap() {
            let resolved: Vec<SocketAddrV4> = nodes
                .iter()
                .filter_map(|n| n.to_socket_addrs().ok())
                .flatten()
                .filter_map(|a| match a {
                    SocketAddr::V4(a) => Some(a),
                    SocketAddr::V6(_) => None,
                })
                .collect();
            builder.bootstrap(&resolved);
            if resolved.iter().all(|a| a.ip().is_loopback()) {
                builder.bind_address(Ipv4Addr::LOCALHOST);
            }
        }
        let dht = builder
            .build()
            .context("impossible de rejoindre le réseau DHT")?
            .as_async();
        match tokio::time::timeout(Duration::from_secs(15), dht.bootstrapped()).await {
            Ok(true) => Ok(Self { dht }),
            _ => bail!("réseau DHT injoignable (pas d'accès à Internet, ou UDP bloqué ?)"),
        }
    }

    async fn get<T: DeserializeOwned>(&self, slots: &Slots, salt: &[u8]) -> Option<(i64, T)> {
        let item = self.dht.get_mutable_most_recent(&slots.key(), Some(salt)).await?;
        Some((item.seq(), slots.decrypt(item.value())?))
    }

    async fn put<T: Serialize>(&self, slots: &Slots, salt: &[u8], seq: i64, value: &T) -> Result<()> {
        let item = MutableItem::new(slots.signer.clone(), &slots.encrypt(value)?, seq, Some(salt));
        self.dht
            .put_mutable(item, None)
            .await
            .map_err(|e| anyhow!("écriture dans l'annuaire : {e}"))?;
        Ok(())
    }

    /// Qui héberge ce serveur d'après l'annuaire (éventuellement périmé : voir
    /// [`HostRecord::is_fresh`]).
    pub async fn lookup(&self, slots: &Slots) -> Option<(i64, HostRecord)> {
        self.get(slots, HOST_SALT).await
    }

    /// Publie (ou rafraîchit) l'adresse de l'hôte. `seq` doit croître à chaque
    /// publication ; renvoie celui utilisé.
    pub async fn publish(&self, slots: &Slots, after: i64, record: &HostRecord) -> Result<i64> {
        let seq = (now_ms() as i64).max(after + 1);
        self.put(slots, HOST_SALT, seq, record).await?;
        Ok(seq)
    }

    /// Signale à l'hôte qu'on arrive, depuis `knock.public`.
    pub async fn knock(&self, slots: &Slots, knock: &Knock) -> Result<()> {
        let mut slot = [0u8];
        SystemRandom::new()
            .fill(&mut slot)
            .map_err(|_| anyhow!("aléa indisponible"))?;
        let salt = knock_salt(slot[0] % KNOCK_SLOTS);
        let after = self
            .dht
            .get_mutable_most_recent(&slots.key(), Some(&salt))
            .await
            .map_or(0, |i| i.seq());
        self.put(slots, &salt, (now_ms() as i64).max(after + 1), knock).await
    }

    /// Lit les cases « on frappe » ; `seen` retient le dernier numéro vu par
    /// case, pour ne renvoyer que les nouveaux venus.
    pub async fn knocks(&self, slots: &Slots, seen: &mut [i64; KNOCK_SLOTS as usize]) -> Vec<Knock> {
        let reads = (0..KNOCK_SLOTS).map(|s| {
            let salt = knock_salt(s);
            async move { self.get::<Knock>(slots, &salt).await }
        });
        let mut fresh = Vec::new();
        for (slot, read) in futures_join_all(reads).await.into_iter().enumerate() {
            if let Some((seq, knock)) = read
                && seq > seen[slot]
            {
                seen[slot] = seq;
                if now_ms().saturating_sub(knock.at_ms) < 60_000 {
                    debug!("annuaire : un membre arrive depuis {:?}", knock.public);
                    fresh.push(knock);
                }
            }
        }
        fresh
    }
}

/// Lance les futures ensemble et attend tous les résultats, dans l'ordre.
async fn futures_join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let mut pinned: Vec<_> = futures.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = pinned.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (f, slot) in pinned.iter_mut().zip(out.iter_mut()) {
            if slot.is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => *slot = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    out.into_iter().map(Option::unwrap).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases_chiffrees_et_propres_au_serveur() {
        let a = Slots::new([1; 16]);
        let b = Slots::new([2; 16]);
        assert_ne!(a.key(), b.key());
        let knock = Knock {
            public: Some("203.0.113.7:51234".parse().unwrap()),
            lan: None,
            at_ms: 42,
        };
        let sealed = a.encrypt(&knock).unwrap();
        assert_eq!(a.decrypt::<Knock>(&sealed), Some(knock));
        assert_eq!(b.decrypt::<Knock>(&sealed), None);
        assert!(!sealed.windows(4).any(|w| w == [203, 0, 113, 7]));
    }

    #[test]
    fn meme_box_essaie_le_reseau_local() {
        let host = HostAddr {
            public: "203.0.113.7:7878".parse().unwrap(),
            lan: Some("192.168.1.20:7878".parse().unwrap()),
        };
        let voisin = "203.0.113.7:51000".parse().ok();
        let loin = "198.51.100.3:40000".parse().ok();
        assert_eq!(host.candidates(voisin), vec![host.lan.unwrap(), host.public]);
        assert_eq!(host.candidates(loin), vec![host.public]);
    }
}
