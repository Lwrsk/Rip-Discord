//! Icônes vectorielles et les widgets qui les portent (boutons, onglets,
//! interrupteurs, lignes de réglage).
//!
//! Les icônes sont dessinées au pinceau sur une grille de 24 × 24 (à la
//! manière des jeux d'icônes « au trait ») : nettes à toutes les tailles,
//! sans police ni image à embarquer, et de la couleur du texte qui les
//! accompagne.

use std::f32::consts::{FRAC_PI_2, PI, TAU};

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Painter, Pos2, Rect, Response, RichText, Sense, Shape, Stroke,
    StrokeKind, Ui, Vec2, Widget, pos2, vec2,
};

use super::style::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Mic,
    Headphones,
    Gear,
    User,
    Users,
    UserPlus,
    Sliders,
    Keyboard,
    Folder,
    Hash,
    Speaker,
    Clock,
    Close,
    Help,
    Screen,
    HangUp,
    Plus,
    Copy,
    Eye,
    EyeOff,
    Trash,
    Refresh,
    Leave,
    Shield,
    Check,
    Ban,
    More,
    ArrowRight,
    Key,
    Info,
    Waves,
    Reply,
    Pencil,
    Smile,
    Video,
    Server,
}

/// Trace l'icône dans `rect` (carré conseillé).
pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let k = rect.width().min(rect.height()) / 24.0;
    let o = rect.center() - vec2(12.0, 12.0) * k;
    let at = |x: f32, y: f32| o + vec2(x, y) * k;
    let s = Stroke::new((1.8 * k).max(1.1), color);
    let line = |pts: &[(f32, f32)]| {
        let pts: Vec<Pos2> = pts.iter().map(|&(x, y)| at(x, y)).collect();
        p.add(Shape::line(pts, s));
    };
    let closed = |pts: &[(f32, f32)]| {
        let pts: Vec<Pos2> = pts.iter().map(|&(x, y)| at(x, y)).collect();
        p.add(Shape::closed_line(pts, s));
    };
    let circle = |x: f32, y: f32, r: f32| {
        p.circle_stroke(at(x, y), r * k, s);
    };
    let dot = |x: f32, y: f32, r: f32| {
        p.circle_filled(at(x, y), r * k, color);
    };
    // Arc de cercle, angles en radians (0 = à droite, sens horaire à l'écran).
    let arc_pts = |x: f32, y: f32, r: f32, a0: f32, a1: f32| -> Vec<Pos2> {
        let n = ((a1 - a0).abs() * r * k / 2.0).clamp(6.0, 48.0) as usize;
        (0..=n)
            .map(|i| {
                let a = a0 + (a1 - a0) * i as f32 / n as f32;
                at(x + r * a.cos(), y + r * a.sin())
            })
            .collect()
    };
    let arc = |x: f32, y: f32, r: f32, a0: f32, a1: f32| {
        p.add(Shape::line(arc_pts(x, y, r, a0, a1), s));
    };
    let rrect = |x0: f32, y0: f32, x1: f32, y1: f32, r: f32| {
        p.rect_stroke(Rect::from_min_max(at(x0, y0), at(x1, y1)), r * k, s, StrokeKind::Middle);
    };
    // Tête et épaules d'une personne.
    let person = |x: f32, head: f32, r: f32| {
        circle(x, head, r);
        arc(x, head + r * 3.1, r * 1.75, PI, TAU);
    };
    match icon {
        Icon::Mic => {
            rrect(9.0, 2.5, 15.0, 13.5, 3.0);
            arc(12.0, 10.5, 6.5, 0.0, PI);
            line(&[(12.0, 17.0), (12.0, 21.0)]);
            line(&[(8.5, 21.0), (15.5, 21.0)]);
        }
        Icon::Headphones => {
            arc(12.0, 13.0, 8.0, PI, TAU);
            line(&[(4.0, 13.0), (4.0, 18.0)]);
            line(&[(20.0, 13.0), (20.0, 18.0)]);
            rrect(4.0, 13.5, 8.0, 20.0, 2.0);
            rrect(16.0, 13.5, 20.0, 20.0, 2.0);
        }
        Icon::Gear => {
            let mut pts = Vec::with_capacity(32);
            for t in 0..8 {
                let a = t as f32 * TAU / 8.0;
                for (da, r) in [(-0.33, 7.0), (-0.17, 9.6), (0.17, 9.6), (0.33, 7.0)] {
                    pts.push(at(12.0 + r * (a + da).cos(), 12.0 + r * (a + da).sin()));
                }
            }
            p.add(Shape::closed_line(pts, s));
            circle(12.0, 12.0, 3.0);
        }
        Icon::User => person(12.0, 8.0, 4.0),
        Icon::Users => {
            person(9.0, 8.0, 3.6);
            arc(16.5, 8.5, 3.0, -FRAC_PI_2 - 0.6, FRAC_PI_2 - 0.4);
            arc(15.5, 19.5, 5.5, -FRAC_PI_2 + 0.2, 0.0);
        }
        Icon::UserPlus => {
            person(9.5, 8.0, 3.8);
            line(&[(19.0, 5.0), (19.0, 11.0)]);
            line(&[(16.0, 8.0), (22.0, 8.0)]);
        }
        Icon::Sliders => {
            for (y, x) in [(6.0, 9.0), (12.0, 15.0), (18.0, 7.0)] {
                line(&[(3.5, y), (x - 2.4, y)]);
                line(&[(x + 2.4, y), (20.5, y)]);
                circle(x, y, 2.4);
            }
        }
        Icon::Keyboard => {
            rrect(2.5, 6.0, 21.5, 18.0, 2.0);
            for x in [6.5, 10.0, 13.5, 17.0] {
                dot(x, 10.0, 0.9);
            }
            line(&[(8.0, 14.3), (16.0, 14.3)]);
        }
        Icon::Folder => closed(&[
            (3.0, 5.5),
            (9.5, 5.5),
            (11.5, 8.0),
            (21.0, 8.0),
            (21.0, 19.0),
            (3.0, 19.0),
        ]),
        Icon::Hash => {
            line(&[(4.5, 9.0), (20.0, 9.0)]);
            line(&[(4.0, 15.0), (19.5, 15.0)]);
            line(&[(10.5, 3.5), (8.5, 20.5)]);
            line(&[(15.5, 3.5), (13.5, 20.5)]);
        }
        Icon::Speaker => {
            closed(&[
                (11.0, 5.0),
                (6.0, 9.0),
                (2.5, 9.0),
                (2.5, 15.0),
                (6.0, 15.0),
                (11.0, 19.0),
            ]);
            arc(12.0, 12.0, 4.5, -0.8, 0.8);
            arc(12.0, 12.0, 8.5, -0.8, 0.8);
        }
        Icon::Clock => {
            circle(12.0, 12.0, 9.0);
            line(&[(12.0, 7.0), (12.0, 12.0), (15.5, 14.0)]);
        }
        Icon::Close => {
            line(&[(6.0, 6.0), (18.0, 18.0)]);
            line(&[(18.0, 6.0), (6.0, 18.0)]);
        }
        Icon::Help => {
            circle(12.0, 12.0, 9.5);
            arc(12.0, 9.6, 2.8, PI, 2.5 * PI);
            line(&[(12.0, 12.4), (12.0, 13.8)]);
            dot(12.0, 17.0, 1.1);
        }
        Icon::Video => {
            rrect(2.5, 6.5, 15.5, 17.5, 2.5);
            closed(&[(15.5, 10.5), (21.5, 7.0), (21.5, 17.0), (15.5, 13.5)]);
        }
        Icon::Screen => {
            rrect(2.5, 3.5, 21.5, 16.0, 2.0);
            line(&[(12.0, 16.0), (12.0, 20.5)]);
            line(&[(8.0, 20.5), (16.0, 20.5)]);
        }
        Icon::HangUp => {
            // Combiné couché : l'arc et ses deux écouteurs.
            let w = Stroke::new((2.6 * k).max(1.6), color);
            p.add(Shape::line(arc_pts(12.0, 18.0, 10.0, PI + 0.5, TAU - 0.5), w));
            for x in [3.8, 20.2] {
                p.rect_filled(Rect::from_center_size(at(x, 14.4), vec2(5.0, 4.0) * k), 1.6 * k, color);
            }
        }
        Icon::Plus => {
            line(&[(12.0, 5.0), (12.0, 19.0)]);
            line(&[(5.0, 12.0), (19.0, 12.0)]);
        }
        Icon::Copy => {
            rrect(8.5, 8.5, 20.5, 20.5, 2.0);
            line(&[(4.0, 15.5), (3.5, 15.5), (3.5, 3.5), (15.5, 3.5), (15.5, 4.0)]);
        }
        Icon::Eye | Icon::EyeOff => {
            let n = 16;
            let mut pts = Vec::with_capacity(2 * n + 2);
            for i in 0..=n {
                let t = i as f32 / n as f32;
                pts.push(at(2.0 + 20.0 * t, 12.0 - 7.0 * (PI * t).sin()));
            }
            for i in 1..n {
                let t = 1.0 - i as f32 / n as f32;
                pts.push(at(2.0 + 20.0 * t, 12.0 + 7.0 * (PI * t).sin()));
            }
            p.add(Shape::closed_line(pts, s));
            circle(12.0, 12.0, 3.0);
            if icon == Icon::EyeOff {
                line(&[(4.0, 3.5), (20.0, 20.5)]);
            }
        }
        Icon::Trash => {
            line(&[(3.5, 6.0), (20.5, 6.0)]);
            line(&[(9.0, 6.0), (9.0, 3.5), (15.0, 3.5), (15.0, 6.0)]);
            line(&[(5.5, 6.0), (6.5, 20.5), (17.5, 20.5), (18.5, 6.0)]);
            line(&[(10.0, 10.5), (10.0, 16.5)]);
            line(&[(14.0, 10.5), (14.0, 16.5)]);
        }
        Icon::Refresh => {
            let (a0, a1) = (-FRAC_PI_2 + 0.5, 1.5 * PI - 0.25);
            arc(12.0, 12.0, 8.0, a0, a1);
            let end = vec2(a1.cos(), a1.sin());
            let tip = (12.0 + 8.0 * end.x, 12.0 + 8.0 * end.y);
            let t = vec2(-end.y, end.x);
            let wing = |sign: f32| {
                let v = -t * 3.8 + end * 3.8 * sign;
                (tip.0 + v.x, tip.1 + v.y)
            };
            line(&[wing(1.0), tip, wing(-1.0)]);
        }
        Icon::Leave => {
            line(&[(9.5, 3.5), (4.5, 3.5), (4.5, 20.5), (9.5, 20.5)]);
            line(&[(10.0, 12.0), (20.5, 12.0)]);
            line(&[(16.0, 7.5), (20.5, 12.0), (16.0, 16.5)]);
        }
        Icon::Shield => closed(&[
            (12.0, 2.5),
            (20.0, 5.5),
            (20.0, 11.5),
            (17.0, 17.5),
            (12.0, 21.0),
            (7.0, 17.5),
            (4.0, 11.5),
            (4.0, 5.5),
        ]),
        Icon::Check => line(&[(5.0, 12.5), (10.0, 17.5), (19.5, 7.0)]),
        Icon::Ban => {
            circle(12.0, 12.0, 9.0);
            line(&[(5.6, 5.6), (18.4, 18.4)]);
        }
        Icon::More => {
            for x in [5.5, 12.0, 18.5] {
                dot(x, 12.0, 1.7);
            }
        }
        Icon::ArrowRight => {
            line(&[(4.5, 12.0), (19.0, 12.0)]);
            line(&[(13.0, 6.0), (19.0, 12.0), (13.0, 18.0)]);
        }
        Icon::Key => {
            circle(7.5, 16.5, 4.5);
            line(&[(10.7, 13.3), (20.5, 3.5)]);
            line(&[(17.0, 7.0), (20.0, 10.0)]);
            line(&[(14.5, 9.5), (16.5, 11.5)]);
        }
        Icon::Info => {
            circle(12.0, 12.0, 9.5);
            line(&[(12.0, 11.0), (12.0, 16.5)]);
            dot(12.0, 7.7, 1.1);
        }
        Icon::Waves => {
            for (x, h) in [(4.0, 3.0), (8.0, 8.0), (12.0, 14.0), (16.0, 8.0), (20.0, 3.0)] {
                line(&[(x, 12.0 - h / 2.0), (x, 12.0 + h / 2.0)]);
            }
        }
        Icon::Reply => {
            line(&[(9.0, 6.5), (4.0, 11.5), (9.0, 16.5)]);
            line(&[(4.0, 11.5), (14.5, 11.5)]);
            arc(14.5, 17.0, 5.5, -FRAC_PI_2, 0.0);
            line(&[(20.0, 17.0), (20.0, 19.5)]);
        }
        Icon::Pencil => {
            closed(&[(16.0, 3.5), (20.5, 8.0), (8.0, 20.5), (3.5, 20.5), (3.5, 16.0)]);
            line(&[(13.0, 6.5), (17.5, 11.0)]);
        }
        Icon::Smile => {
            circle(12.0, 12.0, 9.5);
            arc(12.0, 12.0, 5.5, 0.5, PI - 0.5);
            dot(9.0, 9.5, 1.1);
            dot(15.0, 9.5, 1.1);
        }
        Icon::Server => {
            rrect(3.0, 3.5, 21.0, 10.5, 2.0);
            rrect(3.0, 13.5, 21.0, 20.5, 2.0);
            dot(7.0, 7.0, 1.0);
            dot(7.0, 17.0, 1.0);
        }
    }
}

