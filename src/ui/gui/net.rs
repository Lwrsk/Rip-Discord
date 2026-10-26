//! Côté réseau de l'interface : une session (héberger ou rejoindre), le son,
//! l'ouverture du port sur la box, et la relève quand l'hôte s'en va.
//!
//! Une session enchaîne des « liens » (une connexion à l'hôte du serveur) :
//! quand l'hôte part ou disparaît, on suit l'ordre de succession vers le lien
//! suivant sans fermer l'interface. Le vocal, lui, ne dépend pas de l'hôte :
//! il circule directement entre participants ([`Mesh`]) et continue pendant
//! la relève.

use std::{
    collections::{HashMap, HashSet},
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, mpsc as std_mpsc},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use eframe::egui;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};

use super::{Cmd, Draft, FileState, Invite, Target, Update};
use crate::{
    audio, capture,
    client::{
        self, Client, Event, FileLink, Options, Refused, ServerView, TooFast,
        failover::{self, Outcome, Plan},
        mesh::{Incoming, Mesh},
        station::Station,
    },
    core::{
        DEFAULT_PORT,
        card::{AccessKey, ChannelId, FileRef, Kind, MAX_FILE_BYTES, MAX_FILES, ServerCard, ServerId, plain_text},
        directory::{Directory, HostRecord, REFRESH, Slots},
        files,
        identity::{Identity, UserId},
        invite,
        nat::Mapping,
        protocol::{CertFingerprint, Contact, Control, PeerId, Reach, RelayOffer, VoiceRoom},
        screen,
        store::Store,
        voice::Route,
    },
    server::{
        Host, HostHandle,
        announce::{self, Announcement, Hosting as Announced},
        upnp,
    },
};

pub(super) struct Out {
    pub(super) tx: std_mpsc::Sender<Update>,
    pub(super) ctx: egui::Context,
}

impl Out {
    pub(super) fn send(&self, u: Update) {
        let _ = self.tx.send(u);
        self.ctx.request_repaint();
    }
}

/// L'annuaire décentralisé, rejoint dès le lancement de l'application (ça
/// prend quelques secondes) et partagé par toutes les sessions. S'il était
/// injoignable, on réessaie à la demande, au plus toutes les [`DIR_RETRY`].
#[derive(Clone, Default)]
struct Dir(Arc<tokio::sync::Mutex<(Option<Directory>, Option<Instant>)>>);

const DIR_RETRY: Duration = Duration::from_secs(20);

static DIR: OnceLock<Dir> = OnceLock::new();

/// Commence à rejoindre l'annuaire (dans le runtime tokio).
pub(super) fn warm_up() {
    Dir::shared().warm_up();
}

impl Dir {
    fn shared() -> Self {
        DIR.get_or_init(Dir::default).clone()
    }

    fn warm_up(&self) {
        let me = self.clone();
        tokio::spawn(async move { me.get().await });
    }

    async fn get(&self) -> Option<Directory> {
        let mut guard = self.0.lock().await;
        let (dir, failed) = &mut *guard;
        if dir.is_none() && failed.is_none_or(|t| t.elapsed() > DIR_RETRY) {
            *dir = Directory::start()
                .await
                .inspect_err(|e| tracing::warn!("annuaire : {e:#}"))
                .ok();
            *failed = dir.is_none().then(Instant::now);
        }
        dir.clone()
    }
}

/// Prochaine étape d'une session.
enum Step {
    Join(invite::Invite),
    /// Ouvrir le serveur : rejoindre son hôte du moment, sinon l'héberger.
    Open(Box<ServerCard>),
    /// L'hôte est parti : suivre l'ordre de succession.
    Failover(Plan),
}

/// Comment la session s'est arrêtée sur un lien.
enum End {
    Left,
    Lost(Plan),
    Open(Box<ServerCard>),
    /// Expulsé ou banni : on part sans chercher à revenir.
    Kicked(String),
}

/// Message dont les pièces jointes sont parties chez l'hôte : reste à le
/// signer et l'envoyer.
struct Prepared {
    channel: ChannelId,
    body: String,
    kind: Kind,
}

/// Ce que l'hébergement fait tourner, à arrêter en partant.
struct Hosting {
    handle: HostHandle,
    relay: JoinHandle<Result<()>>,
    announcement: Option<Announcement>,
    /// L'annonce se fait en tâche de fond : on est connecté à son propre
    /// serveur sans attendre l'annuaire.
    announcing: Option<oneshot::Receiver<Result<Announced>>>,
    upnp: Option<(oneshot::Sender<()>, JoinHandle<()>)>,
    port: u16,
    /// Certificat présenté aux arrivants (mis dans les codes d'invitation).
    cert: CertFingerprint,
    /// Hébergé sur ma station (relève) : à libérer en partant.
    station: Option<Station>,
}

impl Hosting {
    async fn stop(self, leave: Option<bool>) {
        let alone = self.handle.remote_members() == 0;
        if let Some(moved) = leave {
            self.handle.leave(moved).await;
        }
        if let Some((stop, task)) = self.upnp {
            let _ = stop.send(());
            let _ = timeout(Duration::from_secs(3), task).await;
        }
        if let Some(a) = self.announcement {
            a.stop(alone).await;
        }
        if let Some(s) = self.station {
            s.stop_hosting();
        }
        self.relay.abort();
    }
}

/// Une connexion à l'hôte du serveur (éventuellement le nôtre).
struct Link {
    client: Client,
    events: mpsc::UnboundedReceiver<Event>,
    hosting: Option<Hosting>,
}

/// Ce qui ne dépend pas de l'hôte : qui je suis, mon port, ce qu'on sait des autres.
struct Ctx {
    name: String,
    store: Store,
    me: Identity,
    station: Station,
    dir: Dir,
    peers: HashMap<PeerId, (UserId, String)>,
    contacts: Vec<Contact>,
    max_peer: PeerId,
    /// Serveur de la session (connu après la première connexion).
    server: Option<ServerId>,
    /// Messages avec pièces jointes, prêts à partir.
    prepared: mpsc::UnboundedSender<Prepared>,
}

impl Ctx {
    /// Affiche une étape de la connexion : au centre la première fois, dans
    /// le bandeau de reconnexion ensuite.
    fn step(&self, out: &Out, text: impl Into<String>) {
        let text = text.into();
        out.send(if self.server.is_some() {
            Update::Reconnecting(Some(text))
        } else {
            Update::Status(text)
        });
    }

    fn latest_card(&self) -> Option<ServerCard> {
        self.store.card(self.server?)
    }

    fn peer_name(&self, id: PeerId) -> String {
        self.peers
            .get(&id)
            .map_or_else(|| "un membre".into(), |(_, n)| n.clone())
    }

    fn see_peer(&mut self, id: PeerId, user: UserId, name: String) {
        self.max_peer = self.max_peer.max(id);
        self.peers.insert(id, (user, name));
    }
}

/// Le vocal : micro, haut-parleurs, connexions directes, écran partagé, webcams.
/// Survit aux changements d'hôte du serveur.
struct Voice {
    prefs: audio::Prefs,
    sound: Sound,
    mesh: Mesh,
    incoming: Incoming,
    /// Touche maintenue : `Some(None)` appui-pour-parler, `Some(Some(…))`
    /// chuchotement à ces personnes.
    talk: Option<Option<Vec<UserId>>>,
    sharer: Option<capture::Sharer>,
    /// Numéros des images et du son de mon partage (continus d'une source à l'autre).
    screen_seq: u32,
    sound_seq: u16,
    /// Décode les écrans regardés (et l'aperçu du mien) hors du fil réseau.
    viewer: capture::video::Viewer,
    /// Le décodage a perdu le fil de cet écran : redemander une image clé.
    asks: mpsc::UnboundedReceiver<PeerId>,
    /// Aperçu de mon partage : mes propres images, décodées comme celles des autres.
    preview: bool,
    /// Écran regardé.
    watching: Option<PeerId>,
    /// … par l'hôte du serveur (pas de connexion directe avec le partageur).
    via_host: bool,
    /// Mon numéro (pour l'aperçu de mon écran).
    me: PeerId,
    /// Ma webcam, quand elle est allumée.
    camera: Option<capture::camera::Camera>,
    /// Numéro des images de ma webcam (continu d'une webcam à l'autre).
    camera_seq: u32,
    /// Décode les webcams du salon (et l'aperçu de la mienne).
    faces: capture::video::Viewer,
    /// Le décodage a perdu le fil de cette webcam.
    face_asks: mpsc::UnboundedReceiver<PeerId>,
    /// Les autres webcams allumées dans mon salon.
    cameras: Vec<PeerId>,
    /// … que l'hôte me relaie (pas de connexion directe avec leur propriétaire).
    relayed: Vec<PeerId>,
}

