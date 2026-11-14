//! Couleurs, petits widgets et texte d'aide.

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Stroke, TextStyle, vec2,
};

use crate::audio;

/// Palette sobre : gris neutres à peine bleutés, une seule couleur d'accent.
/// Les surfaces montent d'un cran à chaque niveau (fond, panneaux, cartes),
/// les bordures sont des filets fins plutôt que des cadres.
pub const BG: Color32 = Color32::from_rgb(0x0d, 0x0e, 0x12);
pub const PANEL: Color32 = Color32::from_rgb(0x12, 0x13, 0x18);
pub const CARD: Color32 = Color32::from_rgb(0x17, 0x18, 0x1f);
pub const SURFACE: Color32 = Color32::from_rgb(0x1d, 0x1e, 0x26);
pub const FIELD: Color32 = Color32::from_rgb(0x0b, 0x0c, 0x10);
pub const BORDER: Color32 = Color32::from_rgb(0x27, 0x29, 0x33);
pub const ACCENT: Color32 = Color32::from_rgb(0x76, 0x63, 0xf6);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x95, 0x86, 0xff);
/// Fond d'un élément survolé, puis enfoncé.
pub const HOVER: Color32 = Color32::from_rgb(0x22, 0x24, 0x2d);
pub const PRESSED: Color32 = Color32::from_rgb(0x2b, 0x2d, 0x38);
pub const TEXT: Color32 = Color32::from_rgb(0xec, 0xed, 0xf2);
pub const MUTED: Color32 = Color32::from_rgb(0x8d, 0x92, 0x9f);
pub const OK: Color32 = Color32::from_rgb(0x3e, 0xd5, 0x98);
pub const WARN: Color32 = Color32::from_rgb(0xf4, 0xb7, 0x50);
pub const DANGER: Color32 = Color32::from_rgb(0xf2, 0x60, 0x76);

/// Voile derrière une fenêtre modale.
pub const BACKDROP: Color32 = Color32::from_rgba_premultiplied(4, 4, 8, 170);

pub const AVATARS: [Color32; 6] = [
    Color32::from_rgb(0x76, 0x63, 0xf6),
    Color32::from_rgb(0x3b, 0x9b, 0xe0),
    Color32::from_rgb(0x2f, 0xb8, 0x8a),
    Color32::from_rgb(0xe0, 0x9a, 0x3e),
    Color32::from_rgb(0xe0, 0x5b, 0x8c),
    Color32::from_rgb(0x2d, 0xb3, 0xc9),
];

/// Graisses de la police (Inter, sous-ensemble latin embarqué : ~70 ko
/// chacune). La police par défaut d'egui n'a ni gras ni demi-gras.
pub const MEDIUM: &str = "medium";
pub const SEMIBOLD: &str = "semibold";

pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MEDIUM.into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

/// `.semi()` / `.medium()` sur un texte : change la graisse sans toucher à
/// la taille.
pub trait Weight {
    fn semi(self) -> Self;
    fn medium(self) -> Self;
}

impl Weight for RichText {
    fn semi(self) -> Self {
        self.family(FontFamily::Name(SEMIBOLD.into())).strong()
    }
    fn medium(self) -> Self {
        self.family(FontFamily::Name(MEDIUM.into()))
    }
}

fn install_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions};
    use std::sync::Arc;
    let mut fonts = FontDefinitions::default();
    let faces: [(&str, &'static [u8]); 3] = [
        ("inter", include_bytes!("../../../assets/fonts/Inter-Regular.otf")),
        ("inter-medium", include_bytes!("../../../assets/fonts/Inter-Medium.otf")),
        (
            "inter-semibold",
            include_bytes!("../../../assets/fonts/Inter-SemiBold.otf"),
        ),
    ];
    for (name, bytes) in faces {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }
    // Les polices d'egui restent derrière : emojis et symboles absents d'Inter.
    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let family = |first: &str| {
        std::iter::once(first.to_owned())
            .chain(fallback.iter().cloned())
            .collect::<Vec<_>>()
    };
    fonts.families.insert(FontFamily::Proportional, family("inter"));
    fonts
        .families
        .insert(FontFamily::Name(MEDIUM.into()), family("inter-medium"));
    fonts
        .families
        .insert(FontFamily::Name(SEMIBOLD.into()), family("inter-semibold"));
    ctx.set_fonts(fonts);
}