/// Trait rouge en travers (micro coupé, son coupé).
pub fn slash(p: &Painter, rect: Rect, color: Color32) {
    let k = rect.width() / 24.0;
    let o = rect.center() - vec2(12.0, 12.0) * k;
    // Liseré de la couleur du fond pour détacher le trait de l'icône.
    let a = o + vec2(3.0, 3.0) * k;
    let b = o + vec2(21.0, 21.0) * k;
    p.line_segment([a, b], Stroke::new(4.0 * k, PANEL));
    p.line_segment([a, b], Stroke::new(2.0 * k, color));
}

/// Icône seule, sans interaction.
pub fn icon(ui: &mut Ui, icon: Icon, size: f32, color: Color32) -> Response {
    icon_in(ui, icon, size, size, color)
}

/// Icône centrée dans une case plus haute qu'elle (à côté d'un champ).
pub fn icon_in(ui: &mut Ui, icon: Icon, size: f32, height: f32, color: Color32) -> Response {
    let (rect, r) = ui.allocate_exact_size(vec2(size, height), Sense::hover());
    paint(
        ui.painter(),
        Rect::from_center_size(rect.center(), Vec2::splat(size)),
        icon,
        color,
    );
    r
}

#[derive(Clone, Copy, PartialEq)]
pub enum Look {
    /// Action principale : violet plein.
    Primary,
    /// Bouton normal : fond de surface et bordure.
    Secondary,
    /// Sans fond tant qu'on ne le survole pas.
    Ghost,
    /// Action risquée : rouge au survol.
    Danger,
    /// Rouge plein (raccrocher).
    DangerSolid,
    /// Interrupteur : violet quand c'est actif.
    Toggle(bool),
}

