//! « Serveur » au sens Discord : un nom, des salons texte, des salons vocaux et
//! une politique d'historique. Il n'existe pas de machine centrale : la
//! [`ServerCard`] est copiée chez chaque membre, et n'importe lequel d'entre eux
//! peut la ré-héberger si le créateur n'est pas là.
//!
//! Ce qui empêche un membre de tout réécrire en ré-hébergeant :
//! - l'identifiant du serveur dérive de la clé publique du créateur ;
//! - la liste des admins est signée par le créateur ;
//! - les réglages (salons, historique) sont signés par le créateur ou un admin ;
//! - chaque message est signé par son auteur.
//!
//! On n'entre pas sans la clé d'accès ([`ServerCard::access_key`]) que porte
//! le code d'invitation ; le créateur ou un admin peut la changer
//! ([`ServerCard::new_access_key`]) pour fermer la porte à un ancien code.
//!
//! Chaque partie porte un numéro de révision : en cas de copies divergentes,
//! la plus récente l'emporte ([`ServerCard::merge`]).

use std::fmt;

use anyhow::{Result, bail, ensure};
use ring::{
    digest::{SHA256, digest},
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::identity::{Identity, UserId, hex};

pub type ChannelId = u32;

const DOMAIN_ADMINS: &[u8] = b"rip-discord/admins/1";
const DOMAIN_SETTINGS: &[u8] = b"rip-discord/settings/1";
const DOMAIN_BANS: &[u8] = b"rip-discord/bans/1";
const DOMAIN_ACCESS: &[u8] = b"rip-discord/access/1";
const DOMAIN_MESSAGE: &[u8] = b"rip-discord/message/1";
/// Messages enrichis (réponse, mentions, fichiers) et actions sur un message
/// (modifier, supprimer, réagir).
const DOMAIN_MESSAGE_2: &[u8] = b"rip-discord/message/2";

/// Longueur maximale d'un message texte (caractères).
pub const MAX_MESSAGE_CHARS: usize = 2000;
/// Pièces jointes par message, au plus.
pub const MAX_FILES: usize = 10;
/// Taille maximale d'une pièce jointe.
pub const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
/// Personnes mentionnées par message, au plus.
pub const MAX_MENTIONS: usize = 32;
/// Avance tolérée sur l'heure d'un message (horloges des PC pas tout à fait
/// à l'heure). Au-delà, il resterait « le plus récent » pour toujours : il
/// fausserait l'ordre du salon et la limite de messages gardés.
pub const MAX_AHEAD_MS: u64 = 5 * 60 * 1000;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ServerId(pub [u8; 16]);

impl ServerId {
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// Préfixe court mis dans les codes d'invitation et donné au point de
    /// rendez-vous. Le client vérifie que la carte reçue le confirme.
    pub fn tag(&self) -> ServerTag {
        self.0[..8].try_into().unwrap()
    }
}

pub type ServerTag = [u8; 8];

/// Clé d'accès d'un serveur : ce que porte le code d'invitation. Sans elle,
/// on ne peut ni rejoindre l'hôte, ni le trouver dans l'annuaire, ni se faire
/// passer pour lui.
pub type AccessKey = [u8; 16];

/// Clé d'accès d'origine, tirée de l'identifiant du serveur : celle des codes
/// d'invitation d'avant [`AccessList`], tant que personne ne l'a changée.
pub fn default_access_key(id: ServerId) -> AccessKey {
    let mut input = b"rip-discord/acces/v1/".to_vec();
    input.extend_from_slice(&id.0);
    digest(&SHA256, &input).as_ref()[..16].try_into().unwrap()
}

impl fmt::Debug for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ServerId({})", &self.to_hex()[..8])
    }
}

/// Un contenu et la signature de celui qui l'a produit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signed<T> {
    pub body: T,
    pub signer: UserId,
    pub sig: Vec<u8>,
}

impl<T: Serialize + DeserializeOwned> Signed<T> {
    fn new(body: T, who: &Identity, domain: &[u8]) -> Self {
        let sig = who.sign(&to_sign(domain, &body));
        Self {
            body,
            signer: who.id(),
            sig,
        }
    }

    fn valid(&self, domain: &[u8]) -> bool {
        self.signer.verify(&to_sign(domain, &self.body), &self.sig)
    }
}

