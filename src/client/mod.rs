//! Mode client : connexion à un hôte, envoi de commandes, réception d'événements.
//! Indépendant de l'interface : l'UI consomme les [`Event`] et appelle [`Client`].

pub mod failover;
mod join;
pub mod mesh;
pub mod station;
mod view;

pub use join::{Progress, endpoint as join_endpoint, join, join_host, join_on};
pub use view::{EVERYONE, ServerView};

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use quinn::{Connection, Endpoint};
use tokio::sync::mpsc;

use crate::core::{
    card::{AccessKey, ChannelId, Kind, Message, ServerCard, ServerId, SignedMessage, now_ms},
    files::{self, FileHash},
    framing::{read_packet, write_packet},
    identity::{Identity, UserId},
    limit::Bucket,
    nat,
    protocol::{
        CertFingerprint, Chat, Contact, Control, PROTOCOL_VERSION, Packet, PeerId, PeerInfo, RelayOffer, VoiceRoom,
        history_batches,
    },
    screen,
    tls::{self, Side},
    voice,
};

/// L'hôte a refusé la connexion (banni, version incompatible…) : inutile
/// d'insister ni d'héberger à sa place.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refusé par l'hôte : {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// Trop de messages d'un coup : réessayer après ce délai.
#[derive(Debug)]
pub struct TooFast(pub Duration);

impl std::fmt::Display for TooFast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "doucement : attends {:.0} s avant d'écrire à nouveau",
            self.0.as_secs_f32().ceil()
        )
    }
}

impl std::error::Error for TooFast {}

/// Ce que l'UI reçoit. Messages et cartes sont déjà vérifiés.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Text {
        from: PeerId,
        msg: SignedMessage,
    },
    /// Historique envoyé par l'hôte à l'arrivée (ou complété par un autre membre).
    History(Vec<SignedMessage>),
    /// Fin de l'historique envoyé à l'arrivée.
    HistoryEnd,
    /// Le serveur a changé (salons, admins, politique d'historique).
    CardUpdated(Box<ServerCard>),
    Voice(VoiceRoom),
    Pong {
        rtt: Duration,
    },
    PeerJoined(PeerInfo),
    PeerLeft {
        peer_id: PeerId,
    },
    /// Nouvel ordre de succession (qui prendra la relève si l'hôte part).
    Succession(Vec<Contact>),
    /// L'hôte part volontairement : relève immédiate.
    HostLeaving,
    /// Le serveur est hébergé ailleurs : le retrouver via l'annuaire.
    Moved,
    /// `candidate` demande à héberger : voter avant `secs` secondes.
    Vote {
        id: u64,
        candidate: PeerId,
        secs: u8,
    },
    VoteResult {
        id: u64,
        candidate: PeerId,
        accepted: bool,
        yes: u16,
        no: u16,
    },
    /// Expulsé ou banni par un admin : la session s'arrête.
    Kicked {
        reason: String,
    },
    /// L'hôte a ignoré mon dernier message (trop de messages d'un coup).
    SlowDown,
    /// L'hôte cherche ce fichier : l'envoyer si on l'a.
    FileWanted {
        hash: FileHash,
    },
    /// Signal non traité, remonté tel quel.
    Control(Control),
    Disconnected {
        reason: String,
    },
}

