//! Salon vocal de PC à PC : chacun envoie sa voix, sa webcam (et son écran à
//! ceux qui le regardent) directement aux autres participants.
//!
//! - Pas d'intermédiaire : un seul trajet réseau, donc moins de latence.
//! - Personne au centre : ni le départ de l'hôte du serveur, ni celui d'un
//!   participant ne coupe les autres.
//! - L'hôte du serveur sert d'annuaire : il diffuse les contacts de chacun
//!   (`Control::Succession`). Pour chaque paire, le membre à la plus petite
//!   identité appelle, l'autre perce sa box vers lui.
//! - Deux PC qui n'arrivent pas à se joindre (box trop strictes) passent par
//!   l'hôte du serveur : [`Mesh::send_voice`] dit pour qui.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use quinn::Connection;
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, time::timeout};
use tracing::{debug, info};

use super::{SoundIn, VoiceIn, failover::first_handshake, station::Station};
use crate::{
    core::{
        card::ServerId,
        framing::{read_msg, write_msg},
        identity::Identity,
        protocol::{Contact, MeshHello, PeerId},
        screen, tls, voice,
    },
    server::announce,
};

/// Délai entre deux tentatives vers un même participant.
const RETRY: Duration = Duration::from_secs(3);
/// Temps laissé à une tentative de connexion directe.
const DIAL_TIMEOUT: Duration = Duration::from_secs(8);

/// Le salon devient celui décrit par l'hôte : on ferme les connexions avec
/// ceux qui en sont sortis.
fn apply_room(s: &mut State) {
    s.room = s.wanted.clone();
    let room = s.room.clone();
    s.links.retain(|peer, link| {
        let keep = room.contains(peer);
        if !keep {
            link.conn.close(0u32.into(), b"sorti du salon");
        }
        keep
    });
    s.watchers.retain(|w| room.contains(w));
}

/// Petits messages sur le flux de contrôle d'une connexion directe.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Signal {
    /// Accepté (réponse au `MeshHello`).
    Welcome,
    /// Je regarde ton écran (ou plus).
    Watch(bool),
    /// J'ai perdu le fil de ton écran : une image clé, s'il te plaît.
    Key,
    /// Pareil pour ta webcam.
    CameraKey,
}

struct Link {
    conn: Connection,
    signals: mpsc::UnboundedSender<Signal>,
    /// Images de mon écran envoyées à ce participant.
    screen: Arc<screen::Lane>,
    /// Images de ma webcam envoyées à ce participant.
    camera: Arc<screen::Lane>,
}

#[derive(Default)]
struct State {
    server: Option<ServerId>,
    me: Option<PeerId>,
    contacts: HashMap<PeerId, Contact>,
    /// Les autres participants de mon salon vocal.
    room: Vec<PeerId>,
    /// Juste après une relève, les participants reviennent un à un chez le
    /// nouvel hôte : jusqu'à cet instant, on n'en déconnecte aucun.
    hold: Option<Instant>,
    /// Le salon tel que l'hôte le décrit (appliqué à la fin de `hold`).
    wanted: Vec<PeerId>,
    links: HashMap<PeerId, Link>,
    /// Dernière tentative vers chacun.
    tried: HashMap<PeerId, Instant>,
    /// Ceux qui regardent mon écran.
    watchers: HashSet<PeerId>,
    /// L'écran que je regarde directement (redemandé si la connexion se refait).
    watching: Option<PeerId>,
}

struct Inner {
    station: Station,
    identity: Identity,
    voice: mpsc::UnboundedSender<VoiceIn>,
    screens: mpsc::UnboundedSender<(PeerId, screen::Frame)>,
    sounds: mpsc::UnboundedSender<SoundIn>,
    keys: mpsc::UnboundedSender<PeerId>,
    camera_keys: mpsc::UnboundedSender<PeerId>,
    state: Mutex<State>,
}

/// Connexions directes avec les autres participants du salon vocal.
#[derive(Clone)]
pub struct Mesh(Arc<Inner>);

/// Voix, images (écran et webcam) et son d'écran reçus directement des
/// autres participants.
pub struct Incoming {
    pub voice: mpsc::UnboundedReceiver<VoiceIn>,
    pub screens: mpsc::UnboundedReceiver<(PeerId, screen::Frame)>,
    pub sounds: mpsc::UnboundedReceiver<SoundIn>,
    /// Ceux qui regardent mon écran directement et attendent une image clé.
    pub keys: mpsc::UnboundedReceiver<PeerId>,
    /// Ceux qui ont perdu le fil de ma webcam.
    pub camera_keys: mpsc::UnboundedReceiver<PeerId>,
}

