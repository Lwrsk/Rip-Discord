//! Interface graphique (egui) : accueil (mes serveurs, créer, rejoindre) puis
//! serveur (salons texte et vocaux), avec deux fenêtres de réglages bien
//! séparées : les miens ([`me`]) et ceux du serveur ([`server`]).
//!
//! Le réseau tourne sur un runtime tokio à part ([`net`]) ; l'UI lui envoie
//! des [`Cmd`] et reçoit des [`Update`], puis se redessine.

mod chat;
mod icons;
mod keys;
mod me;
mod net;
mod notify;
mod server;
mod share;
mod single;
mod style;
mod update;

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, mpsc as std_mpsc},
    time::Duration,
};

use anyhow::Result;
use eframe::egui::{self, Align, Color32, Key, Layout, Margin, RichText, Stroke, vec2};
use tokio::{sync::mpsc, task::JoinHandle, time::timeout};

use crate::{
    audio, capture,
    client::{Event, ServerView},
    core::{
        VERSION,
        card::{ChannelId, FileRef, Kind, MessageId, Role, ServerCard, ServerId, SignedMessage, now_ms},
        files::FileHash,
        identity::{Identity, UserId},
        invite,
        protocol::PeerId,
        store::Store,
    },
};
use icons::*;
use net::Out;
use style::*;

// --- Messages UI <-> réseau --------------------------------------------------

/// Message tel que tapé, avant d'être signé (les pièces jointes partent
/// d'abord chez l'hôte).
struct Draft {
    channel: ChannelId,
    body: String,
    reply_to: Option<MessageId>,
    mentions: Vec<UserId>,
    files: Vec<PathBuf>,
}

enum Cmd {
    Send(Draft),
    /// Modifier, supprimer un message, ou y réagir.
    Act {
        channel: ChannelId,
        body: String,
        kind: Kind,
    },
    /// Déconnecter ce membre (admins).
    Kick(PeerId),
    /// Récupérer une pièce jointe (et la préparer à l'affichage si c'est une image).
    Fetch(FileRef),
    /// Copier une pièce jointe dans le dossier Téléchargements.
    Save(FileRef),
    VoiceJoin(ChannelId),
    VoiceLeave,
    UpdateCard(Box<ServerCard>),
    Mute(bool),
    Deafen(bool),
    /// Raccourcis clavier : inverser micro / son.
    ToggleMute,
    ToggleDeafen,
    /// Touche d'appui-pour-parler (`whisper: None`) ou de chuchotement
    /// (à ces personnes) enfoncée ou relâchée.
    Talk {
        held: bool,
        whisper: Option<Vec<UserId>>,
    },
    /// Demander à héberger le serveur (les membres votent).
    RequestHost,
    Ballot {
        id: u64,
        yes: bool,
    },
    /// Nouveaux réglages audio (appareils, sensibilité, volume).
    Audio(audio::Prefs),
    /// Partager cet écran ou cette fenêtre, ou passer dessus si un partage
    /// tourne déjà (`None` : arrêter).
    Share(Option<(capture::Source, capture::Options)>),
    /// Curseur, qualité : appliqués au partage en cours.
    ShareOptions(capture::Options),
    /// Regarder l'écran de ce participant (`None` : arrêter).
    Watch(Option<PeerId>),
    /// Afficher ou non l'aperçu de mon propre partage.
    Preview(bool),
    /// Allumer ou éteindre ma webcam (dans le salon vocal).
    Camera(bool),
    /// Volume d'un participant en particulier.
    PeerVolume(PeerId, f32),
    /// Volume du son du partage d'écran de ce participant (0 : coupé).
    ShareVolume(PeerId, f32),
    Leave,
}

/// Dernière image décodée d'un écran regardé. Une image plus récente
/// remplace celle que l'interface n'a pas encore affichée (fenêtre réduite,
/// interface en retard) : jamais plus d'une image en attente en mémoire.
type ScreenSlot = Arc<std::sync::Mutex<Option<(PeerId, egui::ColorImage)>>>;

/// Pareil pour les webcams : la dernière image décodée de chacune.
type FaceSlot = Arc<std::sync::Mutex<HashMap<PeerId, egui::ColorImage>>>;

enum Update {
    Status(String),
    /// Connecté à l'hôte ; `hosting` faux si un autre membre héberge déjà.
    Connected {
        host: SocketAddr,
        hosting: bool,
    },
    Joined(Box<ServerView>),
    Invite(Invite),
    /// Message que je viens d'envoyer, signé.
    Own(SignedMessage),
    Net(Event),
    /// Son ouvert (micro, haut-parleurs) ou fermé.
    Audio(Option<Arc<audio::Status>>),
    /// Ce que je partage maintenant (`None` : plus rien).
    Sharing(Option<capture::Source>),
    /// Nouvelle image d'un écran regardé, à prendre dans ce casier.
    Screen(ScreenSlot),
    /// Ma webcam est allumée ou éteinte.
    Camera(bool),
    /// Nouvelles images de webcams, à prendre dans ce casier.
    Faces(FaceSlot),
    /// Petit message affiché sous les messages.
    Notice(String),
    /// Où en est une pièce jointe.
    File {
        hash: FileHash,
        state: FileState,
    },
    /// L'hôte est parti : relève en cours (`None` : c'est fini).
    Reconnecting(Option<String>),
    /// Mon micro / mon son (changé par un raccourci, par exemple).
    SelfAudio {
        muted: bool,
        deafened: bool,
    },
    /// Fin de session : `Some(raison)` si elle n'a pas été voulue.
    Ended(Option<String>),
}

/// Pièce jointe récupérée (ou pas) pour l'interface.
enum FileState {
    Ready {
        /// Aperçu, si c'est une image.
        image: Option<egui::ColorImage>,
    },
    Failed(String),
}

#[derive(Clone)]
enum Invite {
    Searching,
    Public {
        code: String,
        warning: Option<String>,
    },
    /// Joignable sans port ouvert, grâce à l'annuaire décentralisé.
    Anywhere {
        code: String,
        warning: Option<String>,
    },
    LanOnly {
        code: String,
        reason: String,
    },
}

impl Invite {
    fn code(&self) -> Option<&str> {
        match self {
            Invite::Searching => None,
            Invite::Public { code, .. } | Invite::Anywhere { code, .. } | Invite::LanOnly { code, .. } => Some(code),
        }
    }
}

enum Target {
    Host(Box<ServerCard>),
    Join(invite::Invite),
}

/// Ce qui occupe la fenêtre en plein écran.
#[derive(Clone, Copy, PartialEq)]
enum Stage {
    /// L'écran regardé.
    Screen,
    /// La webcam de quelqu'un.
    Face(PeerId),
}

/// Largeur des petites webcams en bas du plein écran.
const STRIP_WIDTH: f32 = 176.0;

// --- Application -------------------------------------------------------------