impl Voice {
    /// Envoie une trame du micro : directement aux participants, et par
    /// l'hôte du serveur à ceux qu'on ne joint pas directement.
    fn send(&self, seq: u16, opus: &[u8], client: Option<&Client>, peers: &HashMap<PeerId, (UserId, String)>) {
        match &self.talk {
            Some(Some(users)) => {
                let targets: Vec<PeerId> = self
                    .sound
                    .members
                    .iter()
                    .copied()
                    .filter(|p| peers.get(p).is_some_and(|(u, _)| users.contains(u)))
                    .collect();
                // Personne de la liste n'est là : rien ne part.
                if targets.is_empty() {
                    return;
                }
                let missing = self.mesh.send_voice(seq, opus, Some(&targets));
                if let Some(c) = client
                    && !missing.is_empty()
                {
                    c.send_voice(seq, opus, Route::Whisper(&missing));
                }
            }
            _ => {
                let missing = self.mesh.send_voice(seq, opus, None);
                if let Some(c) = client
                    && !missing.is_empty()
                {
                    c.send_voice(seq, opus, Route::Relay(&missing));
                }
            }
        }
    }

    fn share(&mut self, shot: Option<capture::Shot>, client: Option<&Client>, out: &Out) {
        match shot {
            Some(capture::Shot::Frame(f)) => self.share_frame(f, client),
            Some(capture::Shot::Sound(opus)) => self.share_sound(&opus, client),
            None => self.share_lost(client, out),
        }
    }

    fn share_frame(&mut self, encoded: capture::video::Encoded, client: Option<&Client>) {
        let frame = screen::Frame {
            seq: self.screen_seq,
            key: encoded.key,
            camera: false,
            data: encoded.data.into(),
        };
        self.screen_seq = self.screen_seq.wrapping_add(1);
        let mut want_key = self.mesh.send_screen(&frame);
        // Quelqu'un du salon n'est joignable que par l'hôte : il y passe aussi.
        if let Some(c) = client
            && !self.mesh.all_linked()
        {
            want_key |= c.send_screen(&frame);
        }
        if self.preview {
            self.viewer.push(self.me, frame);
        }
        // Un spectateur a raté des images : il attend la prochaine image clé.
        if want_key {
            self.request_key();
        }
    }

    fn share_sound(&mut self, opus: &[u8], client: Option<&Client>) {
        let seq = self.sound_seq;
        self.sound_seq = seq.wrapping_add(1);
        self.mesh.send_sound(seq, opus);
        if let Some(c) = client
            && !self.mesh.all_linked()
        {
            c.send_sound(seq, opus);
        }
    }

    /// Une image clé de mon partage au plus vite.
    fn request_key(&self) {
        if let Some(s) = &self.sharer {
            s.request_key();
        }
    }

    /// La source partagée a disparu (fenêtre fermée, écran débranché).
    fn share_lost(&mut self, client: Option<&Client>, out: &Out) {
        self.sharer = None;
        if let Some(c) = client {
            let _ = c.share_screen(false);
        }
        out.send(Update::Sharing(None));
        out.send(Update::Notice(
            "Partage arrêté : la fenêtre ou l'écran partagé n'est plus disponible.".into(),
        ));
    }

    fn show(&self, from: PeerId, frame: screen::Frame) {
        if frame.camera {
            if self.cameras.contains(&from) {
                self.faces.push(from, frame);
            }
        } else if self.watching == Some(from) {
            self.viewer.push(from, frame);
        }
    }

    /// Image suivante de ma webcam : directement à tout le salon, et par
    /// l'hôte à ceux qu'on ne joint pas directement.
    fn camera_frame(&mut self, encoded: Option<capture::video::Encoded>, client: Option<&Client>, out: &Out) {
        let Some(encoded) = encoded else {
            self.camera = None;
            if let Some(c) = client {
                let _ = c.share_camera(false);
            }
            out.send(Update::Camera(false));
            out.send(Update::Notice(
                "Webcam coupée : elle n'est plus disponible (débranchée ?).".into(),
            ));
            return;
        };
        let frame = screen::Frame {
            seq: self.camera_seq,
            key: encoded.key,
            camera: true,
            data: encoded.data.into(),
        };
        self.camera_seq = self.camera_seq.wrapping_add(1);
        let mut want_key = self.mesh.send_camera(&frame);
        if let Some(c) = client
            && !self.mesh.all_linked()
        {
            want_key |= c.send_camera(&frame);
        }
        // Mon aperçu : ce que les autres reçoivent.
        self.faces.push(self.me, frame);
        if want_key {
            self.camera_key();
        }
    }

    fn camera_key(&self) {
        if let Some(c) = &self.camera {
            c.request_key();
        }
    }

    /// Le décodage a perdu le fil de la webcam de `peer` : image clé, par le
    /// chemin que prennent ses images.
    fn ask_face_key(&self, peer: PeerId, client: Option<&Client>) {
        if peer == self.me {
            self.camera_key();
        } else if self.cameras.contains(&peer)
            && !self.mesh.request_camera_key(peer)
            && let Some(c) = client
        {
            let _ = c.request_camera_key(peer);
        }
    }

    /// Les webcams qu'on ne reçoit pas directement passent par l'hôte.
    fn camera_path(&mut self, client: &Client) {
        let want: Vec<PeerId> = self.cameras.iter().copied().filter(|p| !self.mesh.linked(*p)).collect();
        if want != self.relayed {
            let _ = client.relay_cameras(want.clone());
            self.relayed = want;
        }
    }

    /// Allume ma webcam (ou passe sur celle choisie dans les réglages).
    fn start_camera(&mut self, client: &Client, out: &Out) {
        // Libérée avant d'en rouvrir une.
        let was_on = self.camera.take().is_some();
        let wanted = self.prefs.camera.clone();
        match tokio::task::block_in_place(|| capture::camera::Camera::start(wanted.as_deref())) {
            Ok(c) => {
                tracing::info!("webcam allumée : {}", c.name);
                self.camera = Some(c);
                if !was_on {
                    let _ = client.share_camera(true);
                }
                out.send(Update::Camera(true));
            }
            Err(e) => {
                tracing::warn!("webcam : {e:#}");
                if was_on {
                    let _ = client.share_camera(false);
                }
                out.send(Update::Camera(false));
                out.send(Update::Notice(format!("Webcam impossible : {e:#}")));
            }
        }
    }

    fn hear(&self, s: client::SoundIn) {
        if self.watching == Some(s.from) {
            self.sound.play_share(&s);
        }
    }

    /// Le décodage a perdu le fil de l'écran de `peer` : on redemande une
    /// image clé, par le chemin que prennent ses images.
    fn ask_key(&self, peer: PeerId, client: Option<&Client>) {
        if peer == self.me {
            self.request_key();
        } else if self.watching == Some(peer) {
            let direct = !self.via_host && self.mesh.request_key(peer);
            if let Some(c) = client
                && !direct
            {
                let _ = c.request_key(peer);
            }
        }
    }

    /// L'écran regardé passe directement dès que la connexion directe
    /// existe (elle redemande l'écran d'elle-même), par l'hôte sinon.
    fn watch_path(&mut self, client: &Client) {
        let Some(w) = self.watching else { return };
        let direct = self.mesh.linked(w);
        if direct && self.via_host {
            let _ = client.watch_screen(None);
            self.via_host = false;
        } else if !direct && !self.via_host {
            let _ = client.watch_screen(Some(w));
            self.via_host = true;
        }
    }

    /// Pendant une relève : le vocal continue de PC à PC.
    async fn pump(&mut self, peers: &HashMap<PeerId, (UserId, String)>, out: &Out) {
        loop {
            tokio::select! {
                Some((seq, opus)) = self.sound.next_frame() => self.send(seq, &opus, None, peers),
                Some(v) = self.incoming.voice.recv() => self.sound.play(&v),
                shot = next_shot(&mut self.sharer) => self.share(shot, None, out),
                Some((from, frame)) = self.incoming.screens.recv() => self.show(from, frame),
                Some(s) = self.incoming.sounds.recv() => self.hear(s),
                Some(_) = self.incoming.keys.recv() => self.request_key(),
                Some(p) = self.asks.recv() => self.ask_key(p, None),
                frame = next_face(&mut self.camera) => self.camera_frame(frame, None, out),
                Some(_) = self.incoming.camera_keys.recv() => self.camera_key(),
                Some(p) = self.face_asks.recv() => self.ask_face_key(p, None),
                else => std::future::pending::<()>().await,
            }
        }
    }
}