/// Ombre portée douce des cartes et fenêtres.
pub fn shadow(depth: u8) -> egui::epaint::Shadow {
    egui::epaint::Shadow {
        offset: [0, depth as i8 / 2],
        blur: depth.saturating_mul(2),
        spread: 0,
        color: Color32::from_black_alpha(90 + depth.min(40) * 2),
    }
}

pub fn apply_theme(ctx: &egui::Context) {
    install_fonts(ctx);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(14);
    v.window_shadow = shadow(24);
    v.popup_shadow = shadow(16);
    v.menu_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = FIELD;
    v.text_edit_bg_color = Some(FIELD);
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.55);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT_HOVER;
    v.faint_bg_color = SURFACE;
    let r = CornerRadius::same(10);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = r;
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER.gamma_multiply(0.8));
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.weak_bg_fill = SURFACE;
    // Rail des curseurs, fond des cases à cocher : visibles sur les cartes.
    v.widgets.inactive.bg_fill = FIELD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    // Survol visible mais calme : fond plus clair, liseré d'accent discret.
    v.widgets.hovered.weak_bg_fill = HOVER;
    v.widgets.hovered.bg_fill = HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.55));
    v.widgets.active.weak_bg_fill = PRESSED;
    v.widgets.active.bg_fill = PRESSED;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.open.weak_bg_fill = PRESSED;
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);
    ctx.set_visuals(v);

    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = vec2(9.0, 9.0);
        s.spacing.button_padding = vec2(14.0, 9.0);
        s.spacing.menu_margin = Margin::same(6);
        s.spacing.scroll.bar_width = 6.0;
        s.spacing.scroll.floating_allocated_width = 0.0;
        s.text_styles
            .insert(TextStyle::Heading, FontId::new(20.0, FontFamily::Name(SEMIBOLD.into())));
        s.text_styles
            .insert(TextStyle::Body, FontId::new(14.5, FontFamily::Proportional));
        s.text_styles
            .insert(TextStyle::Button, FontId::new(14.5, FontFamily::Name(MEDIUM.into())));
        s.text_styles
            .insert(TextStyle::Small, FontId::new(12.0, FontFamily::Proportional));
        s.text_styles
            .insert(TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace));
    });
}

pub fn text_field<'a>(text: &'a mut String, hint: &'a str) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(text)
        .hint_text(RichText::new(hint).color(MUTED))
        .desired_width(f32::INFINITY)
        .margin(Margin::symmetric(12, 10))
}

/// Bouton à ses propres couleurs, qui change au survol et au clic.
///
/// Un `fill` ou `stroke` posé directement sur un `egui::Button` vaut pour
/// tous ses états : le survol ne se voyait plus. On passe donc par les
/// couleurs du thème, le temps de ce bouton.
pub struct Tinted<'a> {
    button: egui::Button<'a>,
    fill: [Color32; 3],
    stroke: [Stroke; 3],
}

impl<'a> Tinted<'a> {
    /// Fond au repos ; survol et clic l'éclaircissent.
    pub fn new(button: egui::Button<'a>, fill: Color32, stroke: Stroke) -> Self {
        let hover = Stroke::new(stroke.width, lighten(stroke.color, 0.25));
        Self {
            button,
            fill: [fill, lighten(fill, 0.12), lighten(fill, 0.22)],
            stroke: [stroke, hover, hover],
        }
    }

    /// Fond au survol (et un peu plus au clic).
    pub fn hover(mut self, fill: Color32) -> Self {
        self.fill[1] = fill;
        self.fill[2] = lighten(fill, 0.1);
        self
    }

    /// Bordure au survol et au clic.
    pub fn hover_stroke(mut self, stroke: Stroke) -> Self {
        self.stroke[1] = stroke;
        self.stroke[2] = stroke;
        self
    }
}

impl egui::Widget for Tinted<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        ui.scope(|ui| {
            let w = &mut ui.visuals_mut().widgets;
            let states = [
                (&mut w.noninteractive, 0),
                (&mut w.inactive, 0),
                (&mut w.hovered, 1),
                (&mut w.active, 2),
            ];
            for (v, i) in states {
                v.weak_bg_fill = self.fill[i];
                v.bg_stroke = self.stroke[i];
            }
            ui.add(self.button)
        })
        .inner
    }
}

/// Assombrit une couleur.
pub fn darken(c: Color32, t: f32) -> Color32 {
    c.lerp_to_gamma(Color32::from_black_alpha(c.a()), t)
}