pub fn run(code: Option<String>, name: Option<String>) -> Result<()> {
    // Deux fils suffisent au réseau d'un petit groupe (le son, l'écran et
    // l'interface ont les leurs) ; un par cœur, c'est de la mémoire pour rien.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    // L'annuaire met quelques secondes à se joindre : autant commencer tout de
    // suite, avant même qu'on clique sur un serveur.
    let store = Store::open_default()?;
    let single::Claim::First(listener) = single::claim(store.root(), code.as_deref()) else {
        tracing::info!("déjà ouverte : fenêtre ramenée au premier plan");
        return Ok(());
    };
    {
        let _rt = rt.enter();
        net::warm_up();
    }
    let identity = store.identity()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RIP Discord")
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([820.0, 520.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon.png")).unwrap_or_default(),
            ),
        ..Default::default()
    };
    eframe::run_native(
        "RIP Discord",
        options,
        Box::new(|cc| {
            apply_theme(&cc.egui_ctx);
            let relaunched = single::listen(listener, cc.egui_ctx.clone());
            let name = name
                .or_else(|| cc.storage.and_then(|s| s.get_string("name")))
                .unwrap_or_default();
            let prefs = load_prefs(cc.storage);
            audio::cue::configure(&prefs);
            let share_preview = cc.storage.and_then(|s| s.get_string("share_preview")).as_deref() != Some("0");
            let share_options = capture::Options {
                cursor: cc.storage.and_then(|s| s.get_string("share_cursor")).as_deref() != Some("0"),
                quality: capture::Quality::from_key(
                    &cc.storage
                        .and_then(|s| s.get_string("share_quality"))
                        .unwrap_or_default(),
                ),
                sound: cc.storage.and_then(|s| s.get_string("share_sound")).as_deref() != Some("0"),
            };
            let bindings = cc
                .storage
                .and_then(|s| eframe::get_value::<keys::Bindings>(s, "raccourcis"))
                .unwrap_or_default();
            let dialog = code.map(|code| Dialog::Join { code, show: false });
            let updates = cc.storage.and_then(|s| s.get_string("maj_auto")).as_deref() != Some("0");
            Ok(Box::new(App {
                welcome: name.trim().is_empty().then(String::new),
                prefs,
                share_preview,
                share_options,
                rt,
                servers: store.servers(),
                store,
                identity,
                name,
                error: None,
                forget: None,
                session: None,
                leaving: Vec::new(),
                help: false,
                dialog,
                me: None,
                hotkeys: keys::Hotkeys::start(bindings),
                relaunched,
                updater: update::Updater::new(updates),
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("interface graphique : {e}"))
}

struct App {
    rt: tokio::runtime::Runtime,
    store: Store,
    identity: Identity,
    /// Serveurs connus sur cette machine (créés ou rejoints).
    servers: Vec<ServerCard>,
    name: String,
    /// Premier lancement : pseudo en cours de saisie.
    welcome: Option<String>,
    error: Option<String>,
    /// Serveur dont l'oubli attend confirmation.
    forget: Option<ServerId>,
    session: Option<Session>,
    /// Sessions quittées qui finissent de fermer leurs connexions.
    leaving: Vec<JoinHandle<()>>,
    /// Fenêtre « Aide » ouverte.
    help: bool,
    dialog: Option<Dialog>,
    /// Fenêtre « Mes réglages » ouverte.
    me: Option<me::MeWindow>,
    prefs: audio::Prefs,
    /// Voir l'aperçu de mon partage d'écran (retenu d'une fois sur l'autre).
    share_preview: bool,
    /// Curseur et qualité du partage d'écran (retenus aussi).
    share_options: capture::Options,
    /// Raccourcis clavier globaux (mute, appui-pour-parler, chuchotement).
    hotkeys: keys::Hotkeys,
    /// Codes d'invitation passés par un nouveau lancement de l'app.
    relaunched: std_mpsc::Receiver<String>,
    updater: update::Updater,
}

/// Petites fenêtres de l'accueil.
enum Dialog {
    Create { name: String, keep_history: bool },
    Join { code: String, show: bool },
}

impl eframe::App for App {
    /// Appelé même fenêtre réduite (contrairement à `ui`) : ce que le réseau
    /// envoie est traité au fil de l'eau au lieu de s'entasser en mémoire, et
    /// les notifications partent.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.updater.tick(ctx);
        // Lancée de nouveau avec un code : on propose de rejoindre (depuis
        // l'accueil seulement, pour ne pas quitter un serveur sans le vouloir).
        while let Ok(code) = self.relaunched.try_recv() {
            if self.session.is_none() {
                self.dialog = Some(Dialog::Join { code, show: false });
            }
        }
        if let Some(ended) = self.session.as_mut().and_then(Session::pump) {
            self.close_session(ended);
        }
        // Une session quittée finit de prévenir les autres en arrière-plan ;
        // la liste des serveurs est relue quand elle a fini de tout ranger.
        let before = self.leaving.len();
        self.leaving.retain(|t| !t.is_finished());
        if self.leaving.len() < before {
            self.servers = self.store.servers();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(name) = &mut self.welcome {
            if welcome(ui, name) {
                self.name = name.trim().to_string();
                self.welcome = None;
            }
            return;
        }
        match &mut self.session {
            Some(s) => {
                s.ui(ui, self.prefs.threshold_db);
                self.help |= std::mem::take(&mut s.wants_help);
                if std::mem::take(&mut s.wants_me) && self.me.is_none() {
                    self.me = Some(me::MeWindow::new());
                }
                // « Quitter » : retour immédiat à l'accueil, sans attendre que
                // la connexion soit fermée proprement (ça peut prendre des
                // secondes quand on héberge).
                if s.left {
                    self.close_session(None);
                    ui.ctx().request_repaint();
                }
            }
            None => self.home(ui),
        }
        let ctx = ui.ctx().clone();
        if let Some(w) = &mut self.me {
            let session = self.session.as_ref();
            // Ceux à qui on peut chuchoter : les membres du serveur ouvert.
            let people: Vec<(UserId, String)> = session
                .and_then(|s| s.view.as_ref())
                .map(|v| {
                    v.peers
                        .values()
                        .filter(|p| p.user != v.me)
                        .map(|p| (p.user, p.name.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let changed = w.show(
                &ctx,
                &mut self.name,
                &mut self.prefs,
                &self.store,
                &self.identity,
                session.and_then(|s| s.audio.clone()),
                session.is_some(),
                &mut self.help,
                &self.hotkeys,
                &people,
                &mut self.updater,
            );
            if let Some(prefs) = changed
                && let Some(s) = &self.session
            {
                let _ = s.cmd.send(Cmd::Audio(prefs));
            }
            if !w.open {
                self.me = None;
            }
        }
        help_window(&ctx, &mut self.help);
        self.updater.toast(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string("name", self.name.clone());
        save_prefs(storage, &self.prefs);
        let preview = self.session.as_ref().map_or(self.share_preview, |s| s.preview);
        storage.set_string("share_preview", if preview { "1" } else { "0" }.into());
        let o = self.session.as_ref().map_or(self.share_options, |s| s.share_options);
        storage.set_string("share_cursor", if o.cursor { "1" } else { "0" }.into());
        storage.set_string("share_quality", o.quality.key().into());
        storage.set_string("share_sound", if o.sound { "1" } else { "0" }.into());
        eframe::set_value(storage, "raccourcis", &self.hotkeys.bindings());
        storage.set_string("maj_auto", if self.updater.auto { "1" } else { "0" }.into());
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Laisse la session (et celles qu'on vient de quitter) prévenir les
        // autres et fermer le port sur la box.
        if let Some(s) = self.session.take() {
            let _ = s.cmd.send(Cmd::Leave);
            self.leaving.push(s.task);
        }
        finish_leaving(&self.rt, std::mem::take(&mut self.leaving));
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }
}

/// Attend (3 s au plus) que les sessions quittées aient fini de fermer.
/// Appelé depuis le fil de l'interface, hors du runtime : `timeout` arme sa
/// minuterie dès sa création, il doit donc être créé dans le bloc async.
fn finish_leaving(rt: &tokio::runtime::Runtime, tasks: Vec<JoinHandle<()>>) {
    rt.block_on(async {
        let _ = timeout(Duration::from_secs(3), async {
            for t in tasks {
                let _ = t.await;
            }
        })
        .await;
    });
}

/// Premier lancement : on demande juste un pseudo. Renvoie vrai une fois choisi.
fn welcome(ui: &mut egui::Ui, name: &mut String) -> bool {
    let mut done = false;
    egui::CentralPanel::no_frame().show(ui, |ui| {
        ambient_background(ui);
        ui.add_space((ui.available_height() / 2.0 - 205.0).max(24.0));
        ui.vertical_centered(|ui| {
            card().fill(SURFACE).inner_margin(34).show(ui, |ui| {
                ui.set_width(390.0);
                ui.vertical_centered(|ui| {
                    brand_mark(ui, 58.0);
                    ui.add_space(14.0);
                    eyebrow(ui, "BIENVENUE DANS TON ESPACE");
                    ui.label(RichText::new("Ta bande.\nTa voix.").size(30.0).strong().color(TEXT));
                    ui.add_space(6.0);
                    ui.label(RichText::new("Des salons privés, hébergés par vos PC. Sans compte.").color(MUTED));
                });
                ui.add_space(28.0);
                field_label(ui, "Ton pseudo");
                let field = ui.add(text_field(name, "Ex. Lwrsk").char_limit(32));
                if !field.has_focus() && name.is_empty() {
                    field.request_focus();
                }
                hint(
                    ui,
                    "C'est le nom que verront tes amis. Tu pourras le changer dans tes réglages.",
                );
                ui.add_space(18.0);
                let ok = !name.trim().is_empty();
                let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if ui
                    .add_enabled(ok, primary_button("Entrer dans RIP Discord", 390.0))
                    .clicked()
                    || (ok && enter)
                {
                    done = true;
                }
            });
        });
    });
    done
}

impl App {
    /// Revient à l'accueil. La tâche réseau de la session continue seule
    /// jusqu'à avoir prévenu les autres.
    fn close_session(&mut self, error: Option<String>) {
        if let Some(s) = self.session.take() {
            self.share_preview = s.preview;
            self.share_options = s.share_options;
            if !s.task.is_finished() {
                self.leaving.push(s.task);
            }
        }
        self.hotkeys.set_target(None);
        self.error = error;
        self.servers = self.store.servers();
    }

    fn display_name(&self) -> String {
        match self.name.trim() {
            "" => "Anonyme".to_string(),
            n => n.to_string(),
        }
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        egui::Panel::top("home_top")
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.55)))
                    .inner_margin(Margin::symmetric(22, 11)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    brand_mark(ui, 28.0);
                    ui.add_space(2.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("RIP Discord").size(18.0).strong().color(TEXT));
                        ui.label(
                            RichText::new(format!("LOCAL • PRIVÉ • P2P • v{VERSION}"))
                                .color(MUTED)
                                .small(),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if me_chip(ui, &self.display_name(), "Mes réglages").clicked() && self.me.is_none() {
                            self.me = Some(me::MeWindow::new());
                        }
                        if ui
                            .add(Button::new(Icon::Help).height(36.0))
                            .on_hover_text("Aide : comment ça marche, que faire si ça ne se connecte pas")
                            .clicked()
                        {
                            self.help = true;
                        }
                    });
                });
            });

        egui::CentralPanel::no_frame().show(ui, |ui| {
            ambient_background(ui);
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let width = ui.available_width().min(800.0);
                ui.add_space(34.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - width) / 2.0);
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        self.home_body(ui, &mut action);
                    });
                });
                ui.add_space(36.0);
            });
        });

        if let Some(a) = self.dialog_ui(ui.ctx()) {
            action = Some(a);
        }
        if let Some(target) = action {
            let name = self.display_name();
            self.error = None;
            self.dialog = None;
            self.session = Some(Session::start(
                &self.rt,
                ui.ctx().clone(),
                target,
                name,
                self.store.clone(),
                self.identity.clone(),
                self.prefs.clone(),
                std::mem::take(&mut self.leaving),
            ));
            if let Some(s) = &mut self.session {
                s.preview = self.share_preview;
                s.share_options = self.share_options;
                self.hotkeys.set_target(Some(s.cmd.clone()));
            }
        }
    }

    fn home_body(&mut self, ui: &mut egui::Ui, action: &mut Option<Target>) {
        egui::Frame::new()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.34)))
            .corner_radius(22)
            .inner_margin(26)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Parle comme chez toi.").size(30.0).strong().color(TEXT));
                        ui.add_space(5.0);
                        ui.label(
                            RichText::new("Crée un repaire pour ta bande ou rejoins celui d’un ami.").color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        brand_mark(ui, 70.0);
                    });
                });
                ui.add_space(24.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            Button::new(Icon::Plus)
                                .label("Créer un serveur")
                                .kind(Look::Primary)
                                .height(44.0)
                                .width(220.0),
                        )
                        .clicked()
                    {
                        self.dialog = Some(Dialog::Create {
                            name: String::new(),
                            keep_history: true,
                        });
                    }
                    if ui
                        .add(
                            Button::new(Icon::ArrowRight)
                                .label("Rejoindre avec un code")
                                .height(44.0)
                                .width(230.0),
                        )
                        .clicked()
                    {
                        self.dialog = Some(Dialog::Join {
                            code: String::new(),
                            show: false,
                        });
                    }
                });
            });
        ui.add_space(30.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Mes serveurs").size(21.0).strong().color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} enregistré{}",
                        self.servers.len(),
                        if self.servers.len() > 1 { "s" } else { "" }
                    ))
                    .color(MUTED)
                    .small(),
                );
            });
        });
        ui.add_space(12.0);

        let mut dismiss = false;
        if let Some(err) = &self.error {
            egui::Frame::new()
                .fill(DANGER.gamma_multiply(0.12))
                .stroke(Stroke::new(1.0, DANGER.gamma_multiply(0.5)))
                .corner_radius(14)
                .inner_margin(15)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let (head, detail) = err.split_once('\n').unwrap_or((err, ""));
                    ui.horizontal(|ui| {
                        status_dot(ui, DANGER);
                        ui.add(egui::Label::new(RichText::new(head).color(TEXT)).wrap());
                    });
                    if !detail.is_empty() {
                        ui.add(egui::Label::new(RichText::new(detail).color(MUTED).small()).wrap());
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui
                            .link(RichText::new("Voir les pistes de résolution").small())
                            .clicked()
                        {
                            self.help = true;
                        }
                        ui.add_space(12.0);
                        if ui.link(RichText::new("Fermer").small()).clicked() {
                            dismiss = true;
                        }
                    });
                });
            ui.add_space(14.0);
        }
        if dismiss {
            self.error = None;
        }

        if self.servers.is_empty() {
            egui::Frame::new()
                .fill(CARD.gamma_multiply(0.78))
                .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.75)))
                .corner_radius(18)
                .inner_margin(30)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.vertical_centered(|ui| {
                        brand_mark(ui, 42.0);
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new("Ton premier salon t’attend.")
                                .size(18.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.add_space(4.0);
                        hint(
                            ui,
                            "Crée un serveur en quelques secondes, ou utilise le code envoyé par un ami.",
                        );
                    });
                });
            return;
        }

        let me = self.identity.id();
        let mut forget_now = None;
        for c in &self.servers {
            egui::Frame::new()
                .fill(CARD)
                .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.9)))
                .corner_radius(18)
                .inner_margin(16)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        avatar(ui, c.name(), 48.0);
                        ui.add_space(5.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(c.name()).size(17.0).strong().color(TEXT));
                            let role = match c.role(&me) {
                                Role::Owner => "créateur",
                                Role::Admin => "admin",
                                Role::Member => "membre",
                            };
                            let channels = c.settings().text_channels.len() + c.settings().voice_channels.len();
                            ui.label(
                                RichText::new(format!(
                                    "{role}  ·  {channels} salons  ·  {}",
                                    if c.history().keep {
                                        "historique actif"
                                    } else {
                                        "éphémère"
                                    }
                                ))
                                .color(MUTED)
                                .small(),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if self.forget == Some(c.id) {
                                if ui
                                    .add(
                                        Button::new(Icon::Trash)
                                            .label("Effacer")
                                            .kind(Look::DangerSolid)
                                            .height(38.0),
                                    )
                                    .clicked()
                                {
                                    forget_now = Some(c.id);
                                }
                                if ui.add(Button::text("Garder").kind(Look::Ghost).height(38.0)).clicked() {
                                    self.forget = None;
                                }
                                ui.label(RichText::new("L'effacer de ce PC ?").color(MUTED));
                            } else {
                                let more = ui.add(Button::new(Icon::More).height(38.0)).on_hover_text("Plus");
                                egui::Popup::menu(&more).show(|ui| {
                                    if ui
                                        .add(Button::new(Icon::Trash).label("Oublier ce serveur").kind(Look::Danger))
                                        .on_hover_text("Efface cette copie du serveur sur ce PC.")
                                        .clicked()
                                    {
                                        self.forget = Some(c.id);
                                        ui.close();
                                    }
                                });
                                if ui
                                    .add(
                                        Button::new(Icon::ArrowRight)
                                            .label("Ouvrir")
                                            .kind(Look::Primary)
                                            .height(38.0),
                                    )
                                    .clicked()
                                {
                                    *action = Some(Target::Host(Box::new(c.clone())));
                                }
                            }
                        });
                    });
                });
            ui.add_space(10.0);
        }
        if let Some(id) = forget_now {
            if let Err(e) = self.store.forget(id) {
                self.error = Some(describe(&e));
            }
            self.forget = None;
            self.servers = self.store.servers();
        }
        hint(
            ui,
            "Ouvrir connecte à un membre déjà en ligne. S’il n’y en a pas, ton PC prend le relais avec la \
             même copie du serveur.",
        );
    }

    /// Fenêtres « Créer un serveur » et « Rejoindre un serveur ».
    fn dialog_ui(&mut self, ctx: &egui::Context) -> Option<Target> {
        let placeholder = format!("Serveur de {}", self.display_name());
        let dialog = self.dialog.as_mut()?;
        let mut action = None;
        let mut close = false;
        let mut error = None;
        let r = egui::Modal::new(egui::Id::unique("dialog"))
            .frame(card().inner_margin(26))
            .show(ctx, |ui| {
                ui.set_width(400.0);
                let (glyph, title, sub) = match dialog {
                    Dialog::Create { .. } => (
                        Icon::Plus,
                        "Créer un serveur",
                        "Tu en seras le créateur. Tes amis pourront l'héberger quand tu n'es pas là.",
                    ),
                    Dialog::Join { .. } => (
                        Icon::ArrowRight,
                        "Rejoindre un serveur",
                        "Colle le code d'invitation que t'a donné un ami.",
                    ),
                };
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 12, ACCENT.gamma_multiply(0.22));
                    icons::paint(ui.painter(), rect.shrink(10.0), glyph, ACCENT_HOVER);
                    ui.add_space(4.0);
                    ui.label(RichText::new(title).size(20.0).strong().color(TEXT));
                    at_right(ui, |ui| {
                        if ui
                            .add(Button::new(Icon::Close))
                            .on_hover_text("Fermer (Échap)")
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
                ui.add_space(2.0);
                hint(ui, sub);
                ui.add_space(14.0);
                match dialog {
                    Dialog::Create { name, keep_history } => {
                        field_label(ui, "Nom du serveur");
                        let field = ui.add(text_field(name, &placeholder).char_limit(48));
                        ui.add_space(8.0);
                        group(ui, "", |ui| {
                            switch_row(
                                ui,
                                "Garder l'historique",
                                "Ceux qui arrivent plus tard voient ce qui a été dit. Modifiable plus tard.",
                                keep_history,
                            );
                        });
                        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        ui.horizontal(|ui| {
                            let create = ui.add(
                                Button::new(Icon::Check)
                                    .label("Créer")
                                    .kind(Look::Primary)
                                    .height(42.0)
                                    .width(196.0),
                            );
                            if create.clicked() || enter {
                                let name = match name.trim() {
                                    "" => placeholder.clone(),
                                    n => n.to_string(),
                                };
                                let mut card = ServerCard::create(&self.identity, &name);
                                if !*keep_history {
                                    let _ = card.edit(&self.identity, |s| s.history.keep = false);
                                }
                                match self.store.save_card(&card) {
                                    Ok(card) => action = Some(Target::Host(Box::new(card))),
                                    Err(e) => error = Some(describe(&e)),
                                }
                            }
                            if ui
                                .add(Button::text("Annuler").kind(Look::Ghost).height(42.0).width(196.0))
                                .clicked()
                            {
                                close = true;
                            }
                        });
                    }
                    Dialog::Join { code, show } => {
                        field_label(ui, "Code d'invitation");
                        let mut field = None;
                        ui.horizontal(|ui| {
                            let w = ui.available_width() - 50.0;
                            field = Some(
                                ui.add_sized(
                                    [w, 40.0],
                                    text_field(code, "Colle le code ici")
                                        .password(!*show)
                                        .font(egui::TextStyle::Monospace),
                                ),
                            );
                            let (glyph, tip) = if *show {
                                (Icon::EyeOff, "Cacher le code")
                            } else {
                                (Icon::Eye, "Afficher le code")
                            };
                            if ui.add(Button::new(glyph).height(40.0)).on_hover_text(tip).clicked() {
                                *show = !*show;
                            }
                        });
                        hint(
                            ui,
                            "Le code est caché pour éviter qu'il fuite sur un partage d'écran ou un stream.",
                        );
                        ui.add_space(16.0);
                        let enter = field.is_some_and(|f| f.lost_focus()) && ui.input(|i| i.key_pressed(Key::Enter));
                        ui.horizontal(|ui| {
                            let join = ui.add_enabled(
                                !code.trim().is_empty(),
                                Button::new(Icon::ArrowRight)
                                    .label("Rejoindre")
                                    .kind(Look::Primary)
                                    .height(42.0)
                                    .width(196.0),
                            );
                            if join.clicked() || enter {
                                match invite::decode(code) {
                                    Ok(c) => action = Some(Target::Join(c)),
                                    Err(e) => error = Some(describe(&e)),
                                }
                            }
                            if ui
                                .add(Button::text("Annuler").kind(Look::Ghost).height(42.0).width(196.0))
                                .clicked()
                            {
                                close = true;
                            }
                        });
                    }
                }
            });
        if let Some(e) = error {
            self.error = Some(e);
            close = true;
        }
        if close || r.should_close() {
            self.dialog = None;
        }
        action
    }
}