pub struct Client {
    pub peer_id: PeerId,
    pub host_id: PeerId,
    /// Pairs présents à l'arrivée (hors soi-même).
    pub initial_peers: Vec<PeerInfo>,
    /// Serveur tel que l'hôte l'a envoyé à l'arrivée.
    pub card: ServerCard,
    /// Salons vocaux occupés à l'arrivée.
    pub initial_voice: Vec<VoiceRoom>,
    /// Qui héberge (identité), si l'hôte le dit.
    pub host_user: Option<UserId>,
    identity: Identity,
    name: String,
    tx: mpsc::UnboundedSender<Packet>,
    pending_pings: Arc<Mutex<HashMap<u64, Instant>>>,
    next_nonce: u64,
    endpoint: Endpoint,
    conn: Connection,
    voice: Option<mpsc::UnboundedReceiver<VoiceIn>>,
    screens: Option<mpsc::UnboundedReceiver<(PeerId, screen::Frame)>>,
    sounds: Option<mpsc::UnboundedReceiver<SoundIn>>,
    /// Mes images envoyées à l'hôte (pour ceux qu'il relaie).
    screen: Arc<screen::Lane>,
    /// Pareil pour ma webcam.
    camera: Arc<screen::Lane>,
    /// Anti-spam : messages, puis réactions et modifications.
    limits: Mutex<(Bucket, Bucket)>,
    /// Pour connaître l'adresse publique de notre port (si l'endpoint le permet).
    pub(crate) probe: Option<nat::Probe>,
}

/// De quoi envoyer et télécharger des pièces jointes depuis une autre tâche.
#[derive(Clone)]
pub struct FileLink {
    conn: Connection,
}

impl FileLink {
    pub async fn put(&self, bytes: &[u8]) -> Result<()> {
        files::put(&self.conn, bytes).await
    }

    pub async fn get(&self, hash: FileHash) -> Result<Vec<u8>> {
        files::get(&self.conn, hash).await
    }
}

/// Trame de voix reçue d'un autre membre du salon vocal.
#[derive(Debug, Clone)]
pub struct VoiceIn {
    pub from: PeerId,
    pub seq: u16,
    pub opus: Vec<u8>,
    /// Chuchotée à quelques-uns (dont moi).
    pub whisper: bool,
}

/// Trame du son d'un partage d'écran regardé (Opus stéréo).
#[derive(Debug, Clone)]
pub struct SoundIn {
    pub from: PeerId,
    pub seq: u16,
    pub opus: Vec<u8>,
}

impl From<screen::Sound<'_>> for SoundIn {
    fn from(s: screen::Sound) -> Self {
        Self {
            from: s.from,
            seq: s.seq,
            opus: s.opus.to_vec(),
        }
    }
}

/// Options de connexion au-delà du strict nécessaire.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Clé d'accès du serveur (celle du code d'invitation) : on la prouve à
    /// l'hôte, et il doit la prouver à son tour. Sans elle, l'hôte refuse.
    pub key: Option<AccessKey>,
    /// N'accepte que ce certificat (successeur annoncé, code d'invitation,
    /// annuaire) : son empreinte, ou au moins ses 16 premiers octets.
    pub pin: Option<Vec<u8>>,
    /// Numéro à retrouver (relève d'hôte).
    pub resume: Option<PeerId>,
}

impl Client {
    /// Rejoint l'hôte à `addr` avec la clé d'accès `key` (celle du code).
    pub async fn connect(
        addr: SocketAddr,
        key: AccessKey,
        identity: &Identity,
        name: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let endpoint = client_endpoint(addr.is_ipv6())?;
        let opts = Options {
            key: Some(key),
            ..Options::default()
        };
        Self::connect_with(endpoint, addr, identity, name, &opts).await
    }

    /// Connexion depuis un endpoint existant (celui dont l'adresse publique a
    /// été annoncée à l'hôte, pour que la perforation de la box vise le bon port).
    pub async fn connect_with(
        endpoint: Endpoint,
        addr: SocketAddr,
        identity: &Identity,
        name: &str,
        opts: &Options,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let conn = handshake(&endpoint, addr, opts.pin.as_deref()).await?;
        Self::over(endpoint, conn, identity, name, opts).await
    }

