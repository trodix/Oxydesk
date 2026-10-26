//! The login screen of a client that connects without a password: the form is drawn here (`render`), the keys of the
//! client edit it (`GreeterInput`), and the pump shows it as it would show a desktop (`GreeterCapture`). The state
//! and the typing rules are in `oxydesk_core::greeter`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ab_glyph::{point, Font, Glyph, PxScale, ScaleFont};

use oxydesk_core::error::{CaptureError, ClipboardError, InputError};
use oxydesk_core::greeter::{Action, Field, Greeter, Target};
use oxydesk_core::greeter_i18n::{self, Lang, Msg};
use oxydesk_core::greeter_theme::{Rgb, TextKey};
use oxydesk_core::layout::{Layout, OutputId};
use oxydesk_core::ports::{
    CaptureSource, ClipboardBackend, DesktopPorts, EncoderFactory, FrameEncoder, InputSink,
    PointerButton,
};
use oxydesk_core::wake::Wake;
use oxydesk_core::{CursorShape, CursorUpdate};
use oxydesk_core::{Frame, FrameData, PixelFormat, XkbLayout};

use crate::greeter_theme::Theme;

/// Creates the encoder of one output of the login screen (the composition root provides the same encoder as for a
/// desktop).
pub type GreeterEncoder = Arc<dyn Fn(OutputId) -> Box<dyn FrameEncoder> + Send + Sync>;

pub type SharedGreeter = Arc<Mutex<Greeter>>;

/// Position and size of the first screen in the desktop, shared by the capture (which learns the layout) and the input
/// (which turns pointer positions into places on the form).
type Screen = Arc<Mutex<(i32, i32, u32, u32)>>;

/// Called with the user name, the password and the layout of the client when the form is submitted.
pub type Submit = Box<dyn Fn(String, String, Option<XkbLayout>) + Send>;

#[derive(Clone, Copy)]
enum Weight {
    Regular,
    Semibold,
}

impl Weight {
    fn semibold(self) -> bool {
        matches!(self, Self::Semibold)
    }
}

/// Width of `text` once drawn, `size` being the height of the em in pixels.
fn text_width(theme: &Theme, text: &str, size: f32, weight: Weight) -> f32 {
    let scaled = theme.font(weight.semibold()).as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut previous = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = previous {
            width += scaled.kern(p, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width
}

/// The end of `text` that fits in `max` pixels: what is being typed stays visible.
fn fit_tail(theme: &Theme, text: &str, max: f32, size: f32) -> String {
    let mut shown: &str = text;
    while text_width(theme, shown, size, Weight::Regular) > max {
        let mut chars = shown.chars();
        if chars.next().is_none() {
            break;
        }
        shown = chars.as_str();
    }
    shown.to_owned()
}

/// The start of `text` that fits in `max` pixels.
fn fit_head(theme: &Theme, text: &str, max: f32, size: f32) -> String {
    let mut shown = text.to_owned();
    while text_width(theme, &shown, size, Weight::Regular) > max && shown.pop().is_some() {}
    shown
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    std::array::from_fn(|i| (a[i] as f32 * (1.0 - t) + b[i] as f32 * t).round() as u8)
}

struct Canvas<'a> {
    w: usize,
    h: usize,
    px: Vec<u8>,
    theme: &'a Theme,
}

impl<'a> Canvas<'a> {
    /// A screen of `w` x `h` pixels with the background of the theme: a picture, or a gradient (with a soft glow
    /// above the centre unless the theme turns it off).
    fn new(w: u32, h: u32, theme: &'a Theme) -> Self {
        if let Some(picture) = theme.background(w, h) {
            return Self {
                w: w as usize,
                h: h as usize,
                px: picture.to_vec(),
                theme,
            };
        }
        let spec = theme.spec();
        let (w, h) = (w as usize, h as usize);
        let mut px = vec![255u8; w * h * 4];
        for y in 0..h {
            let t = y as f32 / h.max(1) as f32;
            let base = mix(spec.background_top, spec.background_bottom, t);
            for x in 0..w {
                let c = if spec.background_glow {
                    let dx = (x as f32 - w as f32 / 2.0) / w.max(1) as f32;
                    let dy = (y as f32 - h as f32 * 0.35) / h.max(1) as f32;
                    let glow = (1.0 - (dx * dx * 2.5 + dy * dy * 4.0).sqrt() * 1.6).clamp(0.0, 1.0);
                    mix(base, [169, 139, 255], glow * 0.16)
                } else {
                    base
                };
                px[(y * w + x) * 4..][..3].copy_from_slice(&c);
            }
        }
        Self { w, h, px, theme }
    }

    /// Paints `c` over the pixel with the given coverage (0 to 1).
    fn blend(&mut self, x: i32, y: i32, c: Rgb, coverage: f32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h || coverage <= 0.0 {
            return;
        }
        let at = (y as usize * self.w + x as usize) * 4;
        let under: Rgb = [self.px[at], self.px[at + 1], self.px[at + 2]];
        self.px[at..at + 3].copy_from_slice(&mix(under, c, coverage.min(1.0)));
    }

    /// A rounded rectangle with smooth edges, painted by `paint(x, y)` (a flat colour or a gradient) at `alpha`.
    fn round_rect(
        &mut self,
        (x, y, w, h): (f32, f32, f32, f32),
        radius: f32,
        alpha: f32,
        paint: impl Fn(f32, f32) -> Rgb,
    ) {
        let r = radius.min(w / 2.0).min(h / 2.0);
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        for py in (y.floor() as i32 - 1)..=((y + h).ceil() as i32 + 1) {
            for px in (x.floor() as i32 - 1)..=((x + w).ceil() as i32 + 1) {
                let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
                let qx = (fx - cx).abs() - (w / 2.0 - r);
                let qy = (fy - cy).abs() - (h / 2.0 - r);
                let distance =
                    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                self.blend(
                    px,
                    py,
                    paint(fx, fy),
                    (0.5 - distance).clamp(0.0, 1.0) * alpha,
                );
            }
        }
    }

    fn fill(&mut self, rect: (f32, f32, f32, f32), radius: f32, c: Rgb) {
        self.round_rect(rect, radius, 1.0, |_, _| c);
    }

    /// A rounded rectangle with a border of `t` pixels around the `inside` colour.
    fn bordered(
        &mut self,
        rect: (f32, f32, f32, f32),
        radius: f32,
        t: f32,
        border: Rgb,
        inside: Rgb,
    ) {
        self.fill(rect, radius, border);
        let (x, y, w, h) = rect;
        self.fill(
            (x + t, y + t, w - 2.0 * t, h - 2.0 * t),
            (radius - t).max(0.0),
            inside,
        );
    }

    /// A soft shadow: rectangles growing and fading around `rect`.
    fn shadow(&mut self, (x, y, w, h): (f32, f32, f32, f32), radius: f32, spread: f32) {
        let steps = 14;
        for i in (0..steps).rev() {
            let g = spread * (i as f32 + 1.0) / steps as f32;
            self.round_rect(
                (x - g, y - g + spread * 0.45, w + 2.0 * g, h + 2.0 * g),
                radius + g,
                0.035,
                |_, _| [0, 0, 0],
            );
        }
    }

    /// Draws `text` with its baseline at `y` from `x`, anti-aliased; returns the width drawn.
    fn text(&mut self, x: f32, y: f32, text: &str, size: f32, weight: Weight, c: Rgb) -> f32 {
        let font = self.theme.font(weight.semibold()).clone();
        let scaled = font.as_scaled(PxScale::from(size));
        let mut cx = x;
        let mut previous = None;
        for ch in text.chars() {
            let id = scaled.glyph_id(ch);
            if let Some(p) = previous {
                cx += scaled.kern(p, id);
            }
            let glyph: Glyph = id.with_scale_and_position(PxScale::from(size), point(cx, y));
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, coverage| {
                    self.blend(
                        bounds.min.x as i32 + gx as i32,
                        bounds.min.y as i32 + gy as i32,
                        c,
                        coverage,
                    );
                });
            }
            cx += scaled.h_advance(id);
            previous = Some(id);
        }
        cx - x
    }

    fn centered(&mut self, cx: f32, y: f32, text: &str, size: f32, weight: Weight, c: Rgb) {
        let w = text_width(self.theme, text, size, weight);
        self.text(cx - w / 2.0, y, text, size, weight, c);
    }
}

