//! « Mes réglages » : ce qui ne regarde que moi (pseudo, micro, sortie,
//! données). Ouvrable depuis l'accueil comme depuis un serveur, et le micro se
//! teste sans entrer dans un salon vocal.

use std::{
    sync::{Arc, mpsc as std_mpsc},
    time::Duration,
};

use eframe::egui::{self, RichText};

use super::{
    icons::*,
    keys::{Combo, Hotkeys, Recording, Whisper},
    style::*,
};
use crate::{
    audio, capture,
    core::identity::{Identity, UserId},
    core::{journal, store::Store},
};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Profile,
    Audio,
    Video,
    Keys,
    Data,
}

/// Raccourci en cours de saisie.
#[derive(PartialEq, Clone, Copy)]
enum Slot {
    Mute,
    Deafen,
    Talk,
    Whisper(usize),
}

pub struct MeWindow {
    pub open: bool,
    tab: Tab,
    inputs: Vec<String>,
    outputs: Vec<String>,
    cameras: Vec<String>,
    /// Micros, sorties et webcams en cours d'inventaire, sur un autre fil :
    /// sous Windows, ça peut prendre une demi-seconde.
    listing: Option<std_mpsc::Receiver<Devices>>,
    test: Option<MicTest>,
    copied: Option<f64>,
    recording: Option<Slot>,
    key: KeyBackup,
}

/// Sauvegarde ou reprise de ma clé : la fenêtre du système tourne sur un
/// autre fil (elle bloque), puis on affiche ce qui s'est passé.
#[derive(Default)]
struct KeyBackup {
    job: Option<std_mpsc::Receiver<String>>,
    note: Option<String>,
}

/// Micros, sorties, webcams.
type Devices = (Vec<String>, Vec<String>, Vec<String>);

fn list_devices() -> std_mpsc::Receiver<Devices> {
    let (tx, rx) = std_mpsc::channel();
    let _ = std::thread::Builder::new().name("peripheriques".into()).spawn(move || {
        let _ = tx.send((
            audio::input_devices(),
            audio::output_devices(),
            capture::camera::cameras(),
        ));
    });
    rx
}

/// Micro ouvert hors d'un salon, pour voir son niveau (et s'entendre).
struct MicTest {
    engine: audio::Engine,
    frames: audio::Frames,
    devices: (Option<String>, Option<String>),
    loopback: bool,
}

/// Identifiant fictif pour sa propre voix en retour.
const SELF: u32 = u32::MAX;

impl MeWindow {
    pub fn new() -> Self {
        Self {
            open: true,
            tab: Tab::Profile,
            inputs: Vec::new(),
            outputs: Vec::new(),
            cameras: Vec::new(),
            listing: Some(list_devices()),
            test: None,
            copied: None,
            recording: None,
            key: KeyBackup::default(),
        }
    }