/// Bouton avec icône, texte, ou les deux. Ses couleurs suivent le survol et
/// le clic, et il se grise quand il est désactivé.
pub struct Button<'a> {
    icon: Option<Icon>,
    label: Option<&'a str>,
    kind: Look,
    height: f32,
    width: Option<f32>,
    slashed: bool,
    color: Option<Color32>,
}

impl<'a> Button<'a> {
    pub fn new(icon: Icon) -> Self {
        Self {
            icon: Some(icon),
            label: None,
            kind: Look::Ghost,
            height: 34.0,
            width: None,
            slashed: false,
            color: None,
        }
    }

    pub fn text(label: &'a str) -> Self {
        Self {
            icon: None,
            label: Some(label),
            kind: Look::Secondary,
            ..Self::new(Icon::Plus)
        }
    }

    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        if self.kind == Look::Ghost {
            self.kind = Look::Secondary;
        }
        self
    }

    pub fn kind(mut self, kind: Look) -> Self {
        self.kind = kind;
        self
    }

    pub fn height(mut self, h: f32) -> Self {
        self.height = h;
        self
    }

    /// Largeur minimale (contenu centré).
    pub fn width(mut self, w: f32) -> Self {
        self.width = Some(w);
        self
    }

    /// Barré en rouge (micro ou son coupé).
    pub fn slashed(mut self, on: bool) -> Self {
        self.slashed = on;
        self
    }

    /// Couleur de l'icône et du texte (sinon celle du genre de bouton).
    pub fn color(mut self, c: Color32) -> Self {
        self.color = Some(c);
        self
    }
}