impl Mesh {
    pub fn start(station: Station, identity: Identity) -> (Self, Incoming) {
        let (voice, voice_rx) = mpsc::unbounded_channel();
        let (screens, screens_rx) = mpsc::unbounded_channel();
        let (sounds, sounds_rx) = mpsc::unbounded_channel();
        let (keys, keys_rx) = mpsc::unbounded_channel();
        let (camera_keys, camera_keys_rx) = mpsc::unbounded_channel();
        let mut route = station.mesh_route();
        let mesh = Self(Arc::new(Inner {
            station,
            identity,
            voice,
            screens,
            sounds,
            keys,
            camera_keys,
            state: Mutex::default(),
        }));
        let accepting = mesh.clone();
        tokio::spawn(async move {
            while let Some(conn) = route.recv().await {
                let mesh = accepting.clone();
                tokio::spawn(async move {
                    let remote = conn.remote_address();
                    if let Err(e) = mesh.accept(conn).await {
                        debug!("connexion directe de {remote} refusée : {e:#}");
                    }
                });
            }
        });
        (
            mesh,
            Incoming {
                voice: voice_rx,
                screens: screens_rx,
                sounds: sounds_rx,
                keys: keys_rx,
                camera_keys: camera_keys_rx,
            },
        )
    }

    /// Nouvelle connexion au serveur (numéro attribué par l'hôte).
    pub fn set_session(&self, server: ServerId, me: PeerId) {
        let mut s = self.0.state.lock().unwrap();
        s.server = Some(server);
        s.me = Some(me);
    }

    pub fn set_contacts(&self, contacts: &[Contact]) {
        self.0.state.lock().unwrap().contacts = contacts.iter().map(|c| (c.peer, c.clone())).collect();
        self.reconcile();
    }

    /// Qui est dans mon salon vocal (moi compris ou non). Les connexions avec
    /// ceux qui en sortent se ferment ; on appelle ceux qui arrivent.
    pub fn set_room(&self, members: &[PeerId]) {
        {
            let mut s = self.0.state.lock().unwrap();
            let me = s.me;
            s.wanted = members.iter().copied().filter(|m| Some(*m) != me).collect();
            if s.hold.is_some_and(|t| Instant::now() < t) {
                // Ceux qui ne sont pas encore revenus restent reliés.
                let wanted = s.wanted.clone();
                for m in wanted {
                    if !s.room.contains(&m) {
                        s.room.push(m);
                    }
                }
            } else {
                s.hold = None;
                apply_room(&mut s);
            }
        }
        self.reconcile();
    }

    /// L'hôte du serveur vient de changer : pendant `grace`, une liste de
    /// salon incomplète ne coupe personne (la voix continue de PC à PC).
    pub fn hold_room(&self, grace: Duration) {
        self.0.state.lock().unwrap().hold = Some(Instant::now() + grace);
    }

    /// À appeler régulièrement : relance les connexions tombées.
    pub fn tick(&self) {
        {
            let mut s = self.0.state.lock().unwrap();
            if s.hold.is_some_and(|t| Instant::now() >= t) {
                s.hold = None;
                apply_room(&mut s);
            }
        }
        self.reconcile();
    }

    /// Envoie une trame à tout le salon, ou aux seuls `targets`. Renvoie ceux
    /// qu'on n'a pas pu joindre directement (à faire passer par l'hôte).
    pub fn send_voice(&self, seq: u16, opus: &[u8], targets: Option<&[PeerId]>) -> Vec<PeerId> {
        let s = self.0.state.lock().unwrap();
        let Some(me) = s.me else {
            return targets.map_or_else(|| s.room.clone(), <[_]>::to_vec);
        };
        let whisper = targets.is_some();
        let packet = voice::to_member(me, voice::Frame { seq, opus }, whisper);
        let mut missing = Vec::new();
        for peer in s.room.iter().filter(|p| targets.is_none_or(|t| t.contains(p))) {
            match s.links.get(peer) {
                Some(link) if link.conn.send_datagram(packet.clone()).is_ok() => {}
                _ => missing.push(*peer),
            }
        }
        missing
    }

    /// Connecté directement à `peer` ?
    pub fn linked(&self, peer: PeerId) -> bool {
        self.0.state.lock().unwrap().links.contains_key(&peer)
    }

    /// Tous les participants de mon salon sont joignables directement.
    pub fn all_linked(&self) -> bool {
        let s = self.0.state.lock().unwrap();
        s.room.iter().all(|p| s.links.contains_key(p))
    }

    /// Regarder (ou plus) l'écran de `peer`. Faux s'il n'y a pas de connexion
    /// directe (passer alors par l'hôte) ; si elle se fait plus tard, la
    /// demande y repart d'elle-même.
    pub fn watch(&self, peer: PeerId, on: bool) -> bool {
        let mut s = self.0.state.lock().unwrap();
        if on {
            s.watching = Some(peer);
        } else if s.watching == Some(peer) {
            s.watching = None;
        }
        match s.links.get(&peer) {
            Some(link) => link.signals.send(Signal::Watch(on)).is_ok(),
            None => false,
        }
    }

