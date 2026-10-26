//! Types de messages échangés entre clients et hôte.
//!
//! Tout passe par un flux QUIC bidirectionnel « contrôle » (fiable, ordonné),
//! sauf la voix (datagrammes, voir [`super::voice`]), les partages d'écran et
//! les webcams (un flux unidirectionnel par image, le son en datagrammes, voir
//! [`super::screen`]).

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use super::{
    card::{ChannelId, ServerCard, SignedMessage},
    identity::UserId,
};

/// Numéro de connexion attribué par l'hôte (change à chaque connexion).
pub type PeerId = u32;

/// Incrémenté à chaque changement incompatible du protocole.
pub const PROTOCOL_VERSION: u16 = 7;

/// Empreinte SHA-256 d'un certificat d'hôte (voir [`super::tls::fingerprint`]).
pub type CertFingerprint = [u8; 32];

/// Ce que la box d'un membre fait de son port (pour choisir le meilleur
/// successeur : du plus facile à joindre au plus difficile).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Reach {
    /// Adresse publique directe, ou port ouvert : tout le monde entre.
    Open,
    /// Box classique : la perforation marche.
    Stable,
    /// Adresse publique inconnue (STUN injoignable) : réseau local seulement, peut-être.
    Unknown,
    /// NAT symétrique : la perforation échoue souvent.
    Symmetric,
}

/// Ce qu'un membre propose pour prendre le relais si l'hôte disparaît : les
/// adresses de son port QUIC (celui-là même qui parle à l'hôte, donc déjà
/// ouvert dans sa box) et l'empreinte du certificat qu'il présentera.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayOffer {
    pub addrs: Vec<SocketAddr>,
    pub cert: CertFingerprint,
    pub reach: Reach,
}

/// Comment joindre un membre directement : pour lui parler en vocal de PC à
/// PC, et pour le rejoindre s'il prend la relève de l'hôte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contact {
    pub peer: PeerId,
    pub user: UserId,
    /// Adresses annoncées par le membre, plus celle vue par l'hôte.
    pub addrs: Vec<SocketAddr>,
    pub cert: CertFingerprint,
    /// Peut prendre la relève (tout le monde sauf l'hôte lui-même).
    pub heir: bool,
}

/// Premier message d'une connexion directe entre deux membres (vocal de PC à
/// PC) : qui je suis, prouvé comme dans `Hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeshHello {
    pub server: super::card::ServerId,
    pub peer: PeerId,
    pub user: UserId,
    pub proof: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: PeerId,
    pub name: String,
    /// Identité stable (clé publique) derrière la connexion.
    pub user: UserId,
}

/// Qui est dans un salon vocal, et qui en héberge l'audio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceRoom {
    pub channel: ChannelId,
    /// Dans l'ordre d'arrivée.
    pub members: Vec<PeerId>,
    /// Premier arrivé encore présent (pour l'affichage ; la voix circule
    /// directement entre participants).
    pub host: Option<PeerId>,
    /// Participants qui partagent leur écran.
    pub sharing: Vec<PeerId>,
    /// Participants dont la webcam est allumée.
    pub cameras: Vec<PeerId>,
    /// Participants qui ont coupé leur micro / leur son.
    pub muted: Vec<PeerId>,
    pub deafened: Vec<PeerId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Packet {
    Control(Control),
    Chat(Chat),
}

