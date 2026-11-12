//! Salon texte : le fil des messages (réponses, réactions, mentions, pièces
//! jointes, modifier et supprimer) et la zone de saisie.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::mpsc as std_mpsc,
};

use chrono::{DateTime, Local, TimeZone};
use eframe::egui::{self, Color32, Key, Margin, RichText, Stroke, text::LayoutJob, vec2};

use super::{Cmd, Draft, FileState, icons, icons::Icon, markup, style::*};
use crate::{
    client::{EVERYONE, ServerView},
    core::{
        card::{ChannelId, FileRef, Kind, MAX_FILES, MAX_MESSAGE_CHARS, MessageId, SignedMessage, plain_text},
        files::FileHash,
        identity::UserId,
    },
};

/// Réactions proposées d'un clic.
const REACTIONS: [&str; 8] = ["👍", "❤", "😂", "😮", "😢", "🎉", "🔥", "👀"];

/// Pièce jointe côté interface.
enum Attachment {
    Loading,
    Ready {
        image: Option<egui::TextureHandle>,
        /// Dernière fois qu'elle a été affichée (horloge d'egui, en secondes).
        seen: f64,
    },
    Failed(String),
}

/// Une image restée hors de vue aussi longtemps est déchargée (mémoire et
/// carte graphique) ; elle revient du disque si on remonte jusqu'à elle.
const UNLOAD_AFTER: f64 = 60.0;

#[derive(Default)]
pub struct Chat {
    pub input: String,
    reply_to: Option<MessageId>,
    /// Message en cours de modification (le texte est dans `input`).
    editing: Option<MessageId>,
    /// Fichiers à joindre au prochain message.
    attaching: Vec<PathBuf>,
    /// Personnes choisies dans les suggestions « @ ».
    mentions: Vec<(String, UserId)>,
    files: HashMap<FileHash, Attachment>,
    /// À enregistrer dès que le téléchargement est fini.
    save_when_ready: HashMap<FileHash, FileRef>,
    /// Fenêtre « joindre un fichier » ouverte (dans un autre fil).
    picking: Option<std_mpsc::Receiver<Vec<PathBuf>>>,
    /// Message survolé (sa barre d'actions s'affiche).
    hovered: Option<MessageId>,
    /// Où était sa barre d'actions (on peut y aller sans qu'elle disparaisse).
    toolbar: Option<egui::Rect>,
    /// Message dont le choix de réaction est ouvert.
    reacting: Option<MessageId>,
    focus_input: bool,
    /// Ce qu'on avait commencé à écrire dans les autres salons.
    unsent: HashMap<ChannelId, Unsent>,
    /// Hauteur de chaque message déjà affiché, pour la largeur `heights_for` :
    /// hors de vue, un message n'est pas redessiné, juste sa place réservée.
    heights: HashMap<MessageId, f32>,
    heights_for: f32,
    /// Message dont la suppression attend une confirmation.
    deleting: Option<MessageId>,
    /// Image ouverte en grand (et sa version détaillée, quand elle est prête).
    viewing: Option<(FileRef, Option<egui::TextureHandle>)>,
    /// Où ranger les images collées depuis le presse-papiers.
    paste_dir: Option<PathBuf>,
    /// Numéro de la prochaine image collée.
    pasted: u32,
}

/// Saisie laissée dans un salon : on la retrouve en y revenant.
#[derive(Default)]
struct Unsent {
    input: String,
    attaching: Vec<PathBuf>,
    mentions: Vec<(String, UserId)>,
}

impl Chat {
    /// Changement de salon : ce qu'on écrivait reste dans `from` (sauf une
    /// modification ou une réponse en cours), et on retrouve ce qu'on avait
    /// laissé dans `to`.
    pub fn leave_channel(&mut self, from: ChannelId, to: ChannelId) {
        if self.editing.take().is_some() {
            self.input.clear();
        }
        self.reply_to = None;
        let left = Unsent {
            input: std::mem::take(&mut self.input),
            attaching: std::mem::take(&mut self.attaching),
            mentions: std::mem::take(&mut self.mentions),
        };
        if !left.input.trim().is_empty() || !left.attaching.is_empty() {
            self.unsent.insert(from, left);
        }
        let back = self.unsent.remove(&to).unwrap_or_default();
        self.input = back.input;
        self.attaching = back.attaching;
        self.mentions = back.mentions;
    }

    /// Une pièce jointe est prête (ou a échoué).
    pub fn file_ready(&mut self, ctx: &egui::Context, hash: FileHash, state: FileState) -> Option<Cmd> {
        let entry = match state {
            FileState::Ready { image } => Attachment::Ready {
                image: image.map(|i| ctx.load_texture(format!("piece-{}", hash[0]), i, egui::TextureOptions::LINEAR)),
                seen: ctx.input(|i| i.time),
            },
            FileState::Failed(e) => {
                self.save_when_ready.remove(&hash);
                Attachment::Failed(e)
            }
        };
        let ready = matches!(entry, Attachment::Ready { .. });
        self.files.insert(hash, entry);
        if ready {
            return self.save_when_ready.remove(&hash).map(Cmd::Save);
        }
        None
    }

    /// Les images collées (Ctrl+V) sont rangées dans ce dossier, vidé à
    /// chaque lancement.
    pub fn with_paste_dir(mut self, dir: PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(&dir);
        self.paste_dir = Some(dir);
        self
    }

    /// L'image ouverte en grand est décodée en détail (ou n'a pas pu l'être).
    pub fn picture_ready(&mut self, ctx: &egui::Context, hash: FileHash, image: Option<egui::ColorImage>) {
        if let Some((f, tex)) = &mut self.viewing
            && f.hash == hash
            && let Some(image) = image
        {
            *tex = Some(ctx.load_texture("image-en-grand", image, egui::TextureOptions::LINEAR));
        }
    }