fn to_sign<T: Serialize>(domain: &[u8], body: &T) -> Vec<u8> {
    let mut bytes = domain.to_vec();
    bytes.extend(postcard::to_stdvec(body).expect("sérialisation en mémoire"));
    bytes
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminList {
    pub server: ServerId,
    pub revision: u64,
    pub admins: Vec<UserId>,
}

/// Membres bannis : leur connexion est refusée par l'hôte et leurs messages
/// ignorés. Signée par le créateur ou un admin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BanList {
    pub server: ServerId,
    pub revision: u64,
    pub banned: Vec<UserId>,
}

/// Clé d'accès en vigueur, choisie par le créateur ou un admin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccessList {
    pub server: ServerId,
    pub revision: u64,
    pub key: AccessKey,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub name: String,
}

/// Ce que le serveur garde des messages. Décidé par le créateur et les admins ;
/// chaque membre l'applique aussi à sa propre copie.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HistoryPolicy {
    /// `false` : rien n'est écrit sur disque, les nouveaux venus ne voient que
    /// les messages envoyés après leur arrivée, et les copies existantes sont
    /// effacées chez tous les membres.
    pub keep: bool,
    /// Nombre de messages gardés par salon (0 = sans limite).
    pub max_per_channel: u32,
}

impl Default for HistoryPolicy {
    fn default() -> Self {
        Self {
            keep: true,
            max_per_channel: 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub server: ServerId,
    pub revision: u64,
    pub name: String,
    pub text_channels: Vec<Channel>,
    pub voice_channels: Vec<Channel>,
    pub history: HistoryPolicy,
}

impl Settings {
    fn next_channel_id(&self) -> ChannelId {
        self.text_channels
            .iter()
            .chain(&self.voice_channels)
            .map(|c| c.id)
            .max()
            .map_or(1, |m| m + 1)
    }

    pub fn add_text_channel(&mut self, name: &str) -> ChannelId {
        let id = self.next_channel_id();
        self.text_channels.push(Channel { id, name: name.into() });
        id
    }

    pub fn add_voice_channel(&mut self, name: &str) -> ChannelId {
        let id = self.next_channel_id();
        self.voice_channels.push(Channel { id, name: name.into() });
        id
    }

    pub fn remove_channel(&mut self, id: ChannelId) {
        self.text_channels.retain(|c| c.id != id);
        self.voice_channels.retain(|c| c.id != id);
    }

    pub fn rename_channel(&mut self, id: ChannelId, name: &str) {
        for c in self.text_channels.iter_mut().chain(self.voice_channels.iter_mut()) {
            if c.id == id {
                c.name = name.into();
            }
        }
    }

    pub fn is_text_channel(&self, id: ChannelId) -> bool {
        self.text_channels.iter().any(|c| c.id == id)
    }

    pub fn is_voice_channel(&self, id: ChannelId) -> bool {
        self.voice_channels.iter().any(|c| c.id == id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Admin,
    Member,
}

/// Tout ce qu'il faut pour héberger ou rejoindre un serveur (hors historique).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerCard {
    pub id: ServerId,
    pub owner: UserId,
    pub nonce: [u8; 16],
    pub admins: Signed<AdminList>,
    pub settings: Signed<Settings>,
    /// Absente tant que personne n'a été banni.
    pub bans: Option<Signed<BanList>>,
    /// Absente tant que la clé d'accès d'origine n'a pas été changée.
    pub access: Option<Signed<AccessList>>,
}

/// Carte d'avant la clé d'accès (copies locales déjà enregistrées).
#[derive(Deserialize)]
struct CardV2 {
    id: ServerId,
    owner: UserId,
    nonce: [u8; 16],
    admins: Signed<AdminList>,
    settings: Signed<Settings>,
    bans: Option<Signed<BanList>>,
}

/// Carte des versions d'avant les bannissements (copies locales déjà
/// enregistrées) : relue telle quelle, sans liste de bannis.
#[derive(Deserialize)]
struct CardV1 {
    id: ServerId,
    owner: UserId,
    nonce: [u8; 16],
    admins: Signed<AdminList>,
    settings: Signed<Settings>,
}

/// Plus grand numéro de révision accepté : une révision énorme (signée par
/// un admin malveillant) l'emporterait sinon pour toujours et figerait la
/// carte. Il en faudrait une par milliseconde pendant 35 ans pour l'atteindre.
const MAX_REVISION: u64 = 1 << 40;

fn next_revision(current: u64) -> Result<u64> {
    ensure!(current < MAX_REVISION, "révision au maximum : copie du serveur abîmée");
    Ok(current + 1)
}

fn derive_id(owner: &UserId, nonce: &[u8; 16]) -> ServerId {
    let mut input = owner.0.to_vec();
    input.extend_from_slice(nonce);
    let hash = digest(&SHA256, &input);
    ServerId(hash.as_ref()[..16].try_into().unwrap())
}

impl ServerCard {
    /// Nouveau serveur appartenant à `owner`, avec un salon texte et un vocal.
    pub fn create(owner: &Identity, name: &str) -> Self {
        let mut nonce = [0u8; 16];
        SystemRandom::new()
            .fill(&mut nonce)
            .expect("générateur aléatoire du système indisponible");
        let id = derive_id(&owner.id(), &nonce);
        let mut settings = Settings {
            server: id,
            revision: 1,
            name: name.into(),
            text_channels: Vec::new(),
            voice_channels: Vec::new(),
            history: HistoryPolicy::default(),
        };
        settings.add_text_channel("général");
        settings.add_voice_channel("Vocal");
        Self {
            id,
            owner: owner.id(),
            nonce,
            admins: Signed::new(
                AdminList {
                    server: id,
                    revision: 1,
                    admins: Vec::new(),
                },
                owner,
                DOMAIN_ADMINS,
            ),
            settings: Signed::new(settings, owner, DOMAIN_SETTINGS),
            bans: None,
            access: None,
        }
        .with_fresh_key(owner)
    }

    /// Nouveau serveur : une clé d'accès tirée au hasard, pas celle d'origine
    /// (qui se déduit de l'identifiant).
    fn with_fresh_key(mut self, owner: &Identity) -> Self {
        self.new_access_key(owner).expect("le créateur peut changer la clé");
        self
    }

    /// Relit une carte enregistrée, y compris aux formats d'avant la clé
    /// d'accès et d'avant les bannis.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if let Ok(card) = postcard::from_bytes::<ServerCard>(bytes) {
            return Some(card);
        }
        if let Ok(old) = postcard::from_bytes::<CardV2>(bytes) {
            return Some(Self {
                id: old.id,
                owner: old.owner,
                nonce: old.nonce,
                admins: old.admins,
                settings: old.settings,
                bans: old.bans,
                access: None,
            });
        }
        let old = postcard::from_bytes::<CardV1>(bytes).ok()?;
        Some(Self {
            id: old.id,
            owner: old.owner,
            nonce: old.nonce,
            admins: old.admins,
            settings: old.settings,
            bans: None,
            access: None,
        })
    }

    /// Clé d'accès en vigueur (celle du code d'invitation).
    pub fn access_key(&self) -> AccessKey {
        self.access
            .as_ref()
            .map_or_else(|| default_access_key(self.id), |a| a.body.key)
    }

    /// Tire une nouvelle clé d'accès (créateur ou admin) : l'ancien code
    /// d'invitation ne permet plus d'entrer ni de trouver l'hôte.
    pub fn new_access_key(&mut self, me: &Identity) -> Result<()> {
        if !self.can_edit(&me.id()) {
            bail!("seuls le créateur et les admins peuvent changer le code");
        }
        let mut key = [0u8; 16];
        SystemRandom::new()
            .fill(&mut key)
            .map_err(|_| anyhow::anyhow!("aléa indisponible"))?;
        let body = AccessList {
            server: self.id,
            revision: next_revision(self.access.as_ref().map_or(0, |a| a.body.revision))?,
            key,
        };
        self.access = Some(Signed::new(body, me, DOMAIN_ACCESS));
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.settings.body.name
    }

    pub fn settings(&self) -> &Settings {
        &self.settings.body
    }

    pub fn history(&self) -> HistoryPolicy {
        self.settings.body.history
    }

    pub fn role(&self, user: &UserId) -> Role {
        if *user == self.owner {
            Role::Owner
        } else if self.admins.body.admins.contains(user) {
            Role::Admin
        } else {
            Role::Member
        }
    }

    pub fn can_edit(&self, user: &UserId) -> bool {
        self.role(user) != Role::Member
    }

    pub fn banned(&self) -> &[UserId] {
        self.bans.as_ref().map_or(&[], |b| &b.body.banned)
    }

    /// Banni (le créateur et les admins ne le sont jamais).
    pub fn is_banned(&self, user: &UserId) -> bool {
        self.role(user) == Role::Member && self.banned().contains(user)
    }

    /// Bannit ou débannit `user` (créateur ou admin ; un admin se retire
    /// d'abord de la liste des admins pour pouvoir être banni).
    pub fn set_banned(&mut self, me: &Identity, user: UserId, on: bool) -> Result<()> {
        if !self.can_edit(&me.id()) {
            bail!("seuls le créateur et les admins peuvent bannir");
        }
        if on && self.role(&user) != Role::Member {
            bail!("le créateur et les admins ne peuvent pas être bannis");
        }
        let mut banned = self.banned().to_vec();
        banned.retain(|u| *u != user);
        if on {
            banned.push(user);
        }
        let body = BanList {
            server: self.id,
            revision: next_revision(self.bans.as_ref().map_or(0, |b| b.body.revision))?,
            banned,
        };
        self.bans = Some(Signed::new(body, me, DOMAIN_BANS));
        Ok(())
    }

    /// Vérifie toute la chaîne de confiance. À appeler sur toute carte reçue.
    pub fn verify(&self) -> Result<()> {
        ensure!(
            derive_id(&self.owner, &self.nonce) == self.id,
            "identifiant de serveur falsifié"
        );
        let admins = &self.admins;
        ensure!(
            admins.signer == self.owner && admins.body.server == self.id && admins.valid(DOMAIN_ADMINS),
            "liste des admins non signée par le créateur"
        );
        let settings = &self.settings;
        ensure!(
            settings.body.server == self.id && self.can_edit(&settings.signer) && settings.valid(DOMAIN_SETTINGS),
            "réglages non signés par le créateur ou un admin"
        );
        if let Some(bans) = &self.bans {
            ensure!(
                bans.body.server == self.id && self.can_edit(&bans.signer) && bans.valid(DOMAIN_BANS),
                "bannissements non signés par le créateur ou un admin"
            );
        }
        let revisions = [
            Some(self.admins.body.revision),
            Some(self.settings.body.revision),
            self.bans.as_ref().map(|b| b.body.revision),
            self.access.as_ref().map(|a| a.body.revision),
        ];
        ensure!(
            revisions.into_iter().flatten().all(|r| r <= MAX_REVISION),
            "numéro de révision invraisemblable"
        );
        if let Some(access) = &self.access {
            ensure!(
                access.body.server == self.id && self.can_edit(&access.signer) && access.valid(DOMAIN_ACCESS),
                "clé d'accès non signée par le créateur ou un admin"
            );
        }
        Ok(())
    }

    /// Nouveaux réglages, signés par `me` (créateur ou admin).
    pub fn edit(&mut self, me: &Identity, change: impl FnOnce(&mut Settings)) -> Result<()> {
        if !self.can_edit(&me.id()) {
            bail!("seuls le créateur et les admins peuvent modifier le serveur");
        }
        let mut body = self.settings.body.clone();
        change(&mut body);
        body.server = self.id;
        body.revision = next_revision(self.settings.body.revision)?;
        self.settings = Signed::new(body, me, DOMAIN_SETTINGS);
        Ok(())
    }

    /// Change la liste des admins (créateur uniquement).
    pub fn set_admins(&mut self, me: &Identity, admins: Vec<UserId>) -> Result<()> {
        if me.id() != self.owner {
            bail!("seul le créateur peut nommer ou retirer des admins");
        }
        let body = AdminList {
            server: self.id,
            revision: next_revision(self.admins.body.revision)?,
            admins,
        };
        self.admins = Signed::new(body, me, DOMAIN_ADMINS);
        // Des réglages signés par un admin retiré ne seraient plus valides :
        // le créateur les reprend à son compte.
        if !self.can_edit(&self.settings.signer) {
            self.edit(me, |_| {})?;
        }
        if let Some(bans) = &self.bans
            && !self.can_edit(&bans.signer)
        {
            let body = BanList {
                server: self.id,
                revision: next_revision(bans.body.revision)?,
                banned: bans.body.banned.clone(),
            };
            self.bans = Some(Signed::new(body, me, DOMAIN_BANS));
        }
        if let Some(access) = &self.access
            && !self.can_edit(&access.signer)
        {
            let body = AccessList {
                server: self.id,
                revision: next_revision(access.body.revision)?,
                key: access.body.key,
            };
            self.access = Some(Signed::new(body, me, DOMAIN_ACCESS));
        }
        Ok(())
    }

    /// Fusionne une autre copie du même serveur : chaque partie prend la
    /// révision la plus récente. Renvoie `true` si `self` a changé.
    pub fn merge(&mut self, other: &ServerCard) -> Result<bool> {
        ensure!(
            other.id == self.id && other.owner == self.owner,
            "ce n'est pas le même serveur"
        );
        other.verify()?;
        let mut merged = self.clone();
        if other.admins.body.revision > merged.admins.body.revision {
            merged.admins = other.admins.clone();
        }
        if other.settings.body.revision > merged.settings.body.revision {
            merged.settings = other.settings.clone();
        }
        let revision = |b: &Option<Signed<BanList>>| b.as_ref().map_or(0, |b| b.body.revision);
        if revision(&other.bans) > revision(&merged.bans) {
            merged.bans = other.bans.clone();
        }
        let revision = |a: &Option<Signed<AccessList>>| a.as_ref().map_or(0, |a| a.body.revision);
        if revision(&other.access) > revision(&merged.access) {
            merged.access = other.access.clone();
        }
        if merged.verify().is_err() {
            // Réglages signés par un admin retiré entre-temps : on garde la
            // copie dont la liste d'admins est la plus récente, entière.
            merged = if other.admins.body.revision > self.admins.body.revision {
                other.clone()
            } else {
                self.clone()
            };
        }
        let changed = merged != *self;
        *self = merged;
        Ok(changed)
    }
}

/// Message texte signé par son auteur : un hôte ne peut ni le modifier ni en
/// inventer, et l'historique reste vérifiable quand il change de machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub server: ServerId,
    pub channel: ChannelId,
    pub author_name: String,
    /// Horodatage de l'auteur, en millisecondes depuis l'époque Unix.
    pub at_ms: u64,
    /// Texte du message (ou nouveau texte, pour [`Kind::Edit`]).
    pub body: String,
    pub kind: Kind,
}