/// Éclaircit une couleur (transparente : voile blanc léger).
pub fn lighten(c: Color32, t: f32) -> Color32 {
    if c.a() == 0 {
        return Color32::from_white_alpha((t * 90.0) as u8);
    }
    c.lerp_to_gamma(Color32::from_white_alpha(c.a()), t)
}

pub fn primary_button(label: &str, width: f32) -> Tinted<'_> {
    let button = egui::Button::new(RichText::new(label).semi().color(Color32::WHITE)).min_size(vec2(width, 44.0));
    Tinted::new(button, ACCENT, Stroke::new(1.0, ACCENT_HOVER.gamma_multiply(0.7))).hover(ACCENT_HOVER)
}

pub fn secondary_button(label: &str, width: f32) -> Tinted<'_> {
    let button = egui::Button::new(RichText::new(label).semi().color(TEXT)).min_size(vec2(width, 44.0));
    Tinted::new(button, SURFACE, Stroke::new(1.0, BORDER))
        .hover(HOVER)
        .hover_stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.72)))
}

pub fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.with_layout(Layout::top_down(Align::Min), |ui| {
        ui.label(RichText::new(text).color(MUTED).size(12.5).medium());
    });
}

/// Petit titre en capitales espacées, au-dessus d'une liste ou d'un groupe.
pub fn caps(text: &str) -> RichText {
    RichText::new(text.to_uppercase())
        .color(MUTED.gamma_multiply(0.9))
        .size(11.0)
        .extra_letter_spacing(0.8)
        .semi()
}

pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(caps(text));
    ui.add_space(4.0);
}

pub fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

pub fn color_for(name: &str) -> Color32 {
    let h = name
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    AVATARS[h as usize % AVATARS.len()]
}

pub fn avatar(ui: &mut egui::Ui, name: &str, size: f32) -> egui::Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    paint_avatar(ui.painter(), rect.center(), size, name);
    rect
}

/// Pastille ronde d'une personne ou d'un serveur : dégradé léger et initiale.
pub fn paint_avatar(p: &egui::Painter, c: egui::Pos2, size: f32, name: &str) {
    let color = color_for(name);
    gradient_disc(p, c, size / 2.0, lighten(color, 0.16), darken(color, 0.18));
    p.circle_stroke(c, size / 2.0 - 0.5, Stroke::new(1.0, Color32::from_white_alpha(28)));
    let initial = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    p.text(
        c + vec2(0.0, size * 0.01),
        Align2::CENTER_CENTER,
        initial,
        semibold((size * 0.42).round()),
        Color32::WHITE,
    );
}

/// Disque en dégradé vertical (haut plus clair) : quelques triangles, sans
/// image.
pub fn gradient_disc(p: &egui::Painter, c: egui::Pos2, r: f32, top: Color32, bottom: Color32) {
    let n = ((r * 1.6) as usize).clamp(24, 96);
    let mut mesh = egui::Mesh::default();
    let at = |y: f32| top.lerp_to_gamma(bottom, ((y - (c.y - r)) / (2.0 * r)).clamp(0.0, 1.0));
    mesh.colored_vertex(c, at(c.y));
    for i in 0..n {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        let v = c + r * vec2(a.cos(), a.sin());
        mesh.colored_vertex(v, at(v.y));
        mesh.add_triangle(0, 1 + i as u32, 1 + ((i + 1) % n) as u32);
    }
    p.add(egui::Shape::mesh(mesh));
}

/// Lueur radiale qui s'estompe jusqu'au transparent (halo d'ambiance).
pub fn soft_glow(p: &egui::Painter, c: egui::Pos2, r: f32, color: Color32) {
    let mut shapes = Vec::new();
    soft_glow_into(&mut shapes, c, r, color);
    p.extend(shapes);
}

/// Pareil, ajouté à une liste de formes (à peindre plus tard).
pub fn soft_glow_into(out: &mut Vec<egui::Shape>, c: egui::Pos2, r: f32, color: Color32) {
    const RINGS: [f32; 6] = [0.0, 0.2, 0.4, 0.6, 0.8, 1.0];
    let n = 64;
    let mut mesh = egui::Mesh::default();
    for (k, t) in RINGS.iter().enumerate() {
        // Décroissance douce (sans bord visible).
        let a = (1.0 - t) * (1.0 - t) * (1.0 - t * 0.5);
        for i in 0..n {
            let ang = i as f32 / n as f32 * std::f32::consts::TAU;
            mesh.colored_vertex(c + r * t * vec2(ang.cos(), ang.sin()), color.gamma_multiply(a));
            if k > 0 {
                let (i0, i1) = (i as u32, ((i + 1) % n) as u32);
                let (inner, outer) = (((k - 1) * n) as u32, (k * n) as u32);
                mesh.add_triangle(inner + i0, outer + i0, outer + i1);
                mesh.add_triangle(inner + i0, outer + i1, inner + i1);
            }
        }
    }
    out.push(egui::Shape::mesh(mesh));
}

