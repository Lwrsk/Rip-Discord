//! Mises à jour dans l'interface : recherche au lancement (puis toutes les
//! 6 h), petit encart en bas à droite quand une version plus récente est
//! publiée, et réglages dans « Mes réglages > Données ».
//!
//! Le réseau et l'installeur tournent sur un fil à part ([`crate::update`]
//! est bloquant) ; l'interface relit ses messages à chaque image.

use std::{
    sync::mpsc as std_mpsc,
    time::{Duration, Instant},
};

use eframe::egui::{self, Align2, RichText, Stroke, vec2};

use super::{icons::*, style::*};
use crate::update::{self, Release, Source};

/// Écart entre deux recherches quand l'app reste ouverte longtemps.
const EVERY: Duration = Duration::from_secs(6 * 3600);
/// Première recherche un peu après le lancement, pour ne pas le ralentir.
const FIRST: Duration = Duration::from_secs(4);

enum State {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading {
        release: Release,
        done: u64,
    },
    /// Installeur lancé (ou accord administrateur demandé).
    Installing(Release),
    Failed {
        release: Option<Release>,
        error: String,
    },
}

enum Msg {
    Checked(Result<Release, String>),
    Progress(u64),
    Done(Result<(), String>),
}

pub struct Updater {
    /// Chercher tout seul (retenu d'une fois sur l'autre).
    pub auto: bool,
    state: State,
    rx: Option<std_mpsc::Receiver<Msg>>,
    started: Instant,
    last: Option<Instant>,
    /// Encart fermé avec « Plus tard ».
    hidden: bool,
}

impl Updater {
    pub fn new(auto: bool) -> Self {
        Self {
            auto,
            state: State::Idle,
            rx: None,
            started: Instant::now(),
            last: None,
            hidden: false,
        }
    }