/// Déroulé complet d'une session, de la connexion au départ.
pub(super) async fn session(
    target: Target,
    name: String,
    store: Store,
    prefs: audio::Prefs,
    me: Identity,
    mut cmd: mpsc::UnboundedReceiver<Cmd>,
    out: &Out,
) -> Result<()> {
    let station = Station::open().context("impossible d'ouvrir un port réseau")?;
    let (mesh, incoming) = Mesh::start(station.clone(), me.clone());
    let (prepared, mut ready) = mpsc::unbounded_channel();
    let (asks_tx, asks) = mpsc::unbounded_channel();
    let screens = Out {
        tx: out.tx.clone(),
        ctx: out.ctx.clone(),
    };
    let slot = super::ScreenSlot::default();
    let viewer = capture::video::Viewer::start(
        move |from, p| {
            let image = color_image(p);
            // L'interface n'est prévenue que si elle a déjà pris l'image d'avant.
            if slot.lock().unwrap().replace((from, image)).is_none() {
                screens.send(Update::Screen(slot.clone()));
            }
        },
        move |from| {
            let _ = asks_tx.send(from);
        },
    )?;
    let (face_asks_tx, face_asks) = mpsc::unbounded_channel();
    let faces_out = Out {
        tx: out.tx.clone(),
        ctx: out.ctx.clone(),
    };
    let faces_slot = super::FaceSlot::default();
    let faces = capture::video::Viewer::start(
        move |from, p| {
            let image = color_image(p);
            // Une image en attente par webcam, au plus.
            let mut waiting = faces_slot.lock().unwrap();
            let first = waiting.is_empty();
            waiting.insert(from, image);
            if first {
                faces_out.send(Update::Faces(faces_slot.clone()));
            }
        },
        move |from| {
            let _ = face_asks_tx.send(from);
        },
    )?;
    let mut ctx = Ctx {
        name,
        store,
        me,
        station,
        dir: Dir::shared(),
        peers: HashMap::new(),
        contacts: Vec::new(),
        max_peer: 0,
        server: None,
        prepared,
    };
    let mut voice = Voice {
        prefs,
        sound: Sound::default(),
        mesh,
        incoming,
        talk: None,
        sharer: None,
        screen_seq: 0,
        sound_seq: 0,
        viewer,
        asks,
        preview: false,
        watching: None,
        via_host: false,
        me: 0,
        camera: None,
        camera_seq: 0,
        faces,
        face_asks,
        cameras: Vec::new(),
        relayed: Vec::new(),
    };
    let mut backlog = Vec::new();
    let mut step = match target {
        Target::Join(code) => Step::Join(code),
        Target::Host(card) => {
            ctx.dir.warm_up();
            Step::Open(card)
        }
    };
    loop {
        let link = {
            let ctx = &ctx;
            let connecting = async {
                match step {
                    Step::Join(code) => {
                        let progress = |s: String| ctx.step(out, s);
                        // Le port de la session est en IPv4.
                        let (endpoint, probe) = match code.direct {
                            Some(SocketAddr::V6(_)) => client::join_endpoint(true)?,
                            _ => (ctx.station.endpoint.clone(), ctx.station.probe.clone()),
                        };
                        let dir = ctx.dir.clone();
                        let (client, events) = client::join_on(
                            endpoint,
                            probe,
                            &code,
                            &ctx.me,
                            &ctx.name,
                            async move { dir.get().await },
                            &progress,
                        )
                        .await?;
                        Ok(Link {
                            client,
                            events,
                            hosting: None,
                        })
                    }
                    Step::Open(card) => open(*card, ctx, out).await,
                    Step::Failover(plan) => failover(plan, ctx, out).await,
                }
            };
            // Pendant la connexion, seul « Quitter » compte (le reste attend),
            // et le vocal continue de PC à PC.
            tokio::select! {
                link = connecting => link,
                () = wait_leave(&mut cmd, &mut backlog) => return Ok(()),
                () = voice.pump(&ctx.peers, out) => unreachable!(),
            }
        };
        let link = match link {
            Ok(link) => link,
            Err(e) if ctx.server.is_some() && e.downcast_ref::<Refused>().is_none() => {
                // Déjà connecté une fois : on garde l'interface et on réessaie
                // via l'annuaire plutôt que de tout fermer.
                tracing::warn!("reconnexion : {e:#}");
                let card = ctx.latest_card().context("serveur inconnu")?;
                out.send(Update::Reconnecting(Some(format!(
                    "Hôte introuvable ({}), nouvelle tentative…",
                    short_error(&e)
                ))));
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    () = wait_leave(&mut cmd, &mut backlog) => return Ok(()),
                }
                step = Step::Open(Box::new(card));
                continue;
            }
            Err(e) => return Err(e),
        };
        ctx.dir.warm_up();
        step = match run(&mut ctx, &mut voice, &mut backlog, link, &mut cmd, &mut ready, out).await? {
            End::Left => return Ok(()),
            End::Kicked(reason) => bail!(reason),
            End::Lost(plan) => Step::Failover(plan),
            End::Open(card) => Step::Open(card),
        };
    }
}

/// Première ligne d'une erreur, pour un bandeau.
fn short_error(e: &anyhow::Error) -> String {
    let text = e.to_string();
    text.split(['.', '\n']).next().unwrap_or(&text).trim().to_string()
}

/// Attend « Quitter » ; les autres commandes sont gardées pour plus tard.
async fn wait_leave(cmd: &mut mpsc::UnboundedReceiver<Cmd>, backlog: &mut Vec<Cmd>) {
    loop {
        match cmd.recv().await {
            Some(Cmd::Leave) | None => return,
            Some(c) => backlog.push(c),
        }
    }
}

/// Ouvre le serveur : rejoint l'hôte du moment s'il y en a un, sinon l'héberge.
async fn open(card: ServerCard, ctx: &Ctx, out: &Out) -> Result<Link> {
    let me = &ctx.me;
    ensure!(!card.is_banned(&me.id()), "Tu as été banni de ce serveur.");
    ctx.step(out, "Recherche de l'hôte actuel dans l'annuaire…");
    let dir = ctx.dir.get().await;
    // Hôte annoncé mais injoignable : on remplacera son annonce sans attendre
    // qu'elle expire (sinon on repartirait le chercher en boucle).
    let mut replacing = false;
    if let Some(dir) = &dir
        && let Some((_, other)) = dir.lookup(&Slots::new(card.access_key())).await
        && other.user != me.id()
        && other.is_fresh()
    {
        // Annonce pas rafraîchie à temps : l'hôte est sans doute parti sans
        // prévenir (plantage, PC éteint). On ne l'attend pas longtemps.
        let patience = if other.age() > REFRESH + Duration::from_secs(5) {
            Duration::from_secs(4)
        } else {
            Duration::from_secs(10)
        };
        match join_member(dir, &card, &other, patience, ctx, out).await {
            Ok(link) => return Ok(link),
            Err(e) if e.downcast_ref::<Refused>().is_some() => return Err(e),
            // Hôte parti sans prévenir : on prend le relais.
            Err(e) => {
                tracing::warn!("hôte annoncé injoignable : {e:#}");
                replacing = true;
                ctx.step(out, "L'hôte annoncé ne répond pas : ton PC prend le relais…");
            }
        }
    }

    // Le client local écrit l'historique sur disque ; l'hôte en garde une copie en mémoire.
    let history = ctx.store.history(&card).detached();
    let host = Host::bind(([0, 0, 0, 0], DEFAULT_PORT).into(), card.clone(), history.detached())
        .or_else(|_| Host::bind(([0, 0, 0, 0], 0).into(), card.clone(), history))
        .context("impossible d'ouvrir le serveur")?
        .with_store(ctx.store.clone())
        .with_operator(me.id());
    let port = host.local_addr()?.port();
    let cert = host.fingerprint();

    // L'annonce dans l'annuaire (quelques secondes) se fait pendant qu'on se
    // connecte : si un autre membre a ouvert le serveur au même instant, on
    // le rejoindra après coup.
    let handle = host.handle();
    let announcing = dir.is_some().then(|| {
        spawn_announce(
            ctx.dir.clone(),
            host.endpoint(),
            host.probe(),
            cert,
            key_source(&handle),
            me.id(),
            replacing,
        )
    });
    let upnp = announcing
        .is_none()
        .then(|| spawn_port_mapping(port, cert, card.access_key(), None, out));

    let relay = tokio::spawn(host.run());
    out.send(Update::Invite(Invite::Searching));
    let local: SocketAddr = ([127, 0, 0, 1], port).into();
    ctx.step(out, "Ouverture de ton serveur…");
    let hosting = Hosting {
        handle,
        relay,
        announcement: None,
        announcing,
        upnp,
        port,
        cert,
        station: None,
    };
    // Mon client passe par ma station : si je passe la main, il suit la
    // relève comme les autres.
    let opts = Options {
        key: Some(card.access_key()),
        ..Options::default()
    };
    let connect = Client::connect_with(ctx.station.endpoint.clone(), local, me, &ctx.name, &opts);
    match connect.await {
        Ok((client, events)) => Ok(Link {
            client,
            events,
            hosting: Some(hosting),
        }),
        Err(e) => {
            hosting.stop(None).await;
            Err(e)
        }
    }
}

/// Clé d'accès en vigueur chez l'hôte (pour l'annonce dans l'annuaire).
fn key_source(handle: &HostHandle) -> announce::KeySource {
    let handle = handle.clone();
    std::sync::Arc::new(move || handle.access_key())
}