    /// Fil des messages d'un salon ; renvoie les commandes à envoyer.
    /// `new_after` : heure du dernier message déjà lu, s'il y en a de plus
    /// récents à signaler par une ligne « Nouveaux messages ».
    pub fn messages(
        &mut self,
        ui: &mut egui::Ui,
        view: &ServerView,
        channel: ChannelId,
        new_after: Option<u64>,
    ) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let now = ui.input(|i| i.time);
        self.files
            .retain(|_, a| !matches!(a, Attachment::Ready { image: Some(_), seen } if now - *seen > UNLOAD_AFTER));
        let today = Local::now().date_naive();
        let mut last: Option<&SignedMessage> = None;
        let mut hovered = None;
        let reacting = self.reacting.take();
        let pointer = ui
            .input(|i| i.pointer.hover_pos())
            .filter(|p| ui.clip_rect().contains(*p))
            // Une fenêtre ouverte par-dessus : plus de survol en dessous.
            .filter(|_| self.deleting.is_none() && self.viewing.is_none());
        let width = ui.available_width();
        if width != self.heights_for {
            self.heights.clear();
            self.heights_for = width;
        }
        let mut prev_day = None;
        let mut new_line = new_after;
        for (id, m) in view.entries(channel) {
            if new_line.is_some_and(|read| m.body.at_ms > read) {
                new_line = None;
                ui.add_space(8.0);
                separator(ui, "Nouveaux messages", DANGER);
            }
            let at = local(m.body.at_ms);
            let day = at.map(|t| t.date_naive());
            if day != prev_day
                && let Some(d) = day
            {
                ui.add_space(10.0);
                let label = if d == today {
                    "Aujourd'hui".to_string()
                } else if Some(d) == today.pred_opt() {
                    "Hier".to_string()
                } else {
                    d.format("%d/%m/%Y").to_string()
                };
                day_separator(ui, &label);
            }
            let reply = match &m.body.kind {
                Kind::Rich { reply_to, .. } => *reply_to,
                _ => None,
            };
            // Même auteur, peu après, sans réponse : on colle au message précédent.
            let grouped = reply.is_none()
                && last.is_some_and(|l| {
                    l.signer == m.signer && m.body.at_ms.saturating_sub(l.body.at_ms) < 5 * 60 * 1000 && day == prev_day
                });
            last = Some(m);
            prev_day = day;
            if !grouped {
                ui.add_space(10.0);
            }
            // Hors de vue (le fil peut être long) : juste la place qu'il prend,
            // ou à peu près s'il n'a encore jamais été affiché.
            let h = self
                .heights
                .get(&id)
                .copied()
                .unwrap_or_else(|| guess_height(m, grouped));
            let rect = egui::Rect::from_min_size(ui.cursor().min, vec2(width, h));
            if !ui.is_rect_visible(rect.expand(80.0))
                && reacting != Some(id)
                && self.hovered != Some(id)
                && self.deleting != Some(id)
            {
                ui.allocate_space(vec2(width, h));
                continue;
            }
            let mention = view.mentions_me(m);
            let frame = egui::Frame::new()
                .inner_margin(Margin::symmetric(6, 2))
                .corner_radius(8)
                .fill(if mention {
                    WARN.gamma_multiply(0.08)
                } else if self.hovered == Some(id) || self.editing == Some(id) {
                    SURFACE.gamma_multiply(0.55)
                } else {
                    Color32::TRANSPARENT
                })
                .stroke(if mention {
                    Stroke::new(1.0, WARN.gamma_multiply(0.35))
                } else {
                    Stroke::NONE
                });
            let r = frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                if let Some(target) = reply {
                    self.reply_line(ui, view, target);
                }
                let time = at.map_or_else(String::new, |t| t.format("%H:%M").to_string());
                if grouped {
                    ui.horizontal_top(|ui| {
                        ui.add_space(42.0);
                        ui.vertical(|ui| {
                            self.body(ui, view, m, &mut cmds).on_hover_text(&time);
                        });
                    });
                } else {
                    let name = view.author(m);
                    ui.horizontal_top(|ui| {
                        avatar(ui, &m.body.author_name, 38.0);
                        ui.add_space(2.0);
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&name).strong().color(color_for(&m.body.author_name)))
                                    .on_hover_text(format!("Identité : {}", m.signer.short()));
                                ui.label(RichText::new(&time).color(MUTED).small());
                            });
                            self.body(ui, view, m, &mut cmds);
                        });
                    });
                }
            });
            let rect = r.response.rect;
            self.heights.insert(id, rect.height());
            let area = ui.interact(rect, ui.scope_id().with(("message", id)), egui::Sense::click());
            area.context_menu(|ui| self.menu(ui, view, m, &mut cmds));
            // La barre est au-dessus du message : la survoler, ou ouvrir son
            // choix de réaction, ne la fait pas disparaître.
            let on_toolbar = self.hovered == Some(id) && self.toolbar.zip(pointer).is_some_and(|(t, p)| t.contains(p));
            if pointer.is_some_and(|p| rect.contains(p)) || on_toolbar || reacting == Some(id) {
                hovered = Some(id);
                self.toolbar(ui, view, m, rect, &mut cmds);
            }
        }
        self.hovered = hovered;
        ui.add_space(8.0);
        self.confirm_delete(ui.ctx(), view, &mut cmds);
        self.picture(ui.ctx(), &mut cmds);
        cmds
    }

    /// « Supprimer ce message ? » : une suppression est définitive, chez tout
    /// le monde. Maj+clic sur la corbeille s'en passe.
    fn confirm_delete(&mut self, ctx: &egui::Context, view: &ServerView, cmds: &mut Vec<Cmd>) {
        let Some(id) = self.deleting else { return };
        let Some(m) = view.message(&id) else {
            self.deleting = None;
            return;
        };
        let mut close = false;
        let r = egui::Modal::new(egui::Id::unique("supprimer"))
            .frame(card().inner_margin(24))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new("Supprimer ce message ?").size(18.0).strong().color(TEXT));
                ui.add_space(6.0);
                hint(
                    ui,
                    "Il disparaît pour tout le monde, avec ses réactions et ses fichiers. C'est définitif.",
                );
                ui.add_space(10.0);
                egui::Frame::new()
                    .fill(SURFACE)
                    .corner_radius(10)
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(view.author(m))
                                .strong()
                                .color(color_for(&m.body.author_name)),
                        );
                        let text = match (excerpt(view.text(m), 160), &m.body.kind) {
                            (t, Kind::Rich { files, .. }) if t.is_empty() && !files.is_empty() => {
                                "(fichier joint)".into()
                            }
                            (t, _) => t,
                        };
                        ui.add(egui::Label::new(RichText::new(text).color(TEXT)).wrap());
                    });
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 10.0) / 2.0;
                    if ui.add(secondary_button("Annuler", w)).clicked() {
                        close = true;
                    }
                    let confirm = Tinted::new(
                        egui::Button::new(RichText::new("Supprimer").strong().color(Color32::WHITE))
                            .min_size(vec2(w, 44.0)),
                        DANGER.gamma_multiply(0.75),
                        Stroke::new(1.0, DANGER),
                    );
                    if ui.add(confirm).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                        cmds.push(delete(m));
                        close = true;
                    }
                });
                hint(ui, "Astuce : Maj + clic sur la corbeille supprime sans demander.");
            });
        if close || r.should_close() {
            self.deleting = None;
        }
    }

    /// Demande de suppression : confirmation, sauf avec Maj.
    fn ask_delete(&mut self, ui: &egui::Ui, m: &SignedMessage, cmds: &mut Vec<Cmd>) {
        if ui.input(|i| i.modifiers.shift) {
            cmds.push(delete(m));
        } else {
            self.deleting = Some(m.id());
        }
    }

    /// Image ouverte en grand : clic à côté ou Échap pour fermer.
    fn picture(&mut self, ctx: &egui::Context, cmds: &mut Vec<Cmd>) {
        let Some((file, detailed)) = &self.viewing else { return };
        let preview = match self.files.get_mut(&file.hash) {
            Some(Attachment::Ready { image: Some(tex), seen }) => {
                // Tant qu'elle est ouverte, l'aperçu n'est pas déchargé.
                *seen = ctx.input(|i| i.time);
                Some(tex.clone())
            }
            _ => None,
        };
        let Some(tex) = detailed.clone().or(preview) else {
            self.viewing = None;
            return;
        };
        let screen = ctx.content_rect().size();
        let room = vec2((screen.x - 120.0).max(200.0), (screen.y - 190.0).max(150.0));
        // À sa taille réelle si elle tient, réduite sinon.
        let (w, h) = file
            .image
            .map_or((tex.size()[0] as f32, tex.size()[1] as f32), |(w, h)| {
                (w as f32, h as f32)
            });
        let scale = (room.x / w.max(1.0)).min(room.y / h.max(1.0)).min(1.0);
        let mut close = false;
        let mut save = false;
        let r = egui::Modal::new(egui::Id::unique("image"))
            .frame(card().inner_margin(14))
            .show(ctx, |ui| {
                ui.add(
                    egui::Image::from_texture(&tex)
                        .fit_to_exact_size(vec2(w * scale, h * scale))
                        .corner_radius(8),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("{} · {}", plain_text(&file.name), human_size(file.size)))
                                .color(MUTED)
                                .small(),
                        )
                        .truncate(),
                    );
                    at_right(ui, |ui| {
                        if ui
                            .add(icons::Button::new(Icon::Close))
                            .on_hover_text("Fermer (Échap)")
                            .clicked()
                        {
                            close = true;
                        }
                        if ui
                            .add(egui::Button::new("Enregistrer"))
                            .on_hover_text("Copier dans Téléchargements")
                            .clicked()
                        {
                            save = true;
                        }
                    });
                });
            });
        if save {
            cmds.push(Cmd::Save(file.clone()));
        }
        if close || r.should_close() {
            self.viewing = None;
        }
    }

    /// « ↪ Nom : début du message » au-dessus d'une réponse.
    fn reply_line(&self, ui: &mut egui::Ui, view: &ServerView, target: MessageId) {
        let line = match view.message(&target) {
            Some(t) => format!("↪ {} : {}", view.author(t), excerpt(view.text(t), 80)),
            None => "↪ message supprimé ou trop ancien".into(),
        };
        ui.horizontal(|ui| {
            ui.add_space(42.0);
            ui.add(egui::Label::new(RichText::new(line).color(MUTED).small()).truncate());
        });
    }

    /// Texte, pièces jointes et réactions d'un message.
    fn body(&mut self, ui: &mut egui::Ui, view: &ServerView, m: &SignedMessage, cmds: &mut Vec<Cmd>) -> egui::Response {
        let text = view.text(m);
        let r = if text.is_empty() && !view.edited(m) {
            ui.allocate_response(vec2(0.0, 0.0), egui::Sense::hover())
        } else {
            message_text(ui, text, view, view.edited(m))
        };
        if let Kind::Rich { files, .. } = &m.body.kind {
            for f in files {
                self.attachment(ui, f, cmds);
            }
        }
        let reactions = view.reactions(m);
        if !reactions.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (emoji, count, mine) in reactions {
                    let chip = Tinted::new(
                        egui::Button::new(RichText::new(format!("{emoji} {count}")).color(TEXT).small())
                            .corner_radius(10),
                        if mine { ACCENT.gamma_multiply(0.28) } else { SURFACE },
                        Stroke::new(1.0, if mine { ACCENT } else { BORDER }),
                    )
                    .hover_stroke(Stroke::new(1.0, ACCENT_HOVER));
                    if ui
                        .add(chip)
                        .on_hover_text(if mine { "Retirer ma réaction" } else { "Réagir pareil" })
                        .clicked()
                    {
                        cmds.push(react(m, &emoji, !mine));
                    }
                }
            });
        }
        r
    }

    /// Image affichée dans le salon, ou fichier à enregistrer.
    fn attachment(&mut self, ui: &mut egui::Ui, f: &FileRef, cmds: &mut Vec<Cmd>) {
        // Les images se chargent toutes seules quand elles arrivent à l'écran ;
        // les autres fichiers à la demande.
        if let Some((w, h)) = f.image
            && !self.files.contains_key(&f.hash)
        {
            let scale = (360.0 / w.max(1) as f32).min(260.0 / h.max(1) as f32).min(1.0);
            let (rect, _) = ui.allocate_exact_size(vec2(w as f32 * scale, h as f32 * scale), egui::Sense::hover());
            ui.painter().rect_filled(rect, 8, SURFACE);
            if ui.is_rect_visible(rect) {
                self.files.insert(f.hash, Attachment::Loading);
                cmds.push(Cmd::Fetch(f.clone()));
            }
            return;
        }
        let size = human_size(f.size);
        let now = ui.input(|i| i.time);
        match self.files.get_mut(&f.hash) {
            Some(Attachment::Ready { image: Some(tex), seen }) => {
                *seen = now;
                let [w, h] = tex.size_vec2().into();
                let scale = (360.0 / w).min(260.0 / h).min(1.0);
                let r = ui
                    .add(
                        egui::Image::from_texture(&*tex)
                            .fit_to_exact_size(vec2(w * scale, h * scale))
                            .corner_radius(8)
                            .sense(egui::Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(format!("{} ({size}) — clic : voir en grand", plain_text(&f.name)));
                if clicked_on(ui, &r) {
                    self.viewing = Some((f.clone(), None));
                    cmds.push(Cmd::View(f.clone()));
                }
                r.context_menu(|ui| {
                    if ui.button("Voir en grand").clicked() {
                        self.viewing = Some((f.clone(), None));
                        cmds.push(Cmd::View(f.clone()));
                        ui.close();
                    }
                    if ui.button("Enregistrer dans Téléchargements").clicked() {
                        cmds.push(Cmd::Save(f.clone()));
                        ui.close();
                    }
                });
                return;
            }
            Some(Attachment::Loading) if f.image.is_some() => {
                let (w, h) = f.image.unwrap_or((360, 200));
                let scale = (360.0 / w.max(1) as f32).min(260.0 / h.max(1) as f32).min(1.0);
                egui::Frame::new().fill(SURFACE).corner_radius(8).show(ui, |ui| {
                    ui.set_min_size(vec2(w as f32 * scale, h as f32 * scale));
                    ui.centered_and_justified(|ui| ui.spinner());
                });
                return;
            }
            _ => {}
        }
        egui::Frame::new()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(10)
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("📎").color(ACCENT_HOVER));
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(RichText::new(plain_text(&f.name)).color(TEXT)).truncate());
                        let state = match self.files.get(&f.hash) {
                            Some(Attachment::Loading) => "téléchargement…".to_string(),
                            Some(Attachment::Failed(e)) => e.clone(),
                            Some(Attachment::Ready { .. }) => format!("{size} · reçu"),
                            None => size.clone(),
                        };
                        ui.label(RichText::new(state).color(MUTED).small());
                    });
                    ui.add_space(8.0);
                    let loading = matches!(self.files.get(&f.hash), Some(Attachment::Loading));
                    if ui
                        .add_enabled(!loading, egui::Button::new("Enregistrer"))
                        .on_hover_text("Copier dans Téléchargements (le fichier n'est pas ouvert)")
                        .clicked()
                    {
                        match self.files.get(&f.hash) {
                            Some(Attachment::Ready { .. }) => cmds.push(Cmd::Save(f.clone())),
                            _ => {
                                self.files.insert(f.hash, Attachment::Loading);
                                self.save_when_ready.insert(f.hash, f.clone());
                                cmds.push(Cmd::Fetch(f.clone()));
                            }
                        }
                    }
                });
            });
    }

    /// Barre d'actions du message survolé, en haut à droite.
    fn toolbar(
        &mut self,
        ui: &mut egui::Ui,
        view: &ServerView,
        m: &SignedMessage,
        rect: egui::Rect,
        cmds: &mut Vec<Cmd>,
    ) {
        let own = m.signer == view.me;
        let buttons = 2 + own as usize + view.can_delete(m) as usize;
        let width = buttons as f32 * 30.0 + 10.0;
        let pos = egui::pos2(rect.right() - width - 6.0, rect.top() - 14.0);
        let shown = egui::Area::new(ui.scope_id().with(("actions", m.id())))
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(8)
                    .inner_margin(Margin::symmetric(4, 2))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            let smile = ui
                                .add(icons::Button::new(Icon::Smile).height(28.0))
                                .on_hover_text("Réagir");
                            let menu = egui::Popup::menu(&smile).show(|ui| {
                                ui.horizontal(|ui| {
                                    for e in REACTIONS {
                                        if ui.add(ghost_button(RichText::new(e).size(18.0))).clicked() {
                                            cmds.push(react(m, e, !view.reacted(m, e)));
                                            ui.close();
                                        }
                                    }
                                });
                            });
                            if menu.is_some() {
                                self.reacting = Some(m.id());
                            }
                            if ui
                                .add(icons::Button::new(Icon::Reply).height(28.0))
                                .on_hover_text("Répondre")
                                .clicked()
                            {
                                self.reply(m);
                            }
                            if own
                                && ui
                                    .add(icons::Button::new(Icon::Pencil).height(28.0))
                                    .on_hover_text("Modifier")
                                    .clicked()
                            {
                                self.edit(view, m);
                            }
                            if view.can_delete(m)
                                && ui
                                    .add(icons::Button::new(Icon::Trash).kind(icons::Look::Danger).height(28.0))
                                    .on_hover_text(if own { "Supprimer" } else { "Supprimer (modération)" })
                                    .clicked()
                            {
                                self.ask_delete(ui, m, cmds);
                            }
                        });
                    });
            });
        self.toolbar = Some(shown.response.rect);
    }

    /// Clic droit sur un message.
    fn menu(&mut self, ui: &mut egui::Ui, view: &ServerView, m: &SignedMessage, cmds: &mut Vec<Cmd>) {
        ui.horizontal(|ui| {
            for e in REACTIONS {
                if ui.add(ghost_button(RichText::new(e).size(18.0))).clicked() {
                    cmds.push(react(m, e, !view.reacted(m, e)));
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui.button("Répondre").clicked() {
            self.reply(m);
            ui.close();
        }
        if ui.button("Copier le texte").clicked() {
            ui.ctx().copy_text(view.text(m).to_string());
            ui.close();
        }
        if let Some(link) = markup::parse(view.text(m)).into_iter().find(|s| s.link)
            && ui.button("Copier le lien").clicked()
        {
            ui.ctx().copy_text(link.text);
            ui.close();
        }
        if m.signer == view.me && ui.button("Modifier").clicked() {
            self.edit(view, m);
            ui.close();
        }
        if view.can_delete(m) && ui.button(RichText::new("Supprimer").color(DANGER)).clicked() {
            self.ask_delete(ui, m, cmds);
            ui.close();
        }
    }

    fn reply(&mut self, m: &SignedMessage) {
        self.editing = None;
        self.reply_to = Some(m.id());
        self.focus_input = true;
    }

    fn edit(&mut self, view: &ServerView, m: &SignedMessage) {
        self.reply_to = None;
        self.editing = Some(m.id());
        self.input = view.text(m).to_string();
        self.focus_input = true;
    }

    /// Zone de saisie : réponse ou modification en cours, fichiers joints,
    /// suggestions « @ ». Renvoie les commandes à envoyer.
    pub fn input(&mut self, ui: &mut egui::Ui, view: &ServerView, channel: ChannelId, channel_name: &str) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        if let Some(rx) = &self.picking
            && let Ok(paths) = rx.try_recv()
        {
            self.picking = None;
            self.attach(paths);
        }
        // Fichiers glissés sur la fenêtre.
        let dropped: Vec<PathBuf> = ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() && channel != 0 {
            self.attach(dropped);
        }
        // Image collée (Ctrl+V d'une capture d'écran, par exemple).
        let pasted: Vec<_> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::PasteImage(image) => Some(image.clone()),
                    _ => None,
                })
                .collect()
        });
        if channel != 0 && self.editing.is_none() {
            for image in pasted {
                match self.keep_pasted(&image) {
                    Ok(path) => self.attach(vec![path]),
                    Err(e) => tracing::warn!("image collée : {e:#}"),
                }
            }
        }

        if let Some(target) = self.editing.or(self.reply_to) {
            let editing = self.editing.is_some();
            ui.horizontal(|ui| {
                let text = match (editing, view.message(&target)) {
                    (true, _) => "Modification de ton message — Échap pour annuler".to_string(),
                    (false, Some(t)) => format!("Réponse à {} : {}", view.author(t), excerpt(view.text(t), 60)),
                    (false, None) => "Réponse".to_string(),
                };
                ui.add(egui::Label::new(RichText::new(text).color(ACCENT_HOVER).small()).truncate());
                at_right(ui, |ui| {
                    if ui.add(ghost_button(RichText::new("×").color(MUTED))).clicked() {
                        self.cancel();
                    }
                });
            });
        }
        if !self.attaching.is_empty() {
            let mut remove = None;
            ui.horizontal_wrapped(|ui| {
                for (i, p) in self.attaching.iter().enumerate() {
                    let name = p
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                    egui::Frame::new()
                        .fill(SURFACE)
                        .corner_radius(8)
                        .inner_margin(Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("📎 {name}")).color(TEXT).small());
                                if ui.add(ghost_button(RichText::new("×").color(MUTED).small())).clicked() {
                                    remove = Some(i);
                                }
                            });
                        });
                }
            });
            if let Some(i) = remove {
                self.attaching.remove(i);
            }
        }

        let suggestions = self.suggestions(view);
        if !suggestions.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Mentionner :").color(MUTED).small());
                for (i, (name, user)) in suggestions.iter().enumerate() {
                    let label = if i == 0 { format!("{name}  ⇥") } else { name.clone() };
                    if ui.small_button(label).clicked() {
                        self.complete(name, *user);
                    }
                }
            });
        }

        let hint_text = format!("Écrire dans #{channel_name}");
        ui.add_enabled_ui(channel != 0, |ui| {
            // Tab complète la première suggestion (au lieu de changer de champ).
            if !suggestions.is_empty() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Tab)) {
                let (name, user) = suggestions[0].clone();
                self.complete(&name, user);
                self.focus_input = true;
            }
            let mut send = false;
            ui.horizontal(|ui| {
                let attach = ui
                    .add(icons::Button::new(Icon::Plus).kind(icons::Look::Secondary).height(40.0))
                    .on_hover_text(
                        "Joindre des fichiers ou des images (ou glisse-les sur la fenêtre ; Ctrl+V colle une image)",
                    );
                if attach.clicked() && self.picking.is_none() && self.editing.is_none() {
                    self.pick_files();
                }
                // Entrée envoie, Maj+Entrée va à la ligne : le champ grandit
                // avec le texte, jusqu'à quelques lignes.
                let id = ui.make_persistent_id("saisie");
                if ui.memory(|m| m.has_focus(id)) {
                    send = ui.input_mut(|i| {
                        let before = i.events.len();
                        i.events.retain(|e| {
                            !matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. } if !modifiers.shift)
                        });
                        i.events.len() != before
                    });
                }
                let field = egui::ScrollArea::vertical()
                    .id_salt("saisie-lignes")
                    .max_height(132.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.input)
                                .id(id)
                                .hint_text(RichText::new(&hint_text).color(MUTED))
                                .desired_width(f32::INFINITY)
                                .desired_rows(1)
                                .return_key(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, Key::Enter))
                                .char_limit(MAX_MESSAGE_CHARS)
                                .margin(Margin::symmetric(14, 12)),
                        )
                    })
                    .inner;
                if std::mem::take(&mut self.focus_input) {
                    field.request_focus();
                }
                if field.has_focus() {
                    let (up, escape) = ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::Escape)));
                    // Flèche du haut, champ vide : modifier mon dernier message.
                    if up
                        && self.input.is_empty()
                        && let Some(m) = view.last_own(channel)
                    {
                        self.edit(view, m);
                    }
                    if escape {
                        self.cancel();
                    }
                }
            });
            if send && let Some(c) = self.submit(view, channel) {
                cmds.push(c);
            }
        });
        cmds
    }

    fn cancel(&mut self) {
        if self.editing.take().is_some() {
            self.input.clear();
        }
        self.reply_to = None;
    }

    /// Enregistre une image collée (en PNG) pour la joindre comme un fichier.
    fn keep_pasted(&mut self, image: &egui::ColorImage) -> anyhow::Result<PathBuf> {
        use anyhow::Context as _;
        let dir = self
            .paste_dir
            .clone()
            .context("pas de dossier pour les images collées")?;
        let [w, h] = image.size;
        anyhow::ensure!(w > 0 && h > 0, "image vide");
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_srgba_unmultiplied()).collect();
        let picture = image::RgbaImage::from_raw(w as u32, h as u32, rgba).context("image illisible")?;
        std::fs::create_dir_all(&dir)?;
        self.pasted += 1;
        let name = format!(
            "image-{}{}.png",
            Local::now().format("%Y-%m-%d-%Hh%M"),
            if self.pasted > 1 {
                format!("-{}", self.pasted)
            } else {
                String::new()
            }
        );
        let path = dir.join(name);
        picture.save_with_format(&path, image::ImageFormat::Png)?;
        Ok(path)
    }

    fn attach(&mut self, paths: Vec<PathBuf>) {
        for p in paths {
            if self.attaching.len() < MAX_FILES && p.is_file() && !self.attaching.contains(&p) {
                self.attaching.push(p);
            }
        }
    }

    /// Fenêtre native du système, dans un fil à part (elle bloque).
    fn pick_files(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        self.picking = Some(rx);
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Joindre des fichiers")
                .pick_files()
                .unwrap_or_default();
            let _ = tx.send(picked);
        });
    }

    /// Membres dont le pseudo commence par le « @mot » en cours de frappe.
    fn suggestions(&self, view: &ServerView) -> Vec<(String, UserId)> {
        let Some(word) = self.input.rsplit(char::is_whitespace).next() else {
            return Vec::new();
        };
        let Some(prefix) = word.strip_prefix('@') else {
            return Vec::new();
        };
        let prefix = prefix.to_lowercase();
        let mut seen = HashSet::new();
        let mut out: Vec<(String, UserId)> = view
            .peers
            .values()
            .filter(|p| p.user != view.me && seen.insert(p.user))
            .filter(|p| p.name.to_lowercase().starts_with(&prefix) && p.name.to_lowercase() != prefix)
            .map(|p| (p.name.clone(), p.user))
            .take(6)
            .collect();
        if "tous".starts_with(&prefix) && prefix != "tous" {
            out.push(("tous".into(), view.me));
        }
        out
    }

    fn complete(&mut self, name: &str, user: UserId) {
        let cut = self.input.rfind('@').unwrap_or(self.input.len());
        self.input.truncate(cut);
        self.input.push('@');
        self.input.push_str(name);
        self.input.push(' ');
        if name != "tous" {
            self.mentions.push((name.to_string(), user));
        }
    }

    /// Le message tapé, prêt à partir (ou la modification).
    fn submit(&mut self, view: &ServerView, channel: ChannelId) -> Option<Cmd> {
        let text = self.input.trim().to_string();
        if channel == 0 || (text.is_empty() && self.attaching.is_empty()) {
            return None;
        }
        self.input.clear();
        if let Some(target) = self.editing.take() {
            return (!text.is_empty()).then_some(Cmd::Act {
                channel,
                body: text,
                kind: Kind::Edit { target },
            });
        }
        // Mentions : celles choisies dans les suggestions, et les « @pseudo »
        // tapés en entier d'un membre présent.
        let mut mentions: Vec<UserId> = std::mem::take(&mut self.mentions)
            .into_iter()
            .filter(|(name, _)| text.contains(&format!("@{name}")))
            .map(|(_, u)| u)
            .collect();
        for word in text.split_whitespace().filter_map(|w| w.strip_prefix('@')) {
            let word = word.trim_end_matches(|c: char| !c.is_alphanumeric());
            if let Some(p) = view.peers.values().find(|p| p.name.eq_ignore_ascii_case(word))
                && !mentions.contains(&p.user)
                && p.user != view.me
            {
                mentions.push(p.user);
            }
        }
        Some(Cmd::Send(Draft {
            channel,
            body: text,
            reply_to: self.reply_to.take(),
            mentions,
            files: std::mem::take(&mut self.attaching),
        }))
    }
}

