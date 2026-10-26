//! État d'un serveur vu par un membre connecté : salons, messages, vocal,
//! présents. Partagé par les deux interfaces, et responsable de la copie
//! locale (carte + historique) qui permet de ré-héberger le serveur plus tard.
//!
//! Les actions sur un message (modifier, supprimer, réagir) sont des messages
//! comme les autres : la vue les range à part et les applique à l'affichage,
//! en vérifiant que leur auteur en avait le droit.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use tracing::warn;

use super::{Client, Event};
use crate::core::{
    card::{ChannelId, Kind, MessageId, ServerCard, SignedMessage, plain_text},
    identity::UserId,
    protocol::{Control, PeerId, PeerInfo, VoiceRoom},
    store::{HistoryLog, Store},
};

/// Mots qui mentionnent tout le salon.
pub const EVERYONE: [&str; 2] = ["@tous", "@everyone"];

pub struct ServerView {
    pub me: UserId,
    pub my_peer: PeerId,
    /// Qui héberge le serveur en ce moment (identité).
    pub host_user: Option<UserId>,
    pub card: ServerCard,
    pub peers: BTreeMap<PeerId, PeerInfo>,
    /// Salons vocaux occupés.
    pub voice: BTreeMap<ChannelId, VoiceRoom>,
    /// Messages affichés (y compris quand l'historique n'est pas gardé).
    messages: Vec<SignedMessage>,
    /// Identifiant de chaque message, dans le même ordre (un identifiant se
    /// calcule par empreinte : pas à chaque image de l'interface).
    ids: Vec<MessageId>,
    seen: HashSet<Vec<u8>>,
    by_id: HashMap<MessageId, usize>,
    /// Actions reçues, rangées par message visé (qui peut arriver après).
    actions: HashMap<MessageId, Vec<SignedMessage>>,
    /// Pseudo (en minuscules) → identités qui l'ont utilisé : deux personnes
    /// sous le même pseudo sont distinguées à l'affichage.
    names: HashMap<String, BTreeSet<UserId>>,
    store: Option<Store>,
    log: HistoryLog,
}

impl ServerView {
    /// Prépare la vue juste après la connexion. Si la copie locale du serveur
    /// est plus récente que celle de l'hôte, elle lui est renvoyée pour que
    /// tout le monde converge.
    pub fn new(client: &Client, my_name: &str, store: Option<Store>) -> Self {
        let mut card = client.card.clone();
        if let Some(local) = store.as_ref().and_then(|s| s.card(card.id)) {
            let mut newest = local;
            if newest.merge(&card).is_ok() && newest != card {
                let _ = client.update_card(&newest);
                card = newest;
            }
        }
        if let Some(s) = &store {
            match s.save_card(&card) {
                Ok(kept) => card = kept,
                Err(e) => warn!("copie locale du serveur : {e:#}"),
            }
        }
        let mut log = match &store {
            Some(s) => s.history(&card),
            None => HistoryLog::in_memory(card.id, card.history()),
        };
        log.set_moderators(&card);

        let mut peers: BTreeMap<_, _> = client.initial_peers.iter().map(|p| (p.id, p.clone())).collect();
        peers.insert(
            client.peer_id,
            PeerInfo {
                id: client.peer_id,
                name: my_name.into(),
                user: client.identity().id(),
            },
        );
        let mut view = Self {
            me: client.identity().id(),
            my_peer: client.peer_id,
            host_user: client.host_user,
            peers,
            voice: client.initial_voice.iter().map(|r| (r.channel, r.clone())).collect(),
            messages: Vec::new(),
            ids: Vec::new(),
            seen: HashSet::new(),
            by_id: HashMap::new(),
            actions: HashMap::new(),
            names: HashMap::new(),
            store,
            log,
            card,
        };
        for p in view.peers.values().cloned().collect::<Vec<_>>() {
            view.see_name(p.user, &p.name);
        }
        let local: Vec<_> = view.log.messages().to_vec();
        view.show(local);
        view
    }

    /// Messages visibles d'un salon (sans ceux qui ont été supprimés).
    pub fn messages(&self, channel: ChannelId) -> impl Iterator<Item = &SignedMessage> {
        self.entries(channel).map(|(_, m)| m)
    }

    /// Pareil, avec l'identifiant de chaque message.
    pub fn entries(&self, channel: ChannelId) -> impl Iterator<Item = (MessageId, &SignedMessage)> {
        self.ids
            .iter()
            .zip(&self.messages)
            .filter(move |(id, m)| m.body.channel == channel && !self.deleted(id, m))
            .map(|(id, m)| (*id, m))
    }

    /// Un message visible, par son identifiant.
    pub fn message(&self, id: &MessageId) -> Option<&SignedMessage> {
        let m = self.messages.get(*self.by_id.get(id)?)?;
        (!self.deleted(id, m)).then_some(m)
    }