    /// À appeler à chaque image, même fenêtre réduite.
    pub fn tick(&mut self, ctx: &egui::Context) {
        while let Some(msg) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.state = match (std::mem::replace(&mut self.state, State::Idle), msg) {
                (_, Msg::Checked(Ok(r))) if update::is_newer(&r) => {
                    tracing::info!("mise à jour disponible : {}", r.version);
                    self.hidden = false;
                    State::Available(r)
                }
                (_, Msg::Checked(Ok(_))) => State::UpToDate,
                (_, Msg::Checked(Err(error))) => {
                    tracing::info!("recherche de mise à jour : {error}");
                    State::Failed { release: None, error }
                }
                (State::Downloading { release, .. }, Msg::Progress(done)) => State::Downloading { release, done },
                (State::Downloading { release, .. }, Msg::Done(Ok(()))) => {
                    // L'installeur attend que l'app soit fermée pour la remplacer.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    State::Installing(release)
                }
                (State::Downloading { release, .. }, Msg::Done(Err(error))) => {
                    tracing::warn!("mise à jour {} : {error}", release.version);
                    self.hidden = false;
                    State::Failed {
                        release: Some(release),
                        error,
                    }
                }
                (state, _) => state,
            };
        }
        let due = match self.last {
            None => self.started.elapsed() >= FIRST,
            Some(t) => t.elapsed() >= EVERY,
        };
        let busy = matches!(
            self.state,
            State::Checking | State::Downloading { .. } | State::Installing(_)
        );
        if self.auto && due && !busy && !matches!(self.state, State::Available(_)) {
            self.check(ctx);
        }
        // Se réveiller pour la prochaine recherche, même sans rien toucher.
        if self.auto {
            let wait = match self.last {
                None => FIRST.saturating_sub(self.started.elapsed()),
                Some(t) => EVERY.saturating_sub(t.elapsed()),
            };
            ctx.request_repaint_after(wait.max(Duration::from_millis(200)));
        }
    }

    pub fn check(&mut self, ctx: &egui::Context) {
        self.last = Some(Instant::now());
        self.state = State::Checking;
        self.rx = Some(spawn(ctx, "maj-recherche", |tell| {
            tell.send(Msg::Checked(Source::default().latest().map_err(|e| format!("{e:#}"))));
        }));
    }

    fn install(&mut self, ctx: &egui::Context, release: Release) {
        let Some(installer) = release.installer.clone() else {
            open_link(&release.page);
            return;
        };
        self.state = State::Downloading { release, done: 0 };
        self.rx = Some(spawn(ctx, "maj-installeur", move |tell| {
            let result = Source::default()
                .download(&installer, &update::download_dir(), |n| tell.send(Msg::Progress(n)))
                .and_then(|path| update::launch(&path));
            tell.send(Msg::Done(result.map_err(|e| format!("{e:#}"))));
        }));
    }

    fn release(&self) -> Option<&Release> {
        match &self.state {
            State::Available(r) | State::Downloading { release: r, .. } | State::Installing(r) => Some(r),
            State::Failed { release, .. } => release.as_ref(),
            _ => None,
        }
    }

    /// Encart en bas à droite quand une version plus récente attend.
    pub fn toast(&mut self, ctx: &egui::Context) {
        if self.hidden {
            return;
        }
        let Some(release) = self.release().cloned() else {
            return;
        };
        egui::Area::new(egui::Id::unique("mise_a_jour"))
            .anchor(Align2::RIGHT_BOTTOM, vec2(-18.0, -18.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.45)))
                    .corner_radius(14)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        ui.set_width(300.0);
                        ui.horizontal(|ui| {
                            icon(ui, Icon::Refresh, 18.0, ACCENT_HOVER);
                            ui.label(
                                RichText::new(format!("RIP Discord {} est sorti", release.version))
                                    .strong()
                                    .color(TEXT),
                            );
                        });
                        ui.add_space(4.0);
                        self.body(ui, &release, true);
                    });
            });
    }

    /// État et boutons, dans l'encart comme dans les réglages.
    fn body(&mut self, ui: &mut egui::Ui, release: &Release, toast: bool) {
        let ctx = ui.ctx().clone();
        match &self.state {
            State::Available(_) => {
                hint(
                    ui,
                    if release.installer.is_some() && cfg!(windows) {
                        "L'app se ferme le temps de l'installation puis se rouvre. Windows demande ton accord."
                    } else {
                        "Télécharge l'installeur depuis la page de la version."
                    },
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let auto = release.installer.is_some() && cfg!(windows);
                    let label = if auto { "Mettre à jour" } else { "Voir la version" };
                    if ui.add(Button::text(label).kind(Look::Primary).height(32.0)).clicked() {
                        if auto {
                            self.install(&ctx, release.clone());
                        } else {
                            open_link(&release.page);
                        }
                    }
                    if auto
                        && ui
                            .add(Button::text("Nouveautés").kind(Look::Ghost).height(32.0))
                            .on_hover_text("Ouvre la page de la version sur GitHub")
                            .clicked()
                    {
                        open_link(&release.page);
                    }
                    if toast
                        && ui
                            .add(Button::text("Plus tard").kind(Look::Ghost).height(32.0))
                            .clicked()
                    {
                        self.hidden = true;
                    }
                });
            }
            State::Downloading { done, .. } => {
                let total = release.installer.as_ref().map_or(1, |i| i.size.max(1));
                ui.add(
                    egui::ProgressBar::new(*done as f32 / total as f32)
                        .desired_height(8.0)
                        .fill(ACCENT),
                );
                hint(
                    ui,
                    format!(
                        "Téléchargement… {:.1} / {:.1} Mo",
                        *done as f64 / 1e6,
                        total as f64 / 1e6
                    ),
                );
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            State::Installing(_) => hint(ui, "Installation : l'app va se fermer puis se rouvrir."),
            State::Failed { error, .. } => {
                ui.add(egui::Label::new(RichText::new(error).color(DANGER).small()).wrap());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(Button::text("Réessayer").kind(Look::Primary).height(32.0))
                        .clicked()
                    {
                        self.install(&ctx, release.clone());
                    }
                    if ui
                        .add(Button::text("Page de la version").kind(Look::Ghost).height(32.0))
                        .clicked()
                    {
                        open_link(&release.page);
                    }
                    if toast
                        && ui
                            .add(Button::text("Plus tard").kind(Look::Ghost).height(32.0))
                            .clicked()
                    {
                        self.hidden = true;
                    }
                });
            }
            State::Idle | State::Checking | State::UpToDate => {}
        }
    }

    /// Groupe « Mises à jour » des réglages.
    pub fn settings(&mut self, ui: &mut egui::Ui) {
        group(ui, "Mises à jour", |ui| {
            switch_row(
                ui,
                "Chercher les mises à jour",
                "Au lancement puis de temps en temps, sur la page des versions de l'app (GitHub). Rien \
                 n'est installé sans ton clic.",
                &mut self.auto,
            );
            divider(ui);
            let status = match &self.state {
                State::Idle => "Pas encore cherché.".to_string(),
                State::Checking => "Recherche…".to_string(),
                State::UpToDate => "Tu as la dernière version.".to_string(),
                State::Available(r) | State::Downloading { release: r, .. } | State::Installing(r) => {
                    format!("La version {} est disponible.", r.version)
                }
                State::Failed { release: None, error } => format!("Recherche impossible : {error}"),
                State::Failed { release: Some(r), .. } => format!("La version {} n'a pas pu s'installer.", r.version),
            };
            let busy = matches!(
                self.state,
                State::Checking | State::Downloading { .. } | State::Installing(_)
            );
            row_with(ui, Some(Icon::Refresh), "Version disponible", &status, |ui| {
                if ui
                    .add_enabled(!busy, Button::text("Chercher maintenant").height(32.0))
                    .clicked()
                {
                    self.check(ui.ctx());
                }
            });
            if let Some(release) = self.release().cloned() {
                divider(ui);
                self.body(ui, &release, false);
            }
        });
    }
}

/// Envoie les messages du fil de travail et réveille l'interface.
struct Tell {
    tx: std_mpsc::Sender<Msg>,
    ctx: egui::Context,
}

impl Tell {
    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
    }
}

fn spawn(ctx: &egui::Context, name: &str, job: impl FnOnce(Tell) + Send + 'static) -> std_mpsc::Receiver<Msg> {
    let (tx, rx) = std_mpsc::channel();
    let tell = Tell { tx, ctx: ctx.clone() };
    let _ = std::thread::Builder::new().name(name.into()).spawn(move || job(tell));
    rx
}