impl Widget for Button<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let font = FontId::proportional(if self.height >= 38.0 { 15.0 } else { 14.0 });
        let icon_size = (self.height * 0.5).clamp(14.0, 20.0);
        let galley = self
            .label
            .map(|l| ui.painter().layout_no_wrap(l.to_owned(), font, Color32::PLACEHOLDER));
        let pad = if galley.is_some() { 14.0 } else { 0.0 };
        let gap = if galley.is_some() && self.icon.is_some() {
            8.0
        } else {
            0.0
        };
        let content = self.icon.map_or(0.0, |_| icon_size) + gap + galley.as_ref().map_or(0.0, |g| g.size().x);
        let w = (content + 2.0 * pad).max(self.height).max(self.width.unwrap_or(0.0));
        let (rect, r) = ui.allocate_exact_size(vec2(w, self.height), Sense::click());
        if !ui.is_rect_visible(rect) {
            return r;
        }
        let enabled = ui.is_enabled();
        let hot = enabled && r.hovered();
        let down = enabled && r.is_pointer_button_down_on();
        let (fill, stroke, fg) = colors(self.kind, hot, down);
        let fg = self.color.unwrap_or(fg);
        let fade = |c: Color32| if enabled { c } else { c.gamma_multiply(0.45) };
        let p = ui.painter();
        let radius = CornerRadius::same(if self.height >= 38.0 { 12 } else { 10 });
        p.rect(
            rect,
            radius,
            fade(fill),
            Stroke::new(1.0, fade(stroke)),
            StrokeKind::Inside,
        );
        let mut x = rect.center().x - content / 2.0;
        if let Some(i) = self.icon {
            let ir = Rect::from_center_size(pos2(x + icon_size / 2.0, rect.center().y), Vec2::splat(icon_size));
            paint(p, ir, i, fade(fg));
            if self.slashed {
                slash(p, ir, DANGER);
            }
            x += icon_size + gap;
        }
        if let Some(g) = galley {
            p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, fade(fg));
        }
        if enabled {
            r.on_hover_cursor(egui::CursorIcon::PointingHand)
        } else {
            r
        }
    }
}