    /// Affiche la fenêtre. Renvoie les nouveaux réglages audio s'ils ont changé.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        name: &mut String,
        prefs: &mut audio::Prefs,
        store: &Store,
        identity: &Identity,
        live: Option<Arc<audio::Status>>,
        in_session: bool,
        help: &mut bool,
        hotkeys: &Hotkeys,
        people: &[(UserId, String)],
    ) -> Option<audio::Prefs> {
        let before = prefs.clone();
        if let Some(rx) = &self.listing {
            match rx.try_recv() {
                Ok((inputs, outputs, cameras)) => {
                    (self.inputs, self.outputs, self.cameras) = (inputs, outputs, cameras);
                    self.listing = None;
                }
                Err(std_mpsc::TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(50)),
                Err(std_mpsc::TryRecvError::Disconnected) => self.listing = None,
            }
        }
        let pages = [
            (Icon::User, "Profil", Tab::Profile),
            (Icon::Mic, "Voix", Tab::Audio),
            (Icon::Video, "Vidéo", Tab::Video),
            (Icon::Keyboard, "Raccourcis", Tab::Keys),
            (Icon::Folder, "Données", Tab::Data),
        ];
        let mut tab = self.tab;
        let closed = Shell {
            id: "me",
            heading: "Mes réglages",
            sub: "Rien ne quitte ce PC",
            pages: &pages,
        }
        .show(
            ctx,
            &mut tab,
            |ui| {
                ui.label(
                    RichText::new(format!("RIP Discord {}", crate::core::VERSION))
                        .color(MUTED)
                        .size(12.0),
                );
                if ui
                    .add(Button::new(Icon::Help).label("Aide").kind(Look::Ghost).height(32.0))
                    .clicked()
                {
                    *help = true;
                }
            },
            |ui, t| match t {
                Tab::Profile => self.profile(ui, name, identity, in_session),
                Tab::Audio => self.audio(ui, prefs, live.as_deref()),
                Tab::Video => self.video(ui, prefs),
                Tab::Keys => self.keys(ui, hotkeys, people, prefs),
                Tab::Data => data(ui, store, &mut self.key),
            },
            None,
        );
        self.tab = tab;
        if closed {
            self.open = false;
        }
        if !self.open || self.tab != Tab::Audio {
            self.test = None;
        }
        if self.recording.is_some() {
            if !self.open || self.tab != Tab::Keys {
                self.recording = None;
                hotkeys.cancel_recording();
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
        if let Some(t) = &mut self.test {
            // Sans s'écouter, les trames du micro sont jetées.
            while let Ok((seq, opus)) = t.frames.try_recv() {
                if t.loopback {
                    t.engine.play(SELF, seq, &opus, false);
                }
            }
            t.engine.set_threshold(prefs.threshold_db);
            t.engine.set_volume(prefs.volume);
            t.engine.set_clean(prefs.clean());
            ctx.request_repaint_after(Duration::from_millis(30));
        }
        if *prefs != before {
            // Les sons d'interface se jouent aussi hors d'un salon.
            audio::cue::configure(prefs);
        }
        (*prefs != before).then(|| prefs.clone())
    }

    fn profile(&mut self, ui: &mut egui::Ui, name: &mut String, identity: &Identity, in_session: bool) {
        group(ui, "", |ui| {
            ui.horizontal(|ui| {
                let shown = if name.trim().is_empty() { "?" } else { name.trim() };
                avatar(ui, shown, 64.0);
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Pseudo").color(TEXT));
                    ui.add(text_field(name, "Ex. Lwrsk").char_limit(32));
                    hint(
                        ui,
                        if in_session {
                            "Le nouveau pseudo s'affichera chez les autres à ta prochaine connexion."
                        } else {
                            "C'est le nom que verront tes amis."
                        },
                    );
                });
            });
        });
        group(ui, "Identité", |ui| {
            let id = identity.id().short();
            row_with(
                ui,
                Some(Icon::Key),
                "Ta clé",
                "Créée sur ce PC : c'est elle qui prouve que tes messages viennent de toi. Pas de compte \
                 ni de mot de passe.",
                |ui| {
                    let now = ui.input(|i| i.time);
                    let copied = self.copied.is_some_and(|t| now - t < 2.0);
                    let (icon, label) = if copied {
                        (Icon::Check, "Copié")
                    } else {
                        (Icon::Copy, "Copier")
                    };
                    if ui.add(Button::new(icon).label(label).height(32.0)).clicked() {
                        ui.ctx().copy_text(id.clone());
                        self.copied = Some(now);
                    }
                    ui.label(RichText::new(&id).monospace().color(TEXT));
                },
            );
            hint(ui, "Pour la sauvegarder ou en reprendre une : page « Données ».");
        });
    }

    /// Bouton « Actualiser la liste » des périphériques (et son état).
    fn refresh_devices(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.listing.is_none(),
                    Button::new(Icon::Refresh)
                        .label("Actualiser la liste")
                        .kind(Look::Ghost)
                        .height(30.0),
                )
                .on_hover_text("Un micro, un casque ou une webcam vient d'être branché ?")
                .clicked()
            {
                self.listing = Some(list_devices());
            }
            if self.listing.is_some() {
                ui.spinner();
                ui.label(RichText::new("Recherche des périphériques…").color(MUTED).small());
            }
        });
    }

    fn audio(&mut self, ui: &mut egui::Ui, prefs: &mut audio::Prefs, live: Option<&audio::Status>) {
        group(ui, "Périphériques", |ui| {
            let (inputs, outputs) = (&self.inputs, &self.outputs);
            row_with(ui, Some(Icon::Mic), "Micro", "", |ui| {
                device_combo(ui, "audio_in", &mut prefs.input, inputs, "Celui du système");
            });
            divider(ui);
            row_with(ui, Some(Icon::Headphones), "Sortie", "Casque ou haut-parleurs", |ui| {
                device_combo(ui, "audio_out", &mut prefs.output, outputs, "Celui du système");
            });
            divider(ui);
            self.refresh_devices(ui);
        });

        let devices = (prefs.input.clone(), prefs.output.clone());
        if self.test.as_ref().is_some_and(|t| t.devices != devices) {
            // Appareil changé : on relance le test dessus.
            let loopback = self.test.take().is_some_and(|t| t.loopback);
            self.start_test(prefs, loopback);
        }
        group(ui, "Micro", |ui| {
            ui.label(RichText::new("Sensibilité").color(TEXT));
            ui.spacing_mut().slider_width = ui.available_width() - 80.0;
            ui.add(
                egui::Slider::new(&mut prefs.threshold_db, -75.0..=-20.0)
                    .trailing_fill(true)
                    .custom_formatter(|v, _| format!("{v:.0} dB")),
            );
            let status = live.or(self.test.as_ref().map(|t| &*t.engine.status));
            match status {
                Some(s) => {
                    level_meter(ui, s.level_db(), prefs.threshold_db);
                    for p in s.problems.lock().unwrap().iter() {
                        ui.add(egui::Label::new(RichText::new(p).color(WARN).small()).wrap());
                    }
                }
                None => level_meter(ui, -90.0, prefs.threshold_db),
            }
            hint(ui, "La barre doit passer le trait quand tu parles, pas au repos.");
            divider(ui);
            if live.is_some() {
                row_with(
                    ui,
                    Some(Icon::Waves),
                    "Tester mon micro",
                    "Ton micro est déjà ouvert dans le salon vocal : la barre ci-dessus suit ta voix.",
                    |_| {},
                );
            } else {
                let mut start = false;
                let mut stop = false;
                row_with(
                    ui,
                    Some(Icon::Waves),
                    "Tester mon micro",
                    "Voir ton niveau sans entrer dans un salon (et t'entendre, avec un casque).",
                    |ui| match &mut self.test {
                        None => start = ui.add(Button::new(Icon::Mic).label("Tester").height(32.0)).clicked(),
                        Some(t) => {
                            stop = ui.add(Button::new(Icon::Close).label("Arrêter").height(32.0)).clicked();
                            ui.add_space(6.0);
                            ui.label(RichText::new("M'entendre").color(MUTED).small());
                            switch(ui, &mut t.loopback);
                        }
                    },
                );
                if start {
                    self.start_test(prefs, false);
                }
                if stop {
                    self.test = None;
                }
            }
            divider(ui);
            switch_row(
                ui,
                "Appui-pour-parler",
                "Le micro ne s'ouvre que tant que tu tiens la touche (page « Raccourcis »).",
                &mut prefs.push_to_talk,
            );
        });

        group(ui, "Traitement du micro", |ui| {
            switch_row(
                ui,
                "Suppression du bruit",
                "Retire le bruit de fond : ventilateur, clavier, souffle.",
                &mut prefs.denoise,
            );
            divider(ui);
            switch_row(
                ui,
                "Annulation d'écho",
                "Avec des haut-parleurs : les autres ne s'entendent plus en retour.",
                &mut prefs.echo_cancel,
            );
            divider(ui);
            let gain_desc = if prefs.auto_gain {
                "Ta voix arrive toujours au même niveau. Le niveau bouge avec : revois la sensibilité si besoin."
            } else {
                "Monte ou baisse ton micro tout seul, pour que ta voix arrive toujours au même niveau."
            };
            switch_row(ui, "Gain automatique", gain_desc, &mut prefs.auto_gain);
        });

        group(ui, "Écoute", |ui| {
            row(
                ui,
                "Volume des autres",
                "Celui de chaque personne : clic droit sur son nom dans le salon vocal.",
                |ui| {
                    ltr(ui, 230.0, |ui| {
                        ui.spacing_mut().slider_width = 150.0;
                        ui.add(
                            egui::Slider::new(&mut prefs.volume, 0.0..=2.0)
                                .trailing_fill(true)
                                .custom_formatter(|v, _| format!("{:.0} %", v * 100.0)),
                        );
                    });
                },
            );
            divider(ui);
            let cues = switch_row(
                ui,
                "Sons d'interface",
                "Quelqu'un entre ou sort du salon, micro coupé, on parle de toi… Joués chez toi seulement.",
                &mut prefs.cues,
            );
            let mut preview = cues.changed() && prefs.cues;
            if prefs.cues {
                divider(ui);
                row(ui, "Volume des sons", "", |ui| {
                    ltr(ui, 230.0, |ui| {
                        ui.spacing_mut().slider_width = 150.0;
                        let r = ui.add(
                            egui::Slider::new(&mut prefs.cues_volume, 0.0..=1.0)
                                .trailing_fill(true)
                                .custom_formatter(|v, _| format!("{:.0} %", v * 100.0)),
                        );
                        // Un son d'essai au volume choisi, une fois le curseur lâché.
                        preview |= r.drag_stopped() || (r.changed() && !r.dragged());
                    });
                });
            }
            if preview {
                audio::cue::configure(prefs);
                audio::Cue::Join.play();
            }
        });
    }

    fn video(&mut self, ui: &mut egui::Ui, prefs: &mut audio::Prefs) {
        group(ui, "Webcam", |ui| {
            let desc = if self.listing.is_some() {
                ""
            } else if self.cameras.is_empty() {
                "Aucune webcam trouvée : branche-la puis « Actualiser la liste »."
            } else {
                "Elle s'allume dans un salon vocal, avec le bouton « Caméra »."
            };
            let cameras = &self.cameras;
            row_with(ui, Some(Icon::Video), "Caméra", desc, |ui| {
                device_combo(ui, "video_in", &mut prefs.camera, cameras, "La première trouvée");
            });
            divider(ui);
            self.refresh_devices(ui);
        });
        group(ui, "Partage d'écran", |ui| {
            row_with(
                ui,
                Some(Icon::Screen),
                "Source, souris, son et qualité",
                "Se choisissent au moment de partager, avec le bouton « Écran » du salon vocal.",
                |_| {},
            );
        });
    }

    fn keys(&mut self, ui: &mut egui::Ui, hotkeys: &Hotkeys, people: &[(UserId, String)], prefs: &audio::Prefs) {
        if hotkeys.available() == Some(false) {
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Ce système ne laisse pas lire le clavier en arrière-plan : les raccourcis ne \
                         marcheront pas ici.",
                    )
                    .color(WARN)
                    .small(),
                )
                .wrap(),
            );
            ui.add_space(8.0);
        }
        hint(
            ui,
            "Ils marchent même quand la fenêtre n'est pas au premier plan (en jeu, par exemple). \
             Clique sur un raccourci, appuie sur la ou les touches voulues, puis relâche.",
        );
        ui.add_space(10.0);
        let mut b = hotkeys.bindings();
        let before = b.clone();
        // Saisie terminée : on range le raccourci là où on l'attendait.
        if let Some(slot) = self.recording
            && let Some(combo) = hotkeys.take_recorded()
        {
            self.recording = None;
            match slot {
                Slot::Mute => b.mute = Some(combo),
                Slot::Deafen => b.deafen = Some(combo),
                Slot::Talk => b.talk = Some(combo),
                Slot::Whisper(i) => {
                    if let Some(w) = b.whispers.get_mut(i) {
                        w.combo = Some(combo);
                    }
                }
            }
        }
        let listening = match hotkeys.recording() {
            Recording::Listening(keys) => Some(keys),
            _ => None,
        };
        group(ui, "Voix", |ui| {
            let rows = [
                (Icon::Mic, "Couper le micro", Slot::Mute),
                (Icon::Headphones, "Couper le son", Slot::Deafen),
                (Icon::Waves, "Appui-pour-parler", Slot::Talk),
            ];
            for (n, (icon, label, slot)) in rows.into_iter().enumerate() {
                if n > 0 {
                    divider(ui);
                }
                let desc = if slot == Slot::Talk && b.talk.is_some() && !prefs.push_to_talk {
                    "Active « Appui-pour-parler » dans « Voix » pour que le micro reste fermé le reste du temps."
                } else {
                    ""
                };
                let combo = match slot {
                    Slot::Mute => &mut b.mute,
                    Slot::Deafen => &mut b.deafen,
                    _ => &mut b.talk,
                };
                row_with(ui, Some(icon), label, desc, |ui| {
                    self.combo_button(ui, hotkeys, slot, combo, listening.as_ref());
                });
            }
        });
        group(ui, "Chuchoter", |ui| {
            hint(
                ui,
                "Tant que tu tiens la touche, seules les personnes choisies t'entendent (si elles sont dans \
                 ton salon). Quand tu la relâches, tout le salon t'entend de nouveau.",
            );
            let mut remove = None;
            for i in 0..b.whispers.len() {
                divider(ui);
                ui.horizontal(|ui| {
                    let w = &mut b.whispers[i];
                    ui.add(
                        text_field(&mut w.name, "Nom du groupe")
                            .char_limit(24)
                            .desired_width(150.0),
                    );
                    at_right(ui, |ui| {
                        if ui
                            .add(Button::new(Icon::Trash).kind(Look::Danger))
                            .on_hover_text("Supprimer ce groupe")
                            .clicked()
                        {
                            remove = Some(i);
                        }
                        let mut combo = b.whispers[i].combo.take();
                        self.combo_button(ui, hotkeys, Slot::Whisper(i), &mut combo, listening.as_ref());
                        b.whispers[i].combo = combo;
                    });
                });
                whisper_people(ui, &mut b.whispers[i], people);
            }
            if let Some(i) = remove {
                b.whispers.remove(i);
                if self.recording.is_some() {
                    self.recording = None;
                    hotkeys.cancel_recording();
                }
            }
            if b.whispers.len() < 9 {
                divider(ui);
                if ui
                    .add(
                        Button::new(Icon::Plus)
                            .label("Ajouter un groupe")
                            .kind(Look::Ghost)
                            .height(32.0),
                    )
                    .clicked()
                {
                    b.whispers.push(Whisper {
                        name: format!("Groupe {}", b.whispers.len() + 1),
                        combo: None,
                        users: Vec::new(),
                        names: Vec::new(),
                    });
                }
            }
        });
        if b != before {
            hotkeys.set_bindings(b);
        }
    }

    /// Bouton qui montre un raccourci et lance sa saisie au clic. Posé dans
    /// une mise en page de droite à gauche : le dernier ajouté est à gauche.
    fn combo_button(
        &mut self,
        ui: &mut egui::Ui,
        hotkeys: &Hotkeys,
        slot: Slot,
        combo: &mut Option<Combo>,
        listening: Option<&std::collections::BTreeSet<super::keys::Input>>,
    ) {
        if self.recording == Some(slot) {
            if ui
                .add(Button::new(Icon::Close).height(32.0))
                .on_hover_text("Annuler")
                .clicked()
            {
                self.recording = None;
                hotkeys.cancel_recording();
            }
            let text = match listening {
                Some(keys) if !keys.is_empty() => Combo(keys.clone()).to_string(),
                _ => "Appuie sur les touches…".into(),
            };
            ui.add(Button::text(&text).kind(Look::Toggle(true)).height(32.0).width(180.0));
            return;
        }
        let clear = ui.add_enabled(combo.is_some(), Button::new(Icon::Close).height(32.0));
        if clear.on_hover_text("Effacer").clicked() {
            *combo = None;
        }
        let text = combo.as_ref().map_or_else(|| "Aucun".to_string(), Combo::to_string);
        let mut button = Button::new(Icon::Keyboard).label(&text).height(32.0).width(180.0);
        if combo.is_none() {
            button = button.color(MUTED);
        }
        if ui
            .add(button)
            .on_hover_text("Clique puis appuie sur la ou les touches")
            .clicked()
        {
            self.recording = Some(slot);
            hotkeys.record();
        }
    }

    fn start_test(&mut self, prefs: &audio::Prefs, loopback: bool) {
        match audio::Engine::start(prefs) {
            Ok((engine, frames)) => {
                // Le test écoute toujours, même en appui-pour-parler.
                engine.set_push_to_talk(false);
                self.test = Some(MicTest {
                    engine,
                    frames,
                    devices: (prefs.input.clone(), prefs.output.clone()),
                    loopback,
                })
            }
            Err(e) => tracing::warn!("test du micro : {e:#}"),
        }
    }
}