/// Identifiant d'un message : tiré de sa signature, donc impossible à
/// réutiliser pour un autre message.
pub type MessageId = [u8; 16];

/// Ce qu'apporte un message au salon : du texte, ou une action sur un message
/// précédent. Les actions circulent et se gardent comme les messages : un
/// membre qui arrive, ou un nouvel hôte, les rejoue.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Kind {
    /// Texte simple (le format des premières versions).
    #[default]
    Text,
    /// Texte en réponse à un message, avec des mentions ou des fichiers.
    Rich {
        reply_to: Option<MessageId>,
        mentions: Vec<UserId>,
        files: Vec<FileRef>,
    },
    /// Nouveau texte pour `target` (valable venant de son auteur seulement).
    Edit { target: MessageId },
    /// Retire `target` (son auteur, le créateur ou un admin).
    Delete { target: MessageId },
    /// Ajoute (`on`) ou retire une réaction à `target`.
    React { target: MessageId, emoji: String, on: bool },
}

impl Kind {
    /// Une action sur un message plutôt qu'un message à afficher.
    pub fn target(&self) -> Option<MessageId> {
        match self {
            Kind::Edit { target } | Kind::Delete { target } | Kind::React { target, .. } => Some(*target),
            Kind::Text | Kind::Rich { .. } => None,
        }
    }
}