fn colors(kind: Look, hot: bool, down: bool) -> (Color32, Color32, Color32) {
    let pick = |rest: Color32, hover: Color32, press: Color32| {
        if down {
            press
        } else if hot {
            hover
        } else {
            rest
        }
    };
    match kind {
        Look::Primary => (
            pick(ACCENT, ACCENT_HOVER, ACCENT_HOVER.gamma_multiply(0.9)),
            Color32::TRANSPARENT,
            Color32::WHITE,
        ),
        Look::Secondary | Look::Toggle(false) => (
            pick(SURFACE, HOVER, PRESSED),
            pick(BORDER, ACCENT.gamma_multiply(0.6), ACCENT),
            if hot { TEXT } else { TEXT.gamma_multiply(0.92) },
        ),
        Look::Ghost => (
            pick(Color32::TRANSPARENT, HOVER, PRESSED),
            Color32::TRANSPARENT,
            if hot { TEXT } else { MUTED },
        ),
        Look::Danger => (
            pick(
                Color32::TRANSPARENT,
                DANGER.gamma_multiply(0.16),
                DANGER.gamma_multiply(0.26),
            ),
            pick(Color32::TRANSPARENT, DANGER.gamma_multiply(0.5), DANGER),
            if hot { DANGER } else { MUTED },
        ),
        Look::DangerSolid => (
            pick(
                Color32::from_rgb(0xd9, 0x3b, 0x55),
                Color32::from_rgb(0xe8, 0x4d, 0x67),
                Color32::from_rgb(0xc4, 0x32, 0x4b),
            ),
            Color32::TRANSPARENT,
            Color32::WHITE,
        ),
        Look::Toggle(true) => (
            pick(
                ACCENT.gamma_multiply(0.24),
                ACCENT.gamma_multiply(0.34),
                ACCENT.gamma_multiply(0.42),
            ),
            pick(ACCENT.gamma_multiply(0.7), ACCENT_HOVER, ACCENT_HOVER),
            Color32::WHITE,
        ),
    }
}

