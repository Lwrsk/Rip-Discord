//! « Partager » : choix de l'écran, de la fenêtre ou du jeu (avec aperçus),
//! du curseur, du son et de la qualité. Sert aussi à changer de source en plein
//! partage : les spectateurs passent d'une image à l'autre sans coupure.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use eframe::egui::{self, Align2, Color32, FontId, Rect, RichText, Stroke, pos2, vec2};

use super::{icons::*, style::*};
use crate::capture::{self, Options, Quality, Source, SourceInfo};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Screens,
    Windows,
}

pub enum Outcome {
    Open,
    Closed,
    Pick(Source),
    Stop,
}

type Thumbs = Arc<Mutex<HashMap<Source, egui::TextureHandle>>>;

pub struct Picker {
    tab: Tab,
    sources: Vec<SourceInfo>,
    thumbs: Thumbs,
}

impl Picker {
    pub fn new(ctx: &egui::Context, current: Option<Source>) -> Self {
        let mut p = Self {
            tab: if matches!(current, Some(Source::Window(_))) {
                Tab::Windows
            } else {
                Tab::Screens
            },
            sources: Vec::new(),
            thumbs: Thumbs::default(),
        };
        p.refresh(ctx);
        p
    }

    /// Relit les écrans et fenêtres, et refait les aperçus en arrière-plan.
    fn refresh(&mut self, ctx: &egui::Context) {
        self.sources = capture::sources();
        self.thumbs = Thumbs::default();
        let (ctx, weak, list) = (
            ctx.clone(),
            Arc::downgrade(&self.thumbs),
            self.sources.iter().map(|s| s.source).collect::<Vec<_>>(),
        );
        let _ = std::thread::Builder::new()
            .name("apercus".into())
            .spawn(move || thumbnails(ctx, weak, list));
    }

    pub fn show(&mut self, ctx: &egui::Context, options: &mut Options, current: Option<Source>) -> Outcome {
        let mut out = Outcome::Open;
        let screens: Vec<&SourceInfo> = self
            .sources
            .iter()
            .filter(|s| matches!(s.source, Source::Monitor(_)))
            .collect();
        let windows: Vec<&SourceInfo> = self
            .sources
            .iter()
            .filter(|s| matches!(s.source, Source::Window(_)))
            .collect();
        let mut refresh = false;
        let r = egui::Modal::new(egui::Id::unique("partage"))
            .frame(card().inner_margin(24))
            .show(ctx, |ui| {
                ui.set_width(624.0);
                let title = if current.is_some() {
                    "Changer ce que tu partages"
                } else {
                    "Que veux-tu partager ?"
                };
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 12, ACCENT.gamma_multiply(0.22));
                    paint(ui.painter(), rect.shrink(10.0), Icon::Screen, ACCENT_HOVER);
                    ui.add_space(4.0);
                    ui.label(RichText::new(title).size(20.0).strong().color(TEXT));
                    at_right(ui, |ui| {
                        if ui.add(Button::new(Icon::Close)).on_hover_text("Fermer (Échap)").clicked() {
                            out = Outcome::Closed;
                        }
                    });
                });
                if current.is_some() {
                    hint(ui, "Choisis une autre source : tes spectateurs basculent dessus sans coupure.");
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if tab(ui, self.tab == Tab::Screens, &format!("Écrans ({})", screens.len())) {
                        self.tab = Tab::Screens;
                    }
                    if tab(ui, self.tab == Tab::Windows, &format!("Fenêtres et jeux ({})", windows.len())) {
                        self.tab = Tab::Windows;
                    }
                    at_right(ui, |ui| {
                        if ui
                            .add(Button::new(Icon::Refresh).label("Actualiser").kind(Look::Ghost).height(32.0))
                            .on_hover_text("Relire la liste (une fenêtre vient d'être ouverte ?)")
                            .clicked()
                        {
                            refresh = true;
                        }
                    });
                });
                ui.add_space(8.0);
                let list = match self.tab {
                    Tab::Screens => &screens,
                    Tab::Windows => &windows,
                };
                let thumbs = self.thumbs.lock().unwrap().clone();
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    if list.is_empty() {
                        hint(
                            ui,
                            match self.tab {
                                Tab::Screens => "Aucun écran trouvé.",
                                Tab::Windows => {
                                    "Aucune fenêtre à partager. Une fenêtre réduite n'apparaît pas : rouvre-la puis Actualiser."
                                }
                            },
                        );
                    }
                    for row in list.chunks(3) {
                        ui.horizontal(|ui| {
                            for info in row {
                                let on = current == Some(info.source);
                                if tile(ui, info, thumbs.get(&info.source), on) && !on {
                                    out = Outcome::Pick(info.source);
                                }
                            }
                        });
                        ui.add_space(6.0);
                    }
                });
                if self.tab == Tab::Windows {
                    hint(
                        ui,
                        "Un jeu se partage comme une fenêtre : mets-le en « fenêtré » ou « plein écran fenêtré » \
                         s'il reste noir.",
                    );
                }
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    switch(ui, &mut options.cursor);
                    ui.label(RichText::new("Montrer ma souris").color(TEXT));
                    ui.add_space(10.0);
                    ui.add_enabled_ui(capture::sound::SUPPORTED, |ui| {
                        switch(ui, &mut options.sound)
                            .on_hover_text(match self.tab {
                                Tab::Screens => "Le son de tout le PC, sauf les voix du salon",
                                Tab::Windows => "Le son de cette application seulement",
                            })
                            .on_disabled_hover_text("Le son ne se partage que sous Windows");
                        ui.label(RichText::new("Partager le son").color(TEXT));
                    });
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Qualité").color(MUTED));
                    for q in Quality::ALL {
                        if ui
                            .add(Button::text(q.label()).kind(Look::Toggle(options.quality == q)).height(32.0))
                            .on_hover_text(q.hint())
                            .clicked()
                        {
                            options.quality = q;
                        }
                    }
                });
                hint(ui, "Ces réglages s'appliquent tout de suite, même en plein partage.");
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if current.is_some()
                        && ui
                            .add(Button::new(Icon::Close).label("Arrêter le partage").kind(Look::Danger).color(DANGER).height(38.0))
                            .clicked()
                    {
                        out = Outcome::Stop;
                    }
                    at_right(ui, |ui| {
                        if ui.add(Button::text("Fermer").height(38.0).width(120.0)).clicked() {
                            out = Outcome::Closed;
                        }
                    });
                });
            });
        if refresh {
            self.refresh(ctx);
        }
        if matches!(out, Outcome::Open) && r.should_close() {
            out = Outcome::Closed;
        }
        out
    }
}