/// Pièce jointe : le contenu circule à part, désigné par son empreinte (que la
/// signature du message couvre : un hôte ne peut pas l'échanger).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub name: String,
    pub size: u64,
    /// SHA-256 du contenu.
    pub hash: [u8; 32],
    /// Largeur et hauteur, si c'est une image à afficher dans le salon.
    pub image: Option<(u32, u32)>,
}

pub type SignedMessage = Signed<Message>;

/// Ce que signaient les premières versions : le message sans `kind`.
#[derive(Serialize)]
struct LegacyBody<'a> {
    server: ServerId,
    channel: ChannelId,
    author_name: &'a str,
    at_ms: u64,
    body: &'a str,
}

/// Message tel qu'enregistré par les premières versions.
#[derive(Deserialize)]
struct LegacyMessage {
    body: LegacyFields,
    signer: UserId,
    sig: Vec<u8>,
}

#[derive(Deserialize)]
struct LegacyFields {
    server: ServerId,
    channel: ChannelId,
    author_name: String,
    at_ms: u64,
    body: String,
}

impl Message {
    pub fn sign(self, me: &Identity) -> SignedMessage {
        let sig = me.sign(&self.signed_bytes());
        Signed {
            body: self,
            signer: me.id(),
            sig,
        }
    }

