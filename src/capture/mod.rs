//! Capture pour le partage d'écran : un écran entier ou une seule fenêtre
//! (jeu, navigateur, éditeur…), avec ou sans le curseur, en qualité réglable.
//! Tout se change en plein partage, sans que les spectateurs ne le voient
//! s'arrêter.
//!
//! Sous Windows, la capture passe par Windows.Graphics.Capture (celle d'OBS
//! et de Discord) : elle suit une fenêtre même cachée derrière d'autres, voit
//! les jeux en fenêtré ou plein écran fenêtré et dessine le vrai curseur. Si
//! elle n'est pas disponible, on retombe sur la capture classique (GDI).
//!
//! Les images partent en vidéo H.264 ([`video`]), le son de ce qui est
//! partagé à côté ([`sound`]). La webcam passe par le même codage
//! ([`camera`]).

pub mod camera;
mod cursor;
pub mod sound;
pub mod video;
#[cfg(windows)]
mod wgc;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use image::{DynamicImage, RgbaImage, imageops::FilterType};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Ce qu'on partage : identifiants donnés par le système (xcap).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Source {
    Monitor(u32),
    Window(u32),
}

/// Une source proposée dans le choix du partage.
#[derive(Clone, Debug)]
pub struct SourceInfo {
    pub source: Source,
    /// Titre de la fenêtre ou nom de l'écran.
    pub title: String,
    /// Application, taille, « plein écran »…
    pub detail: String,
    /// Fenêtre qui couvre tout son écran : un jeu, le plus souvent.
    pub fullscreen: bool,
}

/// Compromis netteté / fluidité / débit.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Quality {
    /// Texte lisible : code, documents.
    Sharp,
    #[default]
    Balanced,
    /// Jeux et vidéos.
    Smooth,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Sharp, Quality::Balanced, Quality::Smooth];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Sharp => "Texte net",
            Quality::Balanced => "Équilibré",
            Quality::Smooth => "Fluide",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Quality::Sharp => "Grande image, 10 images/s : code, documents, tableaux",
            Quality::Balanced => "Bon pour tout, 20 images/s",
            Quality::Smooth => "Image plus petite, 30 images/s : jeux et vidéos",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Quality::Sharp => "net",
            Quality::Balanced => "equilibre",
            Quality::Smooth => "fluide",
        }
    }

    pub fn from_key(k: &str) -> Self {
        Self::ALL.into_iter().find(|q| q.key() == k).unwrap_or_default()
    }

    /// Plus grand côté de l'image envoyée.
    fn max_side(self) -> u32 {
        match self {
            Quality::Sharp => 1920,
            Quality::Balanced => 1600,
            Quality::Smooth => 1280,
        }
    }

    fn fps(self) -> u32 {
        match self {
            Quality::Sharp => 10,
            Quality::Balanced => 20,
            Quality::Smooth => 30,
        }
    }

    /// Débit visé par la vidéo, en bits/s (par spectateur).
    fn bitrate(self) -> u32 {
        match self {
            Quality::Sharp => 2_500_000,
            Quality::Balanced => 2_500_000,
            Quality::Smooth => 3_000_000,
        }
    }
}

/// Réglages du partage, modifiables pendant qu'il tourne.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Options {
    pub cursor: bool,
    pub quality: Quality,
    /// Partager aussi le son de la fenêtre (ou de tout le PC pour un écran).
    pub sound: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cursor: true,
            quality: Quality::default(),
            sound: true,
        }
    }
}