/// Annonce l'hôte dans l'annuaire en tâche de fond ; le résultat arrive dans
/// la boucle du lien.
fn spawn_announce(
    dir: Dir,
    endpoint: quinn::Endpoint,
    probe: crate::core::nat::Probe,
    cert: CertFingerprint,
    key: announce::KeySource,
    me: UserId,
    takeover: bool,
) -> oneshot::Receiver<Result<Announced>> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let announced = match dir.get().await {
            Some(dir) => announce::announce_on(&dir, endpoint, probe, cert, key, me, takeover).await,
            None => Err(anyhow!("annuaire injoignable")),
        };
        let _ = tx.send(announced);
    });
    rx
}

async fn join_member(
    dir: &Directory,
    card: &ServerCard,
    other: &HostRecord,
    patience: Duration,
    ctx: &Ctx,
    out: &Out,
) -> Result<Link> {
    ctx.step(out, "Un autre membre héberge déjà ce serveur : connexion chez lui…");
    let (endpoint, probe) = (ctx.station.endpoint.clone(), ctx.station.probe.clone());
    let progress = |s: String| ctx.step(out, s);
    let (client, events) = client::join_host(
        dir,
        endpoint,
        probe,
        card.access_key(),
        other,
        &ctx.me,
        &ctx.name,
        patience,
        &progress,
    )
    .await?;
    Ok(Link {
        client,
        events,
        hosting: None,
    })
}

/// L'hôte est parti : rejoindre son successeur, ou prendre la relève.
async fn failover(plan: Plan, ctx: &Ctx, out: &Out) -> Result<Link> {
    let first = plan.first_heir().map(|h| ctx.peer_name(h.peer));
    out.send(Update::Reconnecting(Some(match (&first, plan.my_rank()) {
        (_, Some(0)) => "L'hôte est parti : ton PC prend la relève…".to_string(),
        (Some(name), _) => format!("L'hôte est parti : {name} prend la relève…"),
        (None, _) => "L'hôte est parti : recherche d'un nouvel hôte…".to_string(),
    })));
    let outcome = failover::follow(&plan, &ctx.station.endpoint, &ctx.me, &ctx.name, |h| {
        let name = ctx.peer_name(h.peer);
        tracing::info!("relève : essai de {name}");
        ctx.step(out, format!("L'hôte est parti : connexion chez {name}…"));
    })
    .await;
    match outcome {
        Outcome::Joined(client, events) => Ok(Link {
            client: *client,
            events,
            hosting: None,
        }),
        Outcome::TakeOver => take_over(&plan, ctx, out).await,
        Outcome::Lost => {
            let card = ctx.latest_card().context("serveur inconnu")?;
            open(card, ctx, out).await
        }
    }
}

/// Héberge sur ma station, perce ma box vers les autres membres et m'y
/// connecte moi-même.
async fn take_over(plan: &Plan, ctx: &Ctx, out: &Out) -> Result<Link> {
    let card = ctx.latest_card().context("serveur inconnu")?;
    tracing::info!("je prends la relève de l'hôte");
    let station = &ctx.station;
    let history = ctx.store.history(&card).detached();
    let host = Host::on_station(station, card.clone(), history, ctx.max_peer + 1)?
        .with_store(ctx.store.clone())
        .with_operator(ctx.me.id());
    let handle = host.handle();
    // Les autres viennent vers moi : j'ouvre ma box vers chacun d'eux.
    for c in plan.contacts.iter().filter(|c| c.peer != plan.me) {
        for addr in &c.addrs {
            announce::punch_for(&station.endpoint, *addr, 12);
        }
    }
    let relay = tokio::spawn(host.run());
    let port = station.port();
    let local = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let opts = Options {
        key: Some(card.access_key()),
        pin: Some(station.cert.fingerprint.to_vec()),
        resume: Some(plan.me),
    };
    let hosting = Hosting {
        handle,
        relay,
        announcement: None,
        announcing: None,
        upnp: None,
        port,
        cert: station.cert.fingerprint,
        station: Some(station.clone()),
    };
    let (client, events) = match Client::connect_with(station.endpoint.clone(), local, &ctx.me, &ctx.name, &opts).await
    {
        Ok(c) => c,
        Err(e) => {
            hosting.stop(None).await;
            return Err(e);
        }
    };

    // Annonce dans l'annuaire pour ceux qui arrivent ensuite (ou qui ont raté la relève).
    let rx = spawn_announce(
        ctx.dir.clone(),
        station.endpoint.clone(),
        station.probe.clone(),
        station.cert.fingerprint,
        key_source(&hosting.handle),
        ctx.me.id(),
        true,
    );
    out.send(Update::Invite(Invite::Searching));
    Ok(Link {
        client,
        events,
        hosting: Some(Hosting {
            announcing: Some(rx),
            ..hosting
        }),
    })
}

