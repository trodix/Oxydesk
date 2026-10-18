//! The login screen of a client that connects without a password: the form is drawn here (`render`), the keys of the
//! client edit it (`GreeterInput`), and the pump shows it as it would show a desktop (`GreeterCapture`). The state
//! and the typing rules are in `oxydesk_core::greeter`.

use std::sync::{Arc, Mutex, OnceLock};

use ab_glyph::{point, Font, FontRef, Glyph, PxScale, ScaleFont};

use oxydesk_core::error::{CaptureError, ClipboardError, InputError};
use oxydesk_core::greeter::{Action, Field, Greeter};
use oxydesk_core::layout::{Layout, OutputId};
use oxydesk_core::ports::{
    CaptureSource, ClipboardBackend, DesktopPorts, EncoderFactory, FrameEncoder, InputSink,
};
use oxydesk_core::wake::Wake;
use oxydesk_core::{CursorShape, CursorUpdate};
use oxydesk_core::{Frame, FrameData, PixelFormat, XkbLayout};

/// Creates the encoder of one output of the login screen (the composition root provides the same encoder as for a
/// desktop).
pub type GreeterEncoder = Arc<dyn Fn(OutputId) -> Box<dyn FrameEncoder> + Send + Sync>;

pub type SharedGreeter = Arc<Mutex<Greeter>>;

/// Called with the user name, the password and the layout of the client when the form is submitted.
pub type Submit = Box<dyn Fn(String, String, Option<XkbLayout>) + Send>;

type Rgb = [u8; 3];

const BACKGROUND_TOP: Rgb = [29, 40, 58];
const BACKGROUND_BOTTOM: Rgb = [9, 12, 19];
const CARD: Rgb = [31, 39, 54];
const CARD_BORDER: Rgb = [52, 64, 86];
const FIELD: Rgb = [20, 26, 38];
const FIELD_IDLE: Rgb = [55, 68, 92];
const ACCENT: Rgb = [76, 154, 255];
const ACCENT_DEEP: Rgb = [48, 112, 224];
const TEXT: Rgb = [235, 240, 247];
const MUTED: Rgb = [140, 154, 176];
const ERROR: Rgb = [255, 123, 114];

/// Noto Sans (SIL Open Font License, see `assets/NotoSans-OFL.txt`), reduced to the Latin letters, digits and punctuation.
const REGULAR: &[u8] = include_bytes!("../assets/NotoSans-Regular.subset.ttf");
const SEMIBOLD: &[u8] = include_bytes!("../assets/NotoSans-SemiBold.subset.ttf");

fn fonts() -> &'static (FontRef<'static>, FontRef<'static>) {
    static FONTS: OnceLock<(FontRef<'static>, FontRef<'static>)> = OnceLock::new();
    FONTS.get_or_init(|| {
        (
            FontRef::try_from_slice(REGULAR).expect("the regular font is valid"),
            FontRef::try_from_slice(SEMIBOLD).expect("the semibold font is valid"),
        )
    })
}

#[derive(Clone, Copy)]
enum Weight {
    Regular,
    Semibold,
}

fn font(weight: Weight) -> &'static FontRef<'static> {
    match weight {
        Weight::Regular => &fonts().0,
        Weight::Semibold => &fonts().1,
    }
}