/// Pastille d'état en bas à droite d'un avatar, détourée du fond.
pub fn presence_dot(ui: &egui::Ui, avatar: egui::Rect, color: Color32) {
    let r = (avatar.width() * 0.16).max(4.0);
    let c = avatar.right_bottom() - vec2(r * 0.75, r * 0.75);
    let p = ui.painter();
    p.circle_filled(c, r + 2.0, PANEL);
    p.circle_filled(c, r, color);
}

/// Anneau vert autour d'un avatar : la personne parle.
pub fn speaking_ring(ui: &egui::Ui, rect: egui::Rect) {
    ui.painter()
        .circle_stroke(rect.center(), rect.width() / 2.0 + 2.0, Stroke::new(2.0, OK));
}

/// Bouton discret (sans fond tant qu'on ne le survole pas).
pub fn ghost_button(text: impl Into<egui::WidgetText>) -> Tinted<'static> {
    Tinted::new(egui::Button::new(text), Color32::TRANSPARENT, Stroke::NONE).hover(HOVER)
}

/// Ligne (membre, participant) surlignée au survol. Le fond est réservé
/// avant le contenu pour être peint dessous.
pub fn hover_row<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> {
    let bg = ui.painter().add(egui::Shape::Noop);
    let row = ui.horizontal(add);
    if ui.rect_contains_pointer(row.response.rect) {
        let rect = row.response.rect.expand2(vec2(4.0, 2.0));
        ui.painter()
            .set(bg, egui::Shape::rect_filled(rect, 8, HOVER.gamma_multiply(0.7)));
    }
    row
}

/// Onglet d'une fenêtre de réglages.
pub fn tab(ui: &mut egui::Ui, selected: bool, label: &str) -> bool {
    let text = RichText::new(label).color(if selected { TEXT } else { MUTED });
    ui.add(egui::Button::selectable(selected, text)).clicked()
}

/// Cadre arrondi d'une carte, posé sur une ombre douce.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(16)
        .inner_margin(20)
        .shadow(shadow(18))
}

/// Halo d'ambiance : deux lueurs radiales très diffuses, sans image ni
/// animation. Peint sous le contenu, il ne coûte rien à la mise en page.
pub fn ambient_background(ui: &egui::Ui) {
    let rect = ui.max_rect();
    let p = ui.painter();
    let m = rect.width().max(rect.height());
    soft_glow(
        p,
        egui::pos2(rect.center().x + rect.width() * 0.18, rect.top() - m * 0.12),
        m * 0.62,
        ACCENT.gamma_multiply(0.16),
    );
    soft_glow(
        p,
        egui::pos2(rect.left() - m * 0.05, rect.bottom() + m * 0.08),
        m * 0.45,
        Color32::from_rgb(0x2d, 0xb3, 0xc9).gamma_multiply(0.06),
    );
}

/// Marque de l'application, dessinée à la demande pour rester nette à toutes
/// les tailles et ne pas ajouter de ressource graphique au binaire.
pub fn brand_mark(ui: &mut egui::Ui, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    gradient_disc(p, c, size / 2.0, ACCENT_HOVER, Color32::from_rgb(0x55, 0x40, 0xd8));
    p.circle_stroke(c, size / 2.0 - 0.5, Stroke::new(1.0, Color32::from_white_alpha(40)));
    // Bulle de dialogue et pastille « en ligne ».
    let bubble = egui::Rect::from_center_size(
        egui::pos2(c.x - size * 0.05, c.y + size * 0.04),
        vec2(size * 0.38, size * 0.25),
    );
    p.rect_filled(bubble, size * 0.09, Color32::WHITE);
    let dot = egui::pos2(c.x + size * 0.17, c.y - size * 0.17);
    p.circle_filled(dot, size * 0.13, Color32::from_rgb(0x55, 0x40, 0xd8));
    p.circle_filled(dot, size * 0.1, OK);
    response
}

/// Petit titre d'accent pour introduire une zone sans bordure de plus.
pub fn eyebrow(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .color(ACCENT_HOVER)
            .size(11.0)
            .extra_letter_spacing(1.0)
            .semi(),
    );
}

