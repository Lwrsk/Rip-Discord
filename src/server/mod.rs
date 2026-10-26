//! Mode hôte : relais central de la session (texte, voix, écrans, pièces
//! jointes). Chaque client ouvre un flux de contrôle ; l'hôte relaie. Quand il
//! part, un membre prend la relève (voir [`migration`]).

pub mod announce;
pub mod migration;
pub mod upnp;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use quinn::{Connection, Endpoint};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::client::station::Station;
use crate::core::{
    ALPN, VERSION,
    card::{AccessKey, ChannelId, Kind, Role, ServerCard, ServerId, SignedMessage, plain_text},
    files::{self, FileHash, FileRequest},
    framing::{read_packet, write_packet},
    identity::UserId,
    limit::Bucket,
    nat,
    protocol::{
        CertFingerprint, Chat, Contact, Control, PROTOCOL_VERSION, Packet, PeerId, PeerInfo, RelayOffer, VoiceRoom,
        history_batches,
    },
    screen,
    store::{HistoryLog, Store},
    tls::{self, HostCert, Side},
    voice,
};

/// Identifiant réservé à l'hôte lui-même (le relais, pas son client local).
pub const HOST_ID: PeerId = 0;

struct Peer {
    info: PeerInfo,
    tx: mpsc::Sender<Packet>,
    conn: Connection,
    /// De quoi le joindre s'il doit prendre la relève.
    offer: Option<RelayOffer>,
    /// Images de l'écran qu'il regarde, relayées par l'hôte.
    screen: Arc<screen::Lane>,
    /// Images des webcams qu'il reçoit par l'hôte, une file par webcam.
    cameras: HashMap<PeerId, Arc<screen::Lane>>,
    /// Anti-spam : messages, puis réactions et modifications.
    limits: (Bucket, Bucket),
    /// Messages déjà repris de sa copie de l'historique (borné).
    backfilled: usize,
    /// Octets de pièces jointes reçus de lui récemment, et depuis quand.
    uploads: (u64, Instant),
    /// Dernière fois qu'il a demandé à devenir l'hôte.
    asked_host: Option<Instant>,
}

/// Paquets en attente vers un pair. Un pair qui ne lit plus rien est
/// déconnecté au lieu de faire grossir la mémoire de l'hôte.
const OUTBOX: usize = 1024;
/// Temps laissé à une connexion pour se présenter (`Hello`).
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Connexions ouvertes en même temps, au plus (largement de quoi accueillir
/// tout un groupe d'amis).
const MAX_CONNECTIONS: usize = 256;
/// Délai entre deux demandes « Devenir l'hôte » d'un même membre.
const VOTE_COOLDOWN: Duration = Duration::from_secs(60);

/// Messages qu'un membre peut faire reprendre de sa copie, par connexion.
const MAX_BACKFILL: usize = 20_000;
/// Pièces jointes qu'un membre peut envoyer par fenêtre de 10 minutes.
const UPLOAD_BUDGET: u64 = 200 * 1024 * 1024;
/// Pièces jointes gardées en mémoire par un hôte sans dossier (tests).
const MEMORY_FILES: usize = 64 * 1024 * 1024;

impl Peer {
    fn push(&self, packet: Packet) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(packet) {
            warn!("{} ne suit plus : déconnecté", self.info.name);
            self.conn.close(0u32.into(), b"trop de retard");
        }
    }

    /// Prévient puis déconnecte (expulsion, bannissement).
    fn kick(&self, reason: String) {
        self.push(Control::Kicked { reason }.into());
        let conn = self.conn.clone();
        tokio::spawn(async move {
            // Laisse partir le message avant de couper.
            tokio::time::sleep(Duration::from_millis(300)).await;
            conn.close(0u32.into(), b"expulse");
        });
    }
}

/// Le serveur hébergé : sa carte, son historique, ses salons vocaux.
struct Space {
    card: ServerCard,
    history: HistoryLog,
    /// Membres de chaque salon vocal occupé, dans l'ordre d'arrivée.
    voice: BTreeMap<ChannelId, Vec<PeerId>>,
    /// Participants qui partagent leur écran.
    sharing: BTreeSet<PeerId>,
    /// Spectateur → écran regardé.
    watching: HashMap<PeerId, PeerId>,
    /// Participants dont la webcam est allumée.
    cameras: BTreeSet<PeerId>,
    /// Membre → webcams qu'il reçoit par l'hôte (pas de connexion directe).
    camera_relay: HashMap<PeerId, BTreeSet<PeerId>>,
    /// Micro / son coupé (affiché aux autres).
    muted: BTreeSet<PeerId>,
    deafened: BTreeSet<PeerId>,
    store: Option<Store>,
    /// Pièces jointes reçues, sans dossier où les ranger.
    files: HashMap<FileHash, Arc<Vec<u8>>>,
    /// Téléchargements qui attendent qu'un membre envoie le fichier.
    wanted: HashMap<FileHash, Vec<tokio::sync::oneshot::Sender<Arc<Vec<u8>>>>>,
}

impl Space {
    /// Dossier où ranger les pièces jointes (lu et écrit hors du verrou : un
    /// fichier de 25 Mo ne doit pas bloquer le relais de la voix).
    fn file_store(&self) -> Option<(Store, ServerId)> {
        Some((self.store.clone()?, self.card.id))
    }

    /// Garde en mémoire un fichier reçu (hôte sans dossier) et le remet à
    /// ceux qui l'attendaient.
    fn keep_file(&mut self, hash: FileHash, bytes: Arc<Vec<u8>>) {
        if self.store.is_none() {
            let used: usize = self.files.values().map(|f| f.len()).sum();
            if used + bytes.len() <= MEMORY_FILES {
                self.files.insert(hash, bytes.clone());
            }
        }
        for tx in self.wanted.remove(&hash).unwrap_or_default() {
            let _ = tx.send(bytes.clone());
        }
    }