/// Une source : aperçu, titre, détail. Vrai si on clique dessus.
fn tile(ui: &mut egui::Ui, info: &SourceInfo, thumb: Option<&egui::TextureHandle>, current: bool) -> bool {
    let (rect, r) = ui.allocate_exact_size(vec2(196.0, 154.0), egui::Sense::click());
    let p = ui.painter();
    let border = if current {
        Stroke::new(2.0, ACCENT)
    } else if r.hovered() {
        Stroke::new(1.5, ACCENT_HOVER.gamma_multiply(0.7))
    } else {
        Stroke::new(1.0, BORDER)
    };
    p.rect(rect, 12.0, SURFACE, border, egui::StrokeKind::Inside);
    let frame = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(rect.width() - 16.0, 100.0));
    p.rect_filled(frame, 8.0, FIELD);
    match thumb {
        Some(t) => {
            let size = t.size_vec2();
            let scale = (frame.width() / size.x).min(frame.height() / size.y);
            let shown = Rect::from_center_size(frame.center(), size * scale);
            p.image(
                t.id(),
                shown,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        None => {
            p.text(
                frame.center(),
                Align2::CENTER_CENTER,
                "…",
                FontId::proportional(18.0),
                MUTED,
            );
        }
    }
    if current {
        let badge = Rect::from_min_size(frame.min + vec2(6.0, 6.0), vec2(62.0, 18.0));
        p.rect_filled(badge, 9.0, DANGER);
        p.text(
            badge.center(),
            Align2::CENTER_CENTER,
            "EN COURS",
            FontId::proportional(10.0),
            BG,
        );
    }
    let left = rect.left() + 10.0;
    p.text(
        pos2(left, frame.bottom() + 8.0),
        Align2::LEFT_TOP,
        elide(&info.title, 26),
        FontId::proportional(13.0),
        TEXT,
    );
    p.text(
        pos2(left, frame.bottom() + 26.0),
        Align2::LEFT_TOP,
        elide(&info.detail, 30),
        FontId::proportional(11.0),
        if info.fullscreen { WARN } else { MUTED },
    );
    let mut tip = info.title.clone();
    if !info.detail.is_empty() {
        tip.push_str(&format!("\n{}", info.detail));
    }
    let r = r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip);
    r.clicked()
}

fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

/// Aperçus, un par un ; s'arrête si la fenêtre de choix est fermée.
fn thumbnails(ctx: egui::Context, thumbs: Weak<Mutex<HashMap<Source, egui::TextureHandle>>>, list: Vec<Source>) {
    for (i, source) in list.into_iter().enumerate() {
        if thumbs.strong_count() == 0 {
            return;
        }
        let Some(img) = capture::thumbnail(source) else {
            continue;
        };
        let image =
            egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
        let tex = ctx.load_texture(format!("apercu-{i}"), image, egui::TextureOptions::LINEAR);
        let Some(t) = thumbs.upgrade() else { return };
        t.lock().unwrap().insert(source, tex);
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::elide;

    #[test]
    fn titres_coupes() {
        assert_eq!(elide("Minecraft", 26), "Minecraft");
        assert_eq!(elide("abcdefghij", 5), "abcd…");
    }
}
