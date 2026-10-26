//! Curseur dessiné par-dessus l'image quand le système ne le capture pas
//! (capture classique) : une flèche, placée là où est la souris.

use device_query::{DeviceQuery, DeviceState};
use image::{Rgba, RgbaImage};

/// `X` contour noir, `o` intérieur blanc.
const ARROW: [&str; 17] = [
    "X",
    "XX",
    "XoX",
    "XooX",
    "XoooX",
    "XooooX",
    "XoooooX",
    "XooooooX",
    "XoooooooX",
    "XooooooooX",
    "XoooooXXXXX",
    "XooXooX",
    "XoX XooX",
    "XX  XooX",
    "X    XooX",
    "     XooX",
    "      XX",
];

pub struct Pointer(DeviceState);

impl Pointer {
    /// `None` si la position de la souris n'est pas lisible (Wayland, par exemple).
    pub fn new() -> Option<Self> {
        DeviceState::checked_new().map(Self)
    }

    /// Dessine la flèche sur une image dont le coin haut-gauche est en
    /// (`left`, `top`) sur le bureau.
    pub fn draw(&self, img: &mut RgbaImage, left: i32, top: i32) {
        let (x, y) = self.0.get_mouse().coords;
        // Plus gros sur les très grands écrans, comme le vrai curseur.
        let scale = (img.height() / 1100 + 1) as i32;
        draw_arrow(img, x - left, y - top, scale);
    }
}

pub fn draw_arrow(img: &mut RgbaImage, x: i32, y: i32, scale: i32) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    for (row, line) in ARROW.iter().enumerate() {
        for (col, c) in line.chars().enumerate() {
            let color = match c {
                'X' => Rgba([0, 0, 0, 255]),
                'o' => Rgba([255, 255, 255, 255]),
                _ => continue,
            };
            for dy in 0..scale {
                for dx in 0..scale {
                    let (px, py) = (x + col as i32 * scale + dx, y + row as i32 * scale + dy);
                    if (0..w).contains(&px) && (0..h).contains(&py) {
                        img.put_pixel(px as u32, py as u32, color);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleche_dans_l_image_et_au_bord() {
        let mut img = RgbaImage::from_pixel(40, 40, Rgba([50, 50, 50, 255]));
        draw_arrow(&mut img, 10, 10, 1);
        assert_eq!(img.get_pixel(10, 10), &Rgba([0, 0, 0, 255]));
        assert_eq!(img.get_pixel(11, 13), &Rgba([255, 255, 255, 255]));
        // À cheval sur le bord ou dehors : rien ne déborde.
        draw_arrow(&mut img, 35, -5, 2);
        draw_arrow(&mut img, -100, 500, 1);
    }
}