fn react(m: &SignedMessage, emoji: &str, on: bool) -> Cmd {
    Cmd::Act {
        channel: m.body.channel,
        body: String::new(),
        kind: Kind::React {
            target: m.id(),
            emoji: emoji.into(),
            on,
        },
    }
}

fn delete(m: &SignedMessage) -> Cmd {
    Cmd::Act {
        channel: m.body.channel,
        body: String::new(),
        kind: Kind::Delete { target: m.id() },
    }
}

/// Texte d'un message : mise en forme, « @mentions » en couleur, liens
/// cliquables. Le texte reste sélectionnable.
fn message_text(ui: &mut egui::Ui, text: &str, view: &ServerView, edited: bool) -> egui::Response {
    let spans = markup::parse(text);
    let font = egui::TextStyle::Body.resolve(ui.style());
    let my_name = view.peers.get(&view.my_peer).map(|p| p.name.to_lowercase());
    let mut job = LayoutJob::default();
    // La police n'a pas de gras : le texte en gras est repassé, décalé d'un
    // demi-point, par une copie du texte où tout le reste est transparent.
    let has_bold = spans.iter().any(|s| s.bold && !s.code);
    let mut over = LayoutJob::default();
    // Liens : caractères couverts dans le texte affiché, et adresse.
    let mut links: Vec<(std::ops::Range<usize>, &str)> = Vec::new();
    let mut chars = 0;
    for s in &spans {
        let mut format = egui::TextFormat {
            font_id: font.clone(),
            color: TEXT,
            italics: s.italic,
            ..Default::default()
        };
        if s.strike {
            format.strikethrough = Stroke::new(1.0, MUTED);
            format.color = MUTED;
        }
        if s.code {
            format.font_id = egui::FontId::monospace(font.size - 1.0);
            format.background = FIELD;
        } else if s.link {
            format.color = ACCENT_HOVER;
            links.push((chars..chars + s.text.chars().count(), &s.text));
        } else if s.mention {
            let bare = s.text.to_lowercase();
            let me = my_name.as_ref().is_some_and(|n| bare == format!("@{n}")) || EVERYONE.contains(&bare.as_str());
            format.color = if me { WARN } else { ACCENT_HOVER };
            format.background = if me {
                WARN.gamma_multiply(0.14)
            } else {
                ACCENT.gamma_multiply(0.16)
            };
        }
        chars += s.text.chars().count();
        if has_bold {
            over.append(
                &s.text,
                0.0,
                egui::TextFormat {
                    color: if s.bold && !s.code {
                        format.color
                    } else {
                        Color32::TRANSPARENT
                    },
                    background: Color32::TRANSPARENT,
                    strikethrough: Stroke::NONE,
                    underline: Stroke::NONE,
                    ..format.clone()
                },
            );
        }
        job.append(&s.text, 0.0, format);
    }
    if edited {
        let note = egui::TextFormat {
            font_id: egui::FontId::proportional(11.5),
            color: MUTED,
            ..Default::default()
        };
        if has_bold {
            over.append(
                " (modifié)",
                0.0,
                egui::TextFormat {
                    color: Color32::TRANSPARENT,
                    ..note.clone()
                },
            );
        }
        job.append(" (modifié)", 0.0, note);
    }
    job.wrap.max_width = ui.available_width();
    over.wrap.max_width = job.wrap.max_width;
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let mut r = ui.add(egui::Label::new(galley.clone()));
    if has_bold && ui.is_rect_visible(r.rect) {
        let over = ui.fonts_mut(|f| f.layout_job(over));
        ui.painter()
            .galley(r.rect.min + vec2(0.6, 0.0), over, Color32::TRANSPARENT);
    }
    if !links.is_empty()
        && ui.rect_contains_pointer(r.rect)
        && let Some(pointer) = ui.input(|i| i.pointer.hover_pos())
        && let Some(at) = char_at(&galley, pointer - r.rect.min)
        && let Some((_, url)) = links.iter().find(|(range, _)| range.contains(&at))
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        if clicked_on(ui, &r) && markup::safe_url(url) {
            ui.ctx().open_url(egui::OpenUrl::new_tab(*url));
        }
        r = r.on_hover_text(format!("Ouvrir dans le navigateur :\n{url}"));
    }
    r
}