/// Signaux de contrôle de session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Control {
    /// Premier message du client.
    Hello {
        name: String,
        version: u16,
        user: UserId,
        /// Signature par `user` d'un secret propre à cette connexion TLS
        /// ([`super::tls::session_binding`]) : prouve qu'on détient la clé, sans
        /// rejeu possible sur une autre connexion.
        proof: Vec<u8>,
        /// Numéro qu'on avait chez l'hôte précédent : après une relève, chacun
        /// le retrouve (volumes, écran regardé, voix en cours restent valables).
        resume: Option<PeerId>,
        /// Preuve qu'on connaît la clé d'accès du serveur, c'est-à-dire le code
        /// d'invitation ([`super::tls::access_proof`]).
        access: [u8; 32],
    },
    /// Réponse de l'hôte : identité attribuée, serveur, pairs et salons vocaux.
    /// L'historique suit dans un ou plusieurs [`Chat::History`].
    Welcome {
        peer_id: PeerId,
        host_id: PeerId,
        peers: Vec<PeerInfo>,
        card: Box<ServerCard>,
        voice: Vec<VoiceRoom>,
        /// Celui qui héberge (son propre client), pour l'afficher.
        host_user: Option<UserId>,
        /// L'hôte prouve à son tour qu'il connaît la clé d'accès : une fausse
        /// adresse glissée dans l'annuaire par quelqu'un qui n'a pas le code
        /// (ou un ancien code) est démasquée.
        access: [u8; 32],
    },
    /// Nouvelle version du serveur (salons, admins, historique). Envoyée par
    /// un admin pour modifier, ou par n'importe quel membre qui détient une
    /// copie plus récente que celle de l'hôte ; l'hôte la rediffuse.
    CardUpdate(Box<ServerCard>),
    /// Entrer dans un salon vocal (on quitte le précédent). `share`,
    /// `camera` : y partager son écran, y allumer sa webcam tout de suite
    /// (reprise après une relève d'hôte).
    VoiceJoin {
        channel: ChannelId,
        share: bool,
        camera: bool,
    },
    VoiceLeave,
    /// Commencer / arrêter de partager son écran (dans son salon vocal).
    ScreenShare(bool),
    /// Regarder l'écran de ce participant (`None` : plus aucun). Seuls les
    /// spectateurs reçoivent les images.
    ScreenWatch(Option<PeerId>),
    /// Il faut une image clé (voir [`super::screen`]) : spectateur → hôte,
    /// pour l'écran de ce participant ; hôte → partageur, de la part de ce
    /// spectateur.
    ScreenKey(PeerId),
    /// Ma webcam est allumée / éteinte (dans mon salon vocal). Les images
    /// circulent directement entre participants.
    Camera(bool),
    /// Webcams de mon salon que je ne reçois pas directement (pas de
    /// connexion avec leur propriétaire) : l'hôte me les relaie.
    CameraRelay(Vec<PeerId>),
    /// Comme [`Control::ScreenKey`], pour une webcam relayée par l'hôte.
    CameraKey(PeerId),
    /// État d'un salon vocal, diffusé à chaque changement.
    VoiceState(VoiceRoom),
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    PeerJoined(PeerInfo),
    PeerLeft {
        peer_id: PeerId,
    },
    /// Départ propre.
    Bye,
    Error {
        reason: String,
    },

    /// Mon micro / mon son coupé (affiché aux autres).
    Status {
        muted: bool,
        deafened: bool,
    },

    // --- Relève d'hôte (voir server::migration) ---
    /// Membre → hôte : de quoi me joindre directement (vocal de PC à PC,
    /// relève).
    Relay(RelayOffer),
    /// Hôte → membres : les contacts de chacun, dans l'ordre de succession,
    /// rediffusés à chaque changement.
    Succession(Vec<Contact>),
    /// Membre → hôte : je voudrais héberger (l'hôte lag, par exemple).
    HostRequest,
    /// Hôte → membres : `candidate` veut héberger ; répondre avant `secs`.
    Vote {
        id: u64,
        candidate: PeerId,
        secs: u8,
    },
    Ballot {
        id: u64,
        yes: bool,
    },
    /// Résultat du vote ; s'il est accepté, l'hôte passe la main juste après.
    VoteResult {
        id: u64,
        candidate: PeerId,
        accepted: bool,
        yes: u16,
        no: u16,
    },
    /// L'hôte part (volontairement) : relève immédiate, sans attendre que
    /// la connexion expire.
    HostLeaving,
    /// Un autre membre héberge ce serveur : le retrouver via l'annuaire.
    Moved,

    // --- Modération et limites ---
    /// Admin → hôte : déconnecter ce membre (il peut revenir, sauf s'il est
    /// aussi banni).
    Kick {
        peer: PeerId,
    },
    /// Hôte → membre : tu es déconnecté (expulsé ou banni). Ne pas chercher
    /// à se reconnecter.
    Kicked {
        reason: String,
    },
    /// Hôte → membre : trop de messages d'un coup, le dernier a été ignoré.
    SlowDown,

    // --- Pièces jointes (voir core::files) ---
    /// Hôte → membres : qui a ce fichier ? (l'hôte ne l'a pas, quelqu'un le
    /// demande). Ceux qui l'ont le lui envoient.
    FileWanted {
        hash: [u8; 32],
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Chat {
    /// `from` est renseigné par l'hôte ; la valeur envoyée par le client est ignorée.
    Text { from: PeerId, msg: SignedMessage },
    /// Hôte → membre : messages gardés, envoyés à l'arrivée (par paquets).
    /// Membre → hôte : messages que l'hôte n'avait pas (ceux que j'ai gardés
    /// pendant qu'il était absent) ; il les ajoute et les rediffuse.
    History(Vec<SignedMessage>),
    /// Fin de l'historique envoyé à l'arrivée.
    HistoryEnd,
}

/// Découpe des messages en paquets qui tiennent dans une trame.
pub fn history_batches(messages: &[SignedMessage]) -> Vec<Vec<SignedMessage>> {
    let budget = super::framing::MAX_FRAME / 2;
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut size = 0;
    for m in messages {
        let len = postcard::experimental::serialized_size(m).unwrap_or(budget);
        if size + len > budget && !current.is_empty() {
            batches.push(std::mem::take(&mut current));
            size = 0;
        }
        size += len;
        current.push(m.clone());
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

impl From<Control> for Packet {
    fn from(c: Control) -> Self {
        Packet::Control(c)
    }
}

impl From<Chat> for Packet {
    fn from(c: Chat) -> Self {
        Packet::Chat(c)
    }
}