    fn room(&self, channel: ChannelId) -> VoiceRoom {
        let members = self.voice.get(&channel).cloned().unwrap_or_default();
        VoiceRoom {
            channel,
            host: migration::elect_voice_host(&members),
            sharing: members.iter().copied().filter(|m| self.sharing.contains(m)).collect(),
            cameras: members.iter().copied().filter(|m| self.cameras.contains(m)).collect(),
            muted: members.iter().copied().filter(|m| self.muted.contains(m)).collect(),
            deafened: members.iter().copied().filter(|m| self.deafened.contains(m)).collect(),
            members,
        }
    }

    fn room_of(&self, id: PeerId) -> Option<ChannelId> {
        self.voice.iter().find(|(_, m)| m.contains(&id)).map(|(c, _)| *c)
    }

    /// Ceux qui regardent l'écran de `id` par l'hôte (dans son salon vocal).
    fn viewers(&self, id: PeerId) -> Vec<PeerId> {
        if !self.sharing.contains(&id) {
            return Vec::new();
        }
        let Some(room) = self.voice.values().find(|m| m.contains(&id)) else {
            return Vec::new();
        };
        self.watching
            .iter()
            .filter(|(v, s)| **s == id && room.contains(v))
            .map(|(v, _)| *v)
            .collect()
    }

    /// Ceux à qui l'hôte relaie la webcam de `id` (dans son salon vocal).
    fn camera_viewers(&self, id: PeerId) -> Vec<PeerId> {
        if !self.cameras.contains(&id) {
            return Vec::new();
        }
        let Some(room) = self.voice.values().find(|m| m.contains(&id)) else {
            return Vec::new();
        };
        room.iter()
            .copied()
            .filter(|v| *v != id && self.camera_relay.get(v).is_some_and(|s| s.contains(&id)))
            .collect()
    }

    /// Webcam de `id` éteinte : plus personne ne la reçoit par l'hôte.
    fn stop_camera(&mut self, id: PeerId) -> bool {
        for sources in self.camera_relay.values_mut() {
            sources.remove(&id);
        }
        self.cameras.remove(&id)
    }

    /// `id` ne partage plus : ceux qui le regardaient arrêtent aussi (sinon
    /// ils recevraient son prochain partage, même depuis un autre salon).
    fn stop_sharing(&mut self, id: PeerId) -> bool {
        self.watching.retain(|_, s| *s != id);
        self.sharing.remove(&id)
    }

    fn forget_status(&mut self, id: PeerId) {
        self.muted.remove(&id);
        self.deafened.remove(&id);
    }

    fn rooms(&self) -> Vec<VoiceRoom> {
        self.voice.keys().map(|c| self.room(*c)).collect()
    }

    /// Retire `id` de tous les salons vocaux ; renvoie les salons modifiés.
    fn leave_voice(&mut self, id: PeerId) -> Vec<VoiceRoom> {
        self.stop_sharing(id);
        self.watching.remove(&id);
        self.stop_camera(id);
        self.camera_relay.remove(&id);
        let mut changed = Vec::new();
        for (channel, members) in self.voice.iter_mut() {
            if let Some(pos) = members.iter().position(|m| *m == id) {
                members.remove(pos);
                changed.push(*channel);
            }
        }
        self.voice.retain(|_, m| !m.is_empty());
        changed.into_iter().map(|c| self.room(c)).collect()
    }

    /// Adopte une version plus récente de la carte et en tire les conséquences
    /// (historique, salons supprimés). Renvoie les salons vocaux vidés.
    fn apply_card(&mut self, card: &ServerCard) -> Result<Option<Vec<VoiceRoom>>> {
        if !self.card.merge(card)? {
            return Ok(None);
        }
        let settings = self.card.settings().clone();
        self.history.apply_card(&self.card);
        let closed: Vec<ChannelId> = self
            .voice
            .keys()
            .copied()
            .filter(|c| !settings.is_voice_channel(*c))
            .collect();
        for c in &closed {
            for m in self.voice.remove(c).unwrap_or_default() {
                self.stop_sharing(m);
                self.watching.remove(&m);
                self.stop_camera(m);
                self.camera_relay.remove(&m);
            }
        }
        if let Some(store) = &self.store
            && let Err(e) = store.save_card(&self.card)
        {
            warn!("enregistrement du serveur : {e:#}");
        }
        info!("serveur mis à jour (révision {})", settings.revision);
        Ok(Some(closed.into_iter().map(|c| self.room(c)).collect()))
    }
}

struct State {
    peers: Mutex<HashMap<PeerId, Peer>>,
    next_id: AtomicU32,
    space: Mutex<Space>,
    /// Celui qui fait tourner l'hôte : son propre client part avec lui, il
    /// n'est donc jamais successeur.
    operator: Mutex<Option<UserId>>,
    /// Dernier ordre de succession diffusé.
    heirs: Mutex<Vec<Contact>>,
    /// Successeur désigné par un vote.
    preferred: Mutex<Option<PeerId>>,
    vote: Mutex<Option<Vote>>,
    next_vote: AtomicU64,
    endpoint: Endpoint,
    /// L'endpoint n'appartient qu'à l'hôte (sinon c'est celui d'un membre,
    /// qui sert aussi à son vocal : on ne le ferme pas en partant).
    owns_endpoint: bool,
    closed: AtomicBool,
}

/// Un membre demande à héberger ; les autres votent.
struct Vote {
    id: u64,
    candidate: PeerId,
    ballots: HashMap<PeerId, bool>,
}

/// Durée d'un vote pour changer d'hôte.
const VOTE_SECS: u8 = 15;

impl State {
    fn send_to(&self, id: PeerId, packet: Packet) {
        if let Some(p) = self.peers.lock().unwrap().get(&id) {
            p.push(packet);
        }
    }

    fn broadcast_except(&self, except: PeerId, packet: &Packet) {
        for (id, p) in self.peers.lock().unwrap().iter() {
            if *id != except {
                p.push(packet.clone());
            }
        }
    }

    fn broadcast(&self, packet: &Packet) {
        self.broadcast_except(HOST_ID, packet);
    }