    /// Se présente à l'hôte sur une connexion QUIC déjà établie.
    pub async fn over(
        endpoint: Endpoint,
        conn: Connection,
        identity: &Identity,
        name: &str,
        opts: &Options,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let Some(key) = opts.key else {
            conn.close(0u32.into(), b"pas de code");
            return Err(Refused(
                "Il faut le code d'invitation du serveur : une adresse IP seule ne suffit pas.".into(),
            )
            .into());
        };
        let (mut send, mut recv) = conn.open_bi().await?;

        write_packet(
            &mut send,
            &Control::Hello {
                name: name.into(),
                version: PROTOCOL_VERSION,
                user: identity.id(),
                proof: identity.sign(&tls::session_binding(&conn)?),
                resume: opts.resume,
                access: tls::access_proof(&conn, &key, Side::Member)?,
            }
            .into(),
        )
        .await?;
        let (peer_id, host_id, initial_peers, card, initial_voice, host_user, access) =
            match read_packet(&mut recv).await? {
                Some(Packet::Control(Control::Welcome {
                    peer_id,
                    host_id,
                    peers,
                    card,
                    voice,
                    host_user,
                    access,
                })) => (peer_id, host_id, peers, *card, voice, host_user, access),
                Some(Packet::Control(Control::Error { reason })) => return Err(Refused(reason).into()),
                Some(Packet::Control(Control::Kicked { reason })) => return Err(Refused(reason).into()),
                other => bail!("Welcome attendu, reçu {other:?}"),
            };
        if !tls::check_access(&conn, &key, Side::Host, &access) {
            conn.close(0u32.into(), b"mauvais hote");
            bail!("cet hôte ne connaît pas le code d'invitation : ce n'est pas le bon serveur");
        }
        card.verify().context("l'hôte a envoyé un serveur invalide")?;

        let (tx, mut out_rx) = mpsc::unbounded_channel::<Packet>();
        tokio::spawn(async move {
            while let Some(p) = out_rx.recv().await {
                if write_packet(&mut send, &p).await.is_err() {
                    break;
                }
            }
            let _ = send.finish();
        });

        let (voice_tx, voice_rx) = mpsc::unbounded_channel();
        let (sound_tx, sound_rx) = mpsc::unbounded_channel();
        let datagrams = conn.clone();
        tokio::spawn(async move {
            while let Ok(d) = datagrams.read_datagram().await {
                if let Some(sound) = screen::sound_from_host(&d) {
                    let _ = sound_tx.send(SoundIn::from(sound));
                } else if let Some(down) = voice::from_host(&d) {
                    let frame = VoiceIn {
                        from: down.from,
                        seq: down.frame.seq,
                        opus: down.frame.opus.to_vec(),
                        whisper: down.whisper,
                    };
                    if voice_tx.send(frame).is_err() {
                        break;
                    }
                }
            }
        });

        let (screen_tx, screen_rx) = mpsc::unbounded_channel();
        let images = conn.clone();
        tokio::spawn(async move {
            while let Ok(mut stream) = images.accept_uni().await {
                let tx = screen_tx.clone();
                tokio::spawn(async move {
                    if let Ok(data) = screen::recv(&mut stream).await
                        && let Some(frame) = screen::from_host(&data)
                    {
                        let _ = tx.send(frame);
                    }
                });
            }
        });

        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let pending_pings = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(read_loop(recv, card.id, tx.clone(), ev_tx, pending_pings.clone()));

        let client = Self {
            peer_id,
            host_id,
            initial_peers,
            card,
            initial_voice,
            host_user,
            identity: identity.clone(),
            name: name.into(),
            tx,
            pending_pings,
            next_nonce: 0,
            endpoint,
            conn,
            voice: Some(voice_rx),
            screens: Some(screen_rx),
            sounds: Some(sound_rx),
            screen: Arc::default(),
            camera: Arc::default(),
            limits: Mutex::new((Bucket::messages(), Bucket::actions())),
            probe: None,
        };
        Ok((client, ev_rx))
    }

    /// Signe et envoie un message ; le renvoie pour l'affichage local.
    pub fn send_text(&self, channel: ChannelId, body: impl Into<String>) -> Result<SignedMessage> {
        self.send_message(channel, body, Kind::Text)
    }