/// Cases à cocher : qui entend ce groupe de chuchotement.
fn whisper_people(ui: &mut egui::Ui, w: &mut Whisper, people: &[(UserId, String)]) {
    // Les membres du serveur ouvert, plus ceux déjà choisis ailleurs.
    let mut list: Vec<(UserId, String)> = people.to_vec();
    for (u, n) in w.users.iter().zip(&w.names) {
        if !list.iter().any(|(x, _)| x == u) {
            list.push((*u, n.clone()));
        }
    }
    if list.is_empty() {
        hint(ui, "Ouvre un serveur pour choisir à qui chuchoter.");
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for (user, name) in list {
            let on = w.users.contains(&user);
            let icon = if on { Icon::Check } else { Icon::Plus };
            if ui
                .add(Button::new(icon).label(&name).kind(Look::Toggle(on)).height(28.0))
                .clicked()
            {
                if on {
                    if let Some(i) = w.users.iter().position(|u| *u == user) {
                        w.users.remove(i);
                        w.names.remove(i);
                    }
                } else {
                    w.users.push(user);
                    w.names.push(name);
                }
            }
        }
    });
    if w.users.is_empty() {
        hint(ui, "Personne n'est choisi : ce raccourci ne fera rien.");
    }
}

/// Liste déroulante d'un périphérique (aucun choisi : celui par défaut).
fn device_combo(ui: &mut egui::Ui, id: &str, current: &mut Option<String>, list: &[String], default: &str) {
    let shown = current.clone().unwrap_or_else(|| default.into());
    egui::ComboBox::from_id_salt(id)
        .width(240.0)
        .truncate()
        .selected_text(shown)
        .show_ui(ui, |ui| {
            ui.selectable_value(current, None, default);
            for d in list {
                ui.selectable_value(current, Some(d.clone()), d);
            }
        });
}

