//! Vidéo des partages d'écran : H.264 (OpenH264). Seuls les changements d'une
//! image à l'autre partent, d'où un débit 5 à 10 fois plus faible qu'une image
//! JPEG complète à chaque fois, surtout dans les jeux.
//!
//! Les images dépendent les unes des autres : le partageur en envoie une
//! « clé » (complète) à chaque nouveau spectateur ou à qui a perdu le fil,
//! et de toute façon toutes les quelques secondes.

use std::{
    collections::HashMap,
    sync::mpsc as std_mpsc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use fast_image_resize::{
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
    images::{Image, ImageRef},
};
use image::RgbaImage;
use openh264::{
    OpenH264API, Timestamp,
    decoder::Decoder,
    encoder::{BitRate, Complexity, Encoder as H264, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType},
    formats::{RgbaSliceU8, YUVBuffer, YUVSource},
};
use tracing::debug;

use super::Quality;
use crate::core::{
    protocol::PeerId,
    screen::{Frame, Reorder},
};

/// Une image encodée, prête à partir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    /// Se décode seule (les suivantes en dépendent).
    pub key: bool,
    pub data: Vec<u8>,
}

/// Ce que vise un encodeur : taille, rythme, débit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// Plus grand côté de l'image envoyée.
    pub max_side: u32,
    pub fps: u32,
    /// Débit visé, en bits/s (par destinataire).
    pub bitrate: u32,
    /// Contenu d'écran (texte, bureau) plutôt qu'une image filmée.
    pub screen: bool,
}

impl Profile {
    /// Webcam : petite image, assez fluide, débit modeste (elle part vers
    /// tout le salon, pas seulement vers ceux qui regardent).
    pub const CAMERA: Profile = Profile {
        max_side: 640,
        fps: 20,
        bitrate: 500_000,
        screen: false,
    };
}

impl Quality {
    pub fn profile(self) -> Profile {
        Profile {
            max_side: self.max_side(),
            fps: self.fps(),
            bitrate: self.bitrate(),
            // Les jeux ressemblent plus à une caméra.
            screen: self != Quality::Smooth,
        }
    }
}

/// Taille envoyée : réduite pour tenir dans le profil (jamais agrandie), en
/// nombres pairs comme le veut H.264 en 4:2:0.
pub fn frame_size(width: u32, height: u32, p: Profile) -> (u32, u32) {
    let max = p.max_side;
    let even = |v: u32| (v & !1).max(2);
    if width <= max && height <= max {
        return (even(width), even(height));
    }
    let scale = max as f64 / width.max(height) as f64;
    // Réduite : un multiple de 8, que le décodage sait convertir plus vite.
    let round = |v: u32| ((v as f64 * scale / 8.0).round() as u32 * 8).max(8);
    (round(width), round(height))
}

/// Encodeur d'un partage ou d'une webcam. Changer de qualité, c'est en
/// créer un autre (qui commence par une image clé).
pub struct Encoder {
    inner: H264,
    profile: Profile,
    started: Instant,
    /// Une image a déjà été encodée (l'encodeur ne s'initialise qu'à la première).
    running: bool,
    yuv: Option<YUVBuffer>,
    /// Réduction des images (SIMD : quelques millisecondes là où
    /// `image::imageops` en met 50, trop pour 30 images par seconde).
    resizer: Resizer,
    scaled: Vec<u8>,
}