    /// Signe et envoie un message ou une action (réponse, réaction,
    /// modification…). Refusé avec [`TooFast`] si l'on écrit trop vite.
    pub fn send_message(&self, channel: ChannelId, body: impl Into<String>, kind: Kind) -> Result<SignedMessage> {
        {
            let mut limits = self.limits.lock().unwrap();
            let bucket = match kind {
                Kind::Text | Kind::Rich { .. } => &mut limits.0,
                _ => &mut limits.1,
            };
            if !bucket.take() {
                return Err(TooFast(bucket.wait()).into());
            }
        }
        let msg = Message {
            server: self.card.id,
            channel,
            author_name: self.name.clone(),
            at_ms: next_at_ms(),
            body: body.into(),
            kind,
        }
        .sign(&self.identity);
        self.send(
            Chat::Text {
                from: self.peer_id,
                msg: msg.clone(),
            }
            .into(),
        )?;
        Ok(msg)
    }

    /// Messages que l'hôte n'avait pas : il les ajoute et les rediffuse.
    pub fn backfill(&self, messages: Vec<SignedMessage>) -> Result<()> {
        for batch in history_batches(&messages) {
            self.send(Chat::History(batch).into())?;
        }
        Ok(())
    }

    /// Déconnecte ce membre (admins).
    pub fn kick(&self, peer: PeerId) -> Result<()> {
        self.send(Control::Kick { peer }.into())
    }

    /// Pour envoyer et télécharger des pièces jointes.
    pub fn files(&self) -> FileLink {
        FileLink {
            conn: self.conn.clone(),
        }
    }

    /// Propose une nouvelle version du serveur (modifiée et signée par un admin,
    /// ou copie locale plus récente que celle de l'hôte).
    pub fn update_card(&self, card: &ServerCard) -> Result<()> {
        self.send(Control::CardUpdate(Box::new(card.clone())).into())
    }

    pub fn join_voice(&self, channel: ChannelId) -> Result<()> {
        self.send(
            Control::VoiceJoin {
                channel,
                share: false,
                camera: false,
            }
            .into(),
        )
    }

    /// Rejoint un salon vocal en y partageant tout de suite son écran et sa
    /// webcam (reprise après une relève : les autres ne les voient pas s'arrêter).
    pub fn rejoin_voice(&self, channel: ChannelId, share: bool, camera: bool) -> Result<()> {
        self.send(Control::VoiceJoin { channel, share, camera }.into())
    }

    /// Propose à l'hôte de prendre la relève s'il disparaît.
    pub fn offer_relay(&self, offer: RelayOffer) -> Result<()> {
        self.send(Control::Relay(offer).into())
    }

    pub fn leave_voice(&self) -> Result<()> {
        self.send(Control::VoiceLeave.into())
    }

    /// Envoie une trame Opus au salon vocal où l'on se trouve, ou aux seuls
    /// `whisper` (perdue sans erreur si la connexion sature : la voix
    /// n'attend pas).
    pub fn send_voice(&self, seq: u16, opus: &[u8], route: voice::Route) {
        let _ = self.conn.send_datagram(voice::for_host(seq, opus, route));
    }

    /// Dit aux autres si mon micro / mon son est coupé.
    pub fn send_status(&self, muted: bool, deafened: bool) -> Result<()> {
        self.send(Control::Status { muted, deafened }.into())
    }

    /// Demande à héberger le serveur (les membres votent).
    pub fn request_host(&self) -> Result<()> {
        self.send(Control::HostRequest.into())
    }

    pub fn ballot(&self, id: u64, yes: bool) -> Result<()> {
        self.send(Control::Ballot { id, yes }.into())
    }

    /// Voix des autres membres du salon vocal (une seule fois).
    pub fn take_voice(&mut self) -> Option<mpsc::UnboundedReceiver<VoiceIn>> {
        self.voice.take()
    }

    pub fn share_screen(&self, on: bool) -> Result<()> {
        self.send(Control::ScreenShare(on).into())
    }

    pub fn watch_screen(&self, peer: Option<PeerId>) -> Result<()> {
        self.send(Control::ScreenWatch(peer).into())
    }