/// Boucle d'un lien : commandes de l'UI, événements réseau, voix, ping.
async fn run(
    ctx: &mut Ctx,
    voice: &mut Voice,
    backlog: &mut Vec<Cmd>,
    link: Link,
    cmd: &mut mpsc::UnboundedReceiver<Cmd>,
    ready: &mut mpsc::UnboundedReceiver<Prepared>,
    out: &Out,
) -> Result<End> {
    let Link {
        mut client,
        mut events,
        mut hosting,
    } = link;
    let hosting_now = hosting.is_some();
    let first = ctx.server.is_none();
    ctx.server = Some(client.card.id);
    ctx.peers.clear();
    for p in &client.initial_peers {
        ctx.see_peer(p.id, p.user, p.name.clone());
    }
    let me = client.peer_id;
    ctx.see_peer(me, ctx.me.id(), ctx.name.clone());
    voice.me = me;
    voice.mesh.set_session(client.card.id, me);
    out.send(Update::Connected {
        host: client.host_addr(),
        hosting: hosting_now,
    });
    // Clé du code affiché : s'il change, le nouveau code remplace l'ancien.
    let mut shown_key = client.card.access_key();
    // Tout membre peut inviter : le code mène à l'hôte du moment.
    if !hosting_now {
        out.send(Update::Invite(Invite::Anywhere {
            code: invite::encode(&invite::Invite::key(client.card.access_key())),
            warning: None,
        }));
    }
    out.send(Update::Joined(Box::new(ServerView::new(
        &client,
        &ctx.name,
        Some(ctx.store.clone()),
    ))));
    if !first {
        out.send(Update::Reconnecting(None));
    }

    // Juste après une relève, les membres « reviennent » un à un : pas de sons
    // d'arrivée ni de départ.
    if !first {
        voice.sound.quiet_until = Some(Instant::now() + Duration::from_secs(15));
        // Ni coupure du vocal direct avec ceux qui ne sont pas encore revenus.
        voice.mesh.hold_room(Duration::from_secs(15));
    }
    // Reprise après une relève : même salon vocal, même partage. Le micro n'a
    // jamais été refermé et la voix a continué de PC à PC.
    if let Some(room) = voice.sound.room {
        voice.sound.pending = Some(room);
        let _ = client.rejoin_voice(room, voice.sharer.is_some(), voice.camera.is_some());
    }
    // Le nouvel hôte ne relaie encore aucune webcam.
    voice.relayed.clear();
    let _ = client.send_status(voice.sound.muted, voice.sound.deafened);
    // Regardé par l'hôte (pas de connexion directe) : à redemander.
    let mut rewatch = voice.watching.filter(|w| !voice.mesh.linked(*w));
    // Le nouvel hôte ne sait pas encore ce que je regarde.
    voice.via_host = false;

    // Proposer mes adresses (vocal direct, relève) : STUN sur ma station.
    let (offer_tx, offer_rx) = oneshot::channel();
    let mut offer_rx = Some(offer_rx);
    {
        let station = ctx.station.clone();
        tokio::spawn(async move {
            let _ = offer_tx.send(relay_offer(&station).await);
        });
    }

    let mut superseded = hosting
        .as_ref()
        .and_then(|h| h.announcement.as_ref())
        .map(|a| a.superseded.clone())
        .unwrap_or_else(|| watch::channel(None).1);
    let mut announcing = hosting.as_mut().and_then(|h| h.announcing.take());

    let mut ping = tokio::time::interval(Duration::from_secs(2));
    let mut voice_in = client.take_voice().expect("voix déjà prise");
    let mut screens = client.take_screens().expect("écrans déjà pris");
    // Messages reçus de l'hôte : ce que j'ai en plus, il ne l'a pas.
    let mut received: HashSet<Vec<u8>> = HashSet::new();
    let mut sounds = client.take_sounds().expect("son des écrans déjà pris");
    for c in std::mem::take(backlog) {
        if let Some(end) = command(c, ctx, voice, &client, out) {
            return finish(end, voice, client, hosting).await;
        }
    }

    let end = loop {
        tokio::select! {
            Some((seq, opus)) = voice.sound.next_frame() => voice.send(seq, &opus, Some(&client), &ctx.peers),
            Some(v) = voice.incoming.voice.recv() => voice.sound.play(&v),
            Some(v) = voice_in.recv() => voice.sound.play(&v),
            shot = next_shot(&mut voice.sharer) => voice.share(shot, Some(&client), out),
            Some((from, frame)) = voice.incoming.screens.recv() => voice.show(from, frame),
            Some((from, frame)) = screens.recv() => voice.show(from, frame),
            Some(s) = voice.incoming.sounds.recv() => voice.hear(s),
            Some(s) = sounds.recv() => voice.hear(s),
            Some(_) = voice.incoming.keys.recv() => voice.request_key(),
            Some(p) = voice.asks.recv() => voice.ask_key(p, Some(&client)),
            frame = next_face(&mut voice.camera) => voice.camera_frame(frame, Some(&client), out),
            Some(_) = voice.incoming.camera_keys.recv() => voice.camera_key(),
            Some(p) = voice.face_asks.recv() => voice.ask_face_key(p, Some(&client)),
            offer = async { offer_rx.as_mut().expect("garde").await }, if offer_rx.is_some() => {
                offer_rx = None;
                if let Ok(offer) = offer {
                    let _ = client.offer_relay(offer);
                }
            }
            a = async { announcing.as_mut().expect("garde").await }, if announcing.is_some() => {
                announcing = None;
                let Some(h) = &mut hosting else { continue };
                match a {
                    Ok(Ok(Announced::Announced(a))) => {
                        superseded = a.superseded.clone();
                        h.upnp = Some(spawn_port_mapping(h.port, h.cert, h.handle.access_key(), Some(a.mapping), out));
                        h.announcement = Some(a);
                    }
                    // Un autre membre a ouvert le serveur au même instant.
                    Ok(Ok(Announced::AlreadyHosted(rival))) => {
                        if h.handle.remote_members() == 0 || rival.user < ctx.me.id() {
                            tracing::info!("un autre membre héberge déjà ce serveur : je le rejoins");
                            out.send(Update::Reconnecting(Some("Un autre membre héberge déjà le serveur : connexion chez lui…".into())));
                            break End::Open(Box::new(client.card.clone()));
                        }
                    }
                    // Annuaire injoignable : reste la box (UPnP) et le réseau local.
                    other => {
                        if let Ok(Err(e)) = other {
                            tracing::warn!("annuaire : {e:#}");
                        }
                        if h.upnp.is_none() {
                            h.upnp = Some(spawn_port_mapping(h.port, h.cert, h.handle.access_key(), None, out));
                        }
                    }
                }
            }
            Ok(()) = superseded.changed() => {
                let rival = *superseded.borrow_and_update();
                if let (Some(rival), Some(h)) = (rival, &hosting)
                    && (h.handle.remote_members() == 0 || rival.user < ctx.me.id())
                {
                    tracing::info!("un autre membre héberge ce serveur : je lui cède la place");
                    out.send(Update::Reconnecting(Some("Un autre membre héberge le serveur : connexion chez lui…".into())));
                    break End::Open(Box::new(client.card.clone()));
                }
            }
            c = cmd.recv() => {
                if let Some(end) = command(c.unwrap_or(Cmd::Leave), ctx, voice, &client, out) {
                    break end;
                }
            }
            Some(p) = ready.recv() => send_message(&client, p.channel, p.body, p.kind, out),
            ev = events.recv() => {
                let ev = ev.unwrap_or(Event::Disconnected { reason: "connexion perdue".into() });
                match ev {
                    Event::Disconnected { reason } => {
                        tracing::warn!("déconnecté : {reason}");
                        // Mon propre hôte qui s'arrête tout seul : on repasse par l'annuaire.
                        break if hosting_now {
                            End::Open(Box::new(client.card.clone()))
                        } else {
                            End::Lost(plan(ctx, &client, false))
                        };
                    }
                    // L'hôte part (y compris le mien, quand je passe la main après un vote).
                    Event::HostLeaving => break End::Lost(plan(ctx, &client, true)),
                    Event::Kicked { reason } => break End::Kicked(reason),
                    Event::CardUpdated(card) if card.is_banned(&ctx.me.id()) => {
                        break End::Kicked("Tu as été banni de ce serveur.".into());
                    }
                    // L'hôte a fini d'envoyer son historique : je lui donne ce
                    // qu'il n'avait pas (écrit pendant qu'il était absent).
                    Event::HistoryEnd => {
                        if let Some(card) = ctx.latest_card().filter(|c| c.history().keep) {
                            let missing: Vec<_> = ctx
                                .store
                                .history(&card)
                                .messages()
                                .iter()
                                .filter(|m| !received.contains(&m.sig))
                                .cloned()
                                .collect();
                            if !missing.is_empty() {
                                tracing::info!("{} messages que l'hôte n'avait pas", missing.len());
                                let _ = client.backfill(missing);
                            }
                        }
                    }
                    // L'hôte cherche une pièce jointe que j'ai : je la lui envoie.
                    Event::FileWanted { hash } => {
                        if let Some(bytes) = ctx.store.file(client.card.id, &hash) {
                            let link = client.files();
                            tokio::spawn(async move {
                                if let Err(e) = link.put(&bytes).await {
                                    tracing::debug!("pièce jointe demandée : {e:#}");
                                }
                            });
                        }
                    }
                    Event::Moved if !hosting_now => break End::Open(Box::new(client.card.clone())),
                    Event::Succession(contacts) => {
                        voice.mesh.set_contacts(&contacts);
                        ctx.contacts = contacts;
                    }
                    // Un spectateur passé par l'hôte attend une image clé.
                    Event::Control(Control::ScreenKey(_)) => voice.request_key(),
                    // Pareil pour ma webcam.
                    Event::Control(Control::CameraKey(_)) => voice.camera_key(),
                    ev => {
                        match &ev {
                            Event::History(batch) => received.extend(batch.iter().map(|m| m.sig.clone())),
                            Event::Text { msg, .. } => {
                                received.insert(msg.sig.clone());
                            }
                            // Nouveau code : l'ancien ne marche plus, on affiche le nouveau.
                            Event::CardUpdated(card) if card.access_key() != shown_key => {
                                shown_key = card.access_key();
                                out.send(Update::Invite(Invite::Anywhere {
                                    code: invite::encode(&invite::Invite::key(shown_key)),
                                    warning: None,
                                }));
                            }
                            Event::PeerJoined(p) => ctx.see_peer(p.id, p.user, p.name.clone()),
                            Event::PeerLeft { peer_id } => { ctx.peers.remove(peer_id); }
                            Event::Voice(room) => {
                                // Le son s'ouvre quand on entre dans un salon vocal, se ferme quand on en sort.
                                voice.sound.on_room(room, me, &voice.prefs, &voice.talk, out);
                                voice.mesh.set_room(&voice.sound.members);
                                // Sorti du salon vocal : l'hôte a arrêté mon partage.
                                if voice.sound.room.is_none() && voice.sharer.take().is_some() {
                                    out.send(Update::Sharing(None));
                                }
                                // Les webcams de mon salon, et ma webcam s'il n'y en a plus.
                                if voice.sound.room == Some(room.channel) {
                                    voice.cameras = room.cameras.iter().copied().filter(|p| *p != me).collect();
                                    voice.camera_path(&client);
                                } else if voice.sound.room.is_none() {
                                    voice.cameras.clear();
                                    if voice.camera.take().is_some() {
                                        out.send(Update::Camera(false));
                                    }
                                }
                                // L'écran que je regardais est de nouveau partagé : on le redemande.
                                if let Some(w) = rewatch
                                    && room.sharing.contains(&w)
                                    && room.members.contains(&me)
                                {
                                    if !voice.mesh.watch(w, true) {
                                        let _ = client.watch_screen(Some(w));
                                        voice.via_host = true;
                                    }
                                    rewatch = None;
                                }
                            }
                            _ => {}
                        }
                        out.send(Update::Net(ev));
                    }
                }
            }
            _ = ping.tick() => {
                let _ = client.ping();
                voice.mesh.tick();
                if rewatch.is_none() {
                    voice.watch_path(&client);
                }
                voice.camera_path(&client);
            }
        }
    };
    finish(end, voice, client, hosting).await
}

/// De quoi suivre la relève quand l'hôte part ou ne répond plus.
fn plan(ctx: &Ctx, client: &Client, graceful: bool) -> Plan {
    Plan {
        key: ctx
            .latest_card()
            .map_or_else(|| client.card.access_key(), |c| c.access_key()),
        old_host: client.host_addr(),
        old_cert: client.host_cert(),
        contacts: ctx.contacts.clone(),
        graceful,
        me: client.peer_id,
    }
}

async fn finish(end: End, voice: &mut Voice, client: Client, hosting: Option<Hosting>) -> Result<End> {
    match &end {
        End::Left | End::Kicked(_) => {
            // Départ voulu : on prévient (l'hôte passe la main) puis on ferme.
            let in_voice = voice.sound.room.is_some();
            voice.sound = Sound::default();
            if in_voice {
                audio::Cue::Leave.play();
            }
            voice.sharer = None;
            voice.camera = None;
            voice.mesh.set_room(&[]);
            client.quit().await;
            if let Some(h) = hosting {
                h.stop(Some(false)).await;
            }
        }
        End::Open(_) => {
            drop(client);
            if let Some(h) = hosting {
                h.stop(Some(true)).await;
            }
        }
        End::Lost(_) => {
            drop(client);
            if let Some(h) = hosting {
                h.stop(None).await;
            }
        }
    }
    Ok(end)
}