    fn actions_on(&self, m: &SignedMessage) -> impl Iterator<Item = &SignedMessage> {
        self.actions.get(&m.id()).into_iter().flatten()
    }

    /// Supprimé par son auteur, le créateur ou un admin.
    pub fn is_deleted(&self, m: &SignedMessage) -> bool {
        self.deleted(&m.id(), m)
    }

    fn deleted(&self, id: &MessageId, m: &SignedMessage) -> bool {
        self.actions.get(id).into_iter().flatten().any(|a| {
            matches!(a.body.kind, Kind::Delete { .. }) && (a.signer == m.signer || self.card.can_edit(&a.signer))
        })
    }

    fn last_edit(&self, m: &SignedMessage) -> Option<&SignedMessage> {
        self.actions_on(m)
            .filter(|a| matches!(a.body.kind, Kind::Edit { .. }) && a.signer == m.signer)
            .max_by_key(|a| (a.body.at_ms, a.sig.clone()))
    }

    /// Texte à afficher : la dernière modification de l'auteur, sinon l'original.
    pub fn text<'a>(&'a self, m: &'a SignedMessage) -> &'a str {
        self.last_edit(m).map_or(&m.body.body, |e| &e.body.body)
    }

    pub fn edited(&self, m: &SignedMessage) -> bool {
        self.last_edit(m).is_some()
    }

    /// Réactions : emoji, nombre de personnes, et si j'en suis.
    pub fn reactions(&self, m: &SignedMessage) -> Vec<(String, usize, bool)> {
        // Dernier geste de chacun pour chaque emoji.
        let mut last: BTreeMap<(&str, UserId), (u64, bool)> = BTreeMap::new();
        for a in self.actions_on(m) {
            if let Kind::React { emoji, on, .. } = &a.body.kind {
                let e = last.entry((emoji.as_str(), a.signer)).or_insert((0, false));
                if a.body.at_ms >= e.0 {
                    *e = (a.body.at_ms, *on);
                }
            }
        }
        let mut out: Vec<(String, usize, bool)> = Vec::new();
        for ((emoji, user), (_, on)) in last {
            if !on {
                continue;
            }
            match out.iter_mut().find(|(e, ..)| e == emoji) {
                Some(r) => {
                    r.1 += 1;
                    r.2 |= user == self.me;
                }
                None => out.push((emoji.to_string(), 1, user == self.me)),
            }
        }
        out
    }

    /// Ai-je réagi avec cet emoji ?
    pub fn reacted(&self, m: &SignedMessage, emoji: &str) -> bool {
        self.reactions(m).iter().any(|(e, _, mine)| e == emoji && *mine)
    }

    /// Le message me mentionne (moi, ou tout le salon).
    pub fn mentions_me(&self, m: &SignedMessage) -> bool {
        if m.signer == self.me {
            return false;
        }
        let named = matches!(&m.body.kind, Kind::Rich { mentions, .. } if mentions.contains(&self.me));
        named || {
            let text = self.text(m).to_lowercase();
            EVERYONE.iter().any(|w| text.contains(w))
        }
    }

    pub fn can_delete(&self, m: &SignedMessage) -> bool {
        m.signer == self.me || self.card.can_edit(&self.me)
    }

    /// Mon dernier message dans ce salon (flèche du haut : le modifier).
    pub fn last_own(&self, channel: ChannelId) -> Option<&SignedMessage> {
        self.messages(channel).filter(|m| m.signer == self.me).last()
    }

    /// Pseudo affiché : suivi du début de l'identité quand quelqu'un d'autre
    /// utilise (ou a utilisé) le même, pour qu'on ne puisse pas se faire
    /// passer pour un autre.
    pub fn display_name(&self, user: &UserId, name: &str) -> String {
        let name = &plain_text(name);
        let taken = self
            .names
            .get(&name.trim().to_lowercase())
            .is_some_and(|users| users.len() > 1);
        if taken {
            format!("{name} #{}", &user.short()[..4])
        } else {
            name.to_string()
        }
    }

    /// Nom affiché de l'auteur d'un message.
    pub fn author(&self, m: &SignedMessage) -> String {
        self.display_name(&m.signer, &m.body.author_name)
    }

    fn see_name(&mut self, user: UserId, name: &str) {
        self.names
            .entry(plain_text(name).trim().to_lowercase())
            .or_default()
            .insert(user);
    }

    pub fn can_edit(&self) -> bool {
        self.card.can_edit(&self.me)
    }

    /// Salon vocal où je suis, s'il y en a un.
    pub fn my_voice(&self) -> Option<ChannelId> {
        self.voice
            .values()
            .find(|r| r.members.contains(&self.my_peer))
            .map(|r| r.channel)
    }

