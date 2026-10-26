//! A login screen theme once its files are read: the fonts, the logo and the background picture, decoded.
//!
//! The settings and their syntax are in `oxydesk_core::greeter_theme`; this is the adapter that reads the files they
//! name. A file that cannot be used is reported in the log and replaced by the built-in look, so a missing logo
//! never keeps anybody from signing in.

use std::path::Path;
use std::sync::{Arc, Mutex};

use ab_glyph::FontArc;
use oxydesk_core::greeter_theme::{self, Rgb, ThemeError, ThemeSpec};

/// Noto Sans (SIL Open Font License, see `assets/NotoSans-OFL.txt`), reduced to the Latin letters, digits and punctuation.
const REGULAR: &[u8] = include_bytes!("../assets/NotoSans-Regular.subset.ttf");
const SEMIBOLD: &[u8] = include_bytes!("../assets/NotoSans-SemiBold.subset.ttf");

/// Largest picture accepted, in pixels per side and in bytes of file.
const MAX_SIDE: u32 = 8192;
const MAX_FILE: u64 = 32 << 20;

/// A decoded picture: RGBA, straight alpha.
pub struct Image {
    pub w: u32,
    pub h: u32,
    rgba: Vec<u8>,
}

impl Image {
    #[cfg(test)]
    pub fn from_rgba(w: u32, h: u32, rgba: Vec<u8>) -> Self {
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        Self { w, h, rgba }
    }