/// Where the pieces of the form are on a screen of `width` x `height`, as (x, y, w, h) in pixels.
struct Form {
    s: f32,
    card: (f32, f32, f32, f32),
    fields: [(f32, f32, f32, f32); 2],
    button: (f32, f32, f32, f32),
}

/// Size in card units of the logo, or of the badge when the theme has none: the logo keeps its proportions, at the
/// height asked for unless that makes it wider than the card.
fn logo_units(theme: &Theme) -> (f32, f32) {
    let spec = theme.spec();
    match theme.logo() {
        Some(logo) => {
            let aspect = logo.w as f32 / logo.h as f32;
            let h = spec.logo_size.min((spec.card_width - 64.0) / aspect);
            (h * aspect, h)
        }
        None => (76.0, 76.0),
    }
}

fn form(theme: &Theme, width: u32, height: u32) -> Form {
    let spec = theme.spec();
    // Everything is drawn for a card of `card_width` x `card_height` units and scaled to the screen.
    let logo_h = logo_units(theme).1;
    let (card_w_units, card_h_units) = (spec.card_width, 364.0 + logo_h);
    let s = (height as f32 / 900.0)
        .clamp(0.6, 1.8)
        .min((width as f32 - 16.0) / card_w_units)
        .min((height as f32 - 16.0) / card_h_units)
        .max(0.05);
    let (card_w, card_h) = (card_w_units * s, card_h_units * s);
    let left = (width as f32 - card_w) / 2.0;
    let top = (height as f32 - card_h) / 2.0;
    let pad = 32.0 * s;
    let inner = card_w - 2.0 * pad;
    let box_h = 44.0 * s;
    let mut y = top + (120.0 + logo_h) * s;
    let mut fields = [(0.0, 0.0, 0.0, 0.0); 2];
    for field in &mut fields {
        y += 20.0 * s;
        *field = (left + pad, y, inner, box_h);
        y += box_h + 14.0 * s;
    }
    Form {
        s,
        card: (left, top, card_w, card_h),
        fields,
        button: (left + pad, y + 2.0 * s, inner, 46.0 * s),
    }
}

/// What is under the pointer at (`x`, `y`) on the first screen, if anything the form reacts to.
fn hit(theme: &Theme, width: u32, height: u32, x: i32, y: i32) -> Option<Target> {
    if width < 8 || height < 8 {
        return None;
    }
    let f = form(theme, width, height);
    let inside = |r: (f32, f32, f32, f32)| {
        let (x, y) = (x as f32, y as f32);
        x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
    };
    if inside(f.button) {
        Some(Target::Button)
    } else if inside(f.fields[0]) {
        Some(Target::Field(Field::User))
    } else if inside(f.fields[1]) {
        Some(Target::Field(Field::Password))
    } else {
        None
    }
}

/// Half period of the text caret blink.
const CARET_BLINK: Duration = Duration::from_millis(530);

/// Whether the caret is shown `idle` after the last change of the form: solid while typing, then blinking.
fn caret_visible(idle: Duration) -> bool {
    (idle.as_millis() / CARET_BLINK.as_millis()).is_multiple_of(2)
}

/// Paints `image` (RGBA, straight alpha) with its top-left corner at (`x`, `y`).
fn blit(c: &mut Canvas, image: &[u8], (w, h): (u32, u32), x: f32, y: f32) {
    let (x, y) = (x.round() as i32, y.round() as i32);
    for (i, p) in image.as_chunks::<4>().0.iter().enumerate() {
        let (px, py) = (i as u32 % w, i as u32 / w);
        c.blend(
            x + px as i32,
            y + py as i32,
            [p[0], p[1], p[2]],
            p[3] as f32 / 255.0,
        );
    }
    debug_assert_eq!(image.len(), (w * h * 4) as usize);
}

