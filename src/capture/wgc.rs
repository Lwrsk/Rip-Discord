//! Capture par Windows.Graphics.Capture : une session continue sur un écran
//! ou une fenêtre. Le système livre une image à chaque changement ; on garde
//! la dernière, au rythme demandé.

use std::{
    ffi::c_void,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use image::RgbaImage;
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl},
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
        SecondaryWindowSettings, Settings,
    },
    window::Window,
};

use super::Source;

#[derive(Default)]
struct Latest {
    image: Option<RgbaImage>,
    /// La fenêtre a été fermée.
    closed: bool,
    /// Écart minimal entre deux images copiées (un jeu peut en livrer 144/s).
    gap: Duration,
    /// Tampon d'une image déjà encodée, rendu pour y copier la suivante.
    spare: Option<Vec<u8>>,
}

type Slot = Arc<Mutex<Latest>>;

struct Handler {
    slot: Slot,
    last: Option<Instant>,
    buf: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Slot;
    type Error = anyhow::Error;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            slot: ctx.flags,
            last: None,
            buf: Vec::new(),
        })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, _: InternalCaptureControl) -> Result<(), Self::Error> {
        let gap = self.slot.lock().unwrap().gap;
        if self.last.is_some_and(|t| t.elapsed() < gap) {
            return Ok(());
        }
        self.last = Some(Instant::now());
        let buffer = frame.buffer()?;
        let (w, h) = (buffer.width(), buffer.height());
        // Pas d'allocation par image : la précédente, pas encore prise, ou
        // un tampon rendu par l'encodeur.
        let mut pixels = {
            let mut l = self.slot.lock().unwrap();
            l.image
                .take()
                .map(RgbaImage::into_raw)
                .or_else(|| l.spare.take())
                .unwrap_or_default()
        };
        pixels.clear();
        pixels.extend_from_slice(buffer.as_nopadding_buffer(&mut self.buf));
        if let Some(img) = RgbaImage::from_raw(w, h, pixels) {
            self.slot.lock().unwrap().image = Some(img);
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.slot.lock().unwrap().closed = true;
        Ok(())
    }
}

pub struct Session {
    control: Option<CaptureControl<Handler, anyhow::Error>>,
    slot: Slot,
    source: Source,
    cursor: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(c) = self.control.take() {
            let _ = c.stop();
        }
    }
}

/// Les identifiants xcap sont les poignées Windows tronquées à 32 bits ; une
/// poignée se ré-étend par le signe.
fn handle(id: u32) -> *mut c_void {
    id as i32 as isize as *mut c_void
}

impl Session {
    pub fn start(source: Source, cursor: bool) -> Result<Self> {
        if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
            bail!("non prise en charge par ce Windows");
        }
        let slot = Slot::default();
        // Sans le cadre jaune autour de la fenêtre partagée quand Windows le
        // permet (Windows 11) ; sinon avec.
        let borderless = GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false);
        let control = match launch(source, cursor, borderless, slot.clone()) {
            Ok(c) => c,
            Err(e) if borderless => {
                tracing::debug!("capture sans cadre refusée : {e:#}");
                launch(source, cursor, false, slot.clone())?
            }
            Err(e) => return Err(e),
        };
        let s = Self {
            control: Some(control),
            slot,
            source,
            cursor,
        };
        // La première image prouve que la capture marche (une fenêtre
        // réduite n'en donne pas : on n'attend pas indéfiniment).
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if s.slot.lock().unwrap().image.is_some() {
                break;
            }
            if s.closed() {
                bail!("source fermée");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(s)
    }

    pub fn grab(&mut self, cursor: bool, gap: Duration) -> Result<Option<RgbaImage>> {
        // Le curseur est un réglage de la session : on la relance.
        if cursor != self.cursor {
            let next = Self::start(self.source, cursor)?;
            *self = next;
        }
        let mut l = self.slot.lock().unwrap();
        l.gap = gap;
        if l.closed {
            bail!("source fermée");
        }
        Ok(l.image.take())
    }

    /// Rend le tampon d'une image dont on n'a plus besoin.
    pub fn recycle(&self, img: RgbaImage) {
        self.slot.lock().unwrap().spare = Some(img.into_raw());
    }

    pub fn closed(&self) -> bool {
        self.slot.lock().unwrap().closed || self.control.as_ref().is_none_or(|c| c.is_finished())
    }
}

fn launch(
    source: Source,
    cursor: bool,
    borderless: bool,
    slot: Slot,
) -> Result<CaptureControl<Handler, anyhow::Error>> {
    let cursor = if GraphicsCaptureApi::is_cursor_settings_supported().unwrap_or(false) {
        if cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        }
    } else {
        CursorCaptureSettings::Default
    };
    let border = if borderless {
        DrawBorderSettings::WithoutBorder
    } else {
        DrawBorderSettings::Default
    };
    macro_rules! go {
        ($item:expr) => {
            Handler::start_free_threaded(Settings::new(
                $item,
                cursor,
                border,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Rgba8,
                slot,
            ))
            .map_err(|e| anyhow!("{e}"))
        };
    }
    match source {
        Source::Monitor(id) => go!(Monitor::from_raw_hmonitor(handle(id))),
        Source::Window(id) => go!(Window::from_raw_hwnd(handle(id))),
    }
}