    /// Anti-spam : un jeton par message, un autre seau pour les actions.
    fn within_limits(&self, id: PeerId, kind: &Kind) -> bool {
        let mut peers = self.peers.lock().unwrap();
        let Some(p) = peers.get_mut(&id) else { return false };
        match kind {
            Kind::Text | Kind::Rich { .. } => p.limits.0.take(),
            _ => p.limits.1.take(),
        }
    }

    /// Reprend les messages de la copie d'un membre que l'hôte n'avait pas.
    /// Renvoie ceux qui sont nouveaux (à faire suivre aux autres).
    fn backfill(&self, id: PeerId, batch: Vec<SignedMessage>) -> Vec<SignedMessage> {
        {
            let mut peers = self.peers.lock().unwrap();
            let Some(p) = peers.get_mut(&id) else { return Vec::new() };
            if p.backfilled >= MAX_BACKFILL {
                return Vec::new();
            }
            p.backfilled += batch.len();
        }
        let mut space = self.space.lock().unwrap();
        if !space.card.history().keep {
            return Vec::new();
        }
        let mut fresh = Vec::new();
        for m in batch {
            let ok = m.verify(space.card.id).is_ok()
                && !m.from_future()
                && space.card.settings().is_text_channel(m.body.channel)
                && !space.card.is_banned(&m.signer);
            if ok && space.history.push(m.clone()) {
                fresh.push(m);
            }
        }
        fresh
    }

    /// Un admin expulse un membre (pas le créateur ; un admin, seulement par
    /// le créateur).
    fn kick(&self, by: PeerId, target: PeerId) {
        let card = self.space.lock().unwrap().card.clone();
        let peers = self.peers.lock().unwrap();
        let (Some(admin), Some(victim)) = (peers.get(&by), peers.get(&target)) else {
            return;
        };
        let allowed = matches!(
            (card.role(&admin.info.user), card.role(&victim.info.user)),
            (Role::Owner, Role::Admin | Role::Member) | (Role::Admin, Role::Member)
        );
        if !allowed || by == target {
            warn!("{} : expulsion de {} refusée", admin.info.name, victim.info.name);
            return;
        }
        info!("{} expulse {}", admin.info.name, victim.info.name);
        victim.kick(format!("{} t'a expulsé du serveur.", admin.info.name));
    }

    fn peer_list(&self) -> Vec<PeerInfo> {
        let mut v: Vec<_> = self.peers.lock().unwrap().values().map(|p| p.info.clone()).collect();
        v.sort_by_key(|p| p.id);
        v
    }

    /// Recalcule l'ordre de succession et le diffuse s'il a changé.
    fn update_succession(&self) {
        let operator = *self.operator.lock().unwrap();
        let preferred = *self.preferred.lock().unwrap();
        let heirs = {
            let peers = self.peers.lock().unwrap();
            migration::succession(
                peers.values().filter_map(|p| {
                    Some(migration::Candidate {
                        peer: p.info.id,
                        user: p.info.user,
                        offer: p.offer.as_ref()?,
                        seen: p.conn.remote_address(),
                        heir: Some(p.info.user) != operator,
                    })
                }),
                preferred,
            )
        };
        let mut last = self.heirs.lock().unwrap();
        if *last != heirs {
            *last = heirs.clone();
            drop(last);
            self.broadcast(&Control::Succession(heirs).into());
        }
    }

    /// Départ : prévient les membres (relève immédiate, ou annuaire si
    /// `moved`), puis coupe.
    async fn leave(&self, moved: bool) {
        let signal = if moved { Control::Moved } else { Control::HostLeaving };
        self.broadcast(&signal.into());
        // Laisse partir le signal avant de couper.
        tokio::time::sleep(Duration::from_millis(250)).await;
        self.closed.store(true, Ordering::Release);
        if self.owns_endpoint {
            self.endpoint.close(0u32.into(), b"hote parti");
        } else {
            for p in self.peers.lock().unwrap().values() {
                p.conn.close(0u32.into(), b"hote parti");
            }
        }
    }

    fn start_vote(self: &Arc<Self>, candidate: PeerId) {
        let operator = *self.operator.lock().unwrap();
        {
            let mut peers = self.peers.lock().unwrap();
            let Some(p) = peers.get_mut(&candidate) else { return };
            // L'hôte lui-même, ou un membre qu'on ne saurait pas rejoindre : non.
            if Some(p.info.user) == operator || p.offer.is_none() {
                return;
            }
            // Pas de votes à la chaîne.
            if p.asked_host.is_some_and(|t| t.elapsed() < VOTE_COOLDOWN) {
                return;
            }
            p.asked_host = Some(Instant::now());
        }
        let id = {
            let mut vote = self.vote.lock().unwrap();
            if vote.is_some() {
                return;
            }
            let id = self.next_vote.fetch_add(1, Ordering::Relaxed);
            *vote = Some(Vote {
                id,
                candidate,
                ballots: HashMap::from([(candidate, true)]),
            });
            id
        };
        info!("vote : le membre {candidate} demande à héberger");
        self.broadcast(
            &Control::Vote {
                id,
                candidate,
                secs: VOTE_SECS,
            }
            .into(),
        );
        let state = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(VOTE_SECS.into())).await;
            state.conclude_vote(id).await;
        });
    }

    async fn ballot(self: &Arc<Self>, voter: PeerId, id: u64, yes: bool) {
        let done = {
            let mut vote = self.vote.lock().unwrap();
            let Some(v) = vote.as_mut().filter(|v| v.id == id) else {
                return;
            };
            v.ballots.insert(voter, yes);
            let peers = self.peers.lock().unwrap();
            peers.keys().all(|p| v.ballots.contains_key(p))
        };
        if done {
            self.conclude_vote(id).await;
        }
    }

    /// Majorité de ceux qui ont répondu (le candidat compte pour oui). Si
    /// c'est oui, l'hôte passe la main au candidat.
    async fn conclude_vote(&self, id: u64) {
        let Some(vote) = self.vote.lock().unwrap().take_if(|v| v.id == id) else {
            return;
        };
        let present: Vec<PeerId> = self.peers.lock().unwrap().keys().copied().collect();
        let counted = vote.ballots.iter().filter(|(p, _)| present.contains(p));
        let (yes, no) = counted.fold((0u16, 0u16), |(y, n), (_, b)| if *b { (y + 1, n) } else { (y, n + 1) });
        let accepted = yes > no && present.contains(&vote.candidate);
        info!("vote : {yes} oui, {no} non");
        self.broadcast(
            &Control::VoteResult {
                id,
                candidate: vote.candidate,
                accepted,
                yes,
                no,
            }
            .into(),
        );
        if accepted {
            *self.preferred.lock().unwrap() = Some(vote.candidate);
            self.update_succession();
            tokio::time::sleep(Duration::from_millis(300)).await;
            self.leave(false).await;
        }
    }
}