/// Texte muet qui revient à la ligne.
pub fn hint(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.add(egui::Label::new(RichText::new(text.into()).color(MUTED).small()).wrap());
}

/// Code d'invitation caché (même longueur quel que soit le code).
pub const MASKED: &str = "••••-••••-••••-••••-••••";

/// Ouvre un dossier dans l'explorateur de fichiers.
pub fn open_folder(path: &std::path::Path) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(program).arg(path).spawn() {
        tracing::warn!("ouverture du dossier : {e}");
    }
}

/// Ouvre une page web dans le navigateur.
pub fn open_link(url: &str) {
    open_folder(std::path::Path::new(url));
}

/// Barre de niveau du micro, avec le seuil de sensibilité.
pub fn level_meter(ui: &mut egui::Ui, db: f32, threshold: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), egui::Sense::hover());
    let frac = |v: f32| ((v + 75.0) / 75.0).clamp(0.0, 1.0);
    let p = ui.painter();
    // Piste bien visible sur le fond du panneau (sinon on ne voit que le repère du seuil).
    p.rect_filled(rect, 4.0, BORDER.gamma_multiply(0.8));
    let mut filled = rect;
    filled.set_width(rect.width() * frac(db));
    p.rect_filled(filled, 4.0, if db >= threshold { OK } else { MUTED });
    let x = rect.left() + rect.width() * frac(threshold);
    p.line_segment(
        [egui::pos2(x, rect.top() - 2.0), egui::pos2(x, rect.bottom() + 2.0)],
        Stroke::new(2.0, TEXT),
    );
}

pub fn save_prefs(storage: &mut dyn eframe::Storage, p: &audio::Prefs) {
    storage.set_string("audio_in", p.input.clone().unwrap_or_default());
    storage.set_string("audio_out", p.output.clone().unwrap_or_default());
    storage.set_string("audio_threshold", p.threshold_db.to_string());
    storage.set_string("audio_volume", p.volume.to_string());
    storage.set_string("audio_ptt", if p.push_to_talk { "1" } else { "0" }.into());
    let flag = |on: bool| if on { "1" } else { "0" }.to_string();
    storage.set_string("audio_denoise", flag(p.denoise));
    storage.set_string("audio_echo", flag(p.echo_cancel));
    storage.set_string("audio_agc", flag(p.auto_gain));
    storage.set_string("audio_cues", flag(p.cues));
    storage.set_string("audio_cues_volume", p.cues_volume.to_string());
    storage.set_string("video_camera", p.camera.clone().unwrap_or_default());
}

pub fn load_prefs(storage: Option<&dyn eframe::Storage>) -> audio::Prefs {
    let mut p = audio::Prefs::default();
    let Some(st) = storage else { return p };
    let device = |key| st.get_string(key).filter(|s| !s.is_empty());
    p.input = device("audio_in");
    p.output = device("audio_out");
    p.camera = device("video_camera");
    if let Some(v) = st.get_string("audio_threshold").and_then(|v| v.parse().ok()) {
        p.threshold_db = v;
    }
    if let Some(v) = st.get_string("audio_volume").and_then(|v| v.parse().ok()) {
        p.volume = v;
    }
    p.push_to_talk = st.get_string("audio_ptt").as_deref() == Some("1");
    // Absents (première fois) : les valeurs par défaut.
    let flag = |key, default| st.get_string(key).map_or(default, |v| v == "1");
    p.denoise = flag("audio_denoise", p.denoise);
    p.echo_cancel = flag("audio_echo", p.echo_cancel);
    p.auto_gain = flag("audio_agc", p.auto_gain);
    p.cues = flag("audio_cues", p.cues);
    if let Some(v) = st.get_string("audio_cues_volume").and_then(|v| v.parse().ok()) {
        p.cues_volume = v;
    }
    p
}