/// Width of `text` once drawn, `size` being the height of the em in pixels.
fn text_width(text: &str, size: f32, weight: Weight) -> f32 {
    let scaled = font(weight).as_scaled(PxScale::from(size));
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
fn fit_tail(text: &str, max: f32, size: f32) -> String {
    let mut shown: &str = text;
    while text_width(shown, size, Weight::Regular) > max {
        let mut chars = shown.chars();
        if chars.next().is_none() {
            break;
        }
        shown = chars.as_str();
    }
    shown.to_owned()
}

/// The start of `text` that fits in `max` pixels.
fn fit_head(text: &str, max: f32, size: f32) -> String {
    let mut shown = text.to_owned();
    while text_width(&shown, size, Weight::Regular) > max && shown.pop().is_some() {}
    shown
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    std::array::from_fn(|i| (a[i] as f32 * (1.0 - t) + b[i] as f32 * t).round() as u8)
}

struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        let (w, h) = (w as usize, h as usize);
        let mut px = vec![255u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                // Dark gradient with a soft glow above the centre.
                let t = y as f32 / h.max(1) as f32;
                let base = mix(BACKGROUND_TOP, BACKGROUND_BOTTOM, t);
                let dx = (x as f32 - w as f32 / 2.0) / w.max(1) as f32;
                let dy = (y as f32 - h as f32 * 0.35) / h.max(1) as f32;
                let glow = (1.0 - (dx * dx * 2.5 + dy * dy * 4.0).sqrt() * 1.6).clamp(0.0, 1.0);
                let c = mix(base, [58, 92, 150], glow * 0.28);
                px[(y * w + x) * 4..][..3].copy_from_slice(&c);
            }
        }
        Self { w, h, px }
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
        let scaled = font(weight).as_scaled(PxScale::from(size));
        let mut cx = x;
        let mut previous = None;
        for ch in text.chars() {
            let id = scaled.glyph_id(ch);
            if let Some(p) = previous {
                cx += scaled.kern(p, id);
            }
            let glyph: Glyph = id.with_scale_and_position(PxScale::from(size), point(cx, y));
            if let Some(outline) = font(weight).outline_glyph(glyph) {
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
        let w = text_width(text, size, weight);
        self.text(cx - w / 2.0, y, text, size, weight, c);
    }
}