    /// Decodes a PNG of any colour type and depth.
    pub fn decode_png(bytes: &[u8]) -> Result<Self, String> {
        let mut decoder = png::Decoder::new(bytes);
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
        let (w, h) = (reader.info().width, reader.info().height);
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return Err(format!("{w}x{h} pixels (at most {MAX_SIDE} per side)"));
        }
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        let data = &buf[..info.buffer_size()];
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        match info.color_type {
            png::ColorType::Rgba => rgba.extend_from_slice(data),
            png::ColorType::Rgb => data
                .as_chunks::<3>()
                .0
                .iter()
                .for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
            png::ColorType::Grayscale => data
                .iter()
                .for_each(|&g| rgba.extend_from_slice(&[g, g, g, 255])),
            png::ColorType::GrayscaleAlpha => data
                .as_chunks::<2>()
                .0
                .iter()
                .for_each(|p| rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]])),
            png::ColorType::Indexed => return Err("palette not expanded".into()),
        }
        Ok(Self { w, h, rgba })
    }

    /// Premultiplied RGBA of the pixel (x, y), edges repeated.
    fn at(&self, x: i64, y: i64) -> [f32; 4] {
        let x = x.clamp(0, self.w as i64 - 1) as usize;
        let y = y.clamp(0, self.h as i64 - 1) as usize;
        let p = &self.rgba[(y * self.w as usize + x) * 4..][..4];
        let a = p[3] as f32 / 255.0;
        [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, a]
    }

    /// The part `(x, y, w, h)` of the picture (in its pixels) drawn on `dw` x `dh` pixels: the mean of the pixels it
    /// covers when it shrinks, a bilinear sample when it grows. RGBA, straight alpha.
    pub fn scaled_region(&self, (x, y, w, h): (f32, f32, f32, f32), dw: u32, dh: u32) -> Vec<u8> {
        let (dw, dh) = (dw.max(1) as usize, dh.max(1) as usize);
        let (rx, ry) = (w / dw as f32, h / dh as f32);
        let mut out = vec![0u8; dw * dh * 4];
        for dy in 0..dh {
            for dx in 0..dw {
                let (x0, y0) = (x + dx as f32 * rx, y + dy as f32 * ry);
                let mut sum = [0.0f32; 4];
                if rx <= 1.0 && ry <= 1.0 {
                    // Bilinear around the centre of the destination pixel.
                    let (cx, cy) = (x0 + rx / 2.0 - 0.5, y0 + ry / 2.0 - 0.5);
                    let (ix, iy) = (cx.floor(), cy.floor());
                    let (fx, fy) = (cx - ix, cy - iy);
                    for (ox, oy, weight) in [
                        (0, 0, (1.0 - fx) * (1.0 - fy)),
                        (1, 0, fx * (1.0 - fy)),
                        (0, 1, (1.0 - fx) * fy),
                        (1, 1, fx * fy),
                    ] {
                        let p = self.at(ix as i64 + ox, iy as i64 + oy);
                        (0..4).for_each(|i| sum[i] += p[i] * weight);
                    }
                } else {
                    let (xa, xb) = (x0.floor() as i64, (x0 + rx).ceil() as i64);
                    let (ya, yb) = (y0.floor() as i64, (y0 + ry).ceil() as i64);
                    let (xb, yb) = (xb.max(xa + 1), yb.max(ya + 1));
                    for sy in ya..yb {
                        for sx in xa..xb {
                            let p = self.at(sx, sy);
                            (0..4).for_each(|i| sum[i] += p[i]);
                        }
                    }
                    let n = ((xb - xa) * (yb - ya)) as f32;
                    sum.iter_mut().for_each(|v| *v /= n);
                }
                let to = (dy * dw + dx) * 4;
                let a = sum[3];
                if a > 0.0 {
                    for i in 0..3 {
                        out[to + i] = (sum[i] / a).round().clamp(0.0, 255.0) as u8;
                    }
                }
                out[to + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
        out
    }

    /// The whole picture on `dw` x `dh` pixels.
    pub fn scaled(&self, dw: u32, dh: u32) -> Vec<u8> {
        self.scaled_region((0.0, 0.0, self.w as f32, self.h as f32), dw, dh)
    }

    /// The picture enlarged to cover `dw` x `dh` pixels, centred, the overflow cut off; transparent parts are
    /// painted over `under`. RGBA, opaque.
    fn covering(&self, dw: u32, dh: u32, under: Rgb) -> Vec<u8> {
        let ratio = (dw as f32 / self.w as f32).max(dh as f32 / self.h as f32);
        let (sw, sh) = (dw as f32 / ratio, dh as f32 / ratio);
        let region = (
            (self.w as f32 - sw) / 2.0,
            (self.h as f32 - sh) / 2.0,
            sw,
            sh,
        );
        let mut px = self.scaled_region(region, dw, dh);
        for p in px.as_chunks_mut::<4>().0 {
            let a = p[3] as f32 / 255.0;
            for i in 0..3 {
                p[i] = (p[i] as f32 * a + under[i] as f32 * (1.0 - a)).round() as u8;
            }
            p[3] = 255;
        }
        px
    }
}

/// The background picture drawn for a screen size.
type Covered = ((u32, u32), Arc<Vec<u8>>);

/// The login screen theme: its settings and what the files they name contain.
pub struct Theme {
    spec: ThemeSpec,
    regular: FontArc,
    semibold: FontArc,
    logo: Option<Image>,
    background: Option<Image>,
    /// The background picture drawn for the size it was last asked for.
    covered: Mutex<Option<Covered>>,
}

impl Default for Theme {
    fn default() -> Self {
        Self::load(ThemeSpec::default())
    }
}

fn read(path: &Path, what: &str) -> Option<Vec<u8>> {
    let size = std::fs::metadata(path).map(|m| m.len());
    match size {
        Ok(n) if n > MAX_FILE => {
            tracing::warn!(
                "greeter theme: {what} {} ignored: larger than {} MiB",
                path.display(),
                MAX_FILE >> 20
            );
            None
        }
        Ok(_) => match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                tracing::warn!("greeter theme: {what} {} ignored: {e}", path.display());
                None
            }
        },
        Err(e) => {
            tracing::warn!("greeter theme: {what} {} ignored: {e}", path.display());
            None
        }
    }
}

fn picture(path: &Option<std::path::PathBuf>, what: &str) -> Option<Image> {
    let path = path.as_ref()?;
    let bytes = read(path, what)?;
    match Image::decode_png(&bytes) {
        Ok(image) => Some(image),
        Err(e) => {
            tracing::warn!(
                "greeter theme: {what} {} ignored: not a usable PNG ({e})",
                path.display()
            );
            None
        }
    }
}

fn font(path: &Option<std::path::PathBuf>, what: &str) -> Option<FontArc> {
    let path = path.as_ref()?;
    let bytes = read(path, what)?;
    match FontArc::try_from_vec(bytes) {
        Ok(font) => Some(font),
        Err(e) => {
            tracing::warn!(
                "greeter theme: {what} {} ignored: not a TTF or OTF font ({e})",
                path.display()
            );
            None
        }
    }
}