/// Ligne de navigation pleine largeur : icône, texte, badge à droite
/// (salons, onglets des réglages).
pub fn nav_item(ui: &mut Ui, icon: Icon, label: &str, selected: bool, strong: bool, badge: &str) -> Response {
    let h = 34.0;
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if !ui.is_rect_visible(rect) {
        return r;
    }
    let hot = r.hovered();
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 9, ACCENT.gamma_multiply(0.2));
        p.rect_filled(
            Rect::from_min_size(rect.min + vec2(0.0, 8.0), vec2(3.0, h - 16.0)),
            2,
            ACCENT_HOVER,
        );
    } else if hot {
        p.rect_filled(rect, 9, HOVER.gamma_multiply(0.85));
    }
    let fg = if selected || strong || hot { TEXT } else { MUTED };
    let ir = Rect::from_center_size(pos2(rect.left() + 20.0, rect.center().y), Vec2::splat(17.0));
    paint(
        p,
        ir,
        icon,
        if selected {
            ACCENT_HOVER
        } else {
            fg.gamma_multiply(0.85)
        },
    );
    let mut right = rect.right() - 10.0;
    if !badge.is_empty() {
        let bg = ui
            .painter()
            .layout_no_wrap(badge.to_owned(), FontId::proportional(11.5), Color32::WHITE);
        let size = vec2((bg.size().x + 12.0).max(18.0), 18.0);
        let br = Rect::from_min_size(pos2(right - size.x, rect.center().y - 9.0), size);
        let dot_only = badge == "●";
        if dot_only {
            p.circle_filled(pos2(right - 5.0, rect.center().y), 4.0, TEXT);
            right -= 14.0;
        } else {
            p.rect_filled(br, 9, DANGER);
            p.galley(br.center() - bg.size() / 2.0, bg, Color32::WHITE);
            right = br.left() - 6.0;
        }
    }
    let font = FontId::proportional(14.5);
    let max = (right - (rect.left() + 36.0)).max(10.0);
    let mut job = egui::text::LayoutJob::simple_singleline(label.to_owned(), font, fg);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max);
    let g = ui.painter().layout_job(job);
    p.galley(pos2(rect.left() + 36.0, rect.center().y - g.size().y / 2.0), g, fg);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Interrupteur marche/arrêt, plus lisible qu'une case à cocher.
pub fn switch(ui: &mut Ui, on: &mut bool) -> Response {
    let size = vec2(40.0, 22.0);
    let (rect, mut r) = ui.allocate_exact_size(size, Sense::click());
    if r.clicked() {
        *on = !*on;
        r.mark_changed();
    }
    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, ui.is_enabled(), *on, ""));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(r.id, *on);
        let enabled = ui.is_enabled();
        let fade = |c: Color32| if enabled { c } else { c.gamma_multiply(0.45) };
        let off_fill = if r.hovered() { HOVER } else { BORDER };
        let fill = off_fill.lerp_to_gamma(ACCENT, t);
        let p = ui.painter();
        p.rect_filled(rect, 11, fade(fill));
        let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
        p.circle_filled(pos2(x, rect.center().y), 8.0, fade(Color32::WHITE));
        if enabled {
            r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
    }
    r
}