impl Encoder {
    pub fn new(profile: Profile) -> Result<Self> {
        // Texte et bureau : l'outil « contenu d'écran » d'OpenH264.
        let screen = profile.screen;
        let usage = if screen {
            UsageType::ScreenContentRealTime
        } else {
            UsageType::CameraVideoRealTime
        };
        let config = EncoderConfig::new()
            .usage_type(usage)
            // Outils « caméra » qu'OpenH264 refuse (en le disant) sur un écran.
            .adaptive_quantization(!screen)
            .background_detection(!screen)
            .bitrate(BitRate::from_bps(profile.bitrate))
            .max_frame_rate(FrameRate::from_hz(profile.fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(true)
            .complexity(Complexity::Low);
        let inner =
            H264::with_api_config(OpenH264API::from_source(), config).map_err(|e| anyhow!("encodeur H.264 : {e}"))?;
        Ok(Self {
            inner,
            profile,
            started: Instant::now(),
            running: false,
            yuv: None,
            resizer: Resizer::new(),
            scaled: Vec::new(),
        })
    }

    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// Encode une image (`key` : en faire une image clé). `None` :
    /// l'encodeur a sauté cette image pour tenir le débit.
    pub fn encode(&mut self, img: &RgbaImage, key: bool) -> Result<Option<Encoded>> {
        let (w, h) = frame_size(img.width(), img.height(), self.profile);
        if (w, h) != img.dimensions() {
            self.scale(img, w, h)?;
        }
        let rgba = if (w, h) == img.dimensions() {
            img.as_raw()
        } else {
            &self.scaled
        };
        let source = RgbaSliceU8::new(rgba, (w as usize, h as usize));
        let mut yuv = self.yuv.take();
        let ready = match &mut yuv {
            Some(y) if y.dimensions() == (w as usize, h as usize) => {
                y.read_rgba8(source);
                y
            }
            slot => slot.insert(YUVBuffer::from_rgba8_source(source)),
        };
        let out = self.encode_yuv(ready, key);
        self.yuv = yuv;
        out
    }

    /// Encode une image déjà en YUV 4:2:0, à la bonne taille (voir
    /// [`frame_size`]) : sans passer par le RGBA ni la réduction.
    pub fn encode_yuv(&mut self, yuv: &impl YUVSource, key: bool) -> Result<Option<Encoded>> {
        if key && self.running {
            self.inner.force_intra_frame();
        }
        self.running = true;
        let at = Timestamp::from_millis(self.started.elapsed().as_millis() as u64);
        let out = self
            .inner
            .encode_at(yuv, at)
            .map_err(|e| anyhow!("encodage H.264 : {e}"))?;
        let key = match out.frame_type() {
            FrameType::IDR => true,
            FrameType::I | FrameType::P | FrameType::IPMixed => false,
            FrameType::Skip | FrameType::Invalid => return Ok(None),
        };
        let data = out.to_vec();
        Ok((!data.is_empty()).then_some(Encoded { key, data }))
    }

    /// Met `img` à la taille `w`×`h` dans `self.scaled`.
    fn scale(&mut self, img: &RgbaImage, w: u32, h: u32) -> Result<()> {
        let src = ImageRef::new(img.width(), img.height(), img.as_raw(), PixelType::U8x4)?;
        self.scaled.resize(w as usize * h as usize * 4, 0);
        let mut dst = Image::from_slice_u8(w, h, &mut self.scaled, PixelType::U8x4)?;
        // Une capture d'écran (ou de webcam) est opaque : pas de calcul de transparence.
        let mut options = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
            .use_alpha(false);
        if w <= img.width() && h <= img.height() && img.width() - w <= 1 && img.height() - h <= 1 {
            // Juste un pixel en trop (taille impaire) : on le coupe plutôt que d'étirer.
            options = options.crop(0.0, 0.0, w as f64, h as f64);
        }
        self.resizer.resize(&src, &mut dst, &options)?;
        Ok(())
    }
}

/// Image en YUV 4:2:0 (un plan de luminance, deux plans de couleur à
/// demi-résolution), dont les tampons servent d'une image à l'autre.
#[derive(Default)]
pub struct Yuv420 {
    width: usize,
    height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl Yuv420 {
    /// Prépare les plans pour une image `width`×`height` (nombres pairs).
    pub fn reset(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.y.resize(width * height, 0);
        self.u.resize(width / 2 * (height / 2), 0);
        self.v.resize(width / 2 * (height / 2), 0);
    }
}

impl YUVSource for Yuv420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

/// Plus grande image reçue acceptée (une image envoyée fait au plus 1920 de
/// côté ; la marge couvre les écrans très hauts).
const MAX_DECODED: usize = 4096;

/// Image décodée, prête à afficher.
pub struct Picture {
    pub width: usize,
    pub height: usize,
    /// Pixels RGBA, ligne par ligne.
    pub rgba: Vec<u8>,
}

/// Décodeur d'un flux : remet les images dans l'ordre et sait quand il a
/// perdu le fil.
pub struct Stream {
    reorder: Reorder,
    decoder: Option<Decoder>,
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream {
    pub fn new() -> Self {
        Self {
            reorder: Reorder::default(),
            decoder: None,
        }
    }

    /// Image reçue ; renvoie la dernière image décodable à afficher.
    pub fn push(&mut self, frame: Frame, now: Instant) -> Option<Picture> {
        let ready = self.reorder.push(frame, now);
        let n = ready.len();
        let mut shown = None;
        for (i, f) in ready.into_iter().enumerate() {
            match self.decode(&f.data, i + 1 == n) {
                Ok(p) => shown = p.or(shown),
                Err(e) => {
                    debug!("écran : {e:#}");
                    // Décodeur neuf à la prochaine image clé.
                    self.decoder = None;
                    self.reorder.lost();
                    return shown;
                }
            }
        }
        shown
    }

    /// Faut-il redemander une image clé au partageur ?
    pub fn want_key(&mut self, now: Instant) -> bool {
        self.reorder.want_key(now)
    }

    /// Décode `data` ; ne convertit en RGBA que si `show`.
    fn decode(&mut self, data: &[u8], show: bool) -> Result<Option<Picture>> {
        let decoder = match &mut self.decoder {
            Some(d) => d,
            slot => slot.insert(Decoder::new().map_err(|e| anyhow!("décodeur H.264 : {e}"))?),
        };
        let Some(yuv) = decoder.decode(data).map_err(|e| anyhow!("image illisible : {e}"))? else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        anyhow::ensure!(
            width <= MAX_DECODED && height <= MAX_DECODED,
            "image démesurée ({width}×{height})"
        );
        if !show {
            return Ok(None);
        }
        let mut rgba = vec![0; width * height * 4];
        yuv.write_rgba8(&mut rgba);
        Ok(Some(Picture { width, height, rgba }))
    }
}

/// Décodage des écrans regardés, sur un fil à part : rien ne bloque
/// l'interface ni le réseau.
pub struct Viewer {
    tx: std_mpsc::Sender<(PeerId, Frame)>,
}

/// Un flux qui ne reçoit plus rien depuis ce délai est oublié.
const IDLE: Duration = Duration::from_secs(10);

impl Viewer {
    /// `show` reçoit chaque image à afficher ; `ask` est appelé quand il
    /// faut redemander une image clé à ce partageur.
    pub fn start(
        mut show: impl FnMut(PeerId, Picture) + Send + 'static,
        mut ask: impl FnMut(PeerId) + Send + 'static,
    ) -> Result<Self> {
        let (tx, rx) = std_mpsc::channel::<(PeerId, Frame)>();
        std::thread::Builder::new()
            .name("decodage".into())
            .spawn(move || {
                let mut streams: HashMap<PeerId, (Stream, Instant)> = HashMap::new();
                loop {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok((from, frame)) => {
                            let now = Instant::now();
                            let (stream, seen) = streams.entry(from).or_insert_with(|| (Stream::new(), now));
                            *seen = now;
                            if let Some(p) = stream.push(frame, now) {
                                show(from, p);
                            }
                        }
                        Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                    let now = Instant::now();
                    streams.retain(|_, (_, seen)| now - *seen < IDLE);
                    for (from, (stream, _)) in &mut streams {
                        if stream.want_key(now) {
                            ask(*from);
                        }
                    }
                }
            })
            .context("fil de décodage")?;
        Ok(Self { tx })
    }

    pub fn push(&self, from: PeerId, frame: Frame) {
        let _ = self.tx.send((from, frame));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// Un « écran » qui bouge : un dégradé et un carré qui se déplace.
    fn screen(w: u32, h: u32, t: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let inside = (x + 400 - t * 16) % 400 < 60 && (y + 300 - t * 8) % 300 < 60;
            if inside {
                image::Rgba([250, 250, 250, 255])
            } else {
                image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 90, 255])
            }
        })
    }

    fn encode_all(q: Quality, w: u32, h: u32, n: u32, key_at: &[u32]) -> Vec<Frame> {
        let mut enc = Encoder::new(q.profile()).unwrap();
        let mut frames = Vec::new();
        for t in 0..n {
            if let Some(e) = enc.encode(&screen(w, h, t), key_at.contains(&t)).unwrap() {
                frames.push(Frame {
                    seq: frames.len() as u32,
                    key: e.key,
                    camera: false,
                    data: Bytes::from(e.data),
                });
            }
        }
        frames
    }

    /// Écart moyen entre deux images, par composante (0 à 255).
    fn distance(a: &[u8], b: &RgbaImage) -> f64 {
        let total: u64 = a
            .iter()
            .zip(b.as_raw())
            .enumerate()
            .filter(|(i, _)| i % 4 != 3)
            .map(|(_, (x, y))| x.abs_diff(*y) as u64)
            .sum();
        total as f64 / (a.len() as f64 * 0.75)
    }

    #[test]
    fn tailles_selon_la_qualite() {
        let (sharp, smooth) = (Quality::Sharp.profile(), Quality::Smooth.profile());
        assert_eq!(frame_size(2560, 1440, sharp), (1920, 1080));
        assert_eq!(frame_size(2560, 1440, smooth), (1280, 720));
        // Écran en hauteur : c'est le grand côté qui compte.
        assert_eq!(frame_size(1440, 2560, smooth), (720, 1280));
        // Une petite fenêtre n'est jamais agrandie, juste rendue paire.
        assert_eq!(frame_size(801, 601, sharp), (800, 600));
        // Webcam HD : envoyée en petit.
        assert_eq!(frame_size(1280, 720, Profile::CAMERA), (640, 360));
        assert_eq!(frame_size(640, 480, Profile::CAMERA), (640, 480));
    }

    #[test]
    fn encode_puis_decode() {
        let (w, h) = (320, 240);
        let frames = encode_all(Quality::Balanced, w, h, 12, &[]);
        assert!(frames.len() >= 8, "{} images", frames.len());
        assert!(frames[0].key, "la première image doit être une image clé");
        assert!(frames[1..].iter().all(|f| !f.key));
        // Bien plus petit que les pixels bruts, même pour la première.
        assert!(frames[0].data.len() < (w * h) as usize, "{}", frames[0].data.len());

        let mut stream = Stream::new();
        let t = Instant::now();
        let mut last = None;
        for f in frames.clone() {
            let p = stream.push(f, t).expect("image non décodée");
            assert_eq!((p.width, p.height), (w as usize, h as usize));
            last = Some(p);
        }
        // L'image décodée ressemble à l'originale.
        let last = last.unwrap();
        let n = frames.len() as u32;
        let original = (0..12).map(|t| screen(w, h, t)).collect::<Vec<_>>();
        let best = original
            .iter()
            .map(|o| distance(&last.rgba, o))
            .fold(f64::MAX, f64::min);
        assert!(best < 12.0, "image trop différente ({best:.1}, {n} images)");
    }

    #[test]
    fn grande_image_reduite() {
        let frames = encode_all(Quality::Smooth, 1600, 900, 2, &[]);
        let mut stream = Stream::new();
        let p = stream.push(frames[0].clone(), Instant::now()).unwrap();
        assert_eq!((p.width, p.height), (1280, 720));
    }

    #[test]
    fn image_cle_a_la_demande_et_trou() {
        let frames = encode_all(Quality::Balanced, 320, 240, 10, &[6]);
        let k = frames
            .iter()
            .position(|f| f.seq > 0 && f.key)
            .expect("pas d'image clé demandée");
        // Un spectateur qui arrive en cours de route démarre sur l'image clé,
        // avec un décodeur neuf.
        let mut late = Stream::new();
        let t = Instant::now();
        assert!(late.push(frames[k - 1].clone(), t).is_none());
        assert!(late.want_key(t), "il doit demander une image clé");
        for f in &frames[k..] {
            assert!(late.push(f.clone(), t).is_some());
        }
        // Une image perdue : rien ne s'affiche jusqu'à la prochaine image clé.
        let mut s = Stream::new();
        s.push(frames[0].clone(), t).unwrap();
        s.push(frames[1].clone(), t).unwrap();
        for f in &frames[3..k] {
            assert!(s.push(f.clone(), t).is_none());
        }
        assert!(s.want_key(t + Duration::from_secs(1)));
        assert!(s.push(frames[k].clone(), t).is_some());
    }

    #[test]
    fn decodage_sur_un_fil_a_part() {
        let frames = encode_all(Quality::Sharp, 320, 240, 3, &[]);
        let (tx, rx) = std_mpsc::channel();
        let viewer = Viewer::start(move |from, p| tx.send((from, p.width)).unwrap(), |_| {}).unwrap();
        for f in frames {
            viewer.push(4, f);
        }
        for _ in 0..3 {
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), (4, 320));
        }
    }
}