/// The login screen as RGBA pixels. `primary`: the screen carries the form; the others only the background.
/// `caret`: whether the text caret of the focused field is shown (it blinks).
pub fn render(
    greeter: &Greeter,
    theme: &Theme,
    width: u32,
    height: u32,
    primary: bool,
    caret: bool,
) -> Vec<u8> {
    let mut c = Canvas::new(width, height, theme);
    if !primary || width < 8 || height < 8 {
        return c.px;
    }
    let spec = theme.spec();
    let lang = spec.lang(Lang::from_layout(greeter.layout()));
    let Form { s, card, .. } = form(theme, width, height);
    let (left, top, card_w, card_h) = card;
    let centre = left + card_w / 2.0;
    c.shadow(card, spec.card_radius * s, 40.0 * s);
    c.bordered(
        card,
        spec.card_radius * s,
        1.0_f32.max(s),
        spec.card_border,
        spec.card,
    );

    // The logo of the theme, or a round badge with the first letter of the name.
    let (logo_w, logo_h) = logo_units(theme);
    let (bx, by) = (centre - logo_w * s / 2.0, top + 34.0 * s);
    if let Some(logo) = theme.logo() {
        let (pw, ph) = (
            (logo_w * s).round().max(1.0) as u32,
            (logo_h * s).round().max(1.0) as u32,
        );
        blit(&mut c, &logo.scaled(pw, ph), (pw, ph), bx, by);
    } else {
        let badge = logo_h * s;
        c.round_rect((bx, by, badge, badge), badge / 2.0, 1.0, |_, y| {
            mix(
                spec.accent,
                spec.accent_deep,
                ((y - by) / badge).clamp(0.0, 1.0),
            )
        });
        let initial = greeter.user().chars().next().map_or_else(
            || {
                spec.text(TextKey::Title, lang)
                    .chars()
                    .next()
                    .map_or("O".to_owned(), |ch| ch.to_uppercase().collect())
            },
            |ch| ch.to_uppercase().collect(),
        );
        c.centered(
            centre,
            by + badge * 0.69,
            &initial,
            badge * 0.5,
            Weight::Semibold,
            spec.button_text,
        );
    }
    c.centered(
        centre,
        top + (72.0 + logo_h) * s,
        spec.text(TextKey::Title, lang),
        26.0 * s,
        Weight::Semibold,
        spec.text,
    );
    c.centered(
        centre,
        top + (96.0 + logo_h) * s,
        spec.text(TextKey::Subtitle, lang),
        13.5 * s,
        Weight::Regular,
        spec.muted,
    );

    let pad = 32.0 * s;
    let inner = card_w - 2.0 * pad;
    let text_size = 17.0 * s;
    let form = form(theme, width, height);
    for ((label, field), rect) in [
        (spec.text(TextKey::UserLabel, lang), Field::User),
        (spec.text(TextKey::PasswordLabel, lang), Field::Password),
    ]
    .into_iter()
    .zip(form.fields)
    {
        let y = rect.1;
        let box_h = rect.3;
        c.text(
            left + pad,
            y - 8.0 * s,
            label,
            12.5 * s,
            Weight::Regular,
            spec.muted,
        );
        let focus = greeter.focus() == field && !greeter.busy();
        if focus {
            // A halo around the focused field.
            c.round_rect(
                (
                    rect.0 - 3.0 * s,
                    rect.1 - 3.0 * s,
                    rect.2 + 6.0 * s,
                    rect.3 + 6.0 * s,
                ),
                (spec.field_radius + 3.0) * s,
                0.28,
                |_, _| spec.accent,
            );
        }
        c.bordered(
            rect,
            spec.field_radius * s,
            1.5_f32.max(1.5 * s),
            if focus {
                spec.accent
            } else {
                spec.field_border
            },
            spec.field,
        );
        let room = inner - 28.0 * s;
        let baseline = y + box_h / 2.0 + text_size * 0.35;
        let shown = match field {
            Field::User => fit_tail(theme, greeter.user(), room, text_size),
            Field::Password => fit_tail(
                theme,
                &"\u{2022}".repeat(greeter.password_len()),
                room,
                text_size,
            ),
        };
        let drawn = c.text(
            left + pad + 14.0 * s,
            baseline,
            &shown,
            text_size,
            Weight::Regular,
            spec.text,
        );
        if focus && caret {
            c.fill(
                (
                    left + pad + 14.0 * s + drawn + 1.0 * s,
                    y + 10.0 * s,
                    1.6_f32.max(1.6 * s),
                    box_h - 20.0 * s,
                ),
                0.5,
                spec.accent,
            );
        }
    }

    // The button.
    let button = form.button;
    let busy = greeter.busy();
    let top_colour = if busy {
        mix(spec.accent, spec.card, 0.45)
    } else {
        spec.accent
    };
    let bottom_colour = if busy {
        mix(spec.accent_deep, spec.card, 0.45)
    } else {
        spec.accent_deep
    };
    c.round_rect(button, spec.field_radius * s, 1.0, |_, py| {
        mix(
            top_colour,
            bottom_colour,
            ((py - button.1) / button.3).clamp(0.0, 1.0),
        )
    });
    c.centered(
        centre,
        button.1 + button.3 / 2.0 + 17.0 * s * 0.35,
        if busy {
            spec.text(TextKey::BusyLabel, lang)
        } else {
            spec.text(TextKey::ButtonLabel, lang)
        },
        17.0 * s,
        Weight::Semibold,
        spec.button_text,
    );

    // What went wrong, or a warning.
    let note_y = button.1 + button.3 + 26.0 * s;
    let (note, colour) = match (greeter.message(), greeter.caps_lock()) {
        (Some(m), _) if !busy => (Some(m.text(lang)), spec.error),
        (_, true) => (
            Some(greeter_i18n::text(lang, Msg::CapsLock).to_owned()),
            spec.muted,
        ),
        _ => (None, spec.muted),
    };
    if let Some(note) = note {
        let shown = fit_head(theme, &note, inner, 14.0 * s);
        c.centered(centre, note_y, &shown, 14.0 * s, Weight::Regular, colour);
    }

    // The footer of the theme (a notice, a contact) below the card, over the background.
    let footer = spec.text(TextKey::Footer, lang);
    if !footer.is_empty() {
        let room = (width as f32 - 32.0).max(1.0);
        let shown = fit_head(theme, footer, room, 13.0 * s);
        c.centered(
            width as f32 / 2.0,
            top + card_h + 30.0 * s,
            &shown,
            13.0 * s,
            Weight::Regular,
            spec.muted,
        );
    }
    c.px
}

