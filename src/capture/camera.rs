//! Webcam : ouverte sur un fil à part, réduite et codée en H.264 comme les
//! partages d'écran ([`super::video`]), au profil [`Profile::CAMERA`].
//!
//! Le pilote donne ses images en MJPEG, YUYV ou NV12 : on prend le format le
//! plus proche de 640×480 à 30 images/s et on les décode nous-mêmes (le JPEG
//! par `image`, déjà là pour les pièces jointes).

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use image::RgbaImage;
use nokhwa::{
    Camera as Device,
    pixel_format::RgbAFormat,
    utils::{
        ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
        buf_nv12_to_rgb, buf_yuyv422_to_rgb,
    },
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{
    Keys,
    video::{self, Profile, Yuv420},
};

/// Webcams branchées, par leur nom.
pub fn cameras() -> Vec<String> {
    match nokhwa::query(ApiBackend::Auto) {
        Ok(list) => list.into_iter().map(|c| c.human_name()).collect(),
        Err(e) => {
            debug!("liste des webcams : {e}");
            Vec::new()
        }
    }
}

/// Après autant d'échecs d'affilée, la webcam est considérée perdue (débranchée).
const MAX_FAILURES: u32 = 20;

/// Webcam allumée ; s'éteint quand elle est lâchée. Débranchée,
/// [`Camera::next`] renvoie `None`.
pub struct Camera {
    stop: Arc<AtomicBool>,
    /// Une image clé est demandée (quelqu'un arrive ou a perdu le fil).
    key: Arc<AtomicBool>,
    frames: mpsc::Receiver<video::Encoded>,
    /// Nom de la webcam ouverte.
    pub name: String,
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Camera {
    /// Allume la webcam `wanted` (par son nom), ou la première trouvée.
    /// Avec `RIP_DISCORD_FAKE_CAMERA`, une mire animée la remplace (tests
    /// sur un PC sans webcam).
    pub fn start(wanted: Option<&str>) -> Result<Self> {
        let index = match std::env::var_os("RIP_DISCORD_FAKE_CAMERA") {
            Some(_) => None,
            None => Some(find(wanted)?),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let key = Arc::new(AtomicBool::new(false));
        let (tx, frames) = mpsc::channel(2);
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let (stopped, wanted_key) = (stop.clone(), key.clone());
        // La webcam s'ouvre sur son fil : sous Windows, elle ne peut pas en changer.
        std::thread::Builder::new().name("webcam".into()).spawn(move || {
            let mut device = match index.map(open).transpose() {
                Ok(d) => {
                    let name = d.as_ref().map_or_else(|| "Mire".into(), |d| d.info().human_name());
                    let _ = ready_tx.send(Ok(name));
                    d.map_or(Source::Pattern(0), Source::Device)
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            if let Source::Device(d) = &device {
                info!("webcam : {}", d.camera_format());
            }
            let mut keys = Keys {
                wanted: wanted_key,
                last: Instant::now(),
            };
            let mut encoder: Option<video::Encoder> = None;
            let mut convert = Convert::default();
            let mut pace = Pace::new(Profile::CAMERA.fps);
            let mut failures = 0;
            while !stopped.load(Ordering::Relaxed) {
                let buffer = match device.frame() {
                    Ok(b) => {
                        failures = 0;
                        b
                    }
                    Err(e) => {
                        failures += 1;
                        debug!("webcam : {e}");
                        if failures >= MAX_FAILURES {
                            warn!("webcam arrêtée : {e}");
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                };
                // La webcam va souvent plus vite que ce qu'on envoie ; plein :
                // le réseau est en retard, on n'encode pas cette image.
                if !pace.due(Instant::now(), keys.asked()) || tx.capacity() == 0 {
                    continue;
                }
                let picture = match convert.picture(buffer) {
                    Ok(p) => p,
                    Err(e) => {
                        debug!("webcam : image illisible : {e:#}");
                        continue;
                    }
                };
                let enc = match &mut encoder {
                    Some(e) => e,
                    slot => match video::Encoder::new(Profile::CAMERA) {
                        Ok(e) => slot.insert(e),
                        Err(e) => {
                            warn!("webcam : {e:#}");
                            break;
                        }
                    },
                };
                let key = keys.now(true);
                let encoded = match picture {
                    Picture::Yuv => enc.encode_yuv(&convert.yuv, key),
                    Picture::Rgba(img) => {
                        let encoded = enc.encode(&img, key);
                        convert.rgba = img.into_raw();
                        encoded
                    }
                };
                match encoded {
                    Ok(Some(frame)) => {
                        if frame.key {
                            keys.sent();
                        }
                        if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(frame) {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("webcam : {e:#}");
                        encoder = None;
                    }
                }
            }
            if let Source::Device(d) = &mut device {
                let _ = d.stop_stream();
            }
        })?;
        let name = ready.recv().context("webcam interrompue")??;
        Ok(Self {
            stop,
            key,
            frames,
            name,
        })
    }

    /// Une image clé au plus vite.
    pub fn request_key(&self) {
        self.key.store(true, Ordering::Relaxed);
    }

    /// Image suivante ; `None` quand la webcam a disparu.
    pub async fn next(&mut self) -> Option<video::Encoded> {
        self.frames.recv().await
    }
}

/// D'où viennent les images.
enum Source {
    Device(Device),
    /// Mire animée (numéro de l'image).
    Pattern(u32),
}

/// Une image telle qu'elle arrive, pas encore décodée.
enum Raw {
    Buffer(nokhwa::Buffer),
    Image(RgbaImage),
}

impl Source {
    /// Image suivante (attend la webcam).
    fn frame(&mut self) -> Result<Raw> {
        match self {
            Source::Device(d) => d.frame().map(Raw::Buffer).map_err(|e| anyhow!("{e}")),
            Source::Pattern(t) => {
                std::thread::sleep(Duration::from_millis(33));
                *t = t.wrapping_add(1);
                let t = *t;
                Ok(Raw::Image(RgbaImage::from_fn(640, 480, |x, y| {
                    let ball = (x + 640 - (t * 6) % 640) % 640 < 120 && (y + 480 - (t * 4) % 480) % 480 < 120;
                    if ball {
                        image::Rgba([240, 200, 60, 255])
                    } else {
                        image::Rgba([(x * 255 / 640) as u8, 60, (y * 255 / 480) as u8, 255])
                    }
                })))
            }
        }
    }
}

/// Image prête pour l'encodeur.
enum Picture {
    /// Dans [`Convert::yuv`].
    Yuv,
    Rgba(RgbaImage),
}

/// Conversion des images de la webcam, avec des tampons gardés d'une image
/// à l'autre (rien n'est alloué à chaque image).
#[derive(Default)]
struct Convert {
    yuv: Yuv420,
    rgba: Vec<u8>,
}

impl Convert {
    fn picture(&mut self, raw: Raw) -> Result<Picture> {
        let b = match raw {
            Raw::Buffer(b) => b,
            Raw::Image(img) => return Ok(Picture::Rgba(img)),
        };
        let (format, res) = (b.source_frame_format(), b.resolution());
        let (w, h) = (res.width(), res.height());
        // Déjà en YUV et à la taille envoyée : directement les plans que
        // veut l'encodeur, sans aller-retour par le RGBA.
        if matches!(format, FrameFormat::YUYV | FrameFormat::NV12) && video::frame_size(w, h, Profile::CAMERA) == (w, h)
        {
            let (w, h) = (w as usize, h as usize);
            self.yuv.reset(w, h);
            if format == FrameFormat::YUYV {
                yuyv_to_i420(b.buffer(), w, h, &mut self.yuv)?;
            } else {
                nv12_to_i420(b.buffer(), w, h, &mut self.yuv)?;
            }
            return Ok(Picture::Yuv);
        }
        let out = std::mem::take(&mut self.rgba);
        to_rgba(format, w, h, b.buffer(), out).map(Picture::Rgba)
    }
}

/// YUYV (4:2:2 entrelacé) vers des plans 4:2:0 : la couleur de deux lignes
/// voisines est moyennée. `w` et `h` sont pairs.
fn yuyv_to_i420(data: &[u8], w: usize, h: usize, out: &mut Yuv420) -> Result<()> {
    if data.len() < w * h * 2 {
        bail!("image tronquée");
    }
    let half = w / 2;
    for r in 0..h / 2 {
        let top = &data[2 * r * w * 2..][..w * 2];
        let bottom = &data[(2 * r + 1) * w * 2..][..w * 2];
        let (y_top, y_bottom) = out.y[2 * r * w..][..2 * w].split_at_mut(w);
        let (u, v) = (&mut out.u[r * half..][..half], &mut out.v[r * half..][..half]);
        for i in 0..half {
            let (a, b) = (&top[i * 4..i * 4 + 4], &bottom[i * 4..i * 4 + 4]);
            y_top[2 * i] = a[0];
            y_top[2 * i + 1] = a[2];
            y_bottom[2 * i] = b[0];
            y_bottom[2 * i + 1] = b[2];
            u[i] = (a[1] as u16 + b[1] as u16).div_ceil(2) as u8;
            v[i] = (a[3] as u16 + b[3] as u16).div_ceil(2) as u8;
        }
    }
    Ok(())
}

/// NV12 (luminance, puis couleur entrelacée U/V) vers des plans séparés.
fn nv12_to_i420(data: &[u8], w: usize, h: usize, out: &mut Yuv420) -> Result<()> {
    let chroma = w / 2 * (h / 2);
    if data.len() < w * h + chroma * 2 {
        bail!("image tronquée");
    }
    out.y.copy_from_slice(&data[..w * h]);
    for (i, uv) in data[w * h..][..chroma * 2].as_chunks::<2>().0.iter().enumerate() {
        out.u[i] = uv[0];
        out.v[i] = uv[1];
    }
    Ok(())
}

fn find(wanted: Option<&str>) -> Result<CameraIndex> {
    let list = nokhwa::query(ApiBackend::Auto).map_err(|e| anyhow!("{e}"))?;
    let chosen = wanted
        .and_then(|w| list.iter().find(|c| c.human_name() == w))
        // Celle choisie n'est plus là : la première venue.
        .or_else(|| list.first())
        .context("aucune webcam trouvée")?;
    Ok(chosen.index().clone())
}

fn open(index: CameraIndex) -> Result<Device> {
    let any = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::None);
    let mut device = Device::new(index, any).map_err(|e| anyhow!("webcam inaccessible : {e}"))?;
    match device.compatible_camera_formats() {
        Ok(formats) => {
            if let Some(f) = choose(&formats)
                && f != device.camera_format()
                && let Err(e) = device.set_camera_requset(RequestedFormat::with_formats(
                    RequestedFormatType::Exact(f),
                    &[f.format()],
                ))
            {
                debug!("webcam : format {f} refusé : {e}");
            }
        }
        Err(e) => debug!("webcam : formats inconnus : {e}"),
    }
    device
        .open_stream()
        .map_err(|e| anyhow!("webcam occupée ou refusée : {e}"))?;
    Ok(device)
}

/// Le format de webcam à prendre : au moins 640 de large si possible, au
/// moins 15 images/s, puis le plus proche de 640×480 à 30 images/s.
fn choose(formats: &[CameraFormat]) -> Option<CameraFormat> {
    const AREA: u32 = 640 * 480;
    formats
        .iter()
        .filter(|f| rank(f.format()).is_some())
        .min_by_key(|f| {
            (
                f.width() < 600,
                f.frame_rate() < 15,
                (f.width() * f.height()).abs_diff(AREA),
                f.frame_rate().abs_diff(30),
                rank(f.format()),
            )
        })
        .copied()
}

/// Préférence entre formats (le plus petit d'abord) ; `None` : illisible.
fn rank(f: FrameFormat) -> Option<u8> {
    match f {
        FrameFormat::YUYV => Some(0),
        FrameFormat::NV12 => Some(1),
        FrameFormat::MJPEG => Some(2),
        FrameFormat::RAWRGB | FrameFormat::RAWBGR => Some(3),
        FrameFormat::GRAY => Some(4),
    }
}

/// Image de la webcam en RGBA, dans `out` (réutilisé).
fn to_rgba(format: FrameFormat, width: u32, height: u32, data: &[u8], mut out: Vec<u8>) -> Result<RgbaImage> {
    let pixels = (width * height) as usize;
    out.clear();
    match format {
        FrameFormat::MJPEG => {
            return Ok(image::load_from_memory_with_format(data, image::ImageFormat::Jpeg)?.into_rgba8());
        }
        FrameFormat::YUYV => {
            out.resize(pixels * 4, 0);
            buf_yuyv422_to_rgb(data, &mut out, true).map_err(|e| anyhow!("{e}"))?;
        }
        FrameFormat::NV12 => {
            out.resize(pixels * 4, 0);
            buf_nv12_to_rgb(Resolution::new(width, height), data, &mut out, true).map_err(|e| anyhow!("{e}"))?;
        }
        FrameFormat::RAWRGB | FrameFormat::RAWBGR => {
            if data.len() < pixels * 3 {
                bail!("image tronquée");
            }
            let bgr = format == FrameFormat::RAWBGR;
            out.extend(data[..pixels * 3].as_chunks::<3>().0.iter().flat_map(|p| {
                if bgr {
                    [p[2], p[1], p[0], 255]
                } else {
                    [p[0], p[1], p[2], 255]
                }
            }));
        }
        FrameFormat::GRAY => {
            if data.len() < pixels {
                bail!("image tronquée");
            }
            out.extend(data[..pixels].iter().flat_map(|g| [*g, *g, *g, 255]));
        }
    }
    RgbaImage::from_raw(width, height, out).context("taille d'image incohérente")
}

/// Rythme d'envoi : une image toutes les `period` en moyenne, quel que soit
/// celui de la webcam.
struct Pace {
    period: Duration,
    next: Option<Instant>,
}

impl Pace {
    fn new(fps: u32) -> Self {
        Self {
            period: Duration::from_secs(1) / fps,
            next: None,
        }
    }

    /// Faut-il envoyer l'image arrivée à `now` ? (`urgent` : une image clé
    /// est demandée.)
    fn due(&mut self, now: Instant, urgent: bool) -> bool {
        // Un peu de marge : une image arrivée juste avant l'heure compte.
        let slack = self.period / 8;
        if !urgent && self.next.is_some_and(|n| now + slack < n) {
            return false;
        }
        // En retard de plus d'une image (webcam lente) : on repart de maintenant.
        let next = match self.next {
            Some(n) if now < n + self.period => n + self.period,
            _ => now + self.period,
        };
        self.next = Some(next);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(w: u32, h: u32, f: FrameFormat, fps: u32) -> CameraFormat {
        CameraFormat::new_from(w, h, f, fps)
    }

    #[test]
    fn format_le_plus_proche_de_640x480() {
        let list = [
            fmt(1920, 1080, FrameFormat::MJPEG, 30),
            fmt(1280, 720, FrameFormat::MJPEG, 30),
            fmt(640, 480, FrameFormat::MJPEG, 30),
            fmt(640, 480, FrameFormat::YUYV, 30),
            fmt(320, 240, FrameFormat::YUYV, 30),
        ];
        assert_eq!(choose(&list), Some(fmt(640, 480, FrameFormat::YUYV, 30)));
        // Pas de 640×480 : plutôt plus grand que trop petit.
        assert_eq!(choose(&list[..2]), Some(fmt(1280, 720, FrameFormat::MJPEG, 30)));
        assert_eq!(
            choose(&[
                fmt(320, 240, FrameFormat::YUYV, 30),
                fmt(1920, 1080, FrameFormat::MJPEG, 30)
            ]),
            Some(fmt(1920, 1080, FrameFormat::MJPEG, 30))
        );
        // Le YUYV en 640×480 ne fait que 5 images/s sur cette webcam : le MJPEG.
        assert_eq!(
            choose(&[
                fmt(640, 480, FrameFormat::YUYV, 5),
                fmt(640, 480, FrameFormat::MJPEG, 30)
            ]),
            Some(fmt(640, 480, FrameFormat::MJPEG, 30))
        );
        assert_eq!(choose(&[]), None);
    }

    #[test]
    fn images_converties() {
        // YUYV : deux pixels gris (Y = 128, U = V = 128).
        let img = to_rgba(FrameFormat::YUYV, 2, 1, &[128, 128, 128, 128], Vec::new()).unwrap();
        assert_eq!(img.dimensions(), (2, 1));
        assert!(img.pixels().all(|p| p[0].abs_diff(128) < 4 && p[3] == 255));
        let img = to_rgba(FrameFormat::RAWBGR, 1, 1, &[1, 2, 3], Vec::new()).unwrap();
        assert_eq!(img.as_raw(), &[3, 2, 1, 255]);
        assert_eq!(
            to_rgba(FrameFormat::GRAY, 1, 1, &[7], Vec::new()).unwrap().as_raw(),
            &[7, 7, 7, 255]
        );
        assert!(to_rgba(FrameFormat::RAWRGB, 2, 2, &[0; 5], Vec::new()).is_err());
        // JPEG, comme le MJPEG de la plupart des webcams.
        let mut jpeg = Vec::new();
        image::RgbImage::from_pixel(16, 8, image::Rgb([200, 30, 30]))
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        let img = to_rgba(FrameFormat::MJPEG, 16, 8, &jpeg, Vec::new()).unwrap();
        assert_eq!(img.dimensions(), (16, 8));
        assert!(img.get_pixel(4, 4)[0] > 150 && img.get_pixel(4, 4)[3] == 255);
    }

    #[test]
    fn yuv_sans_passer_par_le_rgba() {
        // 2×2 en YUYV : Y0 U Y1 V par paire de pixels, sur deux lignes.
        let mut yuv = Yuv420::default();
        yuv.reset(2, 2);
        yuyv_to_i420(&[10, 100, 20, 200, 30, 110, 40, 210], 2, 2, &mut yuv).unwrap();
        assert_eq!(
            (yuv.y.as_slice(), yuv.u.as_slice(), yuv.v.as_slice()),
            (&[10, 20, 30, 40][..], &[105][..], &[205][..])
        );
        assert!(yuyv_to_i420(&[0; 7], 2, 2, &mut yuv).is_err());
        nv12_to_i420(&[1, 2, 3, 4, 50, 60], 2, 2, &mut yuv).unwrap();
        assert_eq!(
            (yuv.y.as_slice(), yuv.u.as_slice(), yuv.v.as_slice()),
            (&[1, 2, 3, 4][..], &[50][..], &[60][..])
        );
        assert!(nv12_to_i420(&[0; 5], 2, 2, &mut yuv).is_err());

        // Une image grise en YUV se code et se relit grise, à la même taille.
        let mut enc = video::Encoder::new(Profile::CAMERA).unwrap();
        yuv.reset(64, 48);
        yuv.y.fill(126);
        yuv.u.fill(128);
        yuv.v.fill(128);
        let e = enc.encode_yuv(&yuv, false).unwrap().expect("image sautée");
        let frame = crate::core::screen::Frame {
            seq: 0,
            key: e.key,
            camera: true,
            data: e.data.into(),
        };
        let p = video::Stream::new().push(frame, Instant::now()).unwrap();
        assert_eq!((p.width, p.height), (64, 48));
        assert!(
            p.rgba
                .chunks(4)
                .all(|px| px[0].abs_diff(px[2]) < 6 && px[0].abs_diff(128) < 20)
        );
    }

    #[test]
    fn rythme_d_envoi() {
        // Webcam à 30 images/s, envoi à 20 : deux images sur trois partent.
        let mut pace = Pace::new(20);
        let t = Instant::now();
        let sent = (0..30)
            .filter(|i| pace.due(t + Duration::from_millis(i * 1000 / 30), false))
            .count();
        assert!((19..=21).contains(&sent), "{sent} images");
        // Une image clé demandée part tout de suite.
        assert!(pace.due(t + Duration::from_millis(1001), true));
        // Webcam lente (10 images/s) : toutes partent.
        let mut pace = Pace::new(20);
        assert!((0..10).all(|i| pace.due(t + Duration::from_millis(i * 100), false)));
    }

    #[test]
    fn sans_webcam_rien_ne_plante() {
        if cameras().is_empty() && std::env::var_os("RIP_DISCORD_FAKE_CAMERA").is_none() {
            assert!(Camera::start(None).is_err());
        }
    }

    /// La mire passe par tout le chemin : capture, rythme, codage H.264, et
    /// se décode en 640×480.
    #[test]
    fn mire_codee_puis_decodee() {
        let mut source = Source::Pattern(0);
        let mut enc = video::Encoder::new(Profile::CAMERA).unwrap();
        let mut stream = video::Stream::new();
        let (mut seq, mut shown) = (0, 0);
        for _ in 0..6 {
            let Picture::Rgba(img) = Convert::default().picture(source.frame().unwrap()).unwrap() else {
                unreachable!()
            };
            if let Some(e) = enc.encode(&img, false).unwrap() {
                seq += 1;
                let frame = crate::core::screen::Frame {
                    seq,
                    key: e.key,
                    camera: true,
                    data: e.data.into(),
                };
                if let Some(p) = stream.push(frame, Instant::now()) {
                    assert_eq!((p.width, p.height), (640, 480));
                    shown += 1;
                }
            }
        }
        assert!(shown >= 4, "{shown} images");
    }
}