    /// Demande une image clé à `peer`, dont je regarde l'écran. Faux sans
    /// connexion directe.
    pub fn request_key(&self, peer: PeerId) -> bool {
        let s = self.0.state.lock().unwrap();
        s.links.get(&peer).is_some_and(|l| l.signals.send(Signal::Key).is_ok())
    }

    /// Envoie une image de mon écran à ceux qui le regardent directement (un
    /// spectateur en retard saute des images jusqu'à la prochaine image clé).
    /// Renvoie vrai s'il faut une image clé.
    pub fn send_screen(&self, frame: &screen::Frame) -> bool {
        let s = self.0.state.lock().unwrap();
        let Some(me) = s.me else { return false };
        let wire = screen::to_member(me, frame);
        let mut want_key = false;
        for w in &s.watchers {
            if let Some(link) = s.links.get(w) {
                want_key |= screen::offer(&link.conn, &link.screen, frame, &wire);
            }
        }
        want_key
    }

    /// Envoie une image de ma webcam à tout le salon (joint directement).
    /// Renvoie vrai s'il faut une image clé.
    pub fn send_camera(&self, frame: &screen::Frame) -> bool {
        let s = self.0.state.lock().unwrap();
        let Some(me) = s.me else { return false };
        let wire = screen::to_member(me, frame);
        let mut want_key = false;
        for peer in &s.room {
            if let Some(link) = s.links.get(peer) {
                want_key |= screen::offer(&link.conn, &link.camera, frame, &wire);
            }
        }
        want_key
    }

    /// Demande une image clé de la webcam de `peer`. Faux sans connexion directe.
    pub fn request_camera_key(&self, peer: PeerId) -> bool {
        let s = self.0.state.lock().unwrap();
        s.links
            .get(&peer)
            .is_some_and(|l| l.signals.send(Signal::CameraKey).is_ok())
    }

    /// Envoie une trame du son de mon partage à ceux qui le regardent directement.
    pub fn send_sound(&self, seq: u16, opus: &[u8]) {
        let s = self.0.state.lock().unwrap();
        let Some(me) = s.me else { return };
        let packet = screen::sound_to_member(screen::Sound { from: me, seq, opus });
        for w in &s.watchers {
            if let Some(link) = s.links.get(w) {
                let _ = link.conn.send_datagram(packet.clone());
            }
        }
    }

    /// Appelle ceux du salon avec qui on n'est pas connecté ; perce sa box
    /// vers ceux qui doivent nous appeler.
    fn reconcile(&self) {
        let mut dial = Vec::new();
        {
            let mut s = self.0.state.lock().unwrap();
            let (Some(me), Some(server)) = (s.me, s.server) else {
                return;
            };
            let my_user = self.0.identity.id();
            let now = Instant::now();
            let todo: Vec<Contact> = s
                .room
                .iter()
                .filter(|p| !s.links.contains_key(p))
                .filter(|p| s.tried.get(p).is_none_or(|t| now - *t >= RETRY))
                .filter_map(|p| s.contacts.get(p).cloned())
                .collect();
            for c in todo {
                s.tried.insert(c.peer, now);
                if my_user < c.user {
                    dial.push((c, me, server));
                } else {
                    // C'est lui qui appelle : on ouvre notre box vers lui.
                    for addr in &c.addrs {
                        announce::punch_for(&self.0.station.endpoint, *addr, 4);
                    }
                }
            }
        }
        for (contact, me, server) in dial {
            let mesh = self.clone();
            tokio::spawn(async move {
                let peer = contact.peer;
                match timeout(DIAL_TIMEOUT, mesh.dial(&contact, me, server)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => debug!("vocal direct vers {peer} : {e:#}"),
                    Err(_) => debug!("vocal direct vers {peer} : pas de réponse (on passe par l'hôte)"),
                }
            });
        }
    }

    async fn dial(&self, contact: &Contact, me: PeerId, server: ServerId) -> Result<()> {
        let config = tls::mesh_client_config(contact.cert)?;
        let conn = first_handshake(&self.0.station.endpoint, &contact.addrs, config).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        let hello = MeshHello {
            server,
            peer: me,
            user: self.0.identity.id(),
            proof: self.0.identity.sign(&tls::session_binding(&conn)?),
        };
        write_msg(&mut send, &hello).await?;
        match read_msg::<Signal>(&mut recv).await? {
            Some(Signal::Welcome) => {}
            other => bail!("refusé ({other:?})"),
        }
        info!("vocal direct avec {} ({})", contact.peer, conn.remote_address());
        self.register(contact.peer, conn, send, recv);
        Ok(())
    }