/// The pointer of the login screen: the usual arrow, black outline and white inside (`X` and `o`).
const ARROW: [&str; 19] = [
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
    "XoooooooooX",
    "XooooooXXXXX",
    "XooXooX",
    "XoX.XooX",
    "XX..XooX",
    "X....XooX",
    ".....XooX",
    "......XooX",
    ".......XX",
];

fn arrow() -> CursorShape {
    let (w, h) = (12usize, ARROW.len());
    let mut rgba = vec![0u8; w * h * 4];
    for (y, row) in ARROW.iter().enumerate() {
        for (x, c) in row.chars().enumerate() {
            let px = match c {
                'X' => [0, 0, 0, 255],
                'o' => [255, 255, 255, 255],
                _ => continue,
            };
            rgba[(y * w + x) * 4..][..4].copy_from_slice(&px);
        }
    }
    CursorShape::new(w as u16, h as u16, 0, 0, rgba).expect("the arrow is a valid pointer")
}

/// The login screen as the capture of a desktop. A frame is produced when the form or the size changed.
pub struct GreeterCapture {
    state: SharedGreeter,
    /// Size of each output.
    sizes: Vec<(u32, u32)>,
    /// Revision last drawn on each output (`None`: not yet).
    drawn: Vec<Option<(u64, bool)>>,
    /// Revision last seen and when it appeared, to blink the caret from there.
    changed: (u64, Instant),
    /// The pointer was handed to the pump.
    cursor_sent: bool,
    screen: Screen,
    theme: Arc<Theme>,
}

impl GreeterCapture {
    pub fn new(
        state: SharedGreeter,
        width: u32,
        height: u32,
        screen: Screen,
        theme: Arc<Theme>,
    ) -> Self {
        Self {
            theme,
            screen,
            state,
            sizes: vec![(width, height)],
            drawn: vec![None],
            changed: (u64::MAX, Instant::now()),
            cursor_sent: false,
        }
    }
}

impl CaptureSource for GreeterCapture {
    fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
        let i = output.0 as usize;
        let &(width, height) = self
            .sizes
            .get(i)
            .ok_or_else(|| CaptureError::Unavailable(format!("unknown output {}", output.0)))?;
        let greeter = self.state.lock().expect("greeter mutex");
        if self.changed.0 != greeter.revision() {
            self.changed = (greeter.revision(), Instant::now());
        }
        let caret = caret_visible(self.changed.1.elapsed());
        if self.drawn[i] == Some((greeter.revision(), caret)) {
            return Ok(None);
        }
        self.drawn[i] = Some((greeter.revision(), caret));
        let px = render(&greeter, &self.theme, width, height, i == 0, caret);
        Ok(Some(Frame {
            output,
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Rgba8,
            data: FrameData::Shm(px),
        }))
    }

    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        (!std::mem::replace(&mut self.cursor_sent, true)).then(|| CursorUpdate::Shape(arrow()))
    }

    fn apply_layout(&mut self, layout: &Layout) -> Result<(), CaptureError> {
        if layout.outputs.is_empty() {
            return Err(CaptureError::ResizeUnsupported);
        }
        self.sizes = layout.outputs.iter().map(|o| (o.width, o.height)).collect();
        let first = &layout.outputs[0];
        *self.screen.lock().expect("screen mutex") = (first.x, first.y, first.width, first.height);
        self.drawn = vec![None; self.sizes.len()];
        Ok(())
    }
}

/// The keyboard of the client types in the form; Enter on a complete form calls `submit`.
pub struct GreeterInput {
    state: SharedGreeter,
    wake: Arc<Wake>,
    submit: Submit,
    screen: Screen,
    theme: Arc<Theme>,
    /// Last position of the pointer in the desktop.
    pointer: (i32, i32),
}

impl GreeterInput {
    pub fn new(
        state: SharedGreeter,
        wake: Arc<Wake>,
        submit: Submit,
        screen: Screen,
        theme: Arc<Theme>,
    ) -> Self {
        Self {
            state,
            wake,
            submit,
            screen,
            theme,
            pointer: (0, 0),
        }
    }

    /// Applies what the form did for a key or a click: redraw, and the login to try.
    fn finish(&mut self, act: impl FnOnce(&mut Greeter) -> Action) {
        let (action, layout) = {
            let mut g = self.state.lock().expect("greeter mutex");
            let before = g.revision();
            let action = act(&mut g);
            if let Action::Submit { .. } = action {
                g.start_login();
            }
            if g.revision() != before {
                self.wake.notify();
            }
            (action, g.layout().clone())
        };
        if let Action::Submit { user, password } = action {
            (self.submit)(user, password, Some(layout));
        }
    }
}

impl InputSink for GreeterInput {
    fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError> {
        self.finish(|g| g.key(scancode, pressed));
        Ok(())
    }

    fn set_keyboard_layout(&mut self, layout: &XkbLayout) -> Result<(), InputError> {
        self.state.lock().expect("greeter mutex").set_layout(layout);
        Ok(())
    }

    fn pointer_move(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        self.pointer = (x, y);
        Ok(())
    }

    fn pointer_button(&mut self, button: PointerButton, pressed: bool) -> Result<(), InputError> {
        if button != PointerButton::Left || !pressed {
            return Ok(());
        }
        let (sx, sy, w, h) = *self.screen.lock().expect("screen mutex");
        if let Some(target) = hit(&self.theme, w, h, self.pointer.0 - sx, self.pointer.1 - sy) {
            self.finish(|g| g.click(target));
        }
        Ok(())
    }
}

/// Nothing is shared before the login.
struct NoClipboard;