/// Pastille « moi » : avatar et pseudo, ouvre mes réglages.
fn me_chip(ui: &mut egui::Ui, name: &str, tooltip: &str) -> egui::Response {
    let font = egui::FontId::proportional(15.0);
    let galley = ui.painter().layout_no_wrap(name.to_string(), font, TEXT);
    let size = vec2(28.0 + 8.0 + galley.size().x + 16.0, 36.0);
    let (rect, r) = ui.allocate_exact_size(size, egui::Sense::click());
    let p = ui.painter();
    if r.hovered() {
        p.rect_filled(rect, 10.0, BORDER);
    }
    let c = egui::pos2(rect.left() + 8.0 + 14.0, rect.center().y);
    p.circle_filled(c, 14.0, color_for(name));
    let initial = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    p.text(
        c,
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(13.0),
        Color32::WHITE,
    );
    p.galley(
        egui::pos2(rect.left() + 8.0 + 28.0 + 8.0, rect.center().y - galley.size().y / 2.0),
        galley,
        TEXT,
    );
    r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tooltip)
}

// --- Session -----------------------------------------------------------------

struct Session {
    ctx: egui::Context,
    hosting: bool,
    /// L'utilisateur a demandé l'aide ou ses réglages (l'app ouvre la fenêtre).
    wants_help: bool,
    wants_me: bool,
    /// L'utilisateur a cliqué « Quitter » (l'app revient à l'accueil).
    left: bool,
    /// Son ouvert (dans un salon vocal).
    audio: Option<Arc<audio::Status>>,
    muted: bool,
    deafened: bool,
    /// Ce que je partage.
    sharing: Option<capture::Source>,
    /// Je veux voir l'aperçu de mon propre partage.
    preview: bool,
    share_options: capture::Options,
    /// Fenêtre de choix de ce qu'on partage.
    picker: Option<share::Picker>,
    /// Plein écran : l'écran regardé ou une webcam.
    big: Option<Stage>,
    /// Vignettes des webcams affichées (sinon toutes cachées).
    faces_shown: bool,
    /// Webcams que j'ai masquées une à une.
    faces_hidden: HashSet<PeerId>,
    /// Écran regardé (le mien pour l'aperçu), et sa dernière image.
    watching: Option<(PeerId, Option<egui::TextureHandle>)>,
    /// Ma webcam est allumée.
    camera: bool,
    /// Dernière image de chaque webcam du salon (la mienne comprise).
    faces: HashMap<PeerId, egui::TextureHandle>,
    /// Adresse de l'hôte quand on n'héberge pas.
    remote: Option<SocketAddr>,
    status: String,
    /// Début de la connexion en cours (affiché en secondes).
    connecting_since: f64,
    /// Mon pseudo et le nom du serveur, avant d'être connecté.
    name: String,
    title: String,
    view: Option<ServerView>,
    identity: Identity,
    channel: ChannelId,
    chat: chat::Chat,
    notice: Option<(String, f64)>,
    invite: Option<Invite>,
    rtt: Option<Duration>,
    /// Messages déjà vus par salon (pour signaler les nouveaux).
    read: HashMap<ChannelId, usize>,
    /// Mentions pas encore vues, par salon.
    pings: HashMap<ChannelId, usize>,
    /// Messages arrivés pendant que la fenêtre était en arrière-plan (titre).
    unseen: usize,
    /// Dernier son « nouveau message » (pas un par message d'une rafale).
    message_cue: Option<std::time::Instant>,
    /// Volume choisi pour certains participants.
    volumes: HashMap<PeerId, f32>,
    /// Son de leur partage d'écran : volume, coupé.
    share_volumes: HashMap<PeerId, (f32, bool)>,
    show_members: bool,
    /// L'hôte est parti : relève en cours (message affiché en bandeau).
    reconnecting: Option<String>,
    /// Dernière reconnexion (on ne signale pas les arrivées qui suivent).
    reconnected_at: f64,
    /// Vote en cours pour changer d'hôte : numéro, candidat, fin, déjà voté.
    vote: Option<(u64, PeerId, f64, bool)>,
    /// J'ai demandé à héberger (le vote est en route).
    asked_host: bool,
    settings: Option<server::ServerWindow>,
    cmd: mpsc::UnboundedSender<Cmd>,
    updates: std_mpsc::Receiver<Update>,
    task: JoinHandle<()>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    fn start(
        rt: &tokio::runtime::Runtime,
        ctx: egui::Context,
        target: Target,
        name: String,
        store: Store,
        identity: Identity,
        prefs: audio::Prefs,
        previous: Vec<JoinHandle<()>>,
    ) -> Self {
        let (cmd, cmd_rx) = mpsc::unbounded_channel();
        let (tx, updates) = std_mpsc::channel();
        let hosting = matches!(target, Target::Host(_));
        let title = match &target {
            Target::Host(card) => card.name().to_string(),
            Target::Join(_) => "Serveur".to_string(),
        };
        let my_name = name.clone();
        let out = Out { tx, ctx: ctx.clone() };
        let me = identity.clone();
        let task = rt.spawn(async move {
            // La session d'avant prévient les autres et se retire de
            // l'annuaire : on lui laisse un court instant (on peut vouloir
            // ré-héberger celui qu'on vient de quitter), le reste se finit en
            // parallèle.
            if previous.iter().any(|t| !t.is_finished()) {
                out.send(Update::Status("Fermeture de la session précédente…".into()));
                let _ = timeout(Duration::from_secs(2), async {
                    for t in previous {
                        let _ = t.await;
                    }
                })
                .await;
                out.send(Update::Status(start_status(hosting).into()));
            }
            let ended = match net::session(target, name, store, prefs, me, cmd_rx, &out).await {
                Ok(()) => None,
                Err(e) => Some(describe(&e)),
            };
            out.send(Update::Ended(ended));
        });
        Self {
            ctx,
            hosting,
            wants_help: false,
            wants_me: false,
            left: false,
            audio: None,
            muted: false,
            deafened: false,
            sharing: None,
            preview: true,
            share_options: capture::Options::default(),
            picker: None,
            big: None,
            faces_shown: true,
            faces_hidden: HashSet::new(),
            watching: None,
            camera: false,
            faces: HashMap::new(),
            remote: None,
            status: start_status(hosting).into(),
            connecting_since: now_secs(),
            name: my_name,
            title,
            view: None,
            identity,
            channel: 0,
            chat: chat::Chat::default(),
            notice: None,
            invite: None,
            rtt: None,
            read: HashMap::new(),
            pings: HashMap::new(),
            unseen: 0,
            message_cue: None,
            volumes: HashMap::new(),
            share_volumes: HashMap::new(),
            show_members: true,
            reconnecting: None,
            reconnected_at: 0.0,
            vote: None,
            asked_host: false,
            settings: None,
            cmd,
            updates,
            task,
        }
    }