    /// Un texte simple se signe comme avant (les historiques déjà gardés
    /// restent valables) ; le reste, avec tout son contenu.
    fn signed_bytes(&self) -> Vec<u8> {
        match self.kind {
            Kind::Text => to_sign(
                DOMAIN_MESSAGE,
                &LegacyBody {
                    server: self.server,
                    channel: self.channel,
                    author_name: &self.author_name,
                    at_ms: self.at_ms,
                    body: &self.body,
                },
            ),
            _ => to_sign(DOMAIN_MESSAGE_2, self),
        }
    }
}

impl SignedMessage {
    pub fn verify(&self, server: ServerId) -> Result<()> {
        ensure!(self.body.server == server, "message d'un autre serveur");
        ensure!(self.body.body.chars().count() <= MAX_MESSAGE_CHARS, "message trop long");
        ensure!(self.body.author_name.chars().count() <= 64, "pseudo trop long");
        match &self.body.kind {
            Kind::Rich { mentions, files, .. } => {
                ensure!(mentions.len() <= MAX_MENTIONS, "trop de mentions");
                ensure!(files.len() <= MAX_FILES, "trop de pièces jointes");
                for f in files {
                    ensure!(f.size <= MAX_FILE_BYTES, "pièce jointe trop grosse");
                    ensure!(f.name.chars().count() <= 200, "nom de fichier trop long");
                }
            }
            Kind::React { emoji, .. } => ensure!(!emoji.is_empty() && emoji.len() <= 32, "réaction invalide"),
            Kind::Text | Kind::Edit { .. } | Kind::Delete { .. } => {}
        }
        ensure!(
            self.signer.verify(&self.body.signed_bytes(), &self.sig),
            "signature du message invalide"
        );
        Ok(())
    }