/// Ce que veut dire chaque état réseau, et quoi faire.
pub const HELP: &[(Color32, &str, &str)] = &[
    (
        OK,
        "Joignable sans ouvrir de port",
        "Tout va bien. Envoie le code à tes amis : il marche quel que soit le membre qui héberge, sans \
         rien régler sur la box. Garde-le entre amis, il sert aussi de clé au serveur.",
    ),
    (
        OK,
        "Accessible depuis Internet",
        "La box a ouvert le port toute seule (UPnP). Le code marche depuis n'importe où, tant que tu \
         héberges.",
    ),
    (
        WARN,
        "Accès Internet incertain",
        "Ta box (ou ton opérateur) complique les connexions entrantes : NAT « symétrique », deuxième box \
         devant la tienne, ou adresse partagée par l'opérateur (CGNAT, fréquent en 4G/5G). Essaie quand \
         même : souvent ça passe. Si un ami n'entre pas : laisse plutôt un autre membre héberger (bouton \
         « Ouvrir » chez lui), ou active UPnP sur la box, ou mets la première box en mode « bridge », \
         ou redirige le port 7878 en UDP vers ton PC (tuto dans le README). Certains opérateurs donnent \
         une adresse dédiée sur demande (« IP fixe », « IPv4 full-stack »).",
    ),
    (
        WARN,
        "Réseau local seulement",
        "L'app n'arrive pas à joindre Internet en UDP (pare-feu d'entreprise, Wi-Fi public, ou pas \
         d'Internet). Le code marche pour ceux qui sont sur le même Wi-Fi que toi. Vérifie que le \
         pare-feu Windows autorise RIP Discord, ou change de réseau.",
    ),
    (
        DANGER,
        "« Personne n'héberge ce serveur »",
        "Aucun membre n'a le serveur ouvert en ce moment. Si tu l'as déjà rejoint une fois, clique \
         « Ouvrir » dans « Mes serveurs » : ton PC l'héberge, avec les mêmes salons et le même \
         historique, et le même code marche pour les autres.",
    ),
    (
        DANGER,
        "« Vos deux box refusent la connexion directe »",
        "L'hôte a été trouvé mais les deux box sont trop strictes. Essayez en inversant les rôles (l'autre \
         héberge), ou ouvrez le port 7878 UDP chez celui qui héberge. Un relais automatique arrive dans \
         une prochaine version.",
    ),
];

pub fn help_window(ctx: &egui::Context, open: &mut bool) {
    if !*open {
        return;
    }
    let max_h = (ctx.content_rect().height() - 160.0).max(200.0);
    // Une fenêtre modale aussi : ouverte depuis « Mes réglages », elle passe
    // par-dessus au lieu de rester cachée derrière.
    let r = egui::Modal::new(egui::Id::unique("aide"))
        .backdrop_color(BACKDROP)
        .frame(card().inner_margin(20))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Aide").size(18.0).semi().color(TEXT));
                at_right(ui, |ui| {
                    let close = super::icons::Button::new(super::icons::Icon::Close);
                    if ui.add(close).on_hover_text("Fermer (Échap)").clicked() {
                        *open = false;
                    }
                });
            });
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(max_h).show(ui, |ui| {
                ui.set_width(460.0);
                section_title(ui, "COMMENT ÇA MARCHE");
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "Pas de serveur central : un des PC du groupe héberge. Pour se trouver, l'app \
                             utilise un annuaire chiffré sur le réseau BitTorrent (rien à installer), puis \
                             les deux PC s'envoient des paquets en même temps pour passer les box sans \
                             ouvrir de port. Les messages vont directement de PC à PC, chiffrés.",
                        )
                        .color(TEXT),
                    )
                    .wrap(),
                );
                ui.add_space(14.0);
                section_title(ui, "SI L'HÔTE PART");
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "Le vocal passe directement d'un participant à l'autre : il continue même si \
                             l'hôte du serveur s'en va. Le membre suivant dans l'ordre de succession reprend \
                             alors le serveur en quelques secondes, sans déconnecter personne. Si l'hôte lag, \
                             « Devenir l'hôte » (sous le nom du serveur) propose un vote aux autres.",
                        )
                        .color(TEXT),
                    )
                    .wrap(),
                );
                ui.add_space(14.0);
                section_title(ui, "CE QUE VEUT DIRE CHAQUE STATUT");
                for (color, title, text) in HELP {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        status_dot(ui, *color);
                        ui.label(RichText::new(*title).semi().color(*color));
                    });
                    ui.add(egui::Label::new(RichText::new(*text).color(MUTED)).wrap());
                }
                ui.add_space(8.0);
            });
        });
    if r.should_close() {
        *open = false;
    }
}

/// Aligne à droite. Dans une fenêtre qui se mesure (premier affichage), la
/// largeur est encore inconnue : on aligne alors simplement à la suite.
pub fn at_right(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    if ui.is_sizing_pass() || !ui.available_width().is_finite() {
        add(ui);
    } else {
        ui.with_layout(Layout::right_to_left(Align::Center), add);
    }
}