    /// Envoie une image de son écran à l'hôte, qui la relaie à ceux qui le
    /// regardent par lui. Si la connexion est en retard, les images sont
    /// sautées jusqu'à la prochaine image clé ; renvoie vrai s'il en faut une.
    pub fn send_screen(&self, frame: &screen::Frame) -> bool {
        screen::offer(&self.conn, &self.screen, frame, &screen::for_host(frame))
    }

    /// Envoie une trame du son de mon partage à l'hôte (perdue sans erreur
    /// si la connexion sature, comme la voix).
    pub fn send_sound(&self, seq: u16, opus: &[u8]) {
        let _ = self.conn.send_datagram(screen::sound_for_host(seq, opus));
    }

    /// Demande une image clé de l'écran de `sharer`, regardé par l'hôte.
    pub fn request_key(&self, sharer: PeerId) -> Result<()> {
        self.send(Control::ScreenKey(sharer).into())
    }

    /// Ma webcam est allumée ou éteinte (les autres le voient).
    pub fn share_camera(&self, on: bool) -> Result<()> {
        self.send(Control::Camera(on).into())
    }

    /// Webcams de mon salon à recevoir par l'hôte (pas de connexion directe).
    pub fn relay_cameras(&self, peers: Vec<PeerId>) -> Result<()> {
        self.send(Control::CameraRelay(peers).into())
    }

    /// Envoie une image de ma webcam à l'hôte, pour ceux qu'il relaie ;
    /// renvoie vrai s'il faut une image clé.
    pub fn send_camera(&self, frame: &screen::Frame) -> bool {
        screen::offer(&self.conn, &self.camera, frame, &screen::for_host(frame))
    }

    /// Demande une image clé de la webcam de `peer`, relayée par l'hôte.
    pub fn request_camera_key(&self, peer: PeerId) -> Result<()> {
        self.send(Control::CameraKey(peer).into())
    }

    /// Images des écrans regardés et des webcams relayées (une seule fois).
    pub fn take_screens(&mut self) -> Option<mpsc::UnboundedReceiver<(PeerId, screen::Frame)>> {
        self.screens.take()
    }

    /// Son des écrans regardés (une seule fois).
    pub fn take_sounds(&mut self) -> Option<mpsc::UnboundedReceiver<SoundIn>> {
        self.sounds.take()
    }