    pub fn author(&self) -> UserId {
        self.signer
    }

    /// Daté de trop loin dans le futur (voir [`MAX_AHEAD_MS`]).
    pub fn from_future(&self) -> bool {
        self.body.at_ms > now_ms().saturating_add(MAX_AHEAD_MS)
    }

    pub fn id(&self) -> MessageId {
        digest(&SHA256, &self.sig).as_ref()[..16].try_into().unwrap()
    }

    /// Place dans le fil, la même chez tout le monde quel que soit l'ordre
    /// d'arrivée : l'heure de l'auteur, puis l'identifiant pour départager deux
    /// messages de la même milliseconde.
    pub fn order(&self) -> (u64, MessageId) {
        (self.body.at_ms, self.id())
    }

    /// Relit et vérifie un message enregistré, y compris au format des
    /// premières versions (un ancien enregistrement peut se lire de travers au
    /// nouveau format : seule la signature départage).
    pub fn decode(bytes: &[u8], server: ServerId) -> Option<Self> {
        if let Ok(m) = postcard::from_bytes::<SignedMessage>(bytes)
            && m.verify(server).is_ok()
        {
            return Some(m);
        }
        let old = postcard::from_bytes::<LegacyMessage>(bytes).ok()?;
        let f = old.body;
        Some(Signed {
            body: Message {
                server: f.server,
                channel: f.channel,
                author_name: f.author_name,
                at_ms: f.at_ms,
                body: f.body,
                kind: Kind::Text,
            },
            signer: old.signer,
            sig: old.sig,
        })
        .filter(|m| m.verify(server).is_ok())
    }
}