    /// Applique les mises à jour reçues. Renvoie `Some` quand la session est finie.
    fn pump(&mut self) -> Option<Option<String>> {
        while let Ok(u) = self.updates.try_recv() {
            match u {
                Update::Status(s) => self.status = s,
                Update::Connected { host, hosting } => {
                    self.hosting = hosting;
                    self.remote = (!hosting).then_some(host);
                }
                Update::Joined(view) => {
                    // Après une relève, on reste dans le même salon texte.
                    let s = view.card.settings();
                    if !s.is_text_channel(self.channel) {
                        self.channel = s.text_channels.first().map_or(0, |c| c.id);
                    }
                    if self.view.is_some() {
                        self.reconnected_at = now_secs();
                    }
                    self.view = Some(*view);
                    self.vote = None;
                    self.asked_host = false;
                    self.mark_all_read();
                }
                Update::Invite(i) => self.invite = Some(i),
                Update::Own(msg) => {
                    if let Some(v) = &mut self.view {
                        v.push_own(msg);
                    }
                }
                Update::Net(ev) => {
                    let history = matches!(ev, Event::History(_));
                    match &ev {
                        Event::Vote { id, candidate, secs } => {
                            let mine = self.view.as_ref().is_some_and(|v| v.my_peer == *candidate);
                            self.vote = Some((*id, *candidate, now_secs() + f64::from(*secs), mine));
                        }
                        Event::VoteResult {
                            candidate,
                            accepted,
                            yes,
                            no,
                            ..
                        } => {
                            self.vote = None;
                            self.asked_host = false;
                            let name = self.view.as_ref().map_or_else(String::new, |v| v.peer_name(*candidate));
                            let line = if *accepted {
                                format!("{name} devient l'hôte du serveur ({yes} pour, {no} contre).")
                            } else {
                                format!("Demande de {name} refusée ({yes} pour, {no} contre) : l'hôte ne change pas.")
                            };
                            self.notice = Some((line, now_secs()));
                        }
                        _ => {}
                    }
                    // Juste après une relève, les membres « reviennent » : pas de quoi le signaler.
                    let quiet = now_secs() - self.reconnected_at < 15.0
                        && matches!(ev, Event::PeerJoined(_) | Event::PeerLeft { .. });
                    if let Some(v) = &mut self.view {
                        if let Some(line) = v.apply(&ev)
                            && !quiet
                        {
                            self.notice = Some((line, now_secs()));
                        }
                        if let Event::Pong { rtt } = ev {
                            self.rtt = Some(rtt);
                        }
                        // L'écran regardé n'est plus partagé : on ferme (le mien
                        // se ferme avec `Update::Sharing(false)`). Juste après une
                        // relève, on laisse au partageur le temps de revenir.
                        if let Some((w, _)) = &self.watching
                            && *w != v.my_peer
                            && now_secs() - self.reconnected_at > 10.0
                            && !v.voice.values().any(|r| r.sharing.contains(w))
                        {
                            self.watching = None;
                        }
                        // Salon courant supprimé : retour au premier.
                        let s = v.card.settings();
                        if !s.is_text_channel(self.channel) {
                            self.channel = s.text_channels.first().map_or(0, |c| c.id);
                        }
                    }
                    // L'historique, c'est du déjà-vu : pas de pastille « nouveau ».
                    if history {
                        self.mark_all_read();
                    }
                    if let Event::Text { msg, .. } = &ev {
                        self.alert(msg);
                    }
                }
                Update::Audio(a) => self.audio = a,
                Update::Sharing(source) => {
                    let on = source.is_some();
                    self.sharing = source;
                    let me = self.view.as_ref().map(|v| v.my_peer);
                    if on && self.preview && self.watching.is_none() {
                        self.set_watching(me);
                    } else if !on && self.watching.as_ref().is_some_and(|(w, _)| Some(*w) == me) {
                        self.set_watching(None);
                    }
                }
                Update::Notice(n) => self.notice = Some((n, now_secs())),
                Update::File { hash, state } => {
                    if let Some(c) = self.chat.file_ready(&self.ctx, hash, state) {
                        let _ = self.cmd.send(c);
                    }
                }
                Update::Reconnecting(r) => {
                    if self.reconnecting.is_none() && r.is_some() {
                        self.connecting_since = now_secs();
                    }
                    self.reconnecting = r;
                }
                Update::SelfAudio { muted, deafened } => {
                    self.muted = muted;
                    self.deafened = deafened;
                }
                Update::Screen(slot) => {
                    let Some((from, image)) = slot.lock().unwrap().take() else {
                        continue;
                    };
                    if let Some((who, tex)) = &mut self.watching
                        && *who == from
                    {
                        match tex {
                            Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                            None => *tex = Some(self.ctx.load_texture("ecran", image, egui::TextureOptions::LINEAR)),
                        }
                    }
                }
                Update::Camera(on) => {
                    self.camera = on;
                    if let (false, Some(v)) = (on, &self.view) {
                        self.faces.remove(&v.my_peer);
                    }
                }
                Update::Faces(slot) => {
                    let images = std::mem::take(&mut *slot.lock().unwrap());
                    for (from, image) in images {
                        match self.faces.get_mut(&from) {
                            Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                            None => {
                                let t = self.ctx.load_texture(
                                    format!("webcam-{from}"),
                                    image,
                                    egui::TextureOptions::LINEAR,
                                );
                                self.faces.insert(from, t);
                            }
                        }
                    }
                }
                Update::Ended(e) => return Some(e),
            }
        }
        None
    }

    /// Change l'écran affiché : celui d'un autre (demandé à l'hôte), le mien
    /// (aperçu local, sans passer par le réseau) ou aucun.
    fn set_watching(&mut self, who: Option<PeerId>) {
        let me = self.view.as_ref().map(|v| v.my_peer);
        let before = self.watching.as_ref().map(|(w, _)| *w);
        if before == who {
            return;
        }
        if before.is_some() && before == me {
            let _ = self.cmd.send(Cmd::Preview(false));
        } else if before.is_some() && who.is_none_or(|w| Some(w) == me) {
            let _ = self.cmd.send(Cmd::Watch(None));
        }
        match who {
            Some(w) if Some(w) == me => {
                let _ = self.cmd.send(Cmd::Preview(true));
            }
            Some(w) => {
                let _ = self.cmd.send(Cmd::Watch(Some(w)));
            }
            None => {}
        }
        self.watching = who.map(|w| (w, None));
    }