    /// Le membre connecté qui héberge le serveur.
    pub fn host_peer(&self) -> Option<PeerId> {
        let host = self.host_user?;
        self.peers.values().find(|p| p.user == host).map(|p| p.id)
    }

    pub fn peer_name(&self, id: PeerId) -> String {
        self.peers
            .get(&id)
            .map_or_else(|| format!("#{id}"), |p| self.display_name(&p.user, &p.name))
    }

    /// Message que je viens d'envoyer.
    pub fn push_own(&mut self, msg: SignedMessage) {
        self.record(msg.clone());
        self.show(vec![msg]);
    }

    /// Applique un événement réseau. Renvoie une ligne à afficher, le cas échéant.
    pub fn apply(&mut self, event: &Event) -> Option<String> {
        match event {
            Event::Text { msg, .. } => {
                self.record(msg.clone());
                self.show(vec![msg.clone()]);
                None
            }
            Event::History(batch) => {
                for m in batch {
                    self.record(m.clone());
                }
                self.show(batch.clone());
                None
            }
            Event::CardUpdated(card) => {
                let before = self.card.clone();
                match self.card.merge(card) {
                    Ok(true) => {}
                    Ok(false) => return None,
                    Err(e) => {
                        warn!("mise à jour du serveur ignorée : {e:#}");
                        return None;
                    }
                }
                if let Some(s) = &self.store
                    && let Err(e) = s.save_card(&self.card)
                {
                    warn!("copie locale du serveur : {e:#}");
                }
                let settings = self.card.settings().clone();
                self.log.apply_card(&self.card);
                self.messages.retain(|m| settings.is_text_channel(m.body.channel));
                self.reindex();
                self.voice.retain(|c, _| settings.is_voice_channel(*c));
                Some(describe_change(&before, &self.card))
            }
            Event::Voice(room) => {
                if room.members.is_empty() {
                    self.voice.remove(&room.channel);
                } else {
                    self.voice.insert(room.channel, room.clone());
                }
                None
            }
            Event::PeerJoined(p) => {
                self.see_name(p.user, &p.name);
                self.peers.insert(p.id, p.clone());
                Some(format!("{} a rejoint le serveur", self.peer_name(p.id)))
            }
            Event::PeerLeft { peer_id } => {
                let name = self.peer_name(*peer_id);
                self.peers.remove(peer_id).map(|_| format!("{name} est parti"))
            }
            Event::SlowDown => Some("Doucement : ton dernier message n'est pas parti, attends un peu.".into()),
            Event::Control(Control::Error { reason }) => Some(reason.clone()),
            Event::Pong { .. }
            | Event::Control(_)
            | Event::Succession(_)
            | Event::HostLeaving
            | Event::Moved
            | Event::Vote { .. }
            | Event::VoteResult { .. }
            | Event::HistoryEnd
            | Event::Kicked { .. }
            | Event::FileWanted { .. }
            | Event::Disconnected { .. } => None,
        }
    }

    fn record(&mut self, msg: SignedMessage) {
        self.log.push(msg);
    }

    fn show(&mut self, batch: Vec<SignedMessage>) {
        let mut added = false;
        for m in batch {
            if !self.card.settings().is_text_channel(m.body.channel)
                || m.from_future()
                || !self.seen.insert(m.sig.clone())
            {
                continue;
            }
            self.see_name(m.signer, &m.body.author_name);
            match m.body.kind.target() {
                Some(target) => self.actions.entry(target).or_default().push(m),
                None => {
                    self.messages.push(m);
                    added = true;
                }
            }
        }
        if added {
            self.messages.sort_by_cached_key(|m| m.order());
            self.reindex();
        }
    }

    fn reindex(&mut self) {
        self.ids = self.messages.iter().map(SignedMessage::id).collect();
        self.by_id = self.ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    }
}

fn describe_change(before: &ServerCard, after: &ServerCard) -> String {
    let (b, a) = (before.settings(), after.settings());
    if b.history.keep != a.history.keep {
        return if a.history.keep {
            "L'historique des messages est maintenant gardé.".into()
        } else {
            "L'historique est désactivé : les copies sont effacées chez tout le monde.".into()
        };
    }
    if before.admins.body.admins != after.admins.body.admins {
        return "La liste des admins a changé.".into();
    }
    if before.banned() != after.banned() {
        return if after.banned().len() > before.banned().len() {
            "Un membre a été banni du serveur.".into()
        } else {
            "Un membre a été débanni.".into()
        };
    }
    if b.name != a.name {
        return format!("Le serveur s'appelle maintenant « {} ».", a.name);
    }
    "Le serveur a été mis à jour.".into()
}