    /// Adresse de l'hôte auquel on est connecté.
    pub fn host_addr(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Empreinte du certificat présenté par l'hôte.
    pub fn host_cert(&self) -> Option<CertFingerprint> {
        let certs = self
            .conn
            .peer_identity()?
            .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
            .ok()?;
        certs.first().map(tls::fingerprint)
    }

    /// Endpoint de la connexion : c'est sur lui qu'on hébergera si l'on prend
    /// la relève (son port est déjà ouvert dans la box).
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn probe(&self) -> Option<nat::Probe> {
        self.probe.clone()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Envoie un ping ; la réponse arrive en [`Event::Pong`] avec le RTT.
    pub fn ping(&mut self) -> Result<()> {
        self.next_nonce += 1;
        self.pending_pings
            .lock()
            .unwrap()
            .insert(self.next_nonce, Instant::now());
        self.send(Control::Ping { nonce: self.next_nonce }.into())
    }

    /// Départ propre : prévient l'hôte puis ferme la connexion.
    pub async fn quit(self) {
        let _ = self.send(Control::Bye.into());
        drop(self.tx);
        // Laisse le temps au Bye de partir avant de fermer.
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.conn.close(0u32.into(), b"bye");
        self.endpoint.wait_idle().await;
    }

    fn send(&self, packet: Packet) -> Result<()> {
        self.tx.send(packet).context("connexion fermée")
    }
}

/// Poignée de main QUIC (sans encore se présenter).
pub async fn handshake(endpoint: &Endpoint, addr: SocketAddr, pin: Option<&[u8]>) -> Result<Connection> {
    let connecting = match pin {
        Some(pin) => endpoint.connect_with(tls::client_config_pinned(pin)?, addr, "localhost")?,
        None => endpoint.connect(addr, "localhost")?,
    };
    connecting
        .await
        .with_context(|| format!("connexion à {addr} impossible"))
}

pub(crate) fn client_endpoint(ipv6: bool) -> Result<Endpoint> {
    let bind: SocketAddr = if ipv6 { "[::]:0" } else { "0.0.0.0:0" }.parse()?;
    let mut endpoint = Endpoint::client(bind)?;
    endpoint.set_default_client_config(tls::client_config()?);
    Ok(endpoint)
}

async fn read_loop(
    mut recv: quinn::RecvStream,
    server: ServerId,
    tx: mpsc::UnboundedSender<Packet>,
    events: mpsc::UnboundedSender<Event>,
    pending_pings: Arc<Mutex<HashMap<u64, Instant>>>,
) {
    let reason = loop {
        let packet = match read_packet(&mut recv).await {
            Ok(Some(p)) => p,
            Ok(None) => break "l'hôte a fermé la session".to_string(),
            Err(e) => break format!("{e:#}"),
        };
        let event = match packet {
            Packet::Chat(Chat::Text { from, msg }) => match msg.verify(server) {
                Ok(()) => Event::Text { from, msg },
                Err(e) => {
                    tracing::warn!("message ignoré : {e:#}");
                    continue;
                }
            },
            Packet::Chat(Chat::History(messages)) => {
                Event::History(messages.into_iter().filter(|m| m.verify(server).is_ok()).collect())
            }
            Packet::Chat(Chat::HistoryEnd) => Event::HistoryEnd,
            Packet::Control(Control::Kicked { reason }) => Event::Kicked { reason },
            Packet::Control(Control::SlowDown) => Event::SlowDown,
            Packet::Control(Control::FileWanted { hash }) => Event::FileWanted { hash },
            Packet::Control(Control::CardUpdate(card)) => {
                if card.id != server || card.verify().is_err() {
                    tracing::warn!("mise à jour du serveur invalide ignorée");
                    continue;
                }
                Event::CardUpdated(card)
            }
            Packet::Control(Control::VoiceState(room)) => Event::Voice(room),
            Packet::Control(Control::Ping { nonce }) => {
                let _ = tx.send(Control::Pong { nonce }.into());
                continue;
            }
            Packet::Control(Control::Pong { nonce }) => match pending_pings.lock().unwrap().remove(&nonce) {
                Some(sent) => Event::Pong { rtt: sent.elapsed() },
                None => continue,
            },
            Packet::Control(Control::PeerJoined(info)) => Event::PeerJoined(info),
            Packet::Control(Control::PeerLeft { peer_id }) => Event::PeerLeft { peer_id },
            Packet::Control(Control::Succession(heirs)) => Event::Succession(heirs),
            Packet::Control(Control::HostLeaving) => Event::HostLeaving,
            Packet::Control(Control::Moved) => Event::Moved,
            Packet::Control(Control::Vote { id, candidate, secs }) => Event::Vote { id, candidate, secs },
            Packet::Control(Control::VoteResult {
                id,
                candidate,
                accepted,
                yes,
                no,
            }) => Event::VoteResult {
                id,
                candidate,
                accepted,
                yes,
                no,
            },
            Packet::Control(other) => Event::Control(other),
        };
        if events.send(event).is_err() {
            return;
        }
    };
    let _ = events.send(Event::Disconnected { reason });
}

/// Heure d'un message que j'écris : toujours après le précédent, pour que mes
/// messages gardent leur ordre d'envoi partout (deux dans la même
/// milliseconde se départageraient sinon au hasard de leur identifiant).
fn next_at_ms() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = now_ms();
    loop {
        let last = LAST.load(Ordering::Relaxed);
        let at = now.max(last + 1);
        if LAST
            .compare_exchange_weak(last, at, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return at;
        }
    }
}