    /// Nouveau message d'un autre : pastille, notification s'il me
    /// mentionne et que je ne le vois pas, barre des tâches qui clignote si
    /// la fenêtre est en arrière-plan.
    fn alert(&mut self, msg: &SignedMessage) {
        let Some(v) = &self.view else { return };
        if msg.signer == v.me || msg.body.kind.target().is_some() {
            return;
        }
        let focused = self.ctx.input(|i| i.viewport().focused).unwrap_or(true);
        let seen = focused && msg.body.channel == self.channel;
        let attention = if v.mentions_me(msg) && !seen {
            *self.pings.entry(msg.body.channel).or_default() += 1;
            let channel = v
                .card
                .settings()
                .text_channels
                .iter()
                .find(|c| c.id == msg.body.channel)
                .map_or_else(String::new, |c| c.name.clone());
            let mut text: String = v.text(msg).chars().take(140).collect();
            if text.is_empty() {
                text = "a envoyé un fichier".into();
            }
            notify::show(&format!("{} dans #{channel}", v.author(msg)), &text);
            audio::Cue::Mention.play();
            Some(egui::UserAttentionType::Critical)
        } else if !focused {
            if self.message_cue.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
                self.message_cue = Some(std::time::Instant::now());
                audio::Cue::Message.play();
            }
            Some(egui::UserAttentionType::Informational)
        } else {
            None
        };
        if let Some(a) = attention {
            self.ctx
                .send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(a));
        }
        if !focused {
            self.unseen += 1;
            self.ctx
                .send_viewport_cmd(egui::ViewportCommand::Title(format!("({}) RIP Discord", self.unseen)));
        }
    }

    fn mark_all_read(&mut self) {
        let Some(v) = &self.view else { return };
        for c in &v.card.settings().text_channels {
            self.read.insert(c.id, v.messages(c.id).count());
        }
    }

    fn unread(&self, channel: ChannelId) -> bool {
        let Some(v) = &self.view else { return false };
        v.messages(channel).count() > self.read.get(&channel).copied().unwrap_or(0)
    }

    fn channel_name(&self) -> String {
        self.view
            .as_ref()
            .and_then(|v| v.card.settings().text_channels.iter().find(|c| c.id == self.channel))
            .map_or_else(String::new, |c| c.name.clone())
    }

    fn open_settings(&mut self, tab: server::Tab) {
        if let Some(v) = &self.view {
            self.settings = Some(server::ServerWindow::new(v, tab));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, threshold: f32) {
        // Le salon affiché est lu.
        if let Some(v) = &self.view {
            self.read.insert(self.channel, v.messages(self.channel).count());
        }
        if ui.input(|i| i.viewport().focused).unwrap_or(true) {
            self.pings.remove(&self.channel);
            if self.unseen > 0 {
                self.unseen = 0;
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Title("RIP Discord".into()));
            }
        }

        // Plein écran : l'écran regardé ou une webcam, les autres webcams en
        // petit dessous (Échap pour sortir).
        if let Some(stage) = self.big {
            let gone = match stage {
                Stage::Screen => self.watching.is_none(),
                Stage::Face(p) => !self.face_ids().contains(&p),
            };
            if gone || ui.input(|i| i.key_pressed(Key::Escape)) {
                self.set_big(ui.ctx(), None);
            }
        }
        if let Some(stage) = self.big {
            egui::CentralPanel::no_frame().show(ui, |ui| {
                egui::Frame::new().inner_margin(10).show(ui, |ui| {
                    match stage {
                        Stage::Screen => self.screen_view(ui),
                        Stage::Face(p) => self.big_face(ui, p),
                    }
                    let exclude = match stage {
                        Stage::Face(p) => Some(p),
                        Stage::Screen => None,
                    };
                    self.face_strip(ui, exclude, STRIP_WIDTH, true);
                });
            });
            return;
        }

        egui::Panel::left("side")
            .resizable(false)
            .exact_size(268.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.7))),
            )
            .show(ui, |ui| self.sidebar(ui, threshold));

        let wide = ui.available_width() > 740.0;
        if self.view.is_some() {
            egui::Panel::top("top")
                .frame(
                    egui::Frame::new()
                        .fill(CARD.gamma_multiply(0.92))
                        .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.48)))
                        .inner_margin(Margin::symmetric(22, 11)),
                )
                .show(ui, |ui| self.top_bar(ui, wide));
        }

        if wide && self.show_members && self.view.is_some() {
            egui::Panel::right("members")
                .resizable(false)
                .exact_size(224.0)
                .frame(
                    egui::Frame::new()
                        .fill(PANEL)
                        .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.7)))
                        .inner_margin(Margin::symmetric(16, 18)),
                )
                .show(ui, |ui| self.members(ui));
        }

        if self.view.is_some() {
            egui::Panel::bottom("input")
                .frame(
                    egui::Frame::new()
                        .fill(CARD.gamma_multiply(0.9))
                        .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.48)))
                        .inner_margin(Margin::symmetric(22, 14)),
                )
                .show(ui, |ui| self.input_bar(ui));
        }

        egui::CentralPanel::no_frame().show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(Margin::symmetric(20, 12))
                .show(ui, |ui| self.center(ui));
        });

        let ctx = ui.ctx().clone();
        self.share_picker(&ctx);
        if let Some(w) = &mut self.settings
            && let Some(view) = &self.view
        {
            let out = w.show(
                &ctx,
                view,
                &self.identity,
                self.invite.as_ref(),
                self.hosting,
                self.remote,
            );
            match out {
                server::Outcome::Open => {}
                server::Outcome::Closed => self.settings = None,
                server::Outcome::Save(card) => {
                    let _ = self.cmd.send(Cmd::UpdateCard(card));
                    self.settings = None;
                }
                server::Outcome::Error(e) => self.notice = Some((e, now_secs())),
                server::Outcome::Help => self.wants_help = true,
            }
        }
    }

    /// L'écran regardé (ou l'aperçu du mien), en carte au-dessus des
    /// messages ou en plein écran.
    fn screen_view(&mut self, ui: &mut egui::Ui) {
        let Some(view) = &self.view else { return };
        if let Some((who, tex)) = &self.watching {
            let who = *who;
            let mine = who == view.my_peer;
            let mut close = false;
            let mut big = self.big == Some(Stage::Screen);
            // En plein écran, la place des petites webcams en dessous.
            // En plein écran : cacher ou montrer les webcams d'un clic.
            let has_faces = big && !self.face_ids().is_empty();
            let mut faces_shown = self.faces_shown;
            let reserve = if big && !self.visible_faces().is_empty() {
                STRIP_WIDTH * 9.0 / 16.0 + 16.0
            } else {
                0.0
            };
            // Son de son partage : volume, coupé.
            let mut sound = self.share_volumes.get(&who).copied().unwrap_or((1.0, false));
            let sound_before = sound;
            egui::Frame::new()
                .fill(Color32::BLACK)
                .corner_radius(12)
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("EN DIRECT").color(DANGER).small().strong());
                        let title = if mine {
                            "Ton écran (aperçu)".to_string()
                        } else {
                            format!("Écran de {}", view.peer_name(who))
                        };
                        ui.label(RichText::new(title).strong().color(TEXT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            close = ui
                                .button(if mine { "Cacher" } else { "Fermer" })
                                .on_hover_text(if mine {
                                    "Le partage continue, seul ton aperçu est caché"
                                } else {
                                    "Arrêter de regarder"
                                })
                                .clicked();
                            if has_faces {
                                faces_button(ui, &mut faces_shown);
                            }
                            if ui
                                .button(if big { "Quitter le plein écran" } else { "Plein écran" })
                                .on_hover_text("Ou double-clic sur l'image ; Échap pour sortir")
                                .clicked()
                            {
                                big = !big;
                            }
                            if !mine {
                                let (volume, muted) = &mut sound;
                                if ui
                                    .button(if *muted { "🔇" } else { "🔊" })
                                    .on_hover_text(if *muted {
                                        "Rétablir le son du partage"
                                    } else {
                                        "Couper le son du partage"
                                    })
                                    .clicked()
                                {
                                    *muted = !*muted;
                                }
                                ui.spacing_mut().slider_width = 90.0;
                                ui.add_enabled(
                                    !*muted,
                                    egui::Slider::new(volume, 0.0..=2.0)
                                        .show_value(false)
                                        .trailing_fill(true),
                                )
                                .on_hover_text(format!("Volume du son partagé : {:.0} %", *volume * 100.0));
                            }
                        });
                    });
                    let max = vec2(
                        ui.available_width(),
                        if big {
                            (ui.available_height() - reserve) * 0.97
                        } else {
                            ui.available_height() * 0.6
                        },
                    );
                    match tex {
                        Some(t) => {
                            let size = t.size_vec2();
                            let mut scale = (max.x / size.x).min(max.y / size.y);
                            // En petit, jamais plus grand que l'original ; en grand, ça remplit.
                            if !big {
                                scale = scale.min(1.0);
                            }
                            let shown = size * scale;
                            let (row, r) =
                                ui.allocate_exact_size(vec2(ui.available_width(), shown.y), egui::Sense::click());
                            egui::Image::new((t.id(), shown))
                                .paint_at(ui, egui::Rect::from_center_size(row.center(), shown));
                            if r.double_clicked() {
                                big = !big;
                            }
                        }
                        None => {
                            ui.allocate_ui(vec2(max.x, 80.0), |ui| {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(RichText::new("En attente de l'image…").color(MUTED));
                                });
                            });
                        }
                    }
                });
            ui.add_space(8.0);
            if sound != sound_before {
                self.share_volumes.insert(who, sound);
                let (volume, muted) = sound;
                let _ = self.cmd.send(Cmd::ShareVolume(who, if muted { 0.0 } else { volume }));
            }
            self.faces_shown = faces_shown;
            if big != (self.big == Some(Stage::Screen)) {
                self.set_big(ui.ctx(), big.then_some(Stage::Screen));
            }
            if close {
                // Pas `set_watching` : `view` emprunte encore la session.
                let _ = self.cmd.send(if mine { Cmd::Preview(false) } else { Cmd::Watch(None) });
                self.preview &= !mine;
                self.watching = None;
            }
        }
    }

    /// Webcams allumées dans mon salon vocal (la mienne comprise).
    fn face_ids(&self) -> Vec<PeerId> {
        let Some(view) = &self.view else { return Vec::new() };
        let me = view.my_peer;
        view.my_voice()
            .and_then(|c| view.voice.get(&c))
            .map(|room| {
                room.members
                    .iter()
                    .copied()
                    .filter(|m| {
                        if *m == me {
                            self.camera
                        } else {
                            room.cameras.contains(m)
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Celles que je n'ai pas masquées.
    fn visible_faces(&self) -> Vec<PeerId> {
        if !self.faces_shown {
            return Vec::new();
        }
        self.face_ids()
            .into_iter()
            .filter(|p| !self.faces_hidden.contains(p))
            .collect()
    }

    /// Les webcams allumées de mon salon vocal, en vignettes au-dessus des
    /// messages (la mienne en miroir, comme dans une glace). Plus petites
    /// quand je regarde un écran : il garde la place.
    fn faces_view(&mut self, ui: &mut egui::Ui) {
        let ids = self.face_ids();
        // Webcam éteinte ou partie : son image est libérée, et elle sera
        // de nouveau montrée si elle se rallume.
        self.faces.retain(|p, _| ids.contains(p));
        self.faces_hidden.retain(|p| ids.contains(p));
        let n = self.visible_faces().len();
        if n == 0 {
            return;
        }
        let gap = 8.0;
        let width = ui.available_width();
        let (most, widest) = if self.watching.is_some() {
            (6, 180.0)
        } else if width > 900.0 {
            (4, 320.0)
        } else {
            (3, 320.0)
        };
        let per_row = n.min(most);
        let w = ((width - gap * (per_row - 1) as f32) / per_row as f32).min(widest);
        self.face_strip(ui, None, w, false);
    }

    /// Vignettes des webcams visibles (sauf `exclude`). Double-clic (simple
    /// clic en plein écran) : la mettre en grand ; clic droit : la masquer.
    fn face_strip(&mut self, ui: &mut egui::Ui, exclude: Option<PeerId>, w: f32, big: bool) {
        let Some(view) = &self.view else { return };
        let me = view.my_peer;
        let shown: Vec<PeerId> = self
            .visible_faces()
            .into_iter()
            .filter(|p| Some(*p) != exclude)
            .collect();
        if shown.is_empty() {
            return;
        }
        let size = vec2(w, w * 9.0 / 16.0);
        // (webcam, masquer ?)
        let mut action = None;
        if big {
            ui.add_space(6.0);
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            for m in &shown {
                let mine = *m == me;
                let speaking = self
                    .audio
                    .as_ref()
                    .is_some_and(|a| if mine { a.me_speaking() } else { a.speaking(*m) });
                let r =
                    face_tile(ui, size, &view.peer_name(*m), self.faces.get(m), mine, speaking).on_hover_text(if big {
                        "Clic : en grand · clic droit : masquer"
                    } else {
                        "Double-clic : plein écran · clic droit : masquer"
                    });
                if (big && r.clicked()) || r.double_clicked() {
                    action = Some((*m, false));
                }
                r.context_menu(|ui| {
                    if ui.button("Plein écran").clicked() {
                        action = Some((*m, false));
                        ui.close();
                    }
                    if ui.button("Masquer cette webcam").clicked() {
                        action = Some((*m, true));
                        ui.close();
                    }
                });
            }
        });
        if !big {
            ui.add_space(8.0);
        }
        match action {
            Some((p, true)) => {
                self.faces_hidden.insert(p);
            }
            Some((p, false)) => self.set_big(ui.ctx(), Some(Stage::Face(p))),
            None => {}
        }
    }

    /// Une webcam en plein écran.
    fn big_face(&mut self, ui: &mut egui::Ui, who: PeerId) {
        let Some(view) = &self.view else { return };
        let mine = who == view.my_peer;
        let name = view.peer_name(who);
        let reserve = if self.visible_faces().iter().any(|p| *p != who) {
            STRIP_WIDTH * 9.0 / 16.0 + 16.0
        } else {
            0.0
        };
        let mut stage = Some(Stage::Face(who));
        let others = self.face_ids().len() > 1;
        let mut faces_shown = self.faces_shown;
        ui.horizontal(|ui| {
            let title = if mine {
                "Ta webcam".to_string()
            } else {
                format!("Webcam de {name}")
            };
            ui.label(RichText::new(title).strong().color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .button("Quitter le plein écran")
                    .on_hover_text("Ou double-clic sur l'image ; Échap pour sortir")
                    .clicked()
                {
                    stage = None;
                }
                if others {
                    faces_button(ui, &mut faces_shown);
                }
                if self.watching.is_some() && ui.button("Voir l'écran partagé").clicked() {
                    stage = Some(Stage::Screen);
                }
            });
        });
        ui.add_space(6.0);
        let size = vec2(ui.available_width(), (ui.available_height() - reserve).max(120.0));
        let speaking = self
            .audio
            .as_ref()
            .is_some_and(|a| if mine { a.me_speaking() } else { a.speaking(who) });
        if face_tile(ui, size, &name, self.faces.get(&who), mine, speaking).double_clicked() {
            stage = None;
        }
        self.faces_shown = faces_shown;
        if stage != self.big {
            self.set_big(ui.ctx(), stage);
        }
    }

    fn set_big(&mut self, ctx: &egui::Context, stage: Option<Stage>) {
        if self.big.is_some() != stage.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(stage.is_some()));
        }
        self.big = stage;
    }

    /// Fenêtre de choix du partage : source, souris, qualité.
    fn share_picker(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.picker else { return };
        // Plus dans un salon vocal : plus rien à partager.
        if self.view.as_ref().is_none_or(|v| v.my_voice().is_none()) {
            self.picker = None;
            return;
        }
        let before = self.share_options;
        let out = p.show(ctx, &mut self.share_options, self.sharing);
        if self.share_options != before && self.sharing.is_some() {
            let _ = self.cmd.send(Cmd::ShareOptions(self.share_options));
        }
        match out {
            share::Outcome::Open => {}
            share::Outcome::Closed => self.picker = None,
            share::Outcome::Pick(source) => {
                let _ = self.cmd.send(Cmd::Share(Some((source, self.share_options))));
                self.picker = None;
            }
            share::Outcome::Stop => {
                let _ = self.cmd.send(Cmd::Share(None));
                self.picker = None;
            }
        }
    }

    // --- Colonne de gauche ----------------------------------------------------

    fn sidebar(&mut self, ui: &mut egui::Ui, threshold: f32) {
        // En-tête : nom du serveur et état de la connexion.
        egui::Panel::top("side_head")
            .frame(
                egui::Frame::new()
                    .fill(CARD.gamma_multiply(0.72))
                    .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.38)))
                    .inner_margin(Margin::symmetric(16, 14)),
            )
            .show(ui, |ui| {
                let title = self
                    .view
                    .as_ref()
                    .map_or_else(|| self.title.clone(), |v| v.card.name().to_string());
                ui.horizontal(|ui| {
                    avatar(ui, &title, 30.0);
                    ui.add(egui::Label::new(RichText::new(title).size(16.0).strong().color(TEXT)).truncate());
                    if self.view.is_some() {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add(Button::new(Icon::Gear))
                                .on_hover_text("Paramètres du serveur")
                                .clicked()
                            {
                                self.open_settings(server::Tab::Invite);
                            }
                        });
                    }
                });
                let host = self.view.as_ref().and_then(|v| v.host_peer().map(|h| v.peer_name(h)));
                let (dot, label) = match (&self.view, self.hosting, &self.reconnecting) {
                    (_, _, Some(_)) => (WARN, "Changement d'hôte…".to_string()),
                    (Some(_), true, _) => (OK, "Hébergé par ton PC".to_string()),
                    (Some(_), false, _) => (
                        OK,
                        host.map_or_else(|| "Connecté".to_string(), |h| format!("Hôte : {h}")),
                    ),
                    (None, _, _) => (WARN, "Connexion…".to_string()),
                };
                ui.horizontal(|ui| {
                    status_dot(ui, dot);
                    let (rtt, color) = match self.rtt.map(|r| r.as_secs_f64() * 1000.0) {
                        Some(ms) if self.reconnecting.is_none() => (
                            format!(" · {ms:.0} ms"),
                            if ms < 80.0 {
                                MUTED
                            } else if ms < 160.0 {
                                WARN
                            } else {
                                DANGER
                            },
                        ),
                        _ => (String::new(), MUTED),
                    };
                    ui.add(egui::Label::new(RichText::new(label).color(MUTED).small()).truncate());
                    ui.label(RichText::new(rtt).color(color).small()).on_hover_text(
                        "Aller-retour jusqu'à l'hôte du serveur (le vocal, lui, va directement chez chacun)",
                    );
                });
                // Demander à héberger (si l'hôte lag, par exemple).
                if self.view.is_some() && !self.hosting && self.reconnecting.is_none() {
                    let label = if self.asked_host || self.vote.is_some() {
                        RichText::new("Vote en cours…").color(MUTED).small()
                    } else {
                        RichText::new("Devenir l'hôte").color(ACCENT_HOVER).small()
                    };
                    let r = ui
                        .add_enabled(
                            !self.asked_host && self.vote.is_none(),
                            egui::Label::new(label).sense(egui::Sense::click()),
                        )
                        .on_hover_text(
                            "Héberger le serveur sur ton PC à la place de l'hôte actuel (s'il lag, par exemple). \
                             Les membres connectés votent ; personne n'est déconnecté.",
                        );
                    if r.clicked() {
                        self.asked_host = true;
                        let _ = self.cmd.send(Cmd::RequestHost);
                    }
                }
            });

        // Bas : vocal en cours, puis moi.
        egui::Panel::bottom("side_me")
            .frame(
                egui::Frame::new()
                    .fill(BG.lerp_to_gamma(PANEL, 0.35))
                    .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.38)))
                    .inner_margin(Margin::symmetric(12, 12)),
            )
            .show(ui, |ui| {
                self.voice_card(ui, threshold);
                self.me_bar(ui);
            });

        egui::Frame::new()
            .inner_margin(Margin::symmetric(10, 12))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("side")
                    .auto_shrink(false)
                    .show(ui, |ui| self.channels(ui));
            });
    }

    fn channels(&mut self, ui: &mut egui::Ui) {
        let Some(view) = &self.view else { return };
        let settings = view.card.settings();

        let can_edit = view.can_edit();
        let mut manage = channel_heading(ui, "SALONS TEXTUELS", can_edit);
        let mut pick = None;
        for c in &settings.text_channels {
            let selected = c.id == self.channel;
            let unread = !selected && self.unread(c.id);
            let pings = self.pings.get(&c.id).copied().unwrap_or(0);
            let badge = match pings {
                0 if unread => "●".to_string(),
                0 => String::new(),
                n => format!("@{n}"),
            };
            let r = nav_item(ui, Icon::Hash, &c.name, selected, unread, &badge);
            if r.clicked() {
                pick = Some(c.id);
            }
        }
        if let Some(id) = pick
            && id != self.channel
        {
            self.chat.leave_channel(self.channel, id);
            self.channel = id;
        }
        ui.add_space(16.0);
        manage |= channel_heading(ui, "SALONS VOCAUX", can_edit);
        if settings.voice_channels.is_empty() {
            hint(ui, "  Aucun salon vocal.");
        }
        let mine = view.my_voice();
        let mut volumes = std::mem::take(&mut self.volumes);
        let mut cmds = Vec::new();
        let mut watch = None;
        for c in &settings.voice_channels {
            let here = mine == Some(c.id);
            let r = nav_item(ui, Icon::Speaker, &c.name, here, false, "").on_hover_text(if here {
                "Tu es dans ce salon"
            } else {
                "Rejoindre ce salon vocal"
            });
            if r.clicked() && !here {
                cmds.push(Cmd::VoiceJoin(c.id));
            }
            let Some(room) = view.voice.get(&c.id) else { continue };
            for m in &room.members {
                let name = view.peer_name(*m);
                let me = *m == view.my_peer;
                let speaking = self
                    .audio
                    .as_ref()
                    .is_some_and(|a| if me { a.me_speaking() } else { a.speaking(*m) });
                let row = hover_row(ui, |ui| {
                    ui.add_space(26.0);
                    let rect = avatar(ui, &name, 22.0);
                    if speaking {
                        speaking_ring(ui, rect);
                    }
                    let label = ui.add(
                        egui::Label::new(RichText::new(&name).color(if speaking { OK } else { TEXT }).small())
                            .sense(egui::Sense::click()),
                    );
                    let (muted, deafened) = if me {
                        (self.muted, self.deafened)
                    } else {
                        (room.muted.contains(m), room.deafened.contains(m))
                    };
                    if muted || deafened {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        status_icon(ui, Icon::Mic).on_hover_text("Micro coupé");
                    }
                    if deafened {
                        status_icon(ui, Icon::Headphones).on_hover_text("Son coupé");
                    }
                    if !me && self.audio.as_ref().is_some_and(|a| a.whispers_to_me(*m)) {
                        ui.label(RichText::new("te chuchote").color(ACCENT_HOVER).small().italics());
                    }
                    if me && self.audio.as_ref().is_some_and(|a| a.whispering()) {
                        ui.label(RichText::new("chuchote").color(ACCENT_HOVER).small().italics());
                    }
                    if (me && self.camera && here) || (!me && room.cameras.contains(m)) {
                        icon(ui, Icon::Video, 15.0, MUTED).on_hover_text("Webcam allumée");
                    }
                    if room.sharing.contains(m) {
                        // Dans le même salon, le badge devient le bouton pour regarder.
                        if !me && here {
                            let watching = self.watching.as_ref().is_some_and(|(w, _)| w == m);
                            let label = if watching { "Fermer" } else { "Regarder" };
                            if ui
                                .small_button(RichText::new(label).color(OK).small())
                                .on_hover_text("Écran partagé")
                                .clicked()
                            {
                                watch = Some((!watching).then_some(*m));
                            }
                        } else {
                            ui.label(RichText::new("EN DIRECT").color(DANGER).small().strong())
                                .on_hover_text("Partage son écran");
                        }
                    }
                    label
                });
                // Clic droit : volume de cette personne.
                if !me && here {
                    row.inner
                        .on_hover_text("Clic droit : régler son volume")
                        .context_menu(|ui| {
                            ui.label(RichText::new(format!("Volume de {name}")).color(MUTED).small());
                            let v = volumes.entry(*m).or_insert(1.0);
                            let r = ui.add(
                                egui::Slider::new(v, 0.0..=2.0)
                                    .trailing_fill(true)
                                    .custom_formatter(|v, _| format!("{:.0} %", v * 100.0)),
                            );
                            if r.changed() {
                                cmds.push(Cmd::PeerVolume(*m, *v));
                            }
                        });
                }
            }
            ui.add_space(2.0);
        }
        self.volumes = volumes;
        if manage {
            self.open_settings(server::Tab::Channels);
        }
        if let Some(w) = watch {
            self.set_watching(w);
        }
        for c in cmds {
            let _ = self.cmd.send(c);
        }
    }

    /// Salon vocal en cours : niveau du micro, écran, raccrocher.
    fn voice_card(&mut self, ui: &mut egui::Ui, threshold: f32) {
        let Some(view) = &self.view else { return };
        let Some(room) = view.my_voice() else { return };
        let name = view
            .card
            .settings()
            .voice_channels
            .iter()
            .find(|c| c.id == room)
            .map_or_else(String::new, |c| c.name.clone());
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    icon(ui, Icon::Waves, 15.0, OK);
                    ui.label(RichText::new("Vocal connecté").color(OK).strong().size(13.5));
                });
                ui.add(egui::Label::new(RichText::new(&name).color(MUTED).small()).truncate());
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(Button::new(Icon::HangUp).kind(Look::DangerSolid).height(34.0))
                    .on_hover_text("Raccrocher : quitter le salon vocal")
                    .clicked()
                {
                    let _ = self.cmd.send(Cmd::VoiceLeave);
                }
            });
        });
        match &self.audio {
            Some(a) => {
                // Niveau et anneaux de parole suivis de près seulement quand
                // il se passe quelque chose : le son prévient quand ça commence.
                let quiet = self.deafened || !a.active();
                ui.ctx()
                    .request_repaint_after(Duration::from_millis(if quiet { 1000 } else { 66 }));
                let db = if self.muted || self.deafened {
                    -90.0
                } else {
                    a.level_db()
                };
                level_meter(ui, db, threshold);
                let problems = a.problems.lock().unwrap();
                if !problems.is_empty() {
                    let r = ui
                        .link(
                            RichText::new("Problème de micro ou de casque : voir mes réglages")
                                .color(WARN)
                                .small(),
                        )
                        .on_hover_text(problems.join("\n"));
                    if r.clicked() {
                        self.wants_me = true;
                    }
                }
            }
            None => {
                if ui
                    .link(
                        RichText::new("Son indisponible : voir mes réglages")
                            .color(WARN)
                            .small(),
                    )
                    .clicked()
                {
                    self.wants_me = true;
                }
            }
        }
        // Webcam et écran côte à côte, pour garder la place du bas de la barre.
        let me = view.my_peer;
        let sharing = self.sharing.is_some();
        let shown = self.watching.as_ref().is_some_and(|(w, _)| *w == me);
        ui.add_space(2.0);
        let half = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
        ui.horizontal(|ui| {
            let camera = ui
                .add(
                    Button::new(Icon::Video)
                        .label("Caméra")
                        .kind(Look::Toggle(self.camera))
                        .width(half),
                )
                .on_hover_text(if self.camera {
                    "Couper ta webcam"
                } else {
                    "Ta webcam, vue par tout le salon (à choisir dans mes réglages, page Vidéo)"
                });
            if camera.clicked() {
                let _ = self.cmd.send(Cmd::Camera(!self.camera));
                self.camera = false;
            }
            let screen = ui
                .add(
                    Button::new(Icon::Screen)
                        .label("Écran")
                        .kind(Look::Toggle(sharing))
                        .width(half),
                )
                .on_hover_text(if sharing {
                    "Changer de source : un autre écran, une fenêtre, un jeu ; la souris, la qualité"
                } else {
                    "Partager un écran, une fenêtre ou un jeu"
                });
            if screen.clicked() {
                self.picker = Some(share::Picker::new(ui.ctx(), self.sharing));
            }
        });
        if sharing {
            ui.horizontal(|ui| {
                if ui
                    .add(
                        Button::new(if shown { Icon::EyeOff } else { Icon::Eye })
                            .label("Aperçu")
                            .kind(Look::Toggle(shown))
                            .width(half),
                    )
                    .on_hover_text(if shown {
                        "Cacher l'aperçu de mon écran"
                    } else {
                        "Aperçu de ce que les autres voient, pour toi seul"
                    })
                    .clicked()
                {
                    self.preview = !shown;
                    self.set_watching((!shown).then_some(me));
                }
                if ui
                    .add(
                        Button::new(Icon::Close)
                            .label("Arrêter")
                            .kind(Look::Danger)
                            .color(DANGER)
                            .width(half),
                    )
                    .on_hover_text("Arrêter le partage d'écran")
                    .clicked()
                {
                    let _ = self.cmd.send(Cmd::Share(None));
                }
            });
        }
        ui.add_space(6.0);
        ui.separator();
    }

    /// Moi : pseudo, micro, son, réglages. Utilisable aussi hors d'un salon.
    fn me_bar(&mut self, ui: &mut egui::Ui) {
        let name = self
            .view
            .as_ref()
            .map_or_else(|| self.name.clone(), |v| v.peer_name(v.my_peer));
        ui.horizontal(|ui| {
            let rect = avatar(ui, &name, 32.0);
            if self.audio.as_ref().is_some_and(|a| a.me_speaking()) {
                speaking_ring(ui, rect);
            }
            ui.vertical(|ui| {
                ui.add(egui::Label::new(RichText::new(&name).color(TEXT).strong()).truncate());
                let state = if self.deafened {
                    "son coupé"
                } else if self.muted {
                    "micro coupé"
                } else if self.view.as_ref().is_some_and(|v| v.my_voice().is_some()) {
                    "en vocal"
                } else if self.view.is_none() {
                    "connexion…"
                } else {
                    "en ligne"
                };
                ui.label(RichText::new(state).color(MUTED).small());
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui
                    .add(Button::new(Icon::Gear))
                    .on_hover_text("Mes réglages : pseudo, micro, sortie audio")
                    .clicked()
                {
                    self.wants_me = true;
                }
                if ui
                    .add(
                        Button::new(Icon::Headphones)
                            .slashed(self.deafened)
                            .color(if self.deafened { DANGER } else { MUTED }),
                    )
                    .on_hover_text(if self.deafened {
                        "Réentendre les autres"
                    } else {
                        "Ne plus rien entendre (coupe aussi ton micro)"
                    })
                    .clicked()
                {
                    self.deafened = !self.deafened;
                    let _ = self.cmd.send(Cmd::Deafen(self.deafened));
                }
                let off = self.muted || self.deafened;
                if ui
                    .add(
                        Button::new(Icon::Mic)
                            .slashed(off)
                            .color(if off { DANGER } else { MUTED }),
                    )
                    .on_hover_text(if self.muted {
                        "Rétablir ton micro"
                    } else {
                        "Couper ton micro"
                    })
                    .clicked()
                {
                    self.muted = !self.muted;
                    let _ = self.cmd.send(Cmd::Mute(self.muted));
                }
            });
        });
    }

    // --- Haut, droite, bas ----------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui, wide: bool) {
        let Some(view) = &self.view else { return };
        let keep = view.card.history().keep;
        let channel = self.channel_name();
        let mut open = None;
        // Hauteur des boutons : le titre du salon reste centré sur eux.
        let row = vec2(ui.available_width(), 38.0);
        ui.allocate_ui_with_layout(row, Layout::left_to_right(Align::Center), |ui| {
            icon(ui, Icon::Hash, 20.0, MUTED);
            ui.label(RichText::new(&channel).size(17.0).strong().color(TEXT));
            if !keep {
                ui.label(RichText::new("éphémère").color(WARN).small().strong())
                    .on_hover_text("Rien n'est gardé : les messages disparaissent à la fermeture.");
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(Button::new(Icon::Leave).kind(Look::Danger).height(36.0))
                    .on_hover_text(if self.hosting {
                        "Quitter le serveur et revenir à l'accueil. Les autres restent connectés : l'un \
                         d'eux prend le relais de l'hébergement."
                    } else {
                        "Quitter le serveur et revenir à l'accueil"
                    })
                    .clicked()
                {
                    let _ = self.cmd.send(Cmd::Leave);
                    self.left = true;
                }
                if wide
                    && ui
                        .add(
                            Button::new(Icon::Users)
                                .kind(if self.show_members {
                                    Look::Toggle(true)
                                } else {
                                    Look::Ghost
                                })
                                .height(36.0),
                        )
                        .on_hover_text(if self.show_members {
                            "Cacher la liste des membres"
                        } else {
                            "Afficher la liste des membres"
                        })
                        .clicked()
                {
                    self.show_members = !self.show_members;
                }
                // Quelles webcams afficher (toutes, aucune, ou au choix).
                let ids = self.face_ids();
                if !ids.is_empty() {
                    let names: Vec<(PeerId, String)> = self
                        .view
                        .as_ref()
                        .map(|v| ids.iter().map(|p| (*p, v.peer_name(*p))).collect())
                        .unwrap_or_default();
                    let me = self.view.as_ref().map(|v| v.my_peer);
                    let hidden = !self.faces_shown || ids.iter().all(|p| self.faces_hidden.contains(p));
                    let r = ui
                        .add(
                            Button::new(Icon::Video)
                                .kind(if hidden { Look::Ghost } else { Look::Toggle(true) })
                                .height(36.0),
                        )
                        .on_hover_text("Choisir les webcams affichées");
                    egui::Popup::menu(&r)
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                        .show(|ui| {
                            ui.checkbox(&mut self.faces_shown, "Afficher les webcams");
                            ui.add_enabled_ui(self.faces_shown, |ui| {
                                for (p, name) in &names {
                                    let mut on = !self.faces_hidden.contains(p);
                                    let label = if Some(*p) == me {
                                        format!("{name} (toi)")
                                    } else {
                                        name.clone()
                                    };
                                    if ui.checkbox(&mut on, label).changed() {
                                        if on {
                                            self.faces_hidden.remove(p);
                                        } else {
                                            self.faces_hidden.insert(*p);
                                        }
                                    }
                                }
                            });
                        });
                }
                ui.add_space(4.0);
                if ui
                    .add(
                        Button::new(Icon::UserPlus)
                            .label("Inviter")
                            .kind(Look::Primary)
                            .height(36.0),
                    )
                    .on_hover_text("Code d'invitation (caché, dans les paramètres du serveur)")
                    .clicked()
                {
                    open = Some(server::Tab::Invite);
                }
            });
        });
        if let Some(tab) = open {
            self.open_settings(tab);
        }
    }

    fn members(&mut self, ui: &mut egui::Ui) {
        let Some(view) = &self.view else { return };
        let my_role = view.card.role(&view.me);
        let mut cmds = Vec::new();
        let mut banned = false;
        egui::ScrollArea::vertical().id_salt("members").show(ui, |ui| {
            eyebrow(ui, &format!("EN LIGNE — {}", view.peers.len()));
            ui.add_space(6.0);
            let in_voice: Vec<PeerId> = view.voice.values().flat_map(|r| r.members.clone()).collect();
            for (id, p) in &view.peers {
                hover_row(ui, |ui| {
                    let rect = avatar(ui, &p.name, 30.0);
                    let speaking = self.audio.as_ref().is_some_and(|a| {
                        if *id == view.my_peer {
                            a.me_speaking()
                        } else {
                            a.speaking(*id)
                        }
                    });
                    if speaking {
                        speaking_ring(ui, rect);
                    }
                    ui.vertical(|ui| {
                        let mut name = RichText::new(view.display_name(&p.user, &p.name)).color(TEXT);
                        if *id == view.my_peer {
                            name = name.strong();
                        }
                        ui.add(egui::Label::new(name).truncate());
                        let mut tags = Vec::new();
                        if view.host_user == Some(p.user) {
                            tags.push("★ hôte");
                        }
                        match view.card.role(&p.user) {
                            Role::Owner => tags.push("créateur"),
                            Role::Admin => tags.push("admin"),
                            Role::Member => {}
                        }
                        if in_voice.contains(id) {
                            tags.push("en vocal");
                        }
                        if !tags.is_empty() {
                            ui.label(RichText::new(tags.join(" · ")).color(MUTED).small());
                        }
                    });
                })
                .response
                .on_hover_text(format!("Identité : {}", p.user.short()))
                .interact(egui::Sense::click())
                .context_menu(|ui| {
                    // Modération : le créateur sur tous, un admin sur les simples membres.
                    let role = view.card.role(&p.user);
                    let kick = *id != view.my_peer
                        && matches!(
                            (my_role, role),
                            (Role::Owner, Role::Admin | Role::Member) | (Role::Admin, Role::Member)
                        );
                    ui.label(RichText::new(view.peer_name(*id)).color(MUTED).small());
                    ui.label(
                        RichText::new(format!("Identité : {}", p.user.short()))
                            .color(MUTED)
                            .small(),
                    );
                    if !kick {
                        return;
                    }
                    ui.separator();
                    if ui
                        .button("Expulser")
                        .on_hover_text("Le déconnecter ; il peut revenir avec le code")
                        .clicked()
                    {
                        cmds.push(Cmd::Kick(*id));
                        ui.close();
                    }
                    if role == Role::Member
                        && ui
                            .button(RichText::new("Bannir").color(DANGER))
                            .on_hover_text("Il ne pourra plus entrer (annulable dans les paramètres du serveur)")
                            .clicked()
                    {
                        let mut card = view.card.clone();
                        if card.set_banned(&self.identity, p.user, true).is_ok() {
                            cmds.push(Cmd::UpdateCard(Box::new(card)));
                            banned = true;
                        }
                        ui.close();
                    }
                });
                ui.add_space(2.0);
            }
        });
        for c in cmds {
            let _ = self.cmd.send(c);
        }
        // Avec un autre compte, il reviendrait avec le même code.
        if banned {
            self.notice = Some((
                "Banni. S'il connaît le code d'invitation, change-le (Paramètres du serveur > Invitation) : \
                 sinon il peut revenir avec un autre compte."
                    .into(),
                now_secs(),
            ));
        }
    }

    fn input_bar(&mut self, ui: &mut egui::Ui) {
        let now = now_secs();
        if let Some((text, at)) = &self.notice
            && now - at < 8.0
        {
            ui.label(RichText::new(text).color(MUTED).italics().small());
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
        let Some(view) = &self.view else { return };
        let name = self.channel_name();
        for c in self.chat.input(ui, view, self.channel, &name) {
            let _ = self.cmd.send(c);
        }
    }

    // --- Centre ---------------------------------------------------------------

    fn center(&mut self, ui: &mut egui::Ui) {
        let elapsed = (now_secs() - self.connecting_since).max(0.0);
        let Some(view) = &self.view else {
            ui.add_space(ui.available_height() / 2.0 - 70.0);
            ui.vertical_centered(|ui| {
                ui.spinner();
                ui.add_space(6.0);
                ui.label(RichText::new(&self.status).color(TEXT));
                ui.label(RichText::new(format!("{elapsed:.0} s")).color(MUTED).small());
                if elapsed > 12.0 {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Plus long que d'habitude : l'hôte vient peut-être de partir, ou les box \
                             mettent du temps à s'ouvrir l'une à l'autre.",
                        )
                        .color(MUTED)
                        .small(),
                    );
                }
                ui.add_space(14.0);
                if ui.add(secondary_button("Annuler", 140.0)).clicked() {
                    let _ = self.cmd.send(Cmd::Leave);
                    self.left = true;
                }
            });
            ui.ctx().request_repaint_after(Duration::from_millis(500));
            return;
        };
        if let Some(text) = &self.reconnecting {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
            egui::Frame::new()
                .fill(WARN.gamma_multiply(0.12))
                .stroke(Stroke::new(1.0, WARN.gamma_multiply(0.5)))
                .corner_radius(12)
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(text).color(TEXT));
                        ui.label(RichText::new(format!("{elapsed:.0} s")).color(MUTED).small());
                    });
                    hint(
                        ui,
                        "Le vocal continue pendant ce temps. Tes messages partiront dès que c'est fini.",
                    );
                });
            ui.add_space(8.0);
        }
        let mut ballot = None;
        if let Some((id, candidate, until, voted)) = self.vote {
            let left = (until - now_secs()).max(0.0);
            ui.ctx().request_repaint_after(Duration::from_millis(250));
            egui::Frame::new()
                .fill(ACCENT.gamma_multiply(0.14))
                .stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.6)))
                .corner_radius(12)
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let name = view.peer_name(candidate);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("★").size(18.0).color(ACCENT_HOVER));
                        let text = if candidate == view.my_peer {
                            "Tu as demandé à devenir l'hôte : les autres votent.".to_string()
                        } else {
                            format!("{name} demande à devenir l'hôte du serveur.")
                        };
                        ui.label(RichText::new(text).strong().color(TEXT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(format!("{left:.0} s")).color(MUTED).small());
                            if !voted {
                                if ui.add(secondary_button("Non", 70.0)).clicked() {
                                    ballot = Some((id, false));
                                }
                                if ui.add(primary_button("Oui", 70.0)).clicked() {
                                    ballot = Some((id, true));
                                }
                            } else if candidate != view.my_peer {
                                ui.label(RichText::new("A voté").color(MUTED).small());
                            }
                        });
                    });
                    hint(
                        ui,
                        "Utile si l'hôte actuel lag. La majorité de ceux qui répondent décide ; personne n'est déconnecté.",
                    );
                });
            ui.add_space(8.0);
        }
        if let Some((id, yes)) = ballot {
            let _ = self.cmd.send(Cmd::Ballot { id, yes });
            if let Some(v) = &mut self.vote {
                v.3 = true;
            }
        }
        self.faces_view(ui);
        if self.watching.is_some() {
            self.screen_view(ui);
        }
        let Some(view) = &self.view else { return };
        let channel = self.channel;
        let channel_name = self.channel_name();
        let chat = &mut self.chat;
        let mut cmds = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt(("messages", channel))
            .auto_shrink(false)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if view.messages(channel).next().is_none() {
                    ui.add_space(30.0);
                    ui.label(
                        RichText::new(format!("Bienvenue dans #{channel_name}"))
                            .size(24.0)
                            .strong()
                            .color(TEXT),
                    );
                    ui.label(RichText::new("C'est le début de ce salon.").color(MUTED));
                    if view.peers.len() <= 1 {
                        ui.add_space(6.0);
                        hint(
                            ui,
                            "Personne d'autre ici pour l'instant : clique « Inviter » en haut à droite.",
                        );
                    }
                }
                cmds = chat.messages(ui, view, channel);
            });
        for c in cmds {
            let _ = self.cmd.send(c);
        }
    }
}