/// Retire les caractères qui inversent le sens d'écriture : sans eux,
/// « photo\u{202E}gpj.exe » s'afficherait « photoexe.jpg ».
pub fn plain_text(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .collect()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_et_verification() {
        let owner = Identity::generate();
        let card = ServerCard::create(&owner, "Les potes");
        card.verify().unwrap();
        assert_eq!(card.role(&owner.id()), Role::Owner);
        assert_eq!(card.settings().text_channels.len(), 1);
        assert_eq!(card.settings().voice_channels.len(), 1);
    }

    #[test]
    fn cle_d_acces() {
        let owner = Identity::generate();
        let admin = Identity::generate();
        let intrus = Identity::generate();
        let mut card = ServerCard::create(&owner, "s");
        // Un nouveau serveur a une clé tirée au hasard, pas celle d'origine.
        assert_ne!(card.access_key(), default_access_key(card.id));
        let first = card.access_key();

        assert!(card.new_access_key(&intrus).is_err());
        card.set_admins(&owner, vec![admin.id()]).unwrap();
        let mut copy = card.clone();
        copy.new_access_key(&admin).unwrap();
        copy.verify().unwrap();
        assert_ne!(copy.access_key(), first);

        // La plus récente l'emporte à la fusion.
        assert!(card.merge(&copy).unwrap());
        assert_eq!(card.access_key(), copy.access_key());

        // Clé forgée par un simple membre : refusée.
        let mut forged = card.clone();
        forged.access = Some(Signed::new(
            AccessList {
                server: card.id,
                revision: 99,
                key: [0; 16],
            },
            &intrus,
            DOMAIN_ACCESS,
        ));
        assert!(forged.verify().is_err());
    }

    #[test]
    fn carte_d_avant_la_cle_relue() {
        let owner = Identity::generate();
        let mut card = ServerCard::create(&owner, "s");
        card.access = None;
        #[derive(Serialize)]
        struct V2<'a> {
            id: ServerId,
            owner: UserId,
            nonce: [u8; 16],
            admins: &'a Signed<AdminList>,
            settings: &'a Signed<Settings>,
            bans: &'a Option<Signed<BanList>>,
        }
        let bytes = postcard::to_stdvec(&V2 {
            id: card.id,
            owner: card.owner,
            nonce: card.nonce,
            admins: &card.admins,
            settings: &card.settings,
            bans: &card.bans,
        })
        .unwrap();
        let read = ServerCard::decode(&bytes).unwrap();
        assert_eq!(read, card);
        // Sans clé changée, c'est la clé d'origine : les anciens codes marchent.
        assert_eq!(read.access_key(), default_access_key(card.id));
    }

    #[test]
    fn revision_enorme_refusee() {
        let owner = Identity::generate();
        let mut card = ServerCard::create(&owner, "s");
        let mut body = card.settings.body.clone();
        body.revision = u64::MAX;
        card.settings = Signed::new(body, &owner, DOMAIN_SETTINGS);
        assert!(card.verify().is_err());
    }

    #[test]
    fn un_membre_ne_peut_pas_modifier() {
        let owner = Identity::generate();
        let intrus = Identity::generate();
        let mut card = ServerCard::create(&owner, "s");
        assert!(card.edit(&intrus, |s| s.name = "piraté".into()).is_err());

        // Même en forgeant la signature à la main, la vérification refuse.
        let mut forged = card.clone();
        let mut body = forged.settings.body.clone();
        body.revision += 1;
        body.name = "piraté".into();
        forged.settings = Signed::new(body, &intrus, DOMAIN_SETTINGS);
        assert!(forged.verify().is_err());
        assert!(card.merge(&forged).is_err());
        assert_eq!(card.name(), "s");
    }

    #[test]
    fn admin_modifie_puis_est_retire() {
        let owner = Identity::generate();
        let admin = Identity::generate();
        let mut card = ServerCard::create(&owner, "s");
        card.set_admins(&owner, vec![admin.id()]).unwrap();
        assert!(card.set_admins(&admin, vec![]).is_err());

        let mut chez_admin = card.clone();
        chez_admin
            .edit(&admin, |s| {
                s.add_voice_channel("Jeux");
                s.history.keep = false;
            })
            .unwrap();
        chez_admin.verify().unwrap();
        assert!(card.merge(&chez_admin).unwrap());
        assert_eq!(card.settings().voice_channels.len(), 2);
        assert!(!card.history().keep);

        card.set_admins(&owner, vec![]).unwrap();
        card.verify().unwrap();
        assert_eq!(card.settings.signer, owner.id());
        // L'ancienne copie de l'admin ne reprend pas le dessus.
        assert!(!card.merge(&chez_admin).unwrap());
        assert_eq!(card.role(&admin.id()), Role::Member);
    }

    #[test]
    fn identifiant_lie_au_createur() {
        let owner = Identity::generate();
        let mut card = ServerCard::create(&owner, "s");
        card.owner = Identity::generate().id();
        assert!(card.verify().is_err());
    }

    #[test]
    fn messages_signes() {
        let me = Identity::generate();
        let card = ServerCard::create(&me, "s");
        let msg = Message {
            server: card.id,
            channel: 1,
            author_name: "moi".into(),
            at_ms: now_ms(),
            body: "salut".into(),
            kind: Kind::Text,
        }
        .sign(&me);
        msg.verify(card.id).unwrap();
        let mut altered = msg.clone();
        altered.body.body = "au revoir".into();
        assert!(altered.verify(card.id).is_err());
        assert!(msg.verify(ServerId([0; 16])).is_err());
    }

    #[test]
    fn anciens_messages_et_cartes_relus() {
        // Enregistrés par une version d'avant `kind` et les bannis.
        #[derive(Serialize)]
        struct OldBody {
            server: ServerId,
            channel: ChannelId,
            author_name: String,
            at_ms: u64,
            body: String,
        }
        #[derive(Serialize)]
        struct OldMessage {
            body: OldBody,
            signer: UserId,
            sig: Vec<u8>,
        }
        #[derive(Serialize)]
        struct OldCard<'a> {
            id: ServerId,
            owner: UserId,
            nonce: [u8; 16],
            admins: &'a Signed<AdminList>,
            settings: &'a Signed<Settings>,
        }
        let me = Identity::generate();
        let card = ServerCard::create(&me, "s");
        let msg = Message {
            server: card.id,
            channel: 1,
            author_name: "moi".into(),
            at_ms: 42,
            body: "d'avant".into(),
            kind: Kind::Text,
        }
        .sign(&me);
        let old = OldMessage {
            body: OldBody {
                server: card.id,
                channel: 1,
                author_name: "moi".into(),
                at_ms: 42,
                body: "d'avant".into(),
            },
            signer: me.id(),
            sig: msg.sig.clone(),
        };
        let bytes = postcard::to_stdvec(&old).unwrap();
        assert_eq!(SignedMessage::decode(&bytes, card.id), Some(msg.clone()));
        let bytes = postcard::to_stdvec(&msg).unwrap();
        assert_eq!(SignedMessage::decode(&bytes, card.id), Some(msg));

        let old = OldCard {
            id: card.id,
            owner: card.owner,
            nonce: card.nonce,
            admins: &card.admins,
            settings: &card.settings,
        };
        let decoded = ServerCard::decode(&postcard::to_stdvec(&old).unwrap()).unwrap();
        assert_eq!(decoded, ServerCard { access: None, ..card });
        decoded.verify().unwrap();
    }

    #[test]
    fn messages_enrichis_et_actions_signes() {
        let me = Identity::generate();
        let card = ServerCard::create(&me, "s");
        let base = |body: &str, kind| Message {
            server: card.id,
            channel: 1,
            author_name: "moi".into(),
            at_ms: now_ms(),
            body: body.into(),
            kind,
        };
        let first = base("salut", Kind::Text).sign(&me);
        let reply = base(
            "re",
            Kind::Rich {
                reply_to: Some(first.id()),
                mentions: vec![me.id()],
                files: vec![FileRef {
                    name: "chat.png".into(),
                    size: 10,
                    hash: [1; 32],
                    image: Some((4, 4)),
                }],
            },
        )
        .sign(&me);
        reply.verify(card.id).unwrap();
        assert_ne!(first.id(), reply.id());
        // La pièce jointe est couverte par la signature.
        let mut swapped = reply.clone();
        if let Kind::Rich { files, .. } = &mut swapped.body.kind {
            files[0].hash = [2; 32];
        }
        assert!(swapped.verify(card.id).is_err());
        let react = base(
            "",
            Kind::React {
                target: first.id(),
                emoji: "👍".into(),
                on: true,
            },
        )
        .sign(&me);
        react.verify(card.id).unwrap();
        assert_eq!(react.body.kind.target(), Some(first.id()));
        let too_big = base(
            "",
            Kind::React {
                target: first.id(),
                emoji: "x".repeat(64),
                on: true,
            },
        )
        .sign(&me);
        assert!(too_big.verify(card.id).is_err());
    }

    #[test]
    fn bannir_un_membre() {
        let owner = Identity::generate();
        let admin = Identity::generate();
        let membre = Identity::generate();
        let mut card = ServerCard::create(&owner, "s");
        card.set_admins(&owner, vec![admin.id()]).unwrap();
        assert!(card.set_banned(&membre, owner.id(), true).is_err());
        assert!(card.set_banned(&admin, owner.id(), true).is_err());
        assert!(card.set_banned(&admin, admin.id(), true).is_err());
        card.set_banned(&admin, membre.id(), true).unwrap();
        card.verify().unwrap();
        assert!(card.is_banned(&membre.id()));

        // Une vieille copie sans le bannissement ne l'annule pas.
        let mut ancienne = ServerCard::create(&owner, "s");
        ancienne.clone_from(&card);
        ancienne.bans = None;
        assert!(!card.merge(&ancienne).unwrap());
        assert!(card.is_banned(&membre.id()));

        // L'admin est retiré : le créateur reprend la liste à son compte.
        card.set_admins(&owner, vec![]).unwrap();
        card.verify().unwrap();
        assert!(card.is_banned(&membre.id()));
        assert_eq!(card.bans.as_ref().unwrap().signer, owner.id());

        // Bannissement forgé par un simple membre : refusé.
        let mut forged = card.clone();
        forged.bans = Some(Signed::new(
            BanList {
                server: card.id,
                revision: 99,
                banned: vec![],
            },
            &membre,
            DOMAIN_BANS,
        ));
        assert!(forged.verify().is_err());

        card.set_banned(&owner, membre.id(), false).unwrap();
        assert!(!card.is_banned(&membre.id()));
    }

    #[test]
    fn sens_d_ecriture_neutralise() {
        assert_eq!(plain_text("photo\u{202E}gpj.exe"), "photogpj.exe");
        assert_eq!(plain_text("Lwrsk\u{2066}"), "Lwrsk");
        assert_eq!(plain_text("مرحبا é"), "مرحبا é");
    }
}
