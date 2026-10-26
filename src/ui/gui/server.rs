//! « Paramètres du serveur » : invitation (code caché), et pour le créateur
//! et les admins, nom, salons, historique, admins, bannis.

use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

use eframe::egui::{self, FontFamily, FontId, RichText};

use super::{Invite, icons::*, style::*};
use crate::{
    client::ServerView,
    core::{
        card::{Role, ServerCard, Settings},
        identity::{Identity, UserId},
    },
};

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Invite,
    General,
    Channels,
    History,
    Members,
}

pub enum Outcome {
    Open,
    Closed,
    /// Réglages modifiés et signés, à envoyer à tous.
    Save(Box<ServerCard>),
    Error(String),
    Help,
}

pub struct ServerWindow {
    tab: Tab,
    settings: Settings,
    admins: Vec<UserId>,
    banned: Vec<UserId>,
    new_text: String,
    new_voice: String,
    /// Code d'invitation affiché en clair.
    reveal: bool,
    copied: Option<f64>,
    /// « Changer le code » cliqué une fois : on attend la confirmation.
    confirm_rotate: bool,
    /// Confirmé : nouveau code à tirer.
    rotate: bool,
}

impl ServerWindow {
    pub fn new(view: &ServerView, tab: Tab) -> Self {
        Self {
            tab,
            settings: view.card.settings().clone(),
            admins: view.card.admins.body.admins.clone(),
            banned: view.card.banned().to_vec(),
            new_text: String::new(),
            new_voice: String::new(),
            reveal: false,
            copied: None,
            confirm_rotate: false,
            rotate: false,
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        view: &ServerView,
        me: &Identity,
        invite: Option<&Invite>,
        hosting: bool,
        remote: Option<SocketAddr>,
    ) -> Outcome {
        let can_edit = view.can_edit();
        let owner = view.card.role(&view.me) == Role::Owner;
        let dirty = self.settings != *view.card.settings()
            || (owner && self.admins != view.card.admins.body.admins)
            || (can_edit && self.banned != view.card.banned());
        let mut out = Outcome::Open;
        let mut pages = vec![(Icon::UserPlus, "Invitation", Tab::Invite)];
        if can_edit {
            pages.extend([
                (Icon::Sliders, "Général", Tab::General),
                (Icon::Hash, "Salons", Tab::Channels),
                (Icon::Clock, "Historique", Tab::History),
            ]);
        }
        pages.push((Icon::Users, "Membres", Tab::Members));
        let (mut save, mut cancel) = (false, false);
        let mut footer = |ui: &mut egui::Ui| {
            icon(ui, Icon::Info, 18.0, WARN);
            ui.label(RichText::new("Modifications non enregistrées").color(WARN));
            at_right(ui, |ui| {
                save = ui
                    .add(
                        Button::new(Icon::Check)
                            .label("Enregistrer")
                            .kind(Look::Primary)
                            .height(38.0),
                    )
                    .clicked();
                cancel = ui.add(Button::text("Annuler").kind(Look::Ghost).height(38.0)).clicked();
            });
        };
        let mut tab = self.tab;
        let mut help = false;
        let closed = Shell {
            id: "server_settings",
            heading: view.card.name(),
            sub: "Paramètres du serveur",
            pages: &pages,
        }
        .show(
            ctx,
            &mut tab,
            |_| {},
            |ui, t| match t {
                Tab::Invite => help = self.invite(ui, invite, hosting, remote, can_edit),
                Tab::General => self.general(ui),
                Tab::Channels => self.channels(ui),
                Tab::History => self.history(ui),
                Tab::Members => self.members(ui, view, owner, can_edit),
            },
            (can_edit && dirty).then_some(&mut footer as &mut dyn FnMut(&mut egui::Ui)),
        );
        self.tab = tab;
        if help {
            out = Outcome::Help;
        }
        if std::mem::take(&mut self.rotate) {
            let mut card = view.card.clone();
            out = match card.new_access_key(me) {
                Ok(()) => Outcome::Save(Box::new(card)),
                Err(e) => Outcome::Error(format!("{e:#}")),
            };
        } else if save {
            out = self.save(view, me, owner);
        } else if cancel {
            *self = Self::new(view, self.tab);
        } else if closed {
            out = Outcome::Closed;
        }
        out
    }

    fn save(&self, view: &ServerView, me: &Identity, owner: bool) -> Outcome {
        let mut card = view.card.clone();
        let mut result = Ok(());
        if self.settings != *card.settings() {
            let mut wanted = self.settings.clone();
            // Un salon sans nom n'a pas de sens : on lui en donne un.
            for c in wanted.text_channels.iter_mut().chain(wanted.voice_channels.iter_mut()) {
                if c.name.trim().is_empty() {
                    c.name = "salon".into();
                }
            }
            result = card.edit(me, |s| *s = wanted);
        }
        if result.is_ok() && owner && self.admins != card.admins.body.admins {
            result = card.set_admins(me, self.admins.clone());
        }
        if result.is_ok() && self.banned != card.banned() {
            let before = card.banned().to_vec();
            for u in before.iter().filter(|u| !self.banned.contains(u)) {
                result = result.and_then(|_| card.set_banned(me, *u, false));
            }
            for u in self.banned.iter().filter(|u| !before.contains(u)) {
                result = result.and_then(|_| card.set_banned(me, *u, true));
            }
        }
        match result {
            Ok(()) if card != view.card => Outcome::Save(Box::new(card)),
            Ok(()) => Outcome::Closed,
            Err(e) => Outcome::Error(format!("{e:#}")),
        }
    }

    /// Renvoie vrai si l'aide est demandée.
    fn invite(
        &mut self,
        ui: &mut egui::Ui,
        invite: Option<&Invite>,
        hosting: bool,
        remote: Option<SocketAddr>,
        can_edit: bool,
    ) -> bool {
        let Some(invite) = invite.filter(|i| i.code().is_some()) else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Préparation du code…").color(MUTED));
            });
            return false;
        };
        let code = invite.code().unwrap_or_default().to_string();
        group(ui, "Code d'invitation", |ui| {
            hint(
                ui,
                "Envoie-le à tes amis, en privé. Il marche quel que soit le membre qui héberge, et il sert \
                 aussi de clé au serveur : ne le montre pas en stream.",
            );
            ui.add_space(4.0);
            egui::Frame::new()
                .fill(FIELD)
                .stroke(egui::Stroke::new(1.0, BORDER))
                .corner_radius(10)
                .inner_margin(14)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let shown = if self.reveal { code.clone() } else { MASKED.to_string() };
                    ui.add(
                        egui::Label::new(
                            RichText::new(shown)
                                .font(FontId::new(17.0, FontFamily::Monospace))
                                .color(if self.reveal { TEXT } else { MUTED }),
                        )
                        .wrap(),
                    );
                });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let now = ui.input(|i| i.time);
                let copied = self.copied.is_some_and(|t| now - t < 2.0);
                let (i, label) = if copied {
                    (Icon::Check, "Copié !")
                } else {
                    (Icon::Copy, "Copier le code")
                };
                if ui
                    .add(
                        Button::new(i)
                            .label(label)
                            .kind(Look::Primary)
                            .height(38.0)
                            .width(180.0),
                    )
                    .clicked()
                {
                    ui.ctx().copy_text(code.clone());
                    self.copied = Some(now);
                }
                if copied {
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
                let (i, label) = if self.reveal {
                    (Icon::EyeOff, "Masquer")
                } else {
                    (Icon::Eye, "Afficher")
                };
                if ui.add(Button::new(i).label(label).height(38.0)).clicked() {
                    self.reveal = !self.reveal;
                }
            });
            hint(ui, "« Copier » marche sans afficher le code.");
        });

        if can_edit {
            group(ui, "Changer le code", |ui| {
                hint(
                    ui,
                    "Le code a fuité, ou tu as banni quelqu'un qui le connaît ? Un nouveau code ferme la porte à \
                     l'ancien : il ne permet plus d'entrer ni de trouver l'hôte. Les membres connectés reçoivent \
                     le nouveau tout seuls ; ceux qui ne le sont pas devront te le redemander.",
                );
                ui.add_space(4.0);
                if self.confirm_rotate {
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                Button::new(Icon::Refresh)
                                    .label("Oui, nouveau code")
                                    .kind(Look::Danger)
                                    .height(38.0),
                            )
                            .clicked()
                        {
                            self.rotate = true;
                            self.confirm_rotate = false;
                            self.reveal = false;
                        }
                        if ui.add(Button::text("Annuler").kind(Look::Ghost).height(38.0)).clicked() {
                            self.confirm_rotate = false;
                        }
                    });
                } else if ui
                    .add(Button::new(Icon::Key).label("Nouveau code").height(38.0))
                    .clicked()
                {
                    self.confirm_rotate = true;
                }
            });
        }

        let (color, title, detail) = match invite {
            Invite::Searching => unreachable!(),
            Invite::Public { warning: None, .. } => (OK, "Accessible depuis Internet", None),
            Invite::Anywhere { warning: None, .. } => (OK, "Joignable sans ouvrir de port", None),
            Invite::Public { warning: Some(w), .. } | Invite::Anywhere { warning: Some(w), .. } => {
                (WARN, "Accès Internet incertain", Some(w.clone()))
            }
            Invite::LanOnly { reason, .. } => (WARN, "Réseau local seulement", Some(reason.clone())),
        };
        let mut help = false;
        group(ui, "Connexion", |ui| {
            ui.horizontal(|ui| {
                status_dot(ui, color);
                ui.label(RichText::new(title).color(color).strong());
            });
            if let Some(d) = detail {
                hint(ui, d);
            }
            divider(ui);
            let host = if hosting {
                "Ton PC héberge le serveur en ce moment.".to_string()
            } else {
                match (self.reveal, remote) {
                    (true, Some(addr)) => format!("Hébergé par un autre membre ({addr})."),
                    _ => "Hébergé par un autre membre.".to_string(),
                }
            };
            row_with(ui, Some(Icon::Server), "Hôte", &host, |ui| {
                help = ui
                    .add(
                        Button::new(Icon::Help)
                            .label("Que veut dire ce statut ?")
                            .kind(Look::Ghost)
                            .height(32.0),
                    )
                    .clicked();
            });
        });
        help
    }

    fn general(&mut self, ui: &mut egui::Ui) {
        group(ui, "Serveur", |ui| {
            ui.horizontal(|ui| {
                let name = if self.settings.name.trim().is_empty() {
                    "?"
                } else {
                    self.settings.name.trim()
                };
                avatar(ui, name, 56.0);
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Nom du serveur").color(TEXT));
                    ui.add(text_field(&mut self.settings.name, "Nom du serveur").char_limit(48));
                    hint(ui, "Visible par tous les membres.");
                });
            });
        });
    }

    fn channels(&mut self, ui: &mut egui::Ui) {
        for voice in [false, true] {
            let (title, hint_text, kind) = if voice {
                ("Salons vocaux", "Nouveau salon vocal", Icon::Speaker)
            } else {
                ("Salons textuels", "Nouveau salon texte", Icon::Hash)
            };
            group(ui, title, |ui| {
                let list = if voice {
                    &mut self.settings.voice_channels
                } else {
                    &mut self.settings.text_channels
                };
                let mut remove = None;
                let last_text = !voice && list.len() == 1;
                for c in list.iter_mut() {
                    ui.horizontal(|ui| {
                        icon_in(ui, kind, 18.0, 34.0, MUTED);
                        let w = ui.available_width() - 44.0;
                        ui.add_sized([w, 34.0], text_field(&mut c.name, "Nom du salon").char_limit(32));
                        let del = ui
                            .add_enabled(!last_text, Button::new(Icon::Trash).kind(Look::Danger))
                            .on_hover_text("Supprimer ce salon")
                            .on_disabled_hover_text("Il faut au moins un salon texte.");
                        if del.clicked() {
                            remove = Some(c.id);
                        }
                    });
                }
                if let Some(id) = remove {
                    self.settings.remove_channel(id);
                }
                divider(ui);
                let new = if voice { &mut self.new_voice } else { &mut self.new_text };
                ui.horizontal(|ui| {
                    icon_in(ui, Icon::Plus, 18.0, 34.0, MUTED);
                    let w = ui.available_width() - 110.0;
                    let field = ui.add_sized([w, 34.0], text_field(new, hint_text).char_limit(32));
                    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let add = ui.add_enabled(
                        !new.trim().is_empty(),
                        Button::text("Ajouter").height(34.0).width(100.0),
                    );
                    if (add.clicked() || enter) && !new.trim().is_empty() {
                        let name = new.trim().to_string();
                        if voice {
                            self.settings.add_voice_channel(&name);
                        } else {
                            self.settings.add_text_channel(&name);
                        }
                        new.clear();
                    }
                });
            });
        }
    }

    fn history(&mut self, ui: &mut egui::Ui) {
        let h = &mut self.settings.history;
        group(ui, "Historique des messages", |ui| {
            let desc = if h.keep {
                "Chaque membre garde une copie : ceux qui arrivent plus tard voient ce qui a été dit, et \
                 n'importe quel membre peut ré-héberger avec l'historique."
            } else {
                "Rien n'est écrit sur disque : les messages disparaissent à la fermeture."
            };
            switch_row(ui, "Garder l'historique", desc, &mut h.keep);
            divider(ui);
            if h.keep {
                row(
                    ui,
                    "Messages gardés par salon",
                    "0 = sans limite. Au-delà, les plus anciens sont effacés.",
                    |ui| {
                        ui.add(egui::DragValue::new(&mut h.max_per_channel).range(0..=1_000_000));
                    },
                );
            } else {
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "En enregistrant, les copies déjà gardées sont effacées chez tous les membres \
                             connectés (et chez les autres à leur prochaine connexion).",
                        )
                        .color(WARN)
                        .small(),
                    )
                    .wrap(),
                );
            }
        });
    }

    fn members(&mut self, ui: &mut egui::Ui, view: &ServerView, owner: bool, can_edit: bool) {
        let mut people: BTreeMap<UserId, String> = view
            .peers
            .values()
            .map(|p| (p.user, view.display_name(&p.user, &p.name)))
            .collect();
        for a in &self.admins {
            people
                .entry(*a)
                .or_insert_with(|| format!("hors ligne ({})", a.short()));
        }
        for b in &self.banned {
            people
                .entry(*b)
                .or_insert_with(|| format!("hors ligne ({})", b.short()));
        }
        people
            .entry(view.card.owner)
            .or_insert_with(|| "créateur (hors ligne)".into());
        let title = format!("Membres — {}", people.len());
        group(ui, &title, |ui| {
            for (n, (user, name)) in people.into_iter().enumerate() {
                if n > 0 {
                    divider(ui);
                }
                ui.horizontal(|ui| {
                    avatar(ui, &name, 32.0);
                    ui.add(egui::Label::new(RichText::new(&name).color(TEXT)).truncate());
                    if user == view.me {
                        ui.label(RichText::new("toi").color(MUTED).small());
                    }
                    let admin_now = self.admins.contains(&user);
                    if user == view.card.owner {
                        badge(ui, ACCENT_HOVER, "créateur");
                    } else if admin_now && !owner {
                        badge(ui, ACCENT_HOVER, "admin");
                    }
                    if self.banned.contains(&user) {
                        badge(ui, DANGER, "banni");
                    }
                    at_right(ui, |ui| {
                        // Bannir : un simple membre, et jamais soi-même.
                        if can_edit && user != view.me && user != view.card.owner && !admin_now {
                            let banned = self.banned.contains(&user);
                            let (label, kind, tip) = if banned {
                                ("Débannir", Look::Danger, "Il pourra de nouveau entrer avec le code")
                            } else {
                                ("Bannir", Look::Danger, "Il ne pourra plus entrer, même avec le code")
                            };
                            if ui
                                .add(Button::new(Icon::Ban).label(label).kind(kind).height(30.0))
                                .on_hover_text(tip)
                                .clicked()
                            {
                                if banned {
                                    self.banned.retain(|b| *b != user);
                                } else {
                                    self.banned.push(user);
                                }
                            }
                        }
                        if owner && user != view.card.owner && !self.banned.contains(&user) {
                            let tip = if admin_now {
                                "Lui retirer le rôle d'admin"
                            } else {
                                "En faire un admin : nom, salons, historique et bannis"
                            };
                            if ui
                                .add(
                                    Button::new(Icon::Shield)
                                        .label("Admin")
                                        .kind(Look::Toggle(admin_now))
                                        .height(30.0),
                                )
                                .on_hover_text(tip)
                                .clicked()
                            {
                                if admin_now {
                                    self.admins.retain(|a| *a != user);
                                } else {
                                    self.admins.push(user);
                                }
                            }
                        }
                    });
                });
            }
        });
        if owner {
            hint(
                ui,
                "Les admins peuvent modifier le nom, les salons et l'historique, et bannir. Toi seul nommes les admins.",
            );
        } else {
            hint(
                ui,
                "Le créateur nomme les admins, qui gèrent les salons, l'historique et les bannis.",
            );
        }
        if can_edit {
            hint(
                ui,
                "Pour seulement déconnecter quelqu'un : clic droit sur lui dans la liste des membres, « Expulser ».",
            );
        }
    }
}