impl ClipboardBackend for NoClipboard {
    fn set_text(&mut self, _: &str) -> Result<(), ClipboardError> {
        Ok(())
    }
    fn poll_text(&mut self) -> Result<Option<String>, ClipboardError> {
        Ok(None)
    }
}

/// The ports of the login screen of a client whose first monitor is `width` x `height`.
pub fn ports(
    state: SharedGreeter,
    wake: Arc<Wake>,
    (width, height): (u32, u32),
    encoder: GreeterEncoder,
    submit: Submit,
    theme: Arc<Theme>,
) -> DesktopPorts {
    let screen: Screen = Arc::new(Mutex::new((0, 0, width, height)));
    let first = encoder(OutputId(0));
    let make: EncoderFactory = Box::new(move |id| encoder(id));
    DesktopPorts {
        capture: Box::new(GreeterCapture::new(
            state.clone(),
            width,
            height,
            screen.clone(),
            theme.clone(),
        )),
        encoders: vec![first],
        make_encoder: Some(make),
        input: Box::new(GreeterInput::new(state, wake, submit, screen, theme)),
        clipboard: Box::new(NoClipboard),
        audio: None,
        drives: None,
        printers: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_core::greeter_i18n::Notice;
    use oxydesk_core::greeter_theme::ThemeSpec;
    use oxydesk_core::layout::Layout;

    fn theme() -> Arc<Theme> {
        Arc::new(Theme::default())
    }

    fn state(user: &str) -> SharedGreeter {
        Arc::new(Mutex::new(Greeter::new(user)))
    }

    fn screen(w: u32, h: u32) -> Screen {
        Arc::new(Mutex::new((0, 0, w, h)))
    }

    fn centre(r: (f32, f32, f32, f32)) -> (i32, i32) {
        ((r.0 + r.2 / 2.0) as i32, (r.1 + r.3 / 2.0) as i32)
    }

    fn click_at(input: &mut GreeterInput, (x, y): (i32, i32)) {
        input.pointer_move(x, y).unwrap();
        input.pointer_button(PointerButton::Left, true).unwrap();
        input.pointer_button(PointerButton::Left, false).unwrap();
    }

    fn pixel(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * w + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2]]
    }

    #[test]
    fn the_form_is_drawn_at_the_centre_and_the_rest_is_background() {
        let g = Greeter::new("");
        let px = render(&g, &Theme::default(), 1280, 800, true, true);
        assert_eq!(px.len(), 1280 * 800 * 4);
        // Inside the card, away from the texts.
        assert_eq!(pixel(&px, 1280, 470, 400), ThemeSpec::default().card);
        assert_ne!(pixel(&px, 1280, 2, 2), ThemeSpec::default().card);
        // Opaque.
        assert!(px.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn the_other_screens_show_the_background_only() {
        let g = Greeter::new("alice");
        let px = render(&g, &Theme::default(), 1280, 800, false, true);
        assert!((0..1280).all(|x| pixel(&px, 1280, x, 400) != ThemeSpec::default().card));
    }

    #[test]
    fn the_caret_blinks_after_a_pause() {
        assert!(caret_visible(Duration::ZERO));
        assert!(caret_visible(Duration::from_millis(529)));
        assert!(!caret_visible(Duration::from_millis(530)));
        assert!(caret_visible(Duration::from_millis(1060)));
        let g = Greeter::new("alice");
        assert_ne!(
            render(&g, &Theme::default(), 800, 600, true, true),
            render(&g, &Theme::default(), 800, 600, true, false)
        );
    }

    fn themed(spec: ThemeSpec) -> Theme {
        Theme::load(spec)
    }

    #[test]
    fn the_colours_of_the_theme_are_the_ones_drawn() {
        let spec = ThemeSpec {
            card: [10, 20, 30],
            background_top: [1, 2, 3],
            background_bottom: [1, 2, 3],
            background_glow: false,
            ..ThemeSpec::default()
        };
        let px = render(&Greeter::new(""), &themed(spec), 1280, 800, true, true);
        assert_eq!(pixel(&px, 1280, 470, 400), [10, 20, 30]);
        // A flat background, with no glow, away from the card and its shadow.
        assert_eq!(pixel(&px, 1280, 2, 2), [1, 2, 3]);
        assert_eq!(pixel(&px, 1280, 1277, 797), [1, 2, 3]);
    }

    #[test]
    fn a_wider_card_moves_the_clickable_areas_with_it() {
        let wide = themed(ThemeSpec {
            card_width: 600.0,
            ..ThemeSpec::default()
        });
        let narrow = Theme::default();
        let (fw, fn_) = (form(&wide, 1920, 1080), form(&narrow, 1920, 1080));
        assert!(fw.card.2 > fn_.card.2 * 1.4);
        let (x, y) = centre(fw.fields[1]);
        assert_eq!(
            hit(&wide, 1920, 1080, x, y),
            Some(Target::Field(Field::Password))
        );
        let (bx, by) = centre(fw.button);
        assert_eq!(hit(&wide, 1920, 1080, bx, by), Some(Target::Button));
    }

    #[test]
    fn a_logo_replaces_the_badge_and_pushes_the_form_down() {
        let mut logo = Vec::new();
        for _ in 0..(40 * 20) {
            logo.extend_from_slice(&[0, 255, 0, 255]);
        }
        let with = Theme::load(ThemeSpec::default());
        let mut with_logo = Theme::load(ThemeSpec {
            logo_size: 120.0,
            ..ThemeSpec::default()
        });
        with_logo.set_logo_for_test(crate::greeter_theme::Image::from_rgba(40, 20, logo));
        // 120 units high but never wider than the card: 2:1 gives 240 x 120 units.
        let (w, h) = logo_units(&with_logo);
        assert_eq!((w, h), (240.0, 120.0));
        assert_eq!(logo_units(&with), (76.0, 76.0));
        assert!(form(&with_logo, 1280, 800).fields[0].1 > form(&with, 1280, 800).fields[0].1 - 1.0);
        let px = render(&Greeter::new("alice"), &with_logo, 1280, 800, true, true);
        let f = form(&with_logo, 1280, 800);
        let (cx, cy) = (
            (f.card.0 + f.card.2 / 2.0) as u32,
            (f.card.1 + (34.0 + 60.0) * f.s) as u32,
        );
        assert_eq!(pixel(&px, 1280, cx, cy), [0, 255, 0]);
        // The default theme draws the orange badge at the same place instead.
        let badge = render(&Greeter::new("alice"), &with, 1280, 800, true, true);
        let fb = form(&with, 1280, 800);
        let p = pixel(
            &badge,
            1280,
            (fb.card.0 + fb.card.2 / 2.0) as u32,
            (fb.card.1 + 40.0 * fb.s) as u32,
        );
        assert!(p[0] > 200 && p[2] < 100, "{p:?}");
    }

    #[test]
    fn a_very_wide_logo_is_shrunk_to_the_card() {
        let mut theme = Theme::load(ThemeSpec {
            logo_size: 100.0,
            ..ThemeSpec::default()
        });
        theme.set_logo_for_test(crate::greeter_theme::Image::from_rgba(
            400,
            20,
            [255u8, 0, 0, 255].repeat(400 * 20),
        ));
        let (w, h) = logo_units(&theme);
        assert!(
            (w - 336.0).abs() < 0.01 && (h - 16.8).abs() < 0.01,
            "{w}x{h}"
        );
    }

    #[test]
    fn the_texts_of_the_theme_change_the_picture_and_the_footer_is_drawn() {
        let base = render(&Greeter::new(""), &Theme::default(), 1280, 800, true, true);
        let retitled = themed(
            ThemeSpec::default()
                .with_text(TextKey::Title, "ACME Corp")
                .with_text(TextKey::ButtonLabel, "Log in"),
        );
        assert_ne!(
            render(&Greeter::new(""), &retitled, 1280, 800, true, true),
            base
        );
        let footer = themed(ThemeSpec::default().with_text(TextKey::Footer, "Authorised use only"));
        let with = render(&Greeter::new(""), &footer, 1280, 800, true, true);
        let f = form(&footer, 1280, 800);
        let below = (f.card.1 + f.card.3) as u32 + 8..(f.card.1 + f.card.3) as u32 + 40;
        let differs = |a: &[u8], b: &[u8]| {
            below
                .clone()
                .any(|y| (400..880).any(|x| pixel(a, 1280, x, y) != pixel(b, 1280, x, y)))
        };
        assert!(differs(&with, &base));
    }

    #[test]
    fn the_screen_speaks_the_language_of_the_keyboard() {
        let theme = Theme::default();
        let english = Greeter::new("");
        let mut french = Greeter::new("");
        french.set_layout(&XkbLayout::new("fr", ""));
        let mut german = Greeter::new("");
        german.set_layout(&XkbLayout::new("de", ""));
        let pictures: Vec<_> = [&english, &french, &german]
            .into_iter()
            .map(|g| render(g, &theme, 1280, 800, true, true))
            .collect();
        assert_ne!(pictures[0], pictures[1]);
        assert_ne!(pictures[1], pictures[2]);
        assert_ne!(pictures[0], pictures[2]);
        // A keyboard that says nothing keeps English.
        let mut unknown = Greeter::new("");
        unknown.set_layout(&XkbLayout::new("jp", ""));
        assert_eq!(render(&unknown, &theme, 1280, 800, true, true), pictures[0]);
    }

    #[test]
    fn the_theme_can_fix_the_language_whatever_the_keyboard() {
        let mut french = Greeter::new("");
        french.set_layout(&XkbLayout::new("fr", ""));
        let fixed = themed(ThemeSpec {
            language: Some(Lang::En),
            ..ThemeSpec::default()
        });
        assert_eq!(
            render(&french, &fixed, 1280, 800, true, true),
            render(&Greeter::new(""), &fixed, 1280, 800, true, true)
        );
    }

    #[test]
    fn a_change_of_keyboard_redraws_the_screen() {
        let s = state("");
        let mut cap = GreeterCapture::new(s.clone(), 640, 480, screen(640, 480), theme());
        assert!(cap.recv_latest(OutputId(0)).unwrap().is_some());
        s.lock().unwrap().set_layout(&XkbLayout::new("fr", ""));
        // The change is seen at the next frame (maybe after a caret blink).
        let mut seen = false;
        for _ in 0..2 {
            seen |= cap.recv_latest(OutputId(0)).unwrap().is_some();
        }
        assert!(seen);
    }

    #[test]
    fn every_text_of_every_language_has_its_glyphs_in_the_font() {
        use ab_glyph::Font;
        let theme = Theme::default();
        for lang in Lang::ALL {
            let mut texts: Vec<String> = [
                Msg::Subtitle,
                Msg::UserName,
                Msg::Password,
                Msg::SignIn,
                Msg::SigningIn,
                Msg::CapsLock,
            ]
            .into_iter()
            .map(|m| greeter_i18n::text(lang, m).to_owned())
            .collect();
            texts.extend(
                [
                    Notice::WrongCredentials,
                    Notice::Blocked(5),
                    Notice::SessionFailed,
                ]
                .iter()
                .map(|n| n.text(lang)),
            );
            for text in texts {
                for ch in text.chars() {
                    for semibold in [false, true] {
                        assert!(
                            theme.font(semibold).glyph_id(ch).0 != 0,
                            "{lang:?}: no glyph for {ch:?} in {text:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_background_picture_is_drawn_behind_the_form() {
        let mut theme = Theme::default();
        theme.set_background_for_test(crate::greeter_theme::Image::from_rgba(
            2,
            2,
            [0, 0, 200, 255].repeat(4),
        ));
        let px = render(&Greeter::new(""), &theme, 1280, 800, true, true);
        assert_eq!(pixel(&px, 1280, 2, 2), [0, 0, 200]);
        // The other screens show it too.
        let other = render(&Greeter::new(""), &theme, 640, 480, false, true);
        assert_eq!(pixel(&other, 640, 300, 200), [0, 0, 200]);
        // The card is still over it.
        assert_eq!(pixel(&px, 1280, 470, 400), ThemeSpec::default().card);
    }

    #[test]
    fn typing_changes_the_picture() {
        let mut g = Greeter::new("alice");
        let before = render(&g, &Theme::default(), 800, 600, true, true);
        g.key(0x1E, true);
        let after = render(&g, &Theme::default(), 800, 600, true, true);
        assert_ne!(before, after);
    }

    #[test]
    fn an_error_is_drawn_in_its_colour() {
        let mut g = Greeter::new("alice");
        g.login_failed(Notice::WrongCredentials);
        // Red, not the orange of the accent (whose blue is lower than its green).
        let reddish = |p: &[u8]| {
            let (r, g, b) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
            r > 150 && r > g + 50 && b + 10 >= g
        };
        let px = render(&g, &Theme::default(), 1280, 800, true, true);
        let n = px.chunks(4).filter(|p| reddish(p)).count();
        assert!(n > 20, "{n} error pixels");
        // The same screen without the message has none.
        let clean = render(
            &Greeter::new("alice"),
            &Theme::default(),
            1280,
            800,
            true,
            true,
        );
        assert!(!clean.chunks(4).any(reddish));
    }

    #[test]
    fn a_tiny_screen_does_not_panic_nor_overflow() {
        let mut g = Greeter::new(&"x".repeat(64));
        g.login_failed(Notice::Text("y".repeat(200)));
        for (w, h) in [(1, 1), (16, 16), (100, 80), (320, 200), (4000, 3000)] {
            assert_eq!(
                render(&g, &Theme::default(), w, h, true, true).len(),
                (w * h * 4) as usize
            );
        }
    }

    #[test]
    fn the_text_is_smooth_not_blocky() {
        // Anti-aliasing gives many intermediate shades between the text colour and what is under it.
        let px = render(
            &Greeter::new("alice"),
            &Theme::default(),
            1280,
            800,
            true,
            true,
        );
        let shades: std::collections::HashSet<_> =
            px.chunks(4).map(|p| [p[0], p[1], p[2]]).collect();
        assert!(shades.len() > 100, "{} colours", shades.len());
    }

    #[test]
    fn a_name_wider_than_the_field_shows_its_end() {
        let long = "x".repeat(60) + "END";
        let mut g = Greeter::new(&long);
        g.login_failed(Notice::Text(String::new()));
        let wide = text_width(&Theme::default(), &long, 17.0, Weight::Regular);
        let shown = fit_tail(&Theme::default(), &long, 300.0, 17.0);
        assert!(wide > 300.0 && shown.ends_with("END"));
        assert!(text_width(&Theme::default(), &shown, 17.0, Weight::Regular) <= 300.0);
        assert_eq!(fit_head(&Theme::default(), "abcdef", 0.0, 17.0), "");
    }

    #[test]
    #[ignore = "writes the picture to $OXYDESK_GREETER_PNG for a look"]
    fn picture() {
        let Ok(path) = std::env::var("OXYDESK_GREETER_PNG") else {
            return;
        };
        let mut g = Greeter::new("sebastien");
        for _ in 0..6 {
            g.key(0x1E, true);
            g.key(0x1E, false);
        }
        let (w, h) = (1920, 1080);
        // OXYDESK_GREETER_THEME: draw it with that theme file.
        let theme = std::env::var("OXYDESK_GREETER_THEME").map_or_else(
            |_| Theme::default(),
            |f| Theme::from_file(f.as_ref()).unwrap(),
        );
        let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .unwrap()
            .write_image_data(&render(&g, &theme, w, h, true, true))
            .unwrap();
    }

    #[test]
    fn the_clickable_areas_are_where_the_form_is_drawn() {
        let g = Greeter::new("");
        for (w, h) in [(1280, 800), (1920, 1080), (640, 480)] {
            let f = form(&Theme::default(), w, h);
            let px = render(&g, &Theme::default(), w, h, true, true);
            let (bx, by) = centre(f.button);
            assert_eq!(hit(&Theme::default(), w, h, bx, by), Some(Target::Button));
            // The button is the orange one, the fields are dark.
            let p = pixel(&px, w, bx as u32 + (f.button.2 / 2.0) as u32 - 6, by as u32);
            assert!(p[0] > 200 && p[2] < 120, "{w}x{h}: {p:?}");
            for (i, field) in [Field::User, Field::Password].into_iter().enumerate() {
                let (x, y) = centre(f.fields[i]);
                assert_eq!(
                    hit(&Theme::default(), w, h, x, y),
                    Some(Target::Field(field))
                );
                let p = pixel(
                    &px,
                    w,
                    x as u32 + (f.fields[i].2 / 2.0) as u32 - 6,
                    y as u32,
                );
                assert_eq!(p, ThemeSpec::default().field, "{w}x{h}");
            }
            assert_eq!(hit(&Theme::default(), w, h, 1, 1), None);
        }
        assert_eq!(hit(&Theme::default(), 4, 4, 2, 2), None);
    }

    #[test]
    fn a_click_on_a_field_gives_it_the_cursor_and_redraws() {
        let s = state("alice");
        let wake = Arc::new(Wake::default());
        let mut input = GreeterInput::new(
            s.clone(),
            wake.clone(),
            Box::new(|_, _, _| {}),
            screen(1280, 800),
            theme(),
        );
        let f = form(&Theme::default(), 1280, 800);
        let seen = wake.generation();
        click_at(&mut input, centre(f.fields[0]));
        assert_eq!(s.lock().unwrap().focus(), Field::User);
        assert_ne!(wake.generation(), seen);
        click_at(&mut input, centre(f.fields[1]));
        assert_eq!(s.lock().unwrap().focus(), Field::Password);
    }

    #[test]
    fn a_click_outside_or_with_another_button_does_nothing() {
        let s = state("alice");
        let wake = Arc::new(Wake::default());
        let mut input = GreeterInput::new(
            s.clone(),
            wake.clone(),
            Box::new(|_, _, _| {}),
            screen(1280, 800),
            theme(),
        );
        let f = form(&Theme::default(), 1280, 800);
        let seen = wake.generation();
        click_at(&mut input, (2, 2));
        let (x, y) = centre(f.fields[0]);
        input.pointer_move(x, y).unwrap();
        input.pointer_button(PointerButton::Right, true).unwrap();
        input.pointer_button(PointerButton::Left, false).unwrap();
        assert_eq!(s.lock().unwrap().focus(), Field::Password);
        assert_eq!(wake.generation(), seen);
    }

    #[test]
    fn the_button_signs_in_with_what_was_typed() {
        let s = state("alice");
        let sent = Arc::new(Mutex::new(Vec::new()));
        let log = sent.clone();
        let mut input = GreeterInput::new(
            s.clone(),
            Arc::new(Wake::default()),
            Box::new(move |u, p, _| log.lock().unwrap().push((u, p))),
            screen(1280, 800),
            theme(),
        );
        let button = centre(form(&Theme::default(), 1280, 800).button);
        // Nothing typed yet: no login, the cursor stays on the password.
        click_at(&mut input, button);
        assert!(sent.lock().unwrap().is_empty());
        input.key(0x1E, true).unwrap();
        click_at(&mut input, button);
        assert_eq!(
            sent.lock().unwrap().as_slice(),
            [("alice".to_owned(), "a".to_owned())]
        );
        assert!(s.lock().unwrap().busy());
        // A second click while signing in sends nothing more.
        click_at(&mut input, button);
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_form_follows_the_position_of_the_first_screen() {
        let s = state("alice");
        let shared = screen(1280, 800);
        let mut cap = GreeterCapture::new(s.clone(), 1280, 800, shared.clone(), theme());
        let mut input = GreeterInput::new(
            s.clone(),
            Arc::new(Wake::default()),
            Box::new(|_, _, _| {}),
            shared,
            theme(),
        );
        let mut layout = Layout::side_by_side(1, 1920, 1080);
        layout.outputs[0].x = 500;
        layout.outputs[0].y = 40;
        cap.apply_layout(&layout).unwrap();
        let (x, y) = centre(form(&Theme::default(), 1920, 1080).fields[0]);
        click_at(&mut input, (x + 500, y + 40));
        assert_eq!(s.lock().unwrap().focus(), Field::User);
    }

    #[test]
    fn the_capture_gives_a_frame_only_when_something_changed() {
        let s = state("alice");
        let mut cap = GreeterCapture::new(s.clone(), 640, 480, screen(640, 480), theme());
        let first = cap.recv_latest(OutputId(0)).unwrap().expect("first frame");
        assert_eq!((first.width, first.height, first.stride), (640, 480, 2560));
        assert!(cap.recv_latest(OutputId(0)).unwrap().is_none());
        s.lock().unwrap().key(0x1E, true);
        assert!(cap.recv_latest(OutputId(0)).unwrap().is_some());
        assert!(cap.recv_latest(OutputId(1)).is_err());
    }

    #[test]
    fn the_login_screen_has_a_pointer_given_once() {
        let mut cap = GreeterCapture::new(state(""), 640, 480, screen(640, 480), theme());
        let Some(CursorUpdate::Shape(s)) = cap.take_cursor() else {
            panic!("a pointer shape");
        };
        assert_eq!((s.width, s.height, s.hot_x, s.hot_y), (12, 19, 0, 0));
        // The outline and the inside are opaque, the rest is transparent.
        assert_eq!(&s.rgba[..4], &[0, 0, 0, 255]);
        assert_eq!(&s.rgba[(2 * 12 + 1) * 4..][..4], &[255, 255, 255, 255]);
        assert_eq!(s.rgba[11 * 4 + 3], 0);
        assert!(cap.take_cursor().is_none());
    }

    #[test]
    fn a_new_layout_resizes_the_screens_and_draws_them_again() {
        let s = state("");
        let mut cap = GreeterCapture::new(s, 640, 480, screen(640, 480), theme());
        cap.recv_latest(OutputId(0)).unwrap();
        cap.apply_layout(&Layout::side_by_side(2, 800, 600))
            .unwrap();
        let a = cap.recv_latest(OutputId(0)).unwrap().unwrap();
        let b = cap.recv_latest(OutputId(1)).unwrap().unwrap();
        assert_eq!((a.width, b.width, b.height), (800, 800, 600));
        assert!(cap.apply_layout(&Layout::default()).is_err());
    }

    #[test]
    fn enter_submits_what_was_typed_with_the_layout_of_the_client() {
        let s = state("alice");
        let sent = Arc::new(Mutex::new(Vec::new()));
        let log = sent.clone();
        let wake = Arc::new(Wake::default());
        let seen = wake.generation();
        let mut input = GreeterInput::new(
            s.clone(),
            wake.clone(),
            Box::new(move |u, p, l| log.lock().unwrap().push((u, p, l))),
            screen(1280, 800),
            theme(),
        );
        input
            .set_keyboard_layout(&XkbLayout::new("fr", ""))
            .unwrap();
        for code in [0x10, 0x1E] {
            input.key(code, true).unwrap();
            input.key(code, false).unwrap();
        }
        assert_ne!(wake.generation(), seen);
        input.key(0x1C, true).unwrap();
        let sent = sent.lock().unwrap();
        assert_eq!(
            sent.as_slice(),
            [(
                "alice".to_owned(),
                "aq".to_owned(),
                Some(XkbLayout::new("fr", ""))
            )]
        );
        assert!(s.lock().unwrap().busy());
    }

    #[test]
    fn the_ports_have_one_encoder_and_a_factory_for_more() {
        struct Enc;
        impl FrameEncoder for Enc {
            fn encode(
                &mut self,
                _: &Frame,
            ) -> Result<oxydesk_core::ports::EncodedFrame, oxydesk_core::error::EncodeError>
            {
                unreachable!("not used")
            }
        }
        let p = ports(
            state(""),
            Arc::new(Wake::default()),
            (640, 480),
            Arc::new(|_| Box::new(Enc)),
            Box::new(|_, _, _| {}),
            theme(),
        );
        assert_eq!(p.encoders.len(), 1);
        assert!(p.make_encoder.is_some());
        assert!(p.audio.is_none() && p.drives.is_none() && p.printers.is_none());
    }
}