/// Signe et envoie un message ; l'affiche chez moi, ou prévient s'il est
/// refusé (trop de messages d'un coup).
fn send_message(client: &Client, channel: ChannelId, body: String, kind: Kind, out: &Out) {
    match client.send_message(channel, body, kind) {
        Ok(msg) => out.send(Update::Own(msg)),
        Err(e) if e.downcast_ref::<TooFast>().is_some() => out.send(Update::Notice(format!("{e}"))),
        Err(e) => tracing::warn!("message : {e:#}"),
    }
}

/// Lit les pièces jointes, les range chez moi et les envoie à l'hôte.
async fn prepare_files(paths: Vec<PathBuf>, link: FileLink, store: Store, server: ServerId) -> Result<Vec<FileRef>> {
    ensure!(paths.len() <= MAX_FILES, "{MAX_FILES} fichiers au plus par message");
    let mut refs = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .map_or_else(|| "fichier".into(), |n| n.to_string_lossy().into_owned());
        let size = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("{name} introuvable"))?
            .len();
        ensure!(size <= MAX_FILE_BYTES, "{name} est trop gros (25 Mo au plus)");
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("lecture de {name}"))?;
        let hash = files::hash(&bytes);
        let image = image_size(&bytes);
        store.save_file(server, &hash, &bytes)?;
        link.put(&bytes).await.with_context(|| format!("envoi de {name}"))?;
        refs.push(FileRef {
            name,
            size: bytes.len() as u64,
            hash,
            image,
        });
    }
    Ok(refs)
}

/// Lecteur d'image borné : une image piège (toute petite, mais qui annonce
/// des dizaines de milliers de pixels de côté) ferait sinon réserver des
/// centaines de Mo à chaque membre qui l'affiche.
fn image_reader(bytes: &[u8]) -> Option<image::ImageReader<std::io::Cursor<&[u8]>>> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    Some(reader)
}

/// Côté maximal d'une image affichée (au-delà : pièce jointe simple).
const MAX_IMAGE_SIDE: u32 = 10_000;
/// Mémoire maximale pour décoder une image.
const MAX_IMAGE_ALLOC: u64 = 256 * 1024 * 1024;

/// Largeur et hauteur, si c'est une image qu'on sait afficher.
fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let (w, h) = image_reader(bytes)?.into_dimensions().ok()?;
    (w <= MAX_IMAGE_SIDE && h <= MAX_IMAGE_SIDE).then_some((w, h))
}