/// Écrans et fenêtres qu'on peut partager (sans les nôtres).
pub fn sources() -> Vec<SourceInfo> {
    let mut out = Vec::new();
    if let Ok(ms) = xcap::Monitor::all() {
        for (i, m) in ms.iter().enumerate() {
            let Ok(id) = m.id() else { continue };
            let name = m.friendly_name().or_else(|_| m.name()).unwrap_or_default();
            let title = match name.trim() {
                "" => format!("Écran {}", i + 1),
                n => format!("Écran {} · {n}", i + 1),
            };
            let mut detail = match (m.width(), m.height()) {
                (Ok(w), Ok(h)) => format!("{w}×{h}"),
                _ => String::new(),
            };
            if m.is_primary().unwrap_or(false) {
                detail.push_str(" · principal");
            }
            out.push(SourceInfo {
                source: Source::Monitor(id),
                title,
                detail,
                fullscreen: false,
            });
        }
    }
    let me = std::process::id();
    if let Ok(ws) = xcap::Window::all() {
        for w in ws {
            let Ok(id) = w.id() else { continue };
            let app = w.app_name().unwrap_or_default();
            let app = app.trim().trim_end_matches(".exe");
            // Certaines fenêtres n'ont pas de titre lisible : le nom de l'appli à la place.
            let title = match w.title().unwrap_or_default().trim() {
                "" => app.to_string(),
                t => t.to_string(),
            };
            if title.is_empty()
                || w.pid().is_ok_and(|p| p == me)
                || w.is_minimized().unwrap_or(false)
                || IGNORED.contains(&title.as_str())
            {
                continue;
            }
            let (width, height) = (w.width().unwrap_or(0), w.height().unwrap_or(0));
            if width < 80 || height < 60 {
                continue;
            }
            let fullscreen = w
                .current_monitor()
                .is_ok_and(|m| m.width().is_ok_and(|mw| mw <= width) && m.height().is_ok_and(|mh| mh <= height));
            let mut detail = if app == title { String::new() } else { app.to_string() };
            if fullscreen {
                detail.push_str(if detail.is_empty() {
                    "plein écran"
                } else {
                    " · plein écran"
                });
            }
            out.push(SourceInfo {
                source: Source::Window(id),
                title,
                detail,
                fullscreen,
            });
        }
    }
    out
}

/// Fenêtres du système qui n'ont rien à faire dans la liste.
const IGNORED: &[&str] = &["Program Manager", "Windows Input Experience", "Paramètres", "Settings"];

/// Petite image d'une source, pour la choisir (lente : à faire hors de l'UI).
pub fn thumbnail(source: Source) -> Option<RgbaImage> {
    let img = match source {
        Source::Monitor(id) => find_monitor(id).ok()?.capture_image().ok()?,
        Source::Window(id) => find_window(id).ok()?.capture_image().ok()?,
    };
    let w = 320.min(img.width()).max(1);
    let h = (img.height() * w / img.width().max(1)).max(1);
    Some(
        DynamicImage::ImageRgba8(img)
            .resize_exact(w, h, FilterType::Triangle)
            .into_rgba8(),
    )
}

fn find_monitor(id: u32) -> Result<xcap::Monitor> {
    xcap::Monitor::all()?
        .into_iter()
        .find(|m| m.id().is_ok_and(|i| i == id))
        .context("écran introuvable (débranché ?)")
}

fn find_window(id: u32) -> Result<xcap::Window> {
    xcap::Window::all()?
        .into_iter()
        .find(|w| w.id().is_ok_and(|i| i == id))
        .context("fenêtre introuvable (fermée ?)")
}

/// Partage en cours ; s'arrête quand il est lâché. Quand la source disparaît
/// (fenêtre fermée, écran débranché), [`Sharer::next`] renvoie `None`.
pub struct Sharer {
    stop: Arc<AtomicBool>,
    options: Arc<Mutex<Options>>,
    /// Une image clé est demandée (nouveau spectateur, ou qui a perdu le fil).
    key: Arc<AtomicBool>,
    pub source: Source,
    frames: mpsc::Receiver<video::Encoded>,
    sound: mpsc::Receiver<Vec<u8>>,
    sound_tx: mpsc::Sender<Vec<u8>>,
    /// Capture du son en cours.
    capture: Option<sound::Capture>,
}

impl Drop for Sharer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Ce que produit un partage.
pub enum Shot {
    Frame(video::Encoded),
    /// Trame Opus stéréo de 20 ms.
    Sound(Vec<u8>),
}

/// Rien ne bouge à l'écran : la dernière image est réencodée au moins à ce
/// rythme (elle s'affine, et le partage reste vivant chez les spectateurs).
const KEEPALIVE: Duration = Duration::from_secs(1);
/// Image clé de toute façon à ce rythme quand l'image change (filet de sécurité).
const KEY_PERIOD: Duration = Duration::from_secs(5);
/// Écart minimal entre deux images clés demandées.
const MIN_KEY_GAP: Duration = Duration::from_millis(500);
/// Après autant d'échecs d'affilée, la source est considérée perdue.
const MAX_FAILURES: u32 = 30;

/// Quand faire une image clé.
struct Keys {
    wanted: Arc<AtomicBool>,
    last: Instant,
}

impl Keys {
    /// Une image clé est demandée (et la précédente n'est pas toute récente).
    fn asked(&self) -> bool {
        self.wanted.load(Ordering::Relaxed) && self.last.elapsed() >= MIN_KEY_GAP
    }