    async fn accept(&self, conn: Connection) -> Result<()> {
        let (mut send, mut recv) = timeout(Duration::from_secs(5), conn.accept_bi())
            .await
            .context("pas de présentation")??;
        let hello: MeshHello = read_msg(&mut recv).await?.context("flux fermé")?;
        {
            let s = self.0.state.lock().unwrap();
            ensure!(Some(hello.server) == s.server, "autre serveur");
            let known = s.contacts.get(&hello.peer).is_some_and(|c| c.user == hello.user);
            ensure!(known, "membre inconnu");
        }
        ensure!(
            hello.user.verify(&tls::session_binding(&conn)?, &hello.proof),
            "preuve d'identité invalide"
        );
        write_msg(&mut send, &Signal::Welcome).await?;
        info!("vocal direct avec {} ({})", hello.peer, conn.remote_address());
        self.register(hello.peer, conn, send, recv);
        Ok(())
    }

    /// Garde la connexion et lance ses lecteurs (voix, images, signaux).
    fn register(&self, peer: PeerId, conn: Connection, mut send: quinn::SendStream, mut recv: quinn::RecvStream) {
        let (signals, mut outgoing) = mpsc::unbounded_channel::<Signal>();
        {
            let mut s = self.0.state.lock().unwrap();
            // Je regardais son écran par l'hôte, ou avant que la connexion tombe.
            if s.watching == Some(peer) {
                let _ = signals.send(Signal::Watch(true));
            }
            if let Some(old) = s.links.insert(
                peer,
                Link {
                    conn: conn.clone(),
                    signals,
                    screen: Arc::default(),
                    camera: Arc::default(),
                },
            ) {
                old.conn.close(0u32.into(), b"remplacee");
            }
        }
        tokio::spawn(async move {
            while let Some(sig) = outgoing.recv().await {
                if write_msg(&mut send, &sig).await.is_err() {
                    break;
                }
            }
        });
        // Signaux reçus : qui regarde mon écran, qui a perdu le fil.
        let mesh = self.clone();
        tokio::spawn(async move {
            while let Ok(Some(sig)) = read_msg::<Signal>(&mut recv).await {
                let mut s = mesh.0.state.lock().unwrap();
                let lane = s.links.get(&peer).map(|l| l.screen.clone());
                match sig {
                    Signal::Watch(true) => {
                        s.watchers.insert(peer);
                    }
                    Signal::Watch(false) => {
                        s.watchers.remove(&peer);
                    }
                    // Il a perdu le fil de ma webcam : rien ne lui part avant une image clé.
                    Signal::CameraKey => {
                        if let Some(link) = s.links.get(&peer) {
                            link.camera.reset(Some(Instant::now()));
                            let _ = mesh.0.camera_keys.send(peer);
                        }
                    }
                    Signal::Key | Signal::Welcome => {}
                }
                // Il (re)commence : une image clé d'abord, demandée tout de suite.
                if matches!(sig, Signal::Watch(true) | Signal::Key)
                    && s.watchers.contains(&peer)
                    && let Some(lane) = lane
                {
                    lane.reset(Some(Instant::now()));
                    let _ = mesh.0.keys.send(peer);
                }
            }
        });
        // Images de son écran (si je le regarde) et de sa webcam.
        let (images, tx) = (conn.clone(), self.0.screens.clone());
        tokio::spawn(async move {
            while let Ok(mut stream) = images.accept_uni().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok(data) = screen::recv(&mut stream).await
                        && let Some((from, frame)) = screen::from_host(&data)
                        && from == peer
                    {
                        let _ = tx.send((from, frame));
                    }
                });
            }
        });
        // Voix, et son de son écran.
        let mesh = self.clone();
        tokio::spawn(async move {
            while let Ok(d) = conn.read_datagram().await {
                if let Some(sound) = screen::sound_from_host(&d) {
                    if sound.from == peer {
                        let _ = mesh.0.sounds.send(SoundIn::from(sound));
                    }
                } else if let Some(down) = voice::from_host(&d)
                    && down.from == peer
                {
                    let _ = mesh.0.voice.send(VoiceIn {
                        from: peer,
                        seq: down.frame.seq,
                        opus: down.frame.opus.to_vec(),
                        whisper: down.whisper,
                    });
                }
            }
            let mut s = mesh.0.state.lock().unwrap();
            if s.links
                .get(&peer)
                .is_some_and(|l| l.conn.stable_id() == conn.stable_id())
            {
                debug!("vocal direct avec {peer} coupé");
                s.links.remove(&peer);
                s.watchers.remove(&peer);
                s.tried.remove(&peer);
            }
        });
    }
}