/// The login screen as RGBA pixels. `primary`: the screen carries the form; the others only the background.
pub fn render(greeter: &Greeter, width: u32, height: u32, primary: bool) -> Vec<u8> {
    let mut c = Canvas::new(width, height);
    if !primary || width < 8 || height < 8 {
        return c.px;
    }
    // Everything below is drawn for a card of 400 x 440 units and scaled to the screen.
    let s = (height as f32 / 900.0)
        .clamp(0.6, 1.8)
        .min((width as f32 - 16.0) / 400.0)
        .min((height as f32 - 16.0) / 440.0)
        .max(0.05);
    let (card_w, card_h) = (400.0 * s, 440.0 * s);
    let left = (width as f32 - card_w) / 2.0;
    let top = (height as f32 - card_h) / 2.0;
    let centre = left + card_w / 2.0;
    let card = (left, top, card_w, card_h);
    c.shadow(card, 18.0 * s, 40.0 * s);
    c.bordered(card, 18.0 * s, 1.0_f32.max(s), CARD_BORDER, CARD);

    // A round badge with the first letter of the name.
    let badge = 76.0 * s;
    let (bx, by) = (centre - badge / 2.0, top + 34.0 * s);
    c.round_rect((bx, by, badge, badge), badge / 2.0, 1.0, |_, y| {
        mix(ACCENT, ACCENT_DEEP, ((y - by) / badge).clamp(0.0, 1.0))
    });
    let initial = greeter
        .user()
        .chars()
        .next()
        .map_or("O".to_owned(), |ch| ch.to_uppercase().collect());
    c.centered(
        centre,
        by + badge * 0.69,
        &initial,
        badge * 0.5,
        Weight::Semibold,
        [255, 255, 255],
    );
    c.centered(
        centre,
        top + 148.0 * s,
        "Oxydesk",
        26.0 * s,
        Weight::Semibold,
        TEXT,
    );
    c.centered(
        centre,
        top + 172.0 * s,
        "Sign in to your desktop",
        13.5 * s,
        Weight::Regular,
        MUTED,
    );

    let pad = 32.0 * s;
    let inner = card_w - 2.0 * pad;
    let text_size = 17.0 * s;
    let mut y = top + 196.0 * s;
    for (label, field) in [("User name", Field::User), ("Password", Field::Password)] {
        c.text(
            left + pad,
            y + 12.0 * s,
            label,
            12.5 * s,
            Weight::Regular,
            MUTED,
        );
        y += 20.0 * s;
        let focus = greeter.focus() == field && !greeter.busy();
        let box_h = 44.0 * s;
        let rect = (left + pad, y, inner, box_h);
        if focus {
            // A halo around the focused field.
            c.round_rect(
                (
                    rect.0 - 3.0 * s,
                    rect.1 - 3.0 * s,
                    rect.2 + 6.0 * s,
                    rect.3 + 6.0 * s,
                ),
                11.0 * s,
                0.28,
                |_, _| ACCENT,
            );
        }
        c.bordered(
            rect,
            8.0 * s,
            1.5_f32.max(1.5 * s),
            if focus { ACCENT } else { FIELD_IDLE },
            FIELD,
        );
        let room = inner - 28.0 * s;
        let baseline = y + box_h / 2.0 + text_size * 0.35;
        let drawn = match field {
            Field::User => {
                let shown = fit_tail(greeter.user(), room, text_size);
                c.text(
                    left + pad + 14.0 * s,
                    baseline,
                    &shown,
                    text_size,
                    Weight::Regular,
                    TEXT,
                )
            }
            Field::Password => {
                let dots = "\u{2022}".repeat(greeter.password_len());
                let shown = fit_tail(&dots, room, text_size);
                c.text(
                    left + pad + 14.0 * s,
                    baseline,
                    &shown,
                    text_size,
                    Weight::Regular,
                    TEXT,
                )
            }
        };
        if focus {
            c.fill(
                (
                    left + pad + 14.0 * s + drawn + 1.0 * s,
                    y + 10.0 * s,
                    1.6_f32.max(1.6 * s),
                    box_h - 20.0 * s,
                ),
                0.5,
                ACCENT,
            );
        }
        y += box_h + 14.0 * s;
    }

    // The button.
    let button = (left + pad, y + 2.0 * s, inner, 46.0 * s);
    let busy = greeter.busy();
    let top_colour = if busy {
        mix(ACCENT, CARD, 0.45)
    } else {
        ACCENT
    };
    let bottom_colour = if busy {
        mix(ACCENT_DEEP, CARD, 0.45)
    } else {
        ACCENT_DEEP
    };
    c.round_rect(button, 8.0 * s, 1.0, |_, py| {
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
            "Signing in\u{2026}"
        } else {
            "Sign in"
        },
        17.0 * s,
        Weight::Semibold,
        [255, 255, 255],
    );

    // What went wrong, or a warning.
    let note_y = button.1 + button.3 + 26.0 * s;
    let (note, colour) = match (greeter.message(), greeter.caps_lock()) {
        (Some(m), _) if !busy => (Some(m), ERROR),
        (_, true) => (Some("Caps Lock is on"), MUTED),
        _ => (None, MUTED),
    };
    if let Some(note) = note {
        let shown = fit_head(note, inner, 14.0 * s);
        c.centered(centre, note_y, &shown, 14.0 * s, Weight::Regular, colour);
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
    drawn: Vec<Option<u64>>,
    /// The pointer was handed to the pump.
    cursor_sent: bool,
}

impl GreeterCapture {
    pub fn new(state: SharedGreeter, width: u32, height: u32) -> Self {
        Self {
            state,
            sizes: vec![(width, height)],
            drawn: vec![None],
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
        if self.drawn[i] == Some(greeter.revision()) {
            return Ok(None);
        }
        self.drawn[i] = Some(greeter.revision());
        let px = render(&greeter, width, height, i == 0);
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
        self.drawn = vec![None; self.sizes.len()];
        Ok(())
    }
}

/// The keyboard of the client types in the form; Enter on a complete form calls `submit`.
pub struct GreeterInput {
    state: SharedGreeter,
    wake: Arc<Wake>,
    submit: Submit,
}

impl GreeterInput {
    pub fn new(state: SharedGreeter, wake: Arc<Wake>, submit: Submit) -> Self {
        Self {
            state,
            wake,
            submit,
        }
    }
}

impl InputSink for GreeterInput {
    fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError> {
        let (action, layout) = {
            let mut g = self.state.lock().expect("greeter mutex");
            let before = g.revision();
            let action = g.key(scancode, pressed);
            if let Action::Submit { .. } = action {
                g.start_login("Signing in...");
            }
            if g.revision() != before {
                self.wake.notify();
            }
            (action, g.layout().clone())
        };
        if let Action::Submit { user, password } = action {
            (self.submit)(user, password, Some(layout));
        }
        Ok(())
    }

    fn set_keyboard_layout(&mut self, layout: &XkbLayout) -> Result<(), InputError> {
        self.state.lock().expect("greeter mutex").set_layout(layout);
        Ok(())
    }

    fn pointer_move(&mut self, _: i32, _: i32) -> Result<(), InputError> {
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
) -> DesktopPorts {
    let first = encoder(OutputId(0));
    let make: EncoderFactory = Box::new(move |id| encoder(id));
    DesktopPorts {
        capture: Box::new(GreeterCapture::new(state.clone(), width, height)),
        encoders: vec![first],
        make_encoder: Some(make),
        input: Box::new(GreeterInput::new(state, wake, submit)),
        clipboard: Box::new(NoClipboard),
        audio: None,
        drives: None,
        printers: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_core::layout::Layout;

    fn state(user: &str) -> SharedGreeter {
        Arc::new(Mutex::new(Greeter::new(user)))
    }

    fn pixel(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * w + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2]]
    }

    #[test]
    fn the_form_is_drawn_at_the_centre_and_the_rest_is_background() {
        let g = Greeter::new("");
        let px = render(&g, 1280, 800, true);
        assert_eq!(px.len(), 1280 * 800 * 4);
        // Inside the card, away from the texts.
        assert_eq!(pixel(&px, 1280, 470, 400), CARD);
        assert_ne!(pixel(&px, 1280, 2, 2), CARD);
        // Opaque.
        assert!(px.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn the_other_screens_show_the_background_only() {
        let g = Greeter::new("alice");
        let px = render(&g, 1280, 800, false);
        assert!((0..1280).all(|x| pixel(&px, 1280, x, 400) != CARD));
    }

    #[test]
    fn typing_changes_the_picture() {
        let mut g = Greeter::new("alice");
        let before = render(&g, 800, 600, true);
        g.key(0x1E, true);
        let after = render(&g, 800, 600, true);
        assert_ne!(before, after);
    }

    #[test]
    fn an_error_is_drawn_in_its_colour() {
        let mut g = Greeter::new("alice");
        g.login_failed("Wrong user name or password");
        let reddish = |p: &[u8]| p[0] > 200 && p[1] < 170 && p[2] < 160;
        let px = render(&g, 1280, 800, true);
        let n = px.chunks(4).filter(|p| reddish(p)).count();
        assert!(n > 20, "{n} error pixels");
        // The same screen without the message has none.
        let clean = render(&Greeter::new("alice"), 1280, 800, true);
        assert!(!clean.chunks(4).any(reddish));
    }

    #[test]
    fn a_tiny_screen_does_not_panic_nor_overflow() {
        let mut g = Greeter::new(&"x".repeat(64));
        g.login_failed(&"y".repeat(200));
        for (w, h) in [(1, 1), (16, 16), (100, 80), (320, 200), (4000, 3000)] {
            assert_eq!(render(&g, w, h, true).len(), (w * h * 4) as usize);
        }
    }

    #[test]
    fn the_text_is_smooth_not_blocky() {
        // Anti-aliasing gives many intermediate shades between the text colour and what is under it.
        let px = render(&Greeter::new("alice"), 1280, 800, true);
        let shades: std::collections::HashSet<_> =
            px.chunks(4).map(|p| [p[0], p[1], p[2]]).collect();
        assert!(shades.len() > 100, "{} colours", shades.len());
    }

    #[test]
    fn a_name_wider_than_the_field_shows_its_end() {
        let long = "x".repeat(60) + "END";
        let mut g = Greeter::new(&long);
        g.login_failed("");
        let wide = text_width(&long, 17.0, Weight::Regular);
        let shown = fit_tail(&long, 300.0, 17.0);
        assert!(wide > 300.0 && shown.ends_with("END"));
        assert!(text_width(&shown, 17.0, Weight::Regular) <= 300.0);
        assert_eq!(fit_head("abcdef", 0.0, 17.0), "");
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
        let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .unwrap()
            .write_image_data(&render(&g, w, h, true))
            .unwrap();
    }

    #[test]
    fn the_capture_gives_a_frame_only_when_something_changed() {
        let s = state("alice");
        let mut cap = GreeterCapture::new(s.clone(), 640, 480);
        let first = cap.recv_latest(OutputId(0)).unwrap().expect("first frame");
        assert_eq!((first.width, first.height, first.stride), (640, 480, 2560));
        assert!(cap.recv_latest(OutputId(0)).unwrap().is_none());
        s.lock().unwrap().key(0x1E, true);
        assert!(cap.recv_latest(OutputId(0)).unwrap().is_some());
        assert!(cap.recv_latest(OutputId(1)).is_err());
    }

    #[test]
    fn the_login_screen_has_a_pointer_given_once() {
        let mut cap = GreeterCapture::new(state(""), 640, 480);
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
        let mut cap = GreeterCapture::new(s, 640, 480);
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
        );
        assert_eq!(p.encoders.len(), 1);
        assert!(p.make_encoder.is_some());
        assert!(p.audio.is_none() && p.drives.is_none() && p.printers.is_none());
    }
}