fn data(ui: &mut egui::Ui, store: &Store, key: &mut KeyBackup) {
    group(ui, "Application", |ui| {
        row_with(
            ui,
            Some(Icon::Info),
            "Version",
            "Pour se retrouver sur un serveur, tout le monde doit avoir une version récente de l'app.",
            |ui| {
                ui.label(RichText::new(format!("RIP Discord {}", crate::core::VERSION)).color(TEXT));
            },
        );
    });
    group(ui, "Dossier de l'app", |ui| {
        ui.add(
            egui::Label::new(
                RichText::new(store.root().display().to_string())
                    .monospace()
                    .size(13.0)
                    .color(TEXT),
            )
            .wrap(),
        );
        hint(
            ui,
            "Tout est là : ta clé, la copie des serveurs que tu as rejoints et leur historique (si le \
             serveur le garde). Rien n'est envoyé ailleurs que chez les membres de tes serveurs. Pour \
             effacer un serveur de ce PC : « ··· » puis « Oublier » sur l'accueil.",
        );
        ui.horizontal(|ui| {
            if ui
                .add(Button::new(Icon::Folder).label("Ouvrir le dossier").height(32.0))
                .clicked()
            {
                open_folder(store.root());
            }
            let journal = journal::path(store.root());
            if ui
                .add_enabled(
                    journal.exists(),
                    Button::new(Icon::Info).label("Ouvrir le journal").height(32.0),
                )
                .on_hover_text("Ce que l'app a noté (connexions, son, erreurs). Utile si quelque chose ne marche pas.")
                .clicked()
            {
                open_folder(&journal);
            }
        });
    });
    if let Some(rx) = &key.job {
        match rx.try_recv() {
            Ok(note) => {
                key.note = Some(note);
                key.job = None;
            }
            Err(std_mpsc::TryRecvError::Empty) => ui.ctx().request_repaint_after(Duration::from_millis(100)),
            Err(std_mpsc::TryRecvError::Disconnected) => key.job = None,
        }
    }
    group(ui, "Ma clé", |ui| {
        let idle = key.job.is_none();
        row_with(
            ui,
            Some(Icon::Key),
            "Sauvegarder ma clé",
            "Garde-la ailleurs (clé USB, cloud perso) : sans elle, ton identité est perdue si ce PC l'est.",
            |ui| {
                if ui
                    .add_enabled(idle, Button::text("Sauvegarder…").height(32.0))
                    .clicked()
                {
                    key.job = Some(key_dialog(store.clone(), false));
                }
            },
        );
        divider(ui);
        row_with(
            ui,
            Some(Icon::Refresh),
            "Reprendre une clé",
            "Après une réinstallation ou sur un autre PC : tu redeviens toi, avec tes rôles.",
            |ui| {
                if ui.add_enabled(idle, Button::text("Reprendre…").height(32.0)).clicked() {
                    key.job = Some(key_dialog(store.clone(), true));
                }
            },
        );
        if let Some(note) = &key.note {
            divider(ui);
            ui.add(egui::Label::new(RichText::new(note.clone()).color(ACCENT_HOVER).small()).wrap());
        }
    });
    hint(
        ui,
        "Ta clé, c'est ton identité : le propriétaire d'un serveur ne peut pas être remplacé. Ne la donne \
         à personne.",
    );
}