/// Clic sur un élément d'un message. La ligne entière du message capte déjà
/// les clics (menu du clic droit) : on regarde donc le pointeur lui-même.
fn clicked_on(ui: &egui::Ui, r: &egui::Response) -> bool {
    r.clicked() || (ui.rect_contains_pointer(r.rect) && ui.input(|i| i.pointer.primary_clicked()))
}

/// Caractère du texte affiché sous ce point (relatif au coin du texte).
fn char_at(galley: &egui::Galley, at: egui::Vec2) -> Option<usize> {
    let mut index = 0;
    for row in &galley.rows {
        if row.rect().contains(at.to_pos2()) {
            let x = at.x - row.pos.x;
            return row
                .glyphs
                .iter()
                .position(|g| x >= g.pos.x && x < g.pos.x + g.advance_width)
                .map(|i| index + i);
        }
        index += row.glyphs.len() + usize::from(row.ends_with_newline);
    }
    None
}

/// Hauteur probable d'un message pas encore affiché (corrigée dès qu'il l'est).
fn guess_height(m: &SignedMessage, grouped: bool) -> f32 {
    let lines = m.body.body.lines().map(|l| 1 + l.len() / 90).sum::<usize>().max(1);
    let files = match &m.body.kind {
        Kind::Rich { files, .. } => files.iter().map(|f| if f.image.is_some() { 260.0 } else { 56.0 }).sum(),
        _ => 0.0,
    };
    (if grouped { 4.0 } else { 26.0 }) + 22.0 * lines as f32 + files
}