/// Poignée pour piloter un hôte lancé (départ, passage de main).
#[derive(Clone)]
pub struct HostHandle {
    state: Arc<State>,
}

impl HostHandle {
    /// Clé d'accès en vigueur (elle change si un admin change le code).
    pub fn access_key(&self) -> AccessKey {
        self.state.space.lock().unwrap().card.access_key()
    }

    /// Membres connectés, son propre client compris.
    pub fn members(&self) -> usize {
        self.state.peers.lock().unwrap().len()
    }

    /// Membres connectés depuis un autre PC que celui de l'hôte.
    pub fn remote_members(&self) -> usize {
        let operator = *self.state.operator.lock().unwrap();
        let peers = self.state.peers.lock().unwrap();
        peers.values().filter(|p| Some(p.info.user) != operator).count()
    }

    /// Départ volontaire : prévient les membres pour qu'ils passent tout de
    /// suite au successeur (au lieu d'attendre que la connexion expire), puis
    /// ferme. `moved` : un autre hôte existe déjà, à retrouver via l'annuaire.
    pub async fn leave(&self, moved: bool) {
        self.state.leave(moved).await;
    }
}

pub struct Host {
    endpoint: Endpoint,
    probe: nat::Probe,
    cert: CertFingerprint,
    state: Arc<State>,
    /// Hébergé sur la station d'un membre : ses connexions arrivent par là.
    feed: Option<mpsc::Receiver<Connection>>,
}

impl Host {
    /// Héberge `card`. L'historique fourni est servi aux arrivants et complété
    /// au fil des messages (selon la politique du serveur).
    pub fn bind(addr: SocketAddr, card: ServerCard, history: HistoryLog) -> Result<Self> {
        let cert = HostCert::generate()?;
        let (endpoint, probe) = nat::endpoint(addr, Some(cert.config.clone()))?;
        Self::new(endpoint, probe, cert.fingerprint, None, card, history, HOST_ID + 1)
    }

    /// Héberge sur la station d'un membre qui prend la relève : son port est
    /// déjà ouvert dans sa box, puisqu'il parlait à l'ancien hôte et aux
    /// autres participants. `first_id` : premier numéro libre (les anciens
    /// membres gardent le leur).
    pub fn on_station(station: &Station, card: ServerCard, history: HistoryLog, first_id: PeerId) -> Result<Self> {
        let feed = station.host_route();
        Self::new(
            station.endpoint.clone(),
            station.probe.clone(),
            station.cert.fingerprint,
            Some(feed),
            card,
            history,
            first_id,
        )
    }

    fn new(
        endpoint: Endpoint,
        probe: nat::Probe,
        cert: CertFingerprint,
        feed: Option<mpsc::Receiver<Connection>>,
        card: ServerCard,
        mut history: HistoryLog,
        first_id: PeerId,
    ) -> Result<Self> {
        card.verify().context("serveur invalide")?;
        ensure!(history.server() == card.id, "historique d'un autre serveur");
        history.set_moderators(&card);
        let state = Arc::new(State {
            peers: Mutex::default(),
            next_id: AtomicU32::new(first_id.max(HOST_ID + 1)),
            space: Mutex::new(Space {
                card,
                history,
                voice: BTreeMap::new(),
                sharing: BTreeSet::new(),
                watching: HashMap::new(),
                cameras: BTreeSet::new(),
                camera_relay: HashMap::new(),
                muted: BTreeSet::new(),
                deafened: BTreeSet::new(),
                store: None,
                files: HashMap::new(),
                wanted: HashMap::new(),
            }),
            operator: Mutex::new(None),
            heirs: Mutex::default(),
            preferred: Mutex::new(None),
            vote: Mutex::new(None),
            next_vote: AtomicU64::new(1),
            endpoint: endpoint.clone(),
            owns_endpoint: feed.is_none(),
            closed: AtomicBool::new(false),
        });
        Ok(Self {
            endpoint,
            probe,
            cert,
            state,
            feed,
        })
    }

    /// Celui qui héberge (son client n'est jamais successeur).
    pub fn with_operator(self, user: UserId) -> Self {
        *self.state.operator.lock().unwrap() = Some(user);
        self
    }

    pub fn handle(&self) -> HostHandle {
        HostHandle {
            state: self.state.clone(),
        }
    }

    /// Enregistre dans `store` les mises à jour du serveur reçues des admins.
    pub fn with_store(self, store: Store) -> Self {
        self.state.space.lock().unwrap().store = Some(store);
        self
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Endpoint du serveur, pour percer la box vers les arrivants depuis le
    /// même port (voir [`announce`]).
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    /// Pour découvrir l'adresse publique du port du serveur (STUN).
    pub fn probe(&self) -> nat::Probe {
        self.probe.clone()
    }

    /// Empreinte du certificat de l'hôte.
    pub fn fingerprint(&self) -> CertFingerprint {
        self.cert
    }

    /// Accepte les connexions jusqu'à la fermeture de l'endpoint (ou de la
    /// station).
    pub async fn run(mut self) -> Result<()> {
        info!("hôte en écoute sur {}", self.local_addr()?);
        let serve = |conn: Connection, state: Arc<State>| {
            tokio::spawn(async move {
                let remote = conn.remote_address();
                if let Err(e) = handle_peer(conn, state).await {
                    warn!("{remote} : {e:#}");
                }
            });
        };
        if let Some(feed) = &mut self.feed {
            while let Some(conn) = feed.recv().await {
                serve(conn, self.state.clone());
            }
            return Ok(());
        }
        while let Some(incoming) = self.endpoint.accept().await {
            // Trop de connexions à la fois (quelqu'un qui en ouvre des
            // milliers sans se présenter) : les suivantes sont refusées.
            if self.endpoint.open_connections() >= MAX_CONNECTIONS {
                incoming.refuse();
                continue;
            }
            let state = self.state.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) if tls::alpn(&conn).as_deref() == Some(ALPN) => serve(conn, state),
                    Ok(conn) => conn.close(0u32.into(), b"protocole inattendu"),
                    Err(e) => warn!("connexion refusée : {e}"),
                }
            });
        }
        Ok(())
    }
}