impl Theme {
    /// The theme of `spec`, with the files it names read now.
    pub fn load(spec: ThemeSpec) -> Self {
        let regular = font(&spec.font, "font");
        let semibold = font(&spec.font_bold, "bold font").or_else(|| regular.clone());
        Self {
            regular: regular.unwrap_or_else(|| {
                FontArc::try_from_slice(REGULAR).expect("the regular font is valid")
            }),
            semibold: semibold.unwrap_or_else(|| {
                FontArc::try_from_slice(SEMIBOLD).expect("the semibold font is valid")
            }),
            logo: picture(&spec.logo, "logo"),
            background: picture(&spec.background_image, "background image"),
            covered: Mutex::new(None),
            spec,
        }
    }

    /// Reads the theme file `path`; an error in the file (syntax, unknown key) is returned, a missing picture or font is
    /// only reported in the log.
    pub fn from_file(path: &Path) -> Result<Self, ThemeError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ThemeError(format!("greeter theme {}: {e}", path.display())))?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        greeter_theme::parse(&text, &path.display().to_string(), base).map(Self::load)
    }

    #[cfg(test)]
    pub fn set_logo_for_test(&mut self, image: Image) {
        self.logo = Some(image);
    }

    #[cfg(test)]
    pub fn set_background_for_test(&mut self, image: Image) {
        self.background = Some(image);
    }

    pub fn spec(&self) -> &ThemeSpec {
        &self.spec
    }

    pub fn font(&self, semibold: bool) -> &FontArc {
        if semibold {
            &self.semibold
        } else {
            &self.regular
        }
    }

    pub fn logo(&self) -> Option<&Image> {
        self.logo.as_ref()
    }

    /// The background picture for a screen of `w` x `h` pixels (RGBA, opaque), if the theme has one.
    pub fn background(&self, w: u32, h: u32) -> Option<Arc<Vec<u8>>> {
        let image = self.background.as_ref()?;
        let mut cache = self.covered.lock().expect("theme cache mutex");
        match &*cache {
            Some((size, px)) if *size == (w, h) => Some(px.clone()),
            _ => {
                let px = Arc::new(image.covering(w, h, self.spec.background_bottom));
                *cache = Some(((w, h), px.clone()));
                Some(px)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ab_glyph::Font;

    fn png_of(color: png::ColorType, w: u32, h: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(data).unwrap();
        out
    }

    fn flat(w: u32, h: u32, rgba: [u8; 4]) -> Image {
        Image::from_rgba(w, h, rgba.repeat((w * h) as usize))
    }

    #[test]
    fn png_of_every_colour_type_decodes_to_rgba() {
        let rgba =
            Image::decode_png(&png_of(png::ColorType::Rgba, 1, 1, &[10, 20, 30, 40])).unwrap();
        assert_eq!(rgba.rgba, [10, 20, 30, 40]);
        let rgb = Image::decode_png(&png_of(png::ColorType::Rgb, 1, 1, &[1, 2, 3])).unwrap();
        assert_eq!(rgb.rgba, [1, 2, 3, 255]);
        let grey = Image::decode_png(&png_of(png::ColorType::Grayscale, 2, 1, &[7, 9])).unwrap();
        assert_eq!(grey.rgba, [7, 7, 7, 255, 9, 9, 9, 255]);
        let grey_a =
            Image::decode_png(&png_of(png::ColorType::GrayscaleAlpha, 1, 1, &[5, 128])).unwrap();
        assert_eq!(grey_a.rgba, [5, 5, 5, 128]);
    }

    #[test]
    fn a_palette_png_is_expanded() {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 2, 1);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_palette(vec![255, 0, 0, 0, 0, 255]);
        enc.write_header()
            .unwrap()
            .write_image_data(&[0, 1])
            .unwrap();
        let image = Image::decode_png(&out).unwrap();
        assert_eq!(&image.rgba[..3], &[255, 0, 0]);
        assert_eq!(&image.rgba[4..7], &[0, 0, 255]);
    }

    #[test]
    fn what_is_not_a_png_is_refused() {
        assert!(Image::decode_png(b"GIF89a....").is_err());
        assert!(Image::decode_png(&[]).is_err());
    }

    #[test]
    fn shrinking_averages_and_keeps_a_flat_colour() {
        let image = flat(8, 8, [200, 100, 50, 255]);
        let small = image.scaled(2, 2);
        assert!(small
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [200, 100, 50, 255]));
        // Half black, half white: the mean is grey.
        let mut halves = flat(2, 1, [0, 0, 0, 255]);
        halves.rgba[4..8].copy_from_slice(&[255, 255, 255, 255]);
        let one = halves.scaled(1, 1);
        assert!(one[0] >= 127 && one[0] <= 128, "{}", one[0]);
    }

    #[test]
    fn growing_keeps_a_flat_colour_and_the_alpha() {
        let image = flat(2, 2, [10, 200, 30, 128]);
        let big = image.scaled(7, 5);
        assert_eq!(big.len(), 7 * 5 * 4);
        assert!(big
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [10, 200, 30, 128]));
    }

    #[test]
    fn a_transparent_pixel_does_not_bleed_its_colour() {
        // Opaque red next to a transparent green one: the mean stays red, half transparent.
        let image = Image::from_rgba(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 0]);
        let one = image.scaled(1, 1);
        assert_eq!(one, [255, 0, 0, 128]);
    }

    #[test]
    fn the_background_covers_the_screen_and_is_cached() {
        let theme = Theme {
            background: Some(flat(40, 10, [9, 8, 7, 255])),
            ..Theme::default()
        };
        let a = theme.background(16, 9).unwrap();
        assert_eq!(a.len(), 16 * 9 * 4);
        assert!(a.as_chunks::<4>().0.iter().all(|p| *p == [9, 8, 7, 255]));
        assert!(Arc::ptr_eq(&a, &theme.background(16, 9).unwrap()));
        assert!(!Arc::ptr_eq(&a, &theme.background(8, 8).unwrap()));
        assert!(Theme::default().background(16, 9).is_none());
    }

    #[test]
    fn the_cover_crops_the_centre_instead_of_squeezing() {
        // Left third red, middle green, right third blue, on a wide picture shown on a square screen.
        let mut px = Vec::new();
        for x in 0..30 {
            px.extend_from_slice(match x / 10 {
                0 => &[255, 0, 0, 255],
                1 => &[0, 255, 0, 255],
                _ => &[0, 0, 255, 255],
            });
        }
        let theme = Theme {
            background: Some(Image::from_rgba(30, 1, px)),
            ..Theme::default()
        };
        let cover = theme.background(10, 10).unwrap();
        assert!(cover
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [0, 255, 0, 255]));
    }

    #[test]
    fn a_translucent_background_is_painted_over_the_bottom_colour() {
        let spec = ThemeSpec {
            background_bottom: [100, 100, 100],
            ..ThemeSpec::default()
        };
        let theme = Theme {
            background: Some(flat(2, 2, [200, 200, 200, 0])),
            ..Theme::load(spec)
        };
        assert_eq!(&theme.background(2, 2).unwrap()[..4], &[100, 100, 100, 255]);
    }

    #[test]
    fn a_theme_file_is_read_with_its_pictures_and_a_broken_one_falls_back() {
        let dir = std::env::temp_dir().join(format!("oxydesk-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("logo.png"),
            png_of(png::ColorType::Rgba, 1, 1, &[1, 2, 3, 255]),
        )
        .unwrap();
        std::fs::write(dir.join("bad.png"), b"not a png").unwrap();
        std::fs::write(dir.join("bad.ttf"), b"not a font").unwrap();
        std::fs::write(
            dir.join("theme.conf"),
            "title = ACME\nlogo = logo.png\nbackground-image = bad.png\nfont = bad.ttf\nfont-bold = missing.ttf\n",
        )
        .unwrap();
        let theme = Theme::from_file(&dir.join("theme.conf")).unwrap();
        assert_eq!(
            theme.spec().text(
                oxydesk_core::greeter_theme::TextKey::Title,
                Default::default()
            ),
            "ACME"
        );
        assert_eq!(theme.logo().map(|l| (l.w, l.h)), Some((1, 1)));
        // The bad picture and the bad fonts are replaced by the built-in look.
        assert!(theme.background(4, 4).is_none());
        assert!(theme.font(false).glyph_id('A').0 != 0);
        std::fs::write(dir.join("typo.conf"), "colour = #fff\n").unwrap();
        assert!(Theme::from_file(&dir.join("typo.conf")).is_err());
        assert!(Theme::from_file(&dir.join("absent.conf")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