/// Fenêtre « Enregistrer » ou « Ouvrir » pour la clé, puis copie.
fn key_dialog(store: Store, import: bool) -> std_mpsc::Receiver<String> {
    let (tx, rx) = std_mpsc::channel();
    let _ = std::thread::Builder::new().name("cle".into()).spawn(move || {
        let dialog = rfd::FileDialog::new().add_filter("Clé RIP Discord", &["key"]);
        let note = if import {
            let Some(path) = dialog.set_title("Reprendre une clé").pick_file() else {
                return;
            };
            match store.import_identity(&path) {
                Ok(_) => "Clé reprise. Ferme et relance l'app pour l'utiliser (l'ancienne est gardée dans le \
                          dossier, sous « identity-avant-import.key »)."
                    .to_string(),
                Err(e) => format!("Clé non reprise : {e:#}"),
            }
        } else {
            let Some(path) = dialog
                .set_title("Sauvegarder ma clé")
                .set_file_name("ma-cle-rip-discord.key")
                .save_file()
            else {
                return;
            };
            match store.export_identity(&path) {
                Ok(()) => format!("Clé sauvegardée dans {}.", path.display()),
                Err(e) => format!("Sauvegarde impossible : {e:#}"),
            }
        };
        let _ = tx.send(note);
    });
    rx
}