/// Titre d'un groupe de réglages, au-dessus de sa carte.
pub fn group(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    if !title.is_empty() {
        ui.label(RichText::new(title.to_uppercase()).color(MUTED).size(11.5).strong());
        ui.add_space(2.0);
    }
    egui::Frame::new()
        .fill(SURFACE.gamma_multiply(0.75))
        .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.75)))
        .corner_radius(12)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            add(ui);
        });
    ui.add_space(14.0);
}

/// Fin trait entre deux lignes d'un groupe.
pub fn divider(ui: &mut Ui) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0, BORDER.gamma_multiply(0.6));
    ui.add_space(4.0);
}

/// Ligne de réglage : titre et explication à gauche, commande à droite.
pub fn row(ui: &mut Ui, title: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    row_with(ui, None, title, desc, control);
}

/// Pareil, précédé d'une icône. Le texte prend toute la place que la
/// commande laisse (sa largeur est retenue d'une image à l'autre).
pub fn row_with(ui: &mut Ui, icon_: Option<Icon>, title: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    let id = ui.scope_id().with(("ligne", title));
    let control_w: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(160.0);
    ui.horizontal(|ui| {
        let text_w = (ui.available_width() - control_w - 18.0).max(140.0);
        ui.allocate_ui_with_layout(vec2(text_w, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(text_w);
            ui.spacing_mut().item_spacing.y = 2.0;
            let indent = if let Some(i) = icon_ {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    icon(ui, i, 18.0, MUTED);
                    ui.label(RichText::new(title).color(TEXT));
                });
                26.0
            } else {
                ui.label(RichText::new(title).color(TEXT));
                0.0
            };
            if !desc.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(indent);
                    ui.add(egui::Label::new(RichText::new(desc).color(MUTED).size(12.5)).wrap());
                });
            }
        });
        if ui.is_sizing_pass() || !ui.available_width().is_finite() {
            control(ui);
        } else {
            let r = ui.with_layout(Layout::right_to_left(Align::Center), control);
            let used = r.response.rect.width();
            if (used - control_w).abs() > 0.5 {
                ui.data_mut(|d| d.insert_temp(id, used));
                ui.ctx().request_discard("largeur de commande");
            }
        }
    });
}

/// Pose un contenu de gauche à droite, sur `width`, dans une mise en page
/// de droite à gauche (curseur suivi de sa valeur, par exemple).
pub fn ltr(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(vec2(width, 30.0), Layout::left_to_right(Align::Center), add);
}

/// Ligne avec un interrupteur. Renvoie la réponse de l'interrupteur.
pub fn switch_row(ui: &mut Ui, title: &str, desc: &str, on: &mut bool) -> Response {
    let mut out = None;
    row(ui, title, desc, |ui| out = Some(switch(ui, on)));
    out.unwrap_or_else(|| ui.allocate_response(Vec2::ZERO, Sense::hover()))
}

/// Étiquette colorée (rôle, état).
pub fn badge(ui: &mut Ui, color: Color32, text: &str) -> Response {
    let g = ui
        .painter()
        .layout_no_wrap(text.to_owned(), FontId::proportional(11.5), color);
    let (rect, r) = ui.allocate_exact_size(vec2(g.size().x + 14.0, 20.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 10, color.gamma_multiply(0.16));
    p.galley(rect.center() - g.size() / 2.0, g, color);
    r
}

/// Fenêtre de réglages : menu à gauche, page à droite, pied facultatif.
/// Renvoie vrai si on demande à la fermer (croix, Échap, clic à côté).
pub struct Shell<'a, T> {
    pub id: &'a str,
    /// Au-dessus du menu (« Mes réglages », nom du serveur).
    pub heading: &'a str,
    pub sub: &'a str,
    pub pages: &'a [(Icon, &'a str, T)],
}