fn excerpt(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_string()
    }
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} o"),
        b if b < 1024 * 1024 => format!("{:.0} Ko", b as f64 / 1024.0),
        b => format!("{:.1} Mo", b as f64 / (1024.0 * 1024.0)),
    }
}

/// Ligne « ─── Aujourd'hui ─── » entre deux jours.
fn day_separator(ui: &mut egui::Ui, label: &str) {
    separator(ui, label, MUTED);
}

/// Ligne avec un mot au milieu : le jour, ou « Nouveaux messages ».
fn separator(ui: &mut egui::Ui, label: &str, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), egui::Sense::hover());
    let p = ui.painter();
    let galley = p.layout_no_wrap(label.to_string(), egui::FontId::proportional(12.5), color);
    let c = rect.center();
    let half = galley.size().x / 2.0 + 10.0;
    let line = Stroke::new(
        1.0,
        if color == MUTED {
            BORDER
        } else {
            color.gamma_multiply(0.6)
        },
    );
    p.hline(rect.left()..=c.x - half, c.y, line);
    p.hline(c.x + half..=rect.right(), c.y, line);
    p.galley(
        egui::pos2(c.x - galley.size().x / 2.0, c.y - galley.size().y / 2.0),
        galley,
        color,
    );
}

fn local(ms: u64) -> Option<DateTime<Local>> {
    Local.timestamp_millis_opt(ms as i64).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::Client,
        core::{card::ServerCard, identity::Identity, store::HistoryLog},
        server::Host,
    };

    /// Quelques images de l'interface sur un fil bien rempli : rien ne
    /// panique, les images sont demandées, Entrée envoie.
    #[tokio::test]
    async fn fil_affiche_et_saisie() {
        let owner = Identity::generate();
        let card = ServerCard::create(&owner, "fil");
        let general = card.settings().text_channels[0].id;
        let host = Host::bind(
            "127.0.0.1:0".parse().unwrap(),
            card.clone(),
            HistoryLog::in_memory(card.id, card.history()),
        )
        .unwrap();
        let addr = host.local_addr().unwrap();
        tokio::spawn(host.run());
        let (alice, _rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
        let mut view = ServerView::new(&alice, "alice", None);

        let first = alice.send_text(general, "salut @tous").unwrap();
        view.push_own(first.clone());
        let image = FileRef {
            name: "chat.png".into(),
            size: 2048,
            hash: [3; 32],
            image: Some((640, 480)),
        };
        let doc = FileRef {
            name: "notes.pdf".into(),
            size: 3_000_000,
            hash: [4; 32],
            image: None,
        };
        let kinds = [
            Kind::Rich {
                reply_to: Some(first.id()),
                mentions: vec![owner.id()],
                files: vec![image.clone(), doc],
            },
            Kind::React {
                target: first.id(),
                emoji: "👍".into(),
                on: true,
            },
            Kind::Edit { target: first.id() },
        ];
        for kind in kinds {
            let m = alice.send_message(general, "modifié", kind).unwrap();
            view.push_own(m);
        }

        let ctx = egui::Context::default();
        let mut chat = Chat::default();
        let mut cmds = Vec::new();
        let frame = |chat: &mut Chat, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(900.0, 700.0))),
                events,
                ..Default::default()
            };
            let mut out = Vec::new();
            let mut full = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    out.extend(chat.messages(ui, &view, general, None));
                    out.extend(chat.input(ui, &view, general, "général"));
                });
            });
            full.textures_delta.clear();
            out
        };
        for y in [40.0, 120.0, 200.0] {
            cmds.extend(frame(&mut chat, vec![egui::Event::PointerMoved(egui::pos2(300.0, y))]));
        }
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Fetch(f) if f.hash == image.hash)));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Fetch(f) if f.hash == [4; 32])));

        let ready = FileState::Ready {
            image: Some(egui::ColorImage::filled([4, 3], Color32::RED)),
        };
        assert!(chat.file_ready(&ctx, image.hash, ready).is_none());
        let again = |cmds: &[Cmd]| cmds.iter().any(|c| matches!(c, Cmd::Fetch(f) if f.hash == image.hash));
        assert!(!again(&frame(&mut chat, vec![])), "image prête : pas redemandée");

        // Restée longtemps hors de vue : déchargée, puis redemandée en revenant à l'écran.
        if let Some(Attachment::Ready { seen, .. }) = chat.files.get_mut(&image.hash) {
            *seen -= 2.0 * UNLOAD_AFTER;
        }
        assert!(again(&frame(&mut chat, vec![])));

        chat.input = "re @alice".into();
        chat.focus_input = true;
        frame(&mut chat, vec![]);
        let sent = frame(
            &mut chat,
            vec![egui::Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
        );
        match sent.iter().find(|c| matches!(c, Cmd::Send(_))) {
            Some(Cmd::Send(d)) => {
                assert_eq!(d.body, "re @alice");
                assert!(d.mentions.is_empty(), "on ne se mentionne pas soi-même");
            }
            _ => panic!("Entrée n'a rien envoyé"),
        }
        assert!(chat.input.is_empty());
    }

    /// Temps d'une image de l'interface sur un long fil, collé en bas
    /// (`cargo test --release fil_long -- --ignored --nocapture`).
    #[tokio::test]
    #[ignore]
    async fn fil_long() {
        use crate::core::card::{Message, now_ms};
        let owner = Identity::generate();
        let card = ServerCard::create(&owner, "fil");
        let general = card.settings().text_channels[0].id;
        let host = Host::bind(
            "127.0.0.1:0".parse().unwrap(),
            card.clone(),
            HistoryLog::in_memory(card.id, card.history()),
        )
        .unwrap();
        let addr = host.local_addr().unwrap();
        tokio::spawn(host.run());
        let (alice, _rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
        let mut view = ServerView::new(&alice, "alice", None);
        let start = now_ms() - 3_000_000;
        for i in 0..2000u64 {
            let m = Message {
                server: card.id,
                channel: general,
                author_name: ["alice", "bob", "chloé"][(i % 7 % 3) as usize].into(),
                at_ms: start + i * 1500,
                body: format!("message numéro {i}, un peu de texte pour remplir la ligne @alice"),
                kind: Kind::Text,
            }
            .sign(&owner);
            view.push_own(m);
        }
        let ctx = egui::Context::default();
        let mut chat = Chat::default();
        let frame = |chat: &mut Chat| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1100.0, 750.0))),
                events: vec![egui::Event::PointerMoved(egui::pos2(500.0, 400.0))],
                ..Default::default()
            };
            let full = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .stick_to_bottom(true)
                        .show(ui, |ui| chat.messages(ui, &view, general, None));
                });
            });
            ctx.tessellate(full.shapes, full.pixels_per_point);
        };
        let t = std::time::Instant::now();
        frame(&mut chat);
        println!("première image : {:?}", t.elapsed());
        for _ in 0..5 {
            frame(&mut chat);
        }
        let t = std::time::Instant::now();
        for _ in 0..50 {
            frame(&mut chat);
        }
        println!("image suivante : {:?}", t.elapsed() / 50);
    }

    /// Un hôte local, alice connectée, et sa vue du serveur.
    async fn alice() -> (Client, ServerView, ChannelId) {
        let owner = Identity::generate();
        let card = ServerCard::create(&owner, "fil");
        let general = card.settings().text_channels[0].id;
        let host = Host::bind(
            "127.0.0.1:0".parse().unwrap(),
            card.clone(),
            HistoryLog::in_memory(card.id, card.history()),
        )
        .unwrap();
        let addr = host.local_addr().unwrap();
        tokio::spawn(host.run());
        let (alice, _rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
        let view = ServerView::new(&alice, "alice", None);
        (alice, view, general)
    }

    fn screen(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(900.0, 700.0))),
            events,
            ..Default::default()
        }
    }

    fn key(key: Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn click(at: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        }
    }

    /// Un clic sur un lien l'ouvre dans le navigateur ; un clic à côté, non.
    #[tokio::test]
    async fn lien_cliquable() {
        let (_alice, view, _) = alice().await;
        let ctx = egui::Context::default();
        let text = "va voir **ici** https://exemple.fr/page?x=1, c'est `bien`";
        let place = std::cell::Cell::new(egui::Rect::NOTHING);
        let frame = |events: Vec<egui::Event>| {
            let mut out = ctx.run_ui(screen(events), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    place.set(message_text(ui, text, &view, false).rect);
                });
            });
            out.textures_delta.clear();
            out.platform_output
                .commands
                .iter()
                .filter_map(|c| match c {
                    egui::OutputCommand::OpenUrl(u) => Some(u.url.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        frame(vec![]);
        frame(vec![]);
        // Où est le lien : on cherche son premier et son dernier caractère dans le texte posé.
        let spans = markup::parse(text);
        let before: usize = spans
            .iter()
            .take_while(|s| !s.link)
            .map(|s| s.text.chars().count())
            .sum();
        let font = egui::FontId::proportional(15.0);
        let width = |n: usize| {
            let shown: String = spans.iter().flat_map(|s| s.text.chars()).take(n).collect();
            ctx.fonts_mut(|f| f.layout_no_wrap(shown, font.clone(), Color32::WHITE).size().x)
        };
        let rect = place.get();
        let y = rect.center().y;
        let on_link = egui::pos2(rect.left() + width(before + 10), y);
        let beside = egui::pos2(rect.left() + width(3), y);

        for at in [beside, on_link] {
            frame(vec![egui::Event::PointerMoved(at)]);
            frame(vec![click(at, true)]);
            let opened = frame(vec![click(at, false)]);
            if at == on_link {
                assert_eq!(opened, ["https://exemple.fr/page?x=1"]);
            } else {
                assert!(opened.is_empty(), "clic hors du lien : {opened:?}");
            }
        }
    }

    /// Entrée envoie, Maj+Entrée va à la ligne ; une image collée est jointe.
    #[tokio::test]
    async fn plusieurs_lignes_et_image_collee() {
        let (_alice, view, general) = alice().await;
        let dir = std::env::temp_dir().join(format!("rip-discord-colle-{}", std::process::id()));
        let ctx = egui::Context::default();
        let mut chat = Chat::default().with_paste_dir(dir.clone());
        let frame = |chat: &mut Chat, events: Vec<egui::Event>| {
            let mut out = Vec::new();
            let mut full = ctx.run_ui(screen(events), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    out.extend(chat.input(ui, &view, general, "général"));
                });
            });
            full.textures_delta.clear();
            out
        };
        chat.focus_input = true;
        frame(&mut chat, vec![]);
        frame(&mut chat, vec![egui::Event::Text("ligne 1".into())]);
        let sent = frame(&mut chat, vec![key(Key::Enter, egui::Modifiers::SHIFT)]);
        assert!(sent.is_empty(), "Maj+Entrée n'envoie pas");
        frame(&mut chat, vec![egui::Event::Text("ligne 2".into())]);
        assert_eq!(chat.input, "ligne 1\nligne 2");

        let image = egui::ColorImage::filled([8, 6], Color32::from_rgb(200, 30, 30));
        frame(&mut chat, vec![egui::Event::PasteImage(std::sync::Arc::new(image))]);
        assert_eq!(chat.attaching.len(), 1, "image collée jointe");
        let pasted = image::open(&chat.attaching[0]).unwrap().to_rgba8();
        assert_eq!((pasted.width(), pasted.height()), (8, 6));
        assert_eq!(pasted.get_pixel(3, 3).0, [200, 30, 30, 255]);

        let sent = frame(&mut chat, vec![key(Key::Enter, egui::Modifiers::NONE)]);
        match sent.as_slice() {
            [Cmd::Send(d)] => {
                assert_eq!(d.body, "ligne 1\nligne 2");
                assert_eq!(d.files.len(), 1);
            }
            _ => panic!("Entrée n'a rien envoyé"),
        }
        assert!(chat.input.is_empty() && chat.attaching.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Supprimer demande confirmation : rien ne part avant « Supprimer ».
    #[tokio::test]
    async fn suppression_confirmee() {
        let (alice, mut view, general) = alice().await;
        let m = alice.send_text(general, "oups").unwrap();
        view.push_own(m.clone());
        let ctx = egui::Context::default();
        let mut chat = Chat::default();
        let frame = |chat: &mut Chat, events: Vec<egui::Event>| {
            let mut out = Vec::new();
            let mut full = ctx.run_ui(screen(events), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    out.extend(chat.messages(ui, &view, general, Some(0)));
                });
            });
            full.textures_delta.clear();
            out
        };
        assert!(frame(&mut chat, vec![]).is_empty());
        chat.deleting = Some(m.id());
        assert!(
            frame(&mut chat, vec![]).is_empty(),
            "la question est posée, rien n'est supprimé"
        );
        assert!(frame(&mut chat, vec![]).is_empty());
        // Échap : on garde le message.
        frame(&mut chat, vec![key(Key::Escape, egui::Modifiers::NONE)]);
        assert!(chat.deleting.is_none());
        chat.deleting = Some(m.id());
        frame(&mut chat, vec![]);
        let sent = frame(&mut chat, vec![key(Key::Enter, egui::Modifiers::NONE)]);
        assert!(
            matches!(sent.as_slice(), [Cmd::Act { kind: Kind::Delete { target }, .. }] if *target == m.id()),
            "Entrée confirme la suppression"
        );
        assert!(chat.deleting.is_none());
    }

    #[test]
    fn saisie_gardee_par_salon() {
        let mut chat = Chat {
            input: "pour le général".into(),
            ..Default::default()
        };
        chat.leave_channel(1, 2);
        assert!(chat.input.is_empty(), "rien ne suit dans l'autre salon");
        chat.input = "pour les jeux".into();
        chat.leave_channel(2, 1);
        assert_eq!(chat.input, "pour le général");
        chat.input.clear();
        chat.leave_channel(1, 2);
        assert_eq!(chat.input, "pour les jeux");
        chat.leave_channel(2, 1);
        assert!(chat.input.is_empty(), "saisie vidée : rien n'est gardé");

        // Une modification en cours ne reste pas en brouillon.
        chat.input = "texte modifié".into();
        chat.editing = Some(MessageId::default());
        chat.leave_channel(1, 2);
        chat.leave_channel(2, 1);
        assert!(chat.input.is_empty());
    }
}