/// Vignette d'une webcam : l'image (en entier, bandes noires au besoin) et
/// le nom, entourée de vert quand la personne parle.
fn face_tile(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    name: &str,
    tex: Option<&egui::TextureHandle>,
    mirror: bool,
    speaking: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 10.0, Color32::BLACK);
    match tex {
        Some(t) => {
            let img = t.size_vec2();
            let scale = (rect.width() / img.x).min(rect.height() / img.y);
            let shown = egui::Rect::from_center_size(rect.center(), img * scale);
            let uv = if mirror {
                egui::Rect::from_min_max(egui::pos2(1.0, 0.0), egui::pos2(0.0, 1.0))
            } else {
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
            };
            p.image(t.id(), shown, uv, Color32::WHITE);
        }
        None => {
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "…",
                egui::FontId::proportional(18.0),
                MUTED,
            );
        }
    }
    let label = if mirror {
        format!("{name} (toi)")
    } else {
        name.to_string()
    };
    let galley = p.layout_no_wrap(label, egui::FontId::proportional(12.0), TEXT);
    let pill = egui::Rect::from_min_size(
        rect.left_bottom() + vec2(8.0, -8.0 - galley.size().y - 4.0),
        galley.size() + vec2(12.0, 4.0),
    );
    p.rect_filled(pill, 6.0, Color32::from_black_alpha(160));
    p.galley(pill.min + vec2(6.0, 2.0), galley, TEXT);
    if speaking {
        p.rect_stroke(rect, 10.0, Stroke::new(2.0, OK), egui::StrokeKind::Inside);
    }
    response
}