/// Aperçu d'une image reçue, réduit pour le salon.
fn preview(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image_reader(bytes)?.decode().ok()?;
    let img = if img.width() > 720 || img.height() > 480 {
        img.thumbnail(720, 480)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

/// Récupère une pièce jointe (chez moi, sinon chez l'hôte) et prépare son aperçu.
fn fetch(file: FileRef, link: FileLink, store: Store, server: ServerId, out: &Out) {
    let (tx, ctx) = (out.tx.clone(), out.ctx.clone());
    tokio::spawn(async move {
        let out = Out { tx, ctx };
        let hash = file.hash;
        let got = async {
            // Lecture, vérification et aperçu hors des fils réseau : une
            // pièce de 25 Mo ne doit pas retarder la voix.
            let image = file.image.is_some();
            let local = {
                let store = store.clone();
                tokio::task::spawn_blocking(move || {
                    let b = store.file(server, &hash).filter(|b| files::hash(b) == hash)?;
                    Some(if image { preview(&b) } else { None })
                })
                .await?
            };
            let image = match local {
                Some(image) => image,
                None => {
                    let b = link.get(hash).await?;
                    tokio::task::spawn_blocking(move || {
                        store.save_file(server, &hash, &b)?;
                        anyhow::Ok(if image { preview(&b) } else { None })
                    })
                    .await??
                }
            };
            anyhow::Ok(FileState::Ready { image })
        };
        let state = got.await.unwrap_or_else(|e| FileState::Failed(format!("{e:#}")));
        out.send(Update::File { hash, state });
    });
}

/// Copie une pièce jointe reçue dans Téléchargements, sans l'ouvrir (elle
/// vient de quelqu'un d'autre), et montre le dossier.
fn save_download(file: &FileRef, from: &Path) -> Result<PathBuf> {
    let dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| anyhow!("dossier Téléchargements introuvable"))?;
    std::fs::create_dir_all(&dir)?;
    let name = safe_file_name(&file.name);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    let mut target = dir.join(&name);
    let mut n = 2;
    while target.exists() {
        target = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    std::fs::copy(from, &target)?;
    mark_from_internet(&target);
    show_in_folder(&target);
    Ok(target)
}

/// Nom de fichier sûr pour Téléchargements : juste le nom (pas de chemin
/// glissé dedans), sans caractère interdit par Windows (`:` y créerait un
/// flux caché), sans nom réservé (`CON`, `NUL`…), sans point ni espace final.
fn safe_file_name(name: &str) -> String {
    // Dernier morceau après `/` ou `\`, à la main : sous Windows, `Path`
    // prendrait « a: » de « a:b.txt » pour un lecteur et le jetterait.
    let name = plain_text(name);
    let name = match name.rsplit(['/', '\\']).next().unwrap_or_default() {
        "." | ".." => "",
        n => n,
    };
    let mut name: String = name
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    while name.ends_with(['.', ' ']) {
        name.pop();
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        name.insert(0, '_');
    }
    if name.is_empty() { "fichier".into() } else { name }
}

/// Sous Windows, marque le fichier comme venu d'Internet (comme un
/// navigateur) : s'il s'agit d'un programme, Windows avertit avant de le
/// lancer.
fn mark_from_internet(path: &Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        if let Err(e) = std::fs::write(&stream, "[ZoneTransfer]\r\nZoneId=3\r\n") {
            tracing::warn!("marque « venu d'Internet » : {e}");
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}

fn show_in_folder(path: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(not(windows))]
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

/// Applique une commande de l'UI (ou d'un raccourci clavier).
fn command(c: Cmd, ctx: &Ctx, voice: &mut Voice, client: &Client, out: &Out) -> Option<End> {
    let before = (voice.sound.muted, voice.sound.deafened);
    let status = |voice: &Voice| {
        let (muted, deafened) = (voice.sound.muted, voice.sound.deafened);
        let _ = client.send_status(muted, deafened);
        out.send(Update::SelfAudio { muted, deafened });
        if let Some(cue) = self_cue(before, (muted, deafened)) {
            cue.play();
        }
    };
    match c {
        Cmd::Send(Draft {
            channel,
            body,
            reply_to,
            mentions,
            files,
        }) => {
            if files.is_empty() {
                let kind = if reply_to.is_none() && mentions.is_empty() {
                    Kind::Text
                } else {
                    Kind::Rich {
                        reply_to,
                        mentions,
                        files: Vec::new(),
                    }
                };
                send_message(client, channel, body, kind, out);
            } else {
                // Les fichiers partent d'abord chez l'hôte ; le message suit.
                let (link, store, server, ready) =
                    (client.files(), ctx.store.clone(), client.card.id, ctx.prepared.clone());
                let (tx, ectx) = (out.tx.clone(), out.ctx.clone());
                out.send(Update::Notice("Envoi des fichiers…".into()));
                tokio::spawn(async move {
                    match prepare_files(files, link, store, server).await {
                        Ok(files) => {
                            let kind = Kind::Rich {
                                reply_to,
                                mentions,
                                files,
                            };
                            let _ = ready.send(Prepared { channel, body, kind });
                        }
                        Err(e) => Out { tx, ctx: ectx }.send(Update::Notice(format!("Fichier non envoyé : {e:#}"))),
                    }
                });
            }
        }
        Cmd::Act { channel, body, kind } => send_message(client, channel, body, kind, out),
        Cmd::Kick(peer) => {
            let _ = client.kick(peer);
        }
        Cmd::Fetch(file) => fetch(file, client.files(), ctx.store.clone(), client.card.id, out),
        Cmd::Save(file) => {
            let from = ctx.store.file_path(client.card.id, &file.hash);
            let line = match save_download(&file, &from) {
                Ok(path) => format!("Enregistré : {}", path.display()),
                Err(e) => format!("Enregistrement impossible : {e:#}"),
            };
            out.send(Update::Notice(line));
        }
        Cmd::VoiceJoin(channel) => {
            // Changer de salon éteint la webcam (l'hôte l'oublie aussi).
            if voice.camera.take().is_some() {
                out.send(Update::Camera(false));
            }
            let _ = client.join_voice(channel);
        }
        Cmd::VoiceLeave => {
            let _ = client.leave_voice();
        }
        Cmd::UpdateCard(card) => {
            let _ = client.update_card(&card);
        }
        Cmd::Mute(m) => {
            voice.sound.set_muted(m);
            status(voice);
        }
        Cmd::Deafen(d) => {
            voice.sound.set_deafened(d);
            status(voice);
        }
        Cmd::ToggleMute => {
            // Réactiver le micro quand le son est coupé rétablit aussi le son (comme Discord).
            if voice.sound.deafened {
                voice.sound.set_deafened(false);
                voice.sound.set_muted(false);
            } else {
                voice.sound.set_muted(!voice.sound.muted);
            }
            status(voice);
        }
        Cmd::ToggleDeafen => {
            voice.sound.set_deafened(!voice.sound.deafened);
            status(voice);
        }
        Cmd::Talk { held, whisper } => {
            voice.talk = held.then_some(whisper);
            voice.sound.set_held(&voice.talk);
        }
        Cmd::Audio(p) => {
            let devices_changed = (&p.input, &p.output) != (&voice.prefs.input, &voice.prefs.output);
            let camera_changed = p.camera != voice.prefs.camera;
            voice.prefs = p;
            voice.sound.apply(&voice.prefs, devices_changed, &voice.talk, out);
            if camera_changed && voice.camera.is_some() {
                voice.start_camera(client, out);
            }
        }
        Cmd::Camera(true) => {
            if voice.sound.room.is_some() {
                voice.start_camera(client, out);
            }
        }
        Cmd::Camera(false) => {
            voice.camera = None;
            let _ = client.share_camera(false);
            out.send(Update::Camera(false));
        }
        // Aussi pour changer de source en plein partage : les spectateurs
        // passent d'une image à l'autre sans que le partage s'arrête.
        Cmd::Share(Some((source, options))) => {
            match tokio::task::block_in_place(|| capture::Sharer::start(source, options)) {
                Ok(s) => {
                    sound_notice(&s, options, out);
                    let already = voice.sharer.replace(s).is_some();
                    if !already {
                        let _ = client.share_screen(true);
                    }
                    out.send(Update::Sharing(Some(source)));
                }
                Err(e) => {
                    tracing::warn!("partage d'écran : {e:#}");
                    out.send(Update::Notice(format!("Partage d'écran impossible : {e:#}")));
                }
            }
        }
        Cmd::Share(None) => {
            voice.sharer = None;
            let _ = client.share_screen(false);
            out.send(Update::Sharing(None));
        }
        Cmd::ShareOptions(o) => {
            if let Some(s) = &mut voice.sharer {
                let asked_sound = s.options().sound;
                s.set_options(o);
                if !asked_sound {
                    sound_notice(s, o, out);
                }
            }
        }
        Cmd::Watch(peer) => {
            if let Some(old) = voice.watching {
                voice.mesh.watch(old, false);
                let _ = client.watch_screen(None);
            }
            // Le son s'arrête avec l'image.
            voice.sound.stop_share();
            voice.watching = peer;
            voice.via_host = false;
            // Directement si possible, sinon par l'hôte du serveur.
            if let Some(p) = peer
                && !voice.mesh.watch(p, true)
            {
                let _ = client.watch_screen(Some(p));
                voice.via_host = true;
            }
        }
        Cmd::Preview(on) => {
            voice.preview = on;
            // L'aperçu se décode comme un autre écran : il commence par une image clé.
            if on {
                voice.request_key();
            }
        }
        Cmd::PeerVolume(peer, v) => voice.sound.set_peer_volume(peer, v),
        Cmd::ShareVolume(peer, v) => voice.sound.set_share_volume(peer, v),
        Cmd::RequestHost => {
            let _ = client.request_host();
        }
        Cmd::Ballot { id, yes } => {
            let _ = client.ballot(id, yes);
        }
        Cmd::Leave => return Some(End::Left),
    }
    None
}

/// Le son qui accompagne un changement de mon micro ou de mon son
/// (`(micro coupé, son coupé)` avant et après). Le son prime : réactiver le
/// micro quand le son était coupé rétablit les deux, un seul son suffit.
fn self_cue(before: (bool, bool), after: (bool, bool)) -> Option<audio::Cue> {
    let ((was_muted, was_deafened), (muted, deafened)) = (before, after);
    if was_deafened != deafened {
        Some(if deafened {
            audio::Cue::Deafen
        } else {
            audio::Cue::Undeafen
        })
    } else if was_muted != muted {
        Some(if muted { audio::Cue::Mute } else { audio::Cue::Unmute })
    } else {
        None
    }
}

/// Le son d'un changement dans mon salon vocal : j'y entre ou quelqu'un
/// arrive, quelqu'un s'en va.
fn room_cue(entered: bool, before: &[PeerId], now: &[PeerId]) -> Option<audio::Cue> {
    if entered || now.iter().any(|p| !before.contains(p)) {
        Some(audio::Cue::Join)
    } else if before.iter().any(|p| !now.contains(p)) {
        Some(audio::Cue::Leave)
    } else {
        None
    }
}

/// Adresses où me joindre (vocal direct, relève) : réseau local et adresse
/// publique vue par STUN, sur le port de ma station.
async fn relay_offer(station: &Station) -> RelayOffer {
    let port = station.port();
    let lan = upnp::lan_ip().map(|ip| SocketAddr::new(ip, port));
    let mut addrs: Vec<SocketAddr> = lan.into_iter().collect();
    let mut probe = station.probe.clone();
    let reach = match probe.discover().await {
        Ok(m) => {
            if !addrs.contains(&m.addr()) {
                addrs.push(m.addr());
            }
            match m {
                // Pas de box entre Internet et moi.
                Mapping::Stable(a) if Some(a) == lan => Reach::Open,
                Mapping::Stable(_) => Reach::Stable,
                Mapping::Symmetric(_) => Reach::Symmetric,
            }
        }
        Err(e) => {
            tracing::debug!("STUN : {e:#}");
            Reach::Unknown
        }
    };
    RelayOffer {
        addrs,
        cert: station.cert.fingerprint,
        reach,
    }
}

/// Image ou son suivant de mon partage ; `None` quand la source a disparu.
async fn next_shot(sharer: &mut Option<capture::Sharer>) -> Option<capture::Shot> {
    match sharer {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

/// Image suivante de ma webcam ; `None` quand elle a disparu.
async fn next_face(camera: &mut Option<capture::camera::Camera>) -> Option<capture::video::Encoded> {
    match camera {
        Some(c) => c.next().await,
        None => std::future::pending().await,
    }
}

/// Image décodée, prête pour une texture.
fn color_image(p: capture::video::Picture) -> egui::ColorImage {
    // Pixels opaques : le tampon décodé sert tel quel, sans recopie.
    let pixels =
        bytemuck::allocation::try_cast_vec(p.rgba).unwrap_or_else(|(_, rgba)| bytemuck::cast_slice(&rgba).to_vec());
    egui::ColorImage::new([p.width, p.height], pixels)
}

/// Le son devait partir avec l'image mais sa capture a échoué (le détail
/// est dans le journal).
fn sound_notice(sharer: &capture::Sharer, options: capture::Options, out: &Out) {
    if options.sound && capture::sound::SUPPORTED && !sharer.has_sound() {
        out.send(Update::Notice(
            "Partage sans le son : impossible de capturer le son de cette source \
             (Windows 10 version 2004 ou plus récent requis)."
                .into(),
        ));
    }
}

/// Le son d'une session : ouvert tant qu'on est dans un salon vocal, et
/// gardé ouvert pendant une relève d'hôte.
#[derive(Default)]
struct Sound {
    engine: Option<(audio::Engine, audio::Frames)>,
    room: Option<ChannelId>,
    /// Salon redemandé après une relève, pas encore confirmé par l'hôte.
    pending: Option<ChannelId>,
    /// Qui est dans mon salon vocal (moi compris).
    members: Vec<PeerId>,
    muted: bool,
    deafened: bool,
    volumes: HashMap<PeerId, f32>,
    /// Pas de sons d'arrivée ni de départ avant cet instant (relève).
    quiet_until: Option<Instant>,
    /// Volume du son de leur partage d'écran.
    share_volumes: HashMap<PeerId, f32>,
}

impl Sound {
    async fn next_frame(&mut self) -> Option<(u16, Vec<u8>)> {
        match &mut self.engine {
            Some((_, frames)) => frames.recv().await,
            None => std::future::pending().await,
        }
    }

    fn play(&self, v: &client::VoiceIn) {
        if let Some((e, _)) = &self.engine {
            e.play(v.from, v.seq, &v.opus, v.whisper);
        }
    }

    fn set_muted(&mut self, m: bool) {
        self.muted = m;
        if let Some((e, _)) = &self.engine {
            e.set_muted(m);
        }
    }

    fn set_deafened(&mut self, d: bool) {
        self.deafened = d;
        if let Some((e, _)) = &self.engine {
            e.set_deafened(d);
        }
    }

    fn set_held(&self, talk: &Option<Option<Vec<UserId>>>) {
        if let Some((e, _)) = &self.engine {
            e.set_held(talk.is_some(), matches!(talk, Some(Some(_))));
        }
    }

    fn set_peer_volume(&mut self, peer: PeerId, v: f32) {
        self.volumes.insert(peer, v);
        if let Some((e, _)) = &self.engine {
            e.set_peer_volume(peer, v);
        }
    }

    fn play_share(&self, s: &client::SoundIn) {
        if let Some((e, _)) = &self.engine {
            e.play_share(s.from, s.seq, &s.opus);
        }
    }

    fn set_share_volume(&mut self, peer: PeerId, v: f32) {
        self.share_volumes.insert(peer, v);
        if let Some((e, _)) = &self.engine {
            e.set_share_volume(peer, v);
        }
    }

    fn stop_share(&self) {
        if let Some((e, _)) = &self.engine {
            e.stop_share();
        }
    }

    fn on_room(
        &mut self,
        room: &VoiceRoom,
        me: PeerId,
        prefs: &audio::Prefs,
        talk: &Option<Option<Vec<UserId>>>,
        out: &Out,
    ) {
        if room.members.contains(&me) {
            let entered = self.room != Some(room.channel);
            let cue = room_cue(entered, &self.members, &room.members);
            self.room = Some(room.channel);
            self.pending = None;
            self.members = room.members.clone();
            match &self.engine {
                Some((e, _)) => e.retain(&room.members),
                None => self.open(prefs, talk, out),
            }
            // Après l'ouverture du son : il passe par la sortie du salon.
            if let Some(cue) = cue
                && (entered || self.quiet_until.is_none_or(|t| Instant::now() >= t))
            {
                cue.play();
            }
        } else if self.room == Some(room.channel) && self.pending != Some(room.channel) {
            self.room = None;
            self.members.clear();
            self.engine = None;
            out.send(Update::Audio(None));
            audio::Cue::Leave.play();
        }
    }

    fn apply(&mut self, prefs: &audio::Prefs, devices_changed: bool, talk: &Option<Option<Vec<UserId>>>, out: &Out) {
        match &self.engine {
            Some(_) if devices_changed => self.open(prefs, talk, out),
            Some((e, _)) => {
                e.set_threshold(prefs.threshold_db);
                e.set_volume(prefs.volume);
                e.set_push_to_talk(prefs.push_to_talk);
                e.set_clean(prefs.clean());
            }
            None => {}
        }
    }

    fn open(&mut self, prefs: &audio::Prefs, talk: &Option<Option<Vec<UserId>>>, out: &Out) {
        self.engine = None;
        match tokio::task::block_in_place(|| audio::Engine::start(prefs)) {
            Ok((e, frames)) => {
                let ctx = out.ctx.clone();
                e.status.on_activity(move || ctx.request_repaint());
                e.set_muted(self.muted);
                e.set_deafened(self.deafened);
                for (p, v) in &self.volumes {
                    e.set_peer_volume(*p, *v);
                }
                for (p, v) in &self.share_volumes {
                    e.set_share_volume(*p, *v);
                }
                out.send(Update::Audio(Some(e.status.clone())));
                self.engine = Some((e, frames));
                self.set_held(talk);
            }
            Err(err) => {
                tracing::warn!("audio : {err:#}");
                out.send(Update::Audio(None));
            }
        }
    }
}

fn spawn_port_mapping(
    port: u16,
    cert: CertFingerprint,
    key: AccessKey,
    reachable: Option<Mapping>,
    out: &Out,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(port_mapping(
        port,
        cert,
        key,
        reachable,
        stop_rx,
        out.tx.clone(),
        out.ctx.clone(),
    ));
    (stop_tx, task)
}

/// Publie le code d'invitation, tente d'ouvrir le port sur la box (UPnP),
/// renouvelle le bail et referme le port quand `stop` arrive.
///
/// `key` : clé d'accès du serveur (dans chaque code). `reachable` : serveur
/// annoncé dans l'annuaire, avec ce que fait la box.
async fn port_mapping(
    port: u16,
    cert: CertFingerprint,
    key: AccessKey,
    reachable: Option<Mapping>,
    mut stop: oneshot::Receiver<()>,
    tx: std_mpsc::Sender<Update>,
    ctx: egui::Context,
) {
    let out = Out { tx, ctx };
    let announced = reachable.is_some();
    let strict = "Ta box change de port selon le destinataire (NAT « symétrique ») : certains amis \
                  risquent de ne pas pouvoir entrer. Activer UPnP sur la box, ou rediriger le port \
                  à la main (voir le README), règle le problème.";
    if let Some(mapping) = reachable {
        out.send(Update::Invite(Invite::Anywhere {
            code: invite::encode(&invite::Invite::key(key)),
            warning: matches!(mapping, Mapping::Symmetric(_)).then(|| format!("{strict} Port : {port} (UDP).")),
        }));
    }
    let opened = tokio::select! {
        r = upnp::PortMapping::open(port) => r,
        _ = &mut stop => return,
    };
    let mapping = match opened {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("UPnP : {e:#}");
            if !announced {
                let lan = upnp::lan_ip().unwrap_or([127, 0, 0, 1].into());
                out.send(Update::Invite(Invite::LanOnly {
                    code: invite::encode(&invite::Invite::host((lan, port).into(), &cert, key)),
                    reason: format!(
                        "L'annuaire décentralisé est injoignable (pas d'accès à Internet, ou UDP bloqué) et \
                         la box n'a pas ouvert le port automatiquement (UPnP). Ce code marche pour les amis \
                         connectés à la même box que toi ; sinon redirige le port {port} (UDP) à la main \
                         (voir le README)."
                    ),
                }));
            }
            return;
        }
    };
    match (announced, &mapping.warning) {
        // La box a ouvert le port, mais derrière une autre box (ou un CGNAT) :
        // ce port ouvert ne sert à rien, l'annuaire suffit. On garde l'état actuel.
        (true, Some(w)) => {
            tracing::info!("UPnP sans effet : {w}");
            mapping.close().await;
            return;
        }
        // Port ouvert en plus de l'annuaire : le code marche même derrière une box stricte.
        (true, None) => out.send(Update::Invite(Invite::Anywhere {
            code: invite::encode(&invite::Invite::host(mapping.public, &cert, key)),
            warning: None,
        })),
        (false, warning) => out.send(Update::Invite(Invite::Public {
            code: invite::encode(&invite::Invite::host(mapping.public, &cert, key)),
            warning: warning.clone(),
        })),
    }
    loop {
        tokio::select! {
            _ = tokio::time::sleep(upnp::PortMapping::renew_interval()) => {
                if let Err(e) = mapping.renew().await {
                    tracing::warn!("UPnP : renouvellement : {e:#}");
                }
            }
            _ = &mut stop => break,
        }
    }
    mapping.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio::Cue;

    #[test]
    fn noms_de_fichiers_surs() {
        assert_eq!(safe_file_name("photo.png"), "photo.png");
        assert_eq!(safe_file_name("../../Windows/evil.exe"), "evil.exe");
        assert_eq!(safe_file_name("a:b.txt"), "a_b.txt");
        assert_eq!(safe_file_name("CON.txt"), "_CON.txt");
        assert_eq!(safe_file_name("com1"), "_com1");
        assert_eq!(safe_file_name("console.txt"), "console.txt");
        assert_eq!(safe_file_name("rapport.pdf. . "), "rapport.pdf");
        assert_eq!(safe_file_name(".."), "fichier");
        assert_eq!(safe_file_name(""), "fichier");
    }

    #[test]
    fn image_piege_refusee() {
        // PNG de quelques octets qui annonce 60 000 × 60 000 pixels.
        let mut png = Vec::new();
        use image::ImageEncoder;
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[0; 4], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        // Largeur et hauteur de l'en-tête IHDR (octets 16 à 23), CRC laissé faux :
        // le décodeur doit refuser avant d'allouer quoi que ce soit.
        png[16..20].copy_from_slice(&60_000u32.to_be_bytes());
        png[20..24].copy_from_slice(&60_000u32.to_be_bytes());
        assert!(preview(&png).is_none());
        assert!(image_size(&png).is_none());
    }

    #[test]
    fn sons_du_micro_et_du_casque() {
        assert_eq!(self_cue((false, false), (true, false)), Some(Cue::Mute));
        assert_eq!(self_cue((true, false), (false, false)), Some(Cue::Unmute));
        assert_eq!(self_cue((false, false), (false, true)), Some(Cue::Deafen));
        // Micro réactivé pendant que le son était coupé : les deux reviennent, un seul son.
        assert_eq!(self_cue((true, true), (false, false)), Some(Cue::Undeafen));
        assert_eq!(self_cue((true, false), (true, false)), None);
    }

    #[test]
    fn sons_du_salon() {
        assert_eq!(room_cue(true, &[], &[1]), Some(Cue::Join));
        assert_eq!(room_cue(false, &[1], &[1, 2]), Some(Cue::Join));
        assert_eq!(room_cue(false, &[1, 2], &[1]), Some(Cue::Leave));
        // Quelqu'un coupe son micro : le salon est renvoyé, mais personne n'a bougé.
        assert_eq!(room_cue(false, &[1, 2], &[2, 1]), None);
    }
}
