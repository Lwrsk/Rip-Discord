//! Couleurs, petits widgets et texte d'aide.

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Stroke, TextStyle, vec2,
};

use crate::audio;

/// La palette est volontairement courte : elle garde l'app lisible dans une
/// pièce sombre et évite textures, images ou animations pour "faire joli".
pub const BG: Color32 = Color32::from_rgb(0x09, 0x0b, 0x12);
pub const PANEL: Color32 = Color32::from_rgb(0x10, 0x13, 0x1d);
pub const CARD: Color32 = Color32::from_rgb(0x17, 0x1b, 0x28);
pub const SURFACE: Color32 = Color32::from_rgb(0x1c, 0x21, 0x30);
pub const FIELD: Color32 = Color32::from_rgb(0x0d, 0x10, 0x18);
pub const BORDER: Color32 = Color32::from_rgb(0x2a, 0x31, 0x44);
pub const ACCENT: Color32 = Color32::from_rgb(0x8b, 0x6c, 0xff);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0xa3, 0x8a, 0xff);
/// Fond d'un élément survolé, puis enfoncé.
pub const HOVER: Color32 = Color32::from_rgb(0x26, 0x2d, 0x41);
pub const PRESSED: Color32 = Color32::from_rgb(0x30, 0x38, 0x50);
pub const TEXT: Color32 = Color32::from_rgb(0xf2, 0xf4, 0xfb);
pub const MUTED: Color32 = Color32::from_rgb(0x99, 0xa2, 0xb5);
pub const OK: Color32 = Color32::from_rgb(0x4e, 0xdf, 0xa7);
pub const WARN: Color32 = Color32::from_rgb(0xff, 0xbd, 0x59);
pub const DANGER: Color32 = Color32::from_rgb(0xff, 0x70, 0x86);

pub const AVATARS: [Color32; 6] = [
    Color32::from_rgb(0x7c, 0x5c, 0xff),
    Color32::from_rgb(0x3d, 0xa5, 0xdc),
    Color32::from_rgb(0x3d, 0xdc, 0x97),
    Color32::from_rgb(0xf5, 0xb1, 0x4c),
    Color32::from_rgb(0xff, 0x6b, 0x9a),
    Color32::from_rgb(0x4c, 0xd4, 0xf5),
];

pub fn apply_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = FIELD;
    v.text_edit_bg_color = Some(FIELD);
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.6);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT_HOVER;
    let r = CornerRadius::same(12);
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
    v.widgets.inactive.weak_bg_fill = SURFACE;
    // Rail des curseurs, fond des cases à cocher : visibles sur les cartes.
    v.widgets.inactive.bg_fill = FIELD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    // Survol bien visible : fond plus clair, liseré violet, main du lien.
    v.widgets.hovered.weak_bg_fill = HOVER;
    v.widgets.hovered.bg_fill = HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.72));
    v.widgets.active.weak_bg_fill = PRESSED;
    v.widgets.active.bg_fill = PRESSED;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);
    ctx.set_visuals(v);

    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = vec2(9.0, 9.0);
        s.spacing.button_padding = vec2(14.0, 10.0);
        s.text_styles
            .insert(TextStyle::Body, FontId::new(15.0, FontFamily::Proportional));
        s.text_styles
            .insert(TextStyle::Button, FontId::new(15.0, FontFamily::Proportional));
        s.text_styles
            .insert(TextStyle::Small, FontId::new(12.5, FontFamily::Proportional));
        s.text_styles
            .insert(TextStyle::Monospace, FontId::new(15.0, FontFamily::Monospace));
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

/// Éclaircit une couleur (transparente : voile blanc léger).
fn lighten(c: Color32, t: f32) -> Color32 {
    if c.a() == 0 {
        return Color32::from_white_alpha((t * 90.0) as u8);
    }
    c.lerp_to_gamma(Color32::from_white_alpha(c.a()), t)
}

pub fn primary_button(label: &str, width: f32) -> Tinted<'_> {
    let button = egui::Button::new(RichText::new(label).strong().color(Color32::WHITE)).min_size(vec2(width, 44.0));
    Tinted::new(button, ACCENT, Stroke::new(1.0, ACCENT_HOVER.gamma_multiply(0.7))).hover(ACCENT_HOVER)
}

pub fn secondary_button(label: &str, width: f32) -> Tinted<'_> {
    let button = egui::Button::new(RichText::new(label).strong().color(TEXT)).min_size(vec2(width, 44.0));
    Tinted::new(button, SURFACE, Stroke::new(1.0, BORDER))
        .hover(HOVER)
        .hover_stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.72)))
}

pub fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.with_layout(Layout::top_down(Align::Min), |ui| {
        ui.label(RichText::new(text).color(MUTED).small().strong());
    });
}

pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(MUTED).small().strong());
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
    let p = ui.painter();
    p.circle_filled(rect.center(), size / 2.0, color_for(name));
    p.circle_stroke(
        rect.center(),
        size / 2.0 - 0.5,
        Stroke::new(1.0, Color32::WHITE.gamma_multiply(0.16)),
    );
    let initial = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        initial,
        FontId::proportional(size * 0.45),
        Color32::WHITE,
    );
    rect
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

/// Cadre arrondi d'une carte.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.88)))
        .corner_radius(18)
        .inner_margin(20)
}

/// Halo décoratif discret : trois cercles GPU, sans image ni animation.
/// Il est peint sous le contenu et ne participe donc pas au coût de mise en
/// page des listes ou du chat.
pub fn ambient_background(ui: &egui::Ui) {
    let rect = ui.max_rect();
    let p = ui.painter();
    p.circle_filled(
        egui::pos2(rect.right() - rect.width() * 0.10, rect.top() + rect.height() * 0.08),
        rect.width().min(rect.height()) * 0.34,
        ACCENT.gamma_multiply(0.075),
    );
    p.circle_filled(
        egui::pos2(rect.left() + rect.width() * 0.02, rect.bottom() - rect.height() * 0.04),
        rect.width().min(rect.height()) * 0.27,
        OK.gamma_multiply(0.035),
    );
}

/// Marque de l'application, dessinée à la demande pour rester nette à toutes
/// les tailles et ne pas ajouter de ressource graphique au binaire.
pub fn brand_mark(ui: &mut egui::Ui, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    p.circle_filled(c, size / 2.0, ACCENT);
    p.circle_stroke(c, size / 2.0 - 0.75, Stroke::new(1.5, ACCENT_HOVER));
    p.circle_filled(egui::pos2(c.x + size * 0.16, c.y - size * 0.16), size * 0.15, OK);
    p.rect_filled(
        egui::Rect::from_center_size(
            egui::pos2(c.x - size * 0.06, c.y + size * 0.03),
            vec2(size * 0.34, size * 0.22),
        ),
        size * 0.08,
        Color32::WHITE,
    );
    response
}

/// Eyebrow pour introduire une zone sans utiliser une bordure supplémentaire.
pub fn eyebrow(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(ACCENT_HOVER).small().strong());
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
        .frame(card().inner_margin(20))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Aide").size(18.0).strong().color(TEXT));
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
                        ui.label(RichText::new(*title).strong().color(*color));
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