    fn now(&self, fresh: bool) -> bool {
        self.asked() || (fresh && self.last.elapsed() >= KEY_PERIOD)
    }

    fn sent(&mut self) {
        self.last = Instant::now();
        self.wanted.store(false, Ordering::Relaxed);
    }
}

impl Sharer {
    /// Partage `source` ; les images et le son arrivent par [`Sharer::next`].
    pub fn start(source: Source, options: Options) -> Result<Self> {
        let with_sound = options.sound;
        let stop = Arc::new(AtomicBool::new(false));
        let key = Arc::new(AtomicBool::new(false));
        let options = Arc::new(Mutex::new(options));
        let (tx, frames) = mpsc::channel(2);
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let (stopped, opts, wanted) = (stop.clone(), options.clone(), key.clone());
        // La source s'ouvre sur le fil de capture : sous Windows elle ne peut
        // pas changer de fil.
        std::thread::Builder::new().name("ecran".into()).spawn(move || {
            let first = *opts.lock().unwrap();
            let mut grabber = match Grabber::open(source, first.cursor) {
                Ok(g) => {
                    let _ = ready_tx.send(Ok(()));
                    g
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let mut keys = Keys {
                wanted,
                last: Instant::now(),
            };
            let mut encoder: Option<video::Encoder> = None;
            let mut last: Option<RgbaImage> = None;
            let mut sent = Instant::now();
            let mut failures = 0;
            while !stopped.load(Ordering::Relaxed) {
                let started = Instant::now();
                let o = *opts.lock().unwrap();
                let period = Duration::from_secs(1) / o.quality.fps();
                let fresh = match grabber.grab(o.cursor, period) {
                    Ok(Some(img)) => {
                        failures = 0;
                        if let Some(old) = last.replace(img) {
                            grabber.recycle(old);
                        }
                        true
                    }
                    Ok(None) => false,
                    Err(e) => {
                        failures += 1;
                        debug!("capture d'écran : {e:#}");
                        if failures >= MAX_FAILURES || !grabber.alive() {
                            warn!("partage d'écran arrêté : {e:#}");
                            break;
                        }
                        false
                    }
                };
                let due = fresh || sent.elapsed() >= KEEPALIVE || keys.asked();
                // Plein : le réseau est en retard. On n'encode pas cette image :
                // en sauter une déjà encodée casserait la suite chez tout le monde.
                if due
                    && tx.capacity() > 0
                    && let Some(img) = &last
                {
                    // Nouvelle qualité : nouvel encodeur, qui repart d'une image clé.
                    if encoder.as_ref().is_none_or(|e| e.profile() != o.quality.profile()) {
                        encoder = video::Encoder::new(o.quality.profile())
                            .inspect_err(|e| warn!("partage d'écran : {e:#}"))
                            .ok();
                    }
                    if let Some(enc) = &mut encoder {
                        match enc.encode(img, keys.now(fresh)) {
                            Ok(Some(frame)) => {
                                sent = Instant::now();
                                if frame.key {
                                    keys.sent();
                                }
                                if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(frame) {
                                    break;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!("partage d'écran : {e:#}");
                                encoder = None;
                            }
                        }
                    }
                }
                std::thread::sleep(period.saturating_sub(started.elapsed()));
            }
        })?;
        ready.recv().context("capture interrompue")??;
        let (sound_tx, sound) = mpsc::channel(64);
        let mut sharer = Self {
            stop,
            options,
            key,
            source,
            frames,
            sound,
            sound_tx,
            capture: None,
        };
        sharer.set_sound(with_sound);
        Ok(sharer)
    }

    pub fn options(&self) -> Options {
        *self.options.lock().unwrap()
    }

    pub fn set_options(&mut self, o: Options) {
        *self.options.lock().unwrap() = o;
        self.set_sound(o.sound);
    }

    /// Une image clé au plus vite (un spectateur arrive ou a perdu le fil).
    pub fn request_key(&self) {
        self.key.store(true, Ordering::Relaxed);
    }

    /// Le son part avec l'image.
    pub fn has_sound(&self) -> bool {
        self.capture.is_some()
    }

    /// Image ou son suivant ; `None` quand la source a disparu.
    pub async fn next(&mut self) -> Option<Shot> {
        tokio::select! {
            f = self.frames.recv() => f.map(Shot::Frame),
            Some(s) = self.sound.recv() => Some(Shot::Sound(s)),
        }
    }

    fn set_sound(&mut self, on: bool) {
        if on == self.capture.is_some() {
            return;
        }
        self.capture = None;
        if !on {
            return;
        }
        if !sound::SUPPORTED {
            debug!("son du partage : pas sur ce système");
            return;
        }
        let target = match self.source {
            Source::Monitor(_) => Ok(sound::Target::AllButMe),
            Source::Window(id) => find_window(id).and_then(|w| Ok(sound::Target::App(w.pid()?))),
        };
        match target.and_then(|t| sound::Capture::start(t, self.sound_tx.clone())) {
            Ok(c) => {
                info!("son du partage capturé");
                self.capture = Some(c);
            }
            // Pas de repli sur tout le son du PC : il renverrait les voix en écho.
            Err(e) => warn!("partage d'écran sans le son : {e:#}"),
        }
    }
}

/// D'où viennent les images.
enum Grabber {
    #[cfg(windows)]
    Wgc(wgc::Session),
    Monitor(xcap::Monitor, Option<cursor::Pointer>),
    Window(xcap::Window, Option<cursor::Pointer>),
}

impl Grabber {
    fn open(source: Source, cursor: bool) -> Result<Self> {
        #[cfg(windows)]
        match wgc::Session::start(source, cursor) {
            Ok(s) => return Ok(Grabber::Wgc(s)),
            Err(e) => warn!("Windows.Graphics.Capture indisponible, capture classique : {e:#}"),
        }
        let _ = cursor;
        let mut g = match source {
            Source::Monitor(id) => Grabber::Monitor(find_monitor(id)?, cursor::Pointer::new()),
            Source::Window(id) => Grabber::Window(find_window(id)?, cursor::Pointer::new()),
        };
        // Une première capture pour signaler tout de suite un refus du système.
        g.grab(false, Duration::ZERO)?
            .context("capture refusée par le système")?;
        Ok(g)
    }

    /// Image suivante ; `None` s'il n'y a rien de neuf.
    fn grab(&mut self, cursor: bool, gap: Duration) -> Result<Option<RgbaImage>> {
        let _ = gap;
        match self {
            #[cfg(windows)]
            Grabber::Wgc(s) => s.grab(cursor, gap),
            Grabber::Monitor(m, pointer) => {
                let mut img = m.capture_image()?;
                if cursor && let Some(p) = pointer {
                    p.draw(&mut img, m.x()?, m.y()?);
                }
                Ok(Some(img))
            }
            Grabber::Window(w, pointer) => {
                if w.is_minimized()? {
                    return Ok(None);
                }
                let mut img = w.capture_image()?;
                if img.width() == 0 || img.height() == 0 {
                    bail!("fenêtre vide");
                }
                if cursor && let Some(p) = pointer {
                    p.draw(&mut img, w.x()?, w.y()?);
                }
                Ok(Some(img))
            }
        }
    }

    /// Rend une image déjà encodée : la capture y copiera une suivante.
    fn recycle(&self, img: RgbaImage) {
        match self {
            #[cfg(windows)]
            Grabber::Wgc(s) => s.recycle(img),
            Grabber::Monitor(..) | Grabber::Window(..) => drop(img),
        }
    }

    /// La source existe encore (une fenêtre fermée ne reviendra pas).
    fn alive(&self) -> bool {
        match self {
            #[cfg(windows)]
            Grabber::Wgc(s) => !s.closed(),
            Grabber::Monitor(..) => true,
            Grabber::Window(w, _) => w.id().is_ok_and(|id| find_window(id).is_ok()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualites_du_plus_net_au_plus_fluide() {
        let [sharp, balanced, smooth] = Quality::ALL;
        assert!(sharp.max_side() > balanced.max_side() && balanced.max_side() > smooth.max_side());
        assert!(sharp.fps() < balanced.fps() && balanced.fps() < smooth.fps());
        assert_eq!(smooth.fps(), 30);
        // Quelques mégabits par seconde, pas des dizaines comme en JPEG.
        assert!(
            Quality::ALL
                .iter()
                .all(|q| (1_000_000..=4_000_000).contains(&q.bitrate()))
        );
    }

    #[test]
    fn qualite_retenue() {
        for q in Quality::ALL {
            assert_eq!(Quality::from_key(q.key()), q);
        }
        assert_eq!(Quality::from_key("?"), Quality::Balanced);
    }
}