async fn handle_peer(conn: Connection, state: Arc<State>) -> Result<()> {
    if state.closed.load(Ordering::Acquire) {
        conn.close(0u32.into(), b"hote parti");
        return Ok(());
    }
    // Une connexion muette ne doit pas occuper l'hôte indéfiniment.
    let hello = async {
        let (send, mut recv) = conn.accept_bi().await?;
        let packet = read_packet(&mut recv).await?;
        anyhow::Ok((send, recv, packet))
    };
    let (mut send, mut recv, hello) = tokio::time::timeout(HELLO_TIMEOUT, hello)
        .await
        .map_err(|_| anyhow::anyhow!("pas de présentation"))??;

    let (name, user, resume) = match hello {
        Some(Packet::Control(Control::Hello {
            name,
            version,
            user,
            proof,
            resume,
            access,
        })) if version == PROTOCOL_VERSION => {
            let key = state.space.lock().unwrap().card.access_key();
            let refused = if !tls::check_access(&conn, &key, Side::Member, &access) {
                Some(
                    "Code d'invitation incorrect ou périmé : demande le code actuel à un membre du serveur."
                        .to_string(),
                )
            } else if !user.verify(&tls::session_binding(&conn)?, &proof) {
                Some("preuve d'identité invalide".to_string())
            } else if state.space.lock().unwrap().card.is_banned(&user) {
                Some("tu as été banni de ce serveur".to_string())
            } else {
                None
            };
            if let Some(reason) = refused {
                write_packet(&mut send, &Control::Error { reason: reason.clone() }.into()).await?;
                let _ = send.finish();
                // Laisse le refus arriver avant que la connexion ne tombe.
                let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
                bail!(reason);
            }
            let name: String = plain_text(name.trim()).chars().take(32).collect();
            (name, user, resume)
        }
        Some(Packet::Control(Control::Hello { version, .. })) => {
            let reason = incompatible(version);
            write_packet(&mut send, &Control::Error { reason: reason.clone() }.into()).await?;
            let _ = send.finish();
            bail!(reason);
        }
        other => bail!("Hello attendu, reçu {other:?}"),
    };

    // Tâche d'écriture : tout ce qui part vers ce pair passe par `tx`.
    let (tx, mut rx) = mpsc::channel::<Packet>(OUTBOX);
    let writer = tokio::spawn(async move {
        while let Some(p) = rx.recv().await {
            if write_packet(&mut send, &p).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    });

    // Accueil et arrivée d'un seul tenant : un message ou un pair arrivé
    // entre les deux serait sinon perdu pour le nouveau venu.
    let info = {
        let space = state.space.lock().unwrap();
        // Après une relève, chacun retrouve son numéro s'il est libre.
        let (id, returning) = {
            let mut peers = state.peers.lock().unwrap();
            match resume {
                Some(r) if r != HOST_ID && !peers.contains_key(&r) => {
                    state.next_id.fetch_max(r + 1, Ordering::Relaxed);
                    (r, false)
                }
                // Sa connexion a sauté sans qu'on le remarque encore : la
                // nouvelle remplace l'ancienne, il garde son numéro (et son salon).
                Some(r) if r != HOST_ID && peers.get(&r).is_some_and(|p| p.info.user == user) => {
                    info!("{name} revient (id {r}) : ancienne connexion remplacée");
                    if let Some(old) = peers.remove(&r) {
                        old.conn.close(0u32.into(), b"remplacee");
                    }
                    (r, true)
                }
                _ => loop {
                    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
                    if !peers.contains_key(&id) {
                        break (id, false);
                    }
                },
            }
        };
        let info = PeerInfo { id, name, user };
        info!("{} rejoint la session (id {id}, {})", info.name, conn.remote_address());
        tx.try_send(
            Control::Welcome {
                peer_id: id,
                host_id: HOST_ID,
                peers: state.peer_list(),
                card: Box::new(space.card.clone()),
                voice: space.rooms(),
                host_user: *state.operator.lock().unwrap(),
                access: tls::access_proof(&conn, &space.card.access_key(), Side::Host)?,
            }
            .into(),
        )?;
        for batch in history_batches(space.history.messages()) {
            tx.try_send(Chat::History(batch).into())?;
        }
        tx.try_send(Chat::HistoryEnd.into())?;
        // Les autres le connaissent déjà s'il revient.
        if !returning {
            state.broadcast_except(id, &Control::PeerJoined(info.clone()).into());
        }
        state.peers.lock().unwrap().insert(
            id,
            Peer {
                info: info.clone(),
                tx,
                conn: conn.clone(),
                offer: None,
                screen: Arc::default(),
                cameras: HashMap::new(),
                limits: (Bucket::messages().lenient(), Bucket::actions().lenient()),
                backfilled: 0,
                uploads: (0, Instant::now()),
                asked_host: None,
            },
        );
        info
    };
    let id = info.id;
    // Le nouveau venu doit connaître l'ordre de succession, même inchangé.
    state.send_to(id, Control::Succession(state.heirs.lock().unwrap().clone()).into());

    let voice = tokio::spawn(relay_voice(conn.clone(), id, state.clone()));
    let screen = tokio::spawn(relay_screen(conn.clone(), id, state.clone()));
    let transfers = tokio::spawn(serve_files(conn.clone(), id, state.clone()));
    let result = peer_loop(&info, &mut recv, &state).await;
    voice.abort();
    screen.abort();
    transfers.abort();
    // Remplacé par une nouvelle connexion du même membre : rien à défaire.
    let replaced = {
        let mut peers = state.peers.lock().unwrap();
        let mine = peers.get(&id).is_some_and(|p| p.conn.stable_id() == conn.stable_id());
        if mine {
            peers.remove(&id);
        }
        !mine
    };
    if replaced {
        writer.abort();
        return result;
    }
    state.space.lock().unwrap().forget_status(id);
    state.update_succession();
    let rooms = state.space.lock().unwrap().leave_voice(id);
    for room in rooms {
        state.broadcast(&Control::VoiceState(room).into());
    }
    state.broadcast_except(id, &Control::PeerLeft { peer_id: id }.into());
    info!("{} a quitté la session", info.name);
    writer.abort();
    result
}

/// Relaie la voix de `id` aux autres membres de son salon vocal (ou aux
/// seuls destinataires d'un chuchotement, s'ils sont dans le salon). Les
/// paquets restent compressés par Opus et ne sont pas décodés.
async fn relay_voice(conn: Connection, id: PeerId, state: Arc<State>) {
    while let Ok(datagram) = conn.read_datagram().await {
        if let Some(sound) = screen::sound_from_member(id, &datagram) {
            relay_sound(sound, &state);
            continue;
        }
        let Some(up) = voice::from_member(&datagram) else {
            continue;
        };
        let others: Vec<PeerId> = {
            let space = state.space.lock().unwrap();
            let Some(members) = space.voice.values().find(|m| m.contains(&id)) else {
                continue;
            };
            members
                .iter()
                .copied()
                .filter(|m| *m != id && up.targets.as_ref().is_none_or(|t| t.contains(m)))
                .collect()
        };
        let out = voice::to_member(id, up.frame, up.whisper);
        let peers = state.peers.lock().unwrap();
        for m in others {
            if let Some(p) = peers.get(&m) {
                let _ = p.conn.send_datagram(out.clone());
            }
        }
    }
}

/// Relaie le son du partage d'écran à ceux qui le regardent par l'hôte.
fn relay_sound(sound: screen::Sound, state: &State) {
    let viewers = state.space.lock().unwrap().viewers(sound.from);
    if viewers.is_empty() {
        return;
    }
    let out = screen::sound_to_member(sound);
    let peers = state.peers.lock().unwrap();
    for v in viewers {
        if let Some(p) = peers.get(&v) {
            let _ = p.conn.send_datagram(out.clone());
        }
    }
}

/// Relaie les images de l'écran de `id` à ceux qui le regardent (dans le
/// même salon vocal), et celles de sa webcam à ceux qui ne la reçoivent pas
/// directement. Un spectateur en retard saute des images jusqu'à la
/// prochaine image clé, alors demandée au partageur.
async fn relay_screen(conn: Connection, id: PeerId, state: Arc<State>) {
    while let Ok(mut stream) = conn.accept_uni().await {
        let Some(frame) = screen::recv(&mut stream)
            .await
            .ok()
            .and_then(|d| screen::from_member(&d))
        else {
            continue;
        };
        if frame.camera {
            relay_camera(id, &frame, &state);
            continue;
        }
        let viewers = state.space.lock().unwrap().viewers(id);
        let wire = screen::to_member(id, &frame);
        let mut asker = None;
        {
            let peers = state.peers.lock().unwrap();
            for v in viewers {
                let Some(p) = peers.get(&v) else { continue };
                if screen::offer(&p.conn, &p.screen, &frame, &wire) {
                    asker = Some(v);
                }
            }
        }
        if let Some(v) = asker {
            state.send_to(id, Control::ScreenKey(v).into());
        }
    }
}

fn relay_camera(id: PeerId, frame: &screen::Frame, state: &State) {
    let viewers = state.space.lock().unwrap().camera_viewers(id);
    if viewers.is_empty() {
        return;
    }
    let wire = screen::to_member(id, frame);
    let mut asker = None;
    {
        let mut peers = state.peers.lock().unwrap();
        for v in viewers {
            let Some(p) = peers.get_mut(&v) else { continue };
            let lane = p.cameras.entry(id).or_default();
            if screen::offer(&p.conn, lane, frame, &wire) {
                asker = Some(v);
            }
        }
    }
    if let Some(v) = asker {
        state.send_to(id, Control::CameraKey(v).into());
    }
}

async fn peer_loop(me: &PeerInfo, recv: &mut quinn::RecvStream, state: &Arc<State>) -> Result<()> {
    let id = me.id;
    while let Some(packet) = read_packet(recv).await? {
        match packet {
            Packet::Control(Control::Ping { nonce }) => state.send_to(id, Control::Pong { nonce }.into()),
            Packet::Control(Control::Pong { .. }) => {}
            Packet::Control(Control::Bye) => break,
            Packet::Chat(Chat::Text { msg, .. }) => {
                if !state.within_limits(id, &msg.body.kind) {
                    state.send_to(id, Control::SlowDown.into());
                    continue;
                }
                if msg.from_future() {
                    // Son horloge avance : il doit le savoir, sinon ses messages
                    // disparaissent sans explication.
                    let reason = "Ton message n'est pas parti : l'horloge de ton PC est en avance. \
                                  Règle la date et l'heure (Windows : Paramètres > Heure et langue > Synchroniser)."
                        .to_string();
                    state.send_to(id, Control::Error { reason }.into());
                    continue;
                }
                if let Err(e) = accept_message(&msg, &me.user, state) {
                    warn!("message de {} refusé : {e:#}", me.name);
                    continue;
                }
                // L'hôte fait foi sur l'expéditeur.
                state.broadcast_except(id, &Chat::Text { from: id, msg }.into());
            }
            // Ce que ce membre a gardé et que je n'avais pas (écrit pendant
            // que j'étais absent) : je le reprends et le fais suivre.
            Packet::Chat(Chat::History(batch)) => {
                let fresh = state.backfill(id, batch);
                if !fresh.is_empty() {
                    info!("{} messages repris de la copie de {}", fresh.len(), me.name);
                    for batch in history_batches(&fresh) {
                        state.broadcast_except(id, &Chat::History(batch).into());
                    }
                }
            }
            Packet::Control(Control::Kick { peer }) => state.kick(id, peer),
            Packet::Control(Control::CardUpdate(card)) => {
                let applied = state.space.lock().unwrap().apply_card(&card);
                match applied {
                    Ok(Some(closed)) => {
                        let card = state.space.lock().unwrap().card.clone();
                        state.broadcast(&Control::CardUpdate(Box::new(card.clone())).into());
                        for room in closed {
                            state.broadcast(&Control::VoiceState(room).into());
                        }
                        // Les bannis présents s'en vont.
                        for p in state.peers.lock().unwrap().values() {
                            if card.is_banned(&p.info.user) {
                                p.kick("Tu as été banni de ce serveur.".into());
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => warn!("mise à jour du serveur par {} refusée : {e:#}", me.name),
                }
            }
            Packet::Control(Control::VoiceJoin { channel, share, camera }) => {
                let rooms = {
                    let mut space = state.space.lock().unwrap();
                    if !space.card.settings().is_voice_channel(channel) {
                        continue;
                    }
                    if space.voice.get(&channel).is_some_and(|m| m.contains(&id)) {
                        continue;
                    }
                    let mut rooms = space.leave_voice(id);
                    space.voice.entry(channel).or_default().push(id);
                    if share {
                        space.sharing.insert(id);
                    }
                    if camera {
                        space.cameras.insert(id);
                    }
                    rooms.push(space.room(channel));
                    rooms
                };
                for room in rooms {
                    state.broadcast(&Control::VoiceState(room).into());
                }
            }
            Packet::Control(Control::ScreenShare(on)) => {
                let room = {
                    let mut space = state.space.lock().unwrap();
                    let Some(channel) = space.voice.iter().find(|(_, m)| m.contains(&id)).map(|(c, _)| *c) else {
                        continue;
                    };
                    let changed = if on {
                        space.sharing.insert(id)
                    } else {
                        space.stop_sharing(id)
                    };
                    if !changed {
                        continue;
                    }
                    space.room(channel)
                };
                state.broadcast(&Control::VoiceState(room).into());
            }
            Packet::Control(Control::ScreenWatch(target)) => {
                let watched = {
                    let mut space = state.space.lock().unwrap();
                    match target {
                        // Seuls les participants du même salon vocal peuvent regarder.
                        Some(s)
                            if space.sharing.contains(&s)
                                && s != id
                                && space.voice.values().any(|m| m.contains(&s) && m.contains(&id)) =>
                        {
                            space.watching.insert(id, s);
                            Some(s)
                        }
                        _ => {
                            space.watching.remove(&id);
                            None
                        }
                    }
                };
                // Il commence par une image clé.
                if let Some(s) = watched {
                    ask_key(state, id, s);
                }
            }
            // Le spectateur a perdu le fil : image clé.
            Packet::Control(Control::ScreenKey(s)) => {
                let watching = state.space.lock().unwrap().watching.get(&id) == Some(&s);
                if watching {
                    ask_key(state, id, s);
                }
            }
            Packet::Control(Control::Camera(on)) => {
                let room = {
                    let mut space = state.space.lock().unwrap();
                    let Some(channel) = space.room_of(id) else { continue };
                    let changed = if on {
                        space.cameras.insert(id)
                    } else {
                        space.stop_camera(id)
                    };
                    if !changed {
                        continue;
                    }
                    space.room(channel)
                };
                state.broadcast(&Control::VoiceState(room).into());
            }
            Packet::Control(Control::CameraRelay(sources)) => {
                let added: Vec<PeerId> = {
                    let mut space = state.space.lock().unwrap();
                    let Some(channel) = space.room_of(id) else { continue };
                    let room = space.voice.get(&channel).cloned().unwrap_or_default();
                    // Seulement les webcams de son salon.
                    let wanted: BTreeSet<PeerId> = sources
                        .into_iter()
                        .filter(|s| *s != id && room.contains(s))
                        .take(32)
                        .collect();
                    let before = space.camera_relay.insert(id, wanted.clone()).unwrap_or_default();
                    wanted
                        .into_iter()
                        .filter(|s| !before.contains(s) && space.cameras.contains(s))
                        .collect()
                };
                // Chacune commence par une image clé.
                for s in added {
                    ask_camera_key(state, id, s);
                }
            }
            Packet::Control(Control::CameraKey(s)) => {
                let relayed = state
                    .space
                    .lock()
                    .unwrap()
                    .camera_relay
                    .get(&id)
                    .is_some_and(|r| r.contains(&s));
                if relayed {
                    ask_camera_key(state, id, s);
                }
            }
            Packet::Control(Control::VoiceLeave) => {
                let rooms = state.space.lock().unwrap().leave_voice(id);
                for room in rooms {
                    state.broadcast(&Control::VoiceState(room).into());
                }
            }
            Packet::Control(Control::Status { muted, deafened }) => {
                let room = {
                    let mut space = state.space.lock().unwrap();
                    let changed = set_flag(&mut space.muted, id, muted) | set_flag(&mut space.deafened, id, deafened);
                    match space.room_of(id) {
                        Some(c) if changed => space.room(c),
                        _ => continue,
                    }
                };
                state.broadcast(&Control::VoiceState(room).into());
            }
            Packet::Control(Control::HostRequest) => state.start_vote(id),
            Packet::Control(Control::Ballot { id: vote, yes }) => state.ballot(id, vote, yes).await,
            Packet::Control(Control::Relay(offer)) => {
                if offer.addrs.len() > 8 {
                    continue;
                }
                if let Some(p) = state.peers.lock().unwrap().get_mut(&id) {
                    p.offer = Some(offer);
                }
                state.update_succession();
            }
            Packet::Chat(Chat::HistoryEnd) => {}
            Packet::Control(other) => warn!("pair {id} : signal non géré {other:?}"),
        }
    }
    Ok(())
}

/// `viewer` doit repartir d'une image clé de l'écran de `sharer` : rien ne
/// lui part avant, et le partageur en fait une.
fn ask_key(state: &State, viewer: PeerId, sharer: PeerId) {
    if let Some(p) = state.peers.lock().unwrap().get(&viewer) {
        p.screen.reset(Some(Instant::now()));
    }
    state.send_to(sharer, Control::ScreenKey(viewer).into());
}

/// Pareil pour la webcam de `owner`, relayée à `viewer`.
fn ask_camera_key(state: &State, viewer: PeerId, owner: PeerId) {
    if let Some(lane) = state
        .peers
        .lock()
        .unwrap()
        .get(&viewer)
        .and_then(|p| p.cameras.get(&owner))
    {
        lane.reset(Some(Instant::now()));
    }
    state.send_to(owner, Control::CameraKey(viewer).into());
}

fn set_flag(set: &mut BTreeSet<PeerId>, id: PeerId, on: bool) -> bool {
    if on { set.insert(id) } else { set.remove(&id) }
}

fn accept_message(msg: &SignedMessage, sender: &UserId, state: &State) -> Result<()> {
    let mut space = state.space.lock().unwrap();
    msg.verify(space.card.id)?;
    ensure!(msg.author() == *sender, "signé par quelqu'un d'autre");
    ensure!(!space.card.is_banned(sender), "auteur banni");
    ensure!(
        space.card.settings().is_text_channel(msg.body.channel),
        "salon texte inconnu"
    );
    space.history.push(msg.clone());
    Ok(())
}

/// Transferts de pièces jointes : un flux bidirectionnel par fichier, ouvert
/// par le membre après son flux de contrôle.
async fn serve_files(conn: Connection, id: PeerId, state: Arc<State>) {
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let state = state.clone();
        tokio::spawn(async move {
            let result = match files::read_request(&mut recv).await {
                Ok(FileRequest::Put { hash, size }) => receive_file(&mut send, &mut recv, id, hash, size, &state).await,
                Ok(FileRequest::Get { hash }) => send_file(&mut send, id, hash, &state).await,
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                warn!("pièce jointe (pair {id}) : {e:#}");
            }
        });
    }
}

async fn receive_file(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    id: PeerId,
    hash: FileHash,
    size: u64,
    state: &State,
) -> Result<()> {
    // Budget d'envoi par membre : de quoi partager, pas de quoi remplir le disque de l'hôte.
    let allowed = {
        let mut peers = state.peers.lock().unwrap();
        let Some(p) = peers.get_mut(&id) else { return Ok(()) };
        if p.uploads.1.elapsed() > Duration::from_secs(600) {
            p.uploads = (0, Instant::now());
        }
        p.uploads.0 += size;
        p.uploads.0 <= UPLOAD_BUDGET
    };
    if !allowed {
        files::answer_put(send, false).await?;
        bail!("trop de pièces jointes d'un coup");
    }
    let bytes = Arc::new(files::read_put(recv, hash, size).await?);
    let disk = state.space.lock().unwrap().file_store();
    if let Some((store, server)) = disk {
        let b = bytes.clone();
        match tokio::task::spawn_blocking(move || store.save_file(server, &hash, &b)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!("pièce jointe : enregistrement impossible : {e:#}"),
            Err(e) => warn!("pièce jointe : enregistrement impossible : {e}"),
        }
    }
    state.space.lock().unwrap().keep_file(hash, bytes);
    files::answer_put(send, true).await
}

/// Envoie un fichier ; si l'hôte ne l'a pas (il vient d'un autre hôte), il le
/// demande aux membres connectés et attend que l'un d'eux l'envoie.
async fn send_file(send: &mut quinn::SendStream, id: PeerId, hash: FileHash, state: &State) -> Result<()> {
    let (memory, disk) = {
        let space = state.space.lock().unwrap();
        (space.files.get(&hash).cloned(), space.file_store())
    };
    let read_disk = || async {
        let (store, server) = disk.clone()?;
        tokio::task::spawn_blocking(move || store.file(server, &hash))
            .await
            .ok()
            .flatten()
            .map(Arc::new)
    };
    let mut bytes = match memory {
        Some(b) => Some(b),
        None => read_disk().await,
    };
    if bytes.is_none() {
        let rx = {
            let mut space = state.space.lock().unwrap();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let first = !space.wanted.contains_key(&hash);
            space.wanted.entry(hash).or_default().push(tx);
            if first {
                state.broadcast_except(id, &Control::FileWanted { hash }.into());
            }
            rx
        };
        // Arrivé sur le disque juste avant la demande ? Sinon, on attend qu'un membre l'envoie.
        bytes = match read_disk().await {
            Some(b) => Some(b),
            None => tokio::time::timeout(Duration::from_secs(20), rx)
                .await
                .ok()
                .and_then(Result::ok),
        };
    }
    {
        // Demande servie ou restée sans réponse : on l'oublie.
        let mut space = state.space.lock().unwrap();
        if let Some(w) = space.wanted.get_mut(&hash) {
            w.retain(|tx| !tx.is_closed());
            if w.is_empty() {
                space.wanted.remove(&hash);
            }
        }
    }
    files::answer_get(send, bytes.as_deref().map(|b| b.as_slice())).await
}

/// Pourquoi l'hôte refuse une app d'une autre version, et qui doit la
/// mettre à jour.
fn incompatible(version: u16) -> String {
    if version < PROTOCOL_VERSION {
        format!(
            "Ton app est trop ancienne pour ce serveur (l'hôte a RIP Discord {VERSION}) : installe la dernière version."
        )
    } else {
        format!(
            "L'hôte a une version plus ancienne de RIP Discord ({VERSION}) : il doit mettre son app à jour, ou \
             quelqu'un d'autre à jour peut héberger."
        )
    }
}