/// En plein écran : cacher ou remontrer les petites webcams du bas.
fn faces_button(ui: &mut egui::Ui, shown: &mut bool) {
    if ui
        .button(if *shown {
            "Cacher les webcams"
        } else {
            "Montrer les webcams"
        })
        .clicked()
    {
        *shown = !*shown;
    }
}

/// Titre d'une liste de salons, avec « + » pour les gérer (créateur et
/// admins). Vrai si on clique sur « + ».
fn channel_heading(ui: &mut egui::Ui, title: &str, can_edit: bool) -> bool {
    let mut manage = false;
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 24.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.add_space(8.0);
            ui.label(RichText::new(title).color(MUTED).size(11.5).strong());
            if can_edit {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    manage = ui
                        .add(Button::new(Icon::Plus).height(24.0))
                        .on_hover_text("Ajouter, renommer ou supprimer des salons")
                        .clicked();
                });
            }
        },
    );
    manage
}

fn start_status(hosting: bool) -> &'static str {
    if hosting {
        "Ouverture du serveur…"
    } else {
        "Connexion…"
    }
}

/// Message d'erreur lisible : la phrase principale, puis sur une seconde
/// ligne le détail technique (traduit quand on sait le faire).
fn describe(e: &anyhow::Error) -> String {
    let mut parts = e.chain().map(|c| {
        c.to_string()
            .replace("timed out", "pas de réponse")
            .replace("reset by peer", "refusée par l'autre PC")
            .replace("closed by peer", "fermée par l'autre PC")
            .replace("locally closed", "fermée")
    });
    let head = parts.next().unwrap_or_default();
    let detail: Vec<String> = parts.filter(|p| !p.is_empty()).collect();
    if detail.is_empty() {
        head
    } else {
        format!("{}\nDétail : {}", head.trim_end(), detail.join(" : "))
    }
}

fn now_secs() -> f64 {
    now_ms() as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La fermeture de la fenêtre se fait hors du runtime : elle ne doit pas
    /// paniquer (« there is no reactor running »), ni attendre plus de 3 s.
    #[test]
    fn fermeture_hors_runtime() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let done = rt.spawn(async {});
        let stuck = rt.spawn(std::future::pending::<()>());
        let start = std::time::Instant::now();
        finish_leaving(&rt, vec![done, stuck]);
        assert!(start.elapsed() < Duration::from_secs(5));
        finish_leaving(&rt, Vec::new());
    }
}