impl<T: Copy + PartialEq> Shell<'_, T> {
    pub fn show(
        self,
        ctx: &egui::Context,
        current: &mut T,
        nav_extra: impl FnOnce(&mut Ui),
        body: impl FnOnce(&mut Ui, T),
        footer: Option<&mut dyn FnMut(&mut Ui)>,
    ) -> bool {
        let screen = ctx.content_rect().size();
        let size = vec2(
            (screen.x - 48.0).clamp(560.0, 860.0),
            (screen.y - 48.0).clamp(380.0, 600.0),
        );
        let nav_w = 210.0;
        let mut close = false;
        let frame = egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(16)
            .shadow(egui::epaint::Shadow {
                offset: [0, 12],
                blur: 40,
                spread: 0,
                color: Color32::from_black_alpha(140),
            });
        let r = egui::Modal::new(egui::Id::unique(self.id))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_min_size(size);
                ui.set_max_size(size);
                let full = Rect::from_min_size(ui.cursor().min, size);
                let nav = Rect::from_min_size(full.min, vec2(nav_w, size.y));
                ui.painter().rect_filled(
                    nav,
                    CornerRadius {
                        nw: 16,
                        sw: 16,
                        ne: 0,
                        se: 0,
                    },
                    PANEL,
                );
                ui.painter()
                    .vline(nav.right(), nav.y_range(), Stroke::new(1.0, BORDER.gamma_multiply(0.7)));
                // Menu.
                let mut nav_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(nav.shrink2(vec2(12.0, 18.0)))
                        .layout(Layout::top_down(Align::Min)),
                );
                nav_ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.add(
                            egui::Label::new(RichText::new(self.heading).size(16.0).strong().color(TEXT)).truncate(),
                        );
                        if !self.sub.is_empty() {
                            ui.label(RichText::new(self.sub).color(MUTED).size(12.0));
                        }
                    });
                });
                nav_ui.add_space(14.0);
                nav_ui.spacing_mut().item_spacing.y = 3.0;
                for (i, label, page) in self.pages {
                    if nav_item(&mut nav_ui, *i, label, *current == *page, false, "").clicked() {
                        *current = *page;
                    }
                }
                nav_ui.with_layout(Layout::bottom_up(Align::Min), nav_extra);

                // Page.
                let page = Rect::from_min_max(pos2(nav.right() + 1.0, full.top()), full.max);
                let mut page_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(page.shrink2(vec2(26.0, 0.0)))
                        .layout(Layout::top_down(Align::Min)),
                );
                let title = self
                    .pages
                    .iter()
                    .find(|(_, _, p)| *p == *current)
                    .map_or("", |(_, l, _)| *l);
                page_ui.add_space(16.0);
                page_ui.allocate_ui_with_layout(
                    vec2(page_ui.available_width(), 34.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.label(RichText::new(title).size(20.0).strong().color(TEXT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add(Button::new(Icon::Close))
                                .on_hover_text("Fermer (Échap)")
                                .clicked()
                            {
                                close = true;
                            }
                        });
                    },
                );
                page_ui.add_space(10.0);
                let footer_h = if footer.is_some() { 62.0 } else { 16.0 };
                let body_h = (page_ui.available_height() - footer_h).max(80.0);
                egui::ScrollArea::vertical()
                    .id_salt((self.id, "page"))
                    .max_height(body_h)
                    .min_scrolled_height(body_h)
                    .auto_shrink([false, false])
                    .show(&mut page_ui, |ui| {
                        ui.set_width(ui.available_width() - 6.0);
                        body(ui, *current);
                    });
                if let Some(f) = footer {
                    page_ui.add_space(6.0);
                    page_ui.allocate_ui_with_layout(
                        vec2(page_ui.available_width(), 44.0),
                        Layout::left_to_right(Align::Center),
                        f,
                    );
                }
                // La place prise est celle de la fenêtre entière.
                ui.allocate_rect(full, Sense::hover());
            });
        close || r.should_close()
    }
}

/// Petite icône d'état barrée (micro ou casque coupé) dans la liste du salon.
pub fn status_icon(ui: &mut Ui, i: Icon) -> Response {
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(15.0), Sense::hover());
    paint(ui.painter(), rect, i, DANGER);
    slash(ui.painter(), rect, DANGER);
    r
}
