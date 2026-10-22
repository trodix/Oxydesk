//! The login screen of a client that connects without a password: the form is drawn here (`render`), the keys of the
//! client edit it (`GreeterInput`), and the pump shows it as it would show a desktop (`GreeterCapture`). The state
//! and the typing rules are in `oxydesk_core::greeter`.

use std::sync::{Arc, Mutex, OnceLock};

use ab_glyph::{point, Font, FontRef, Glyph, PxScale, ScaleFont};

use oxydesk_core::error::{CaptureError, ClipboardError, InputError};
use oxydesk_core::greeter::{Action, Field, Greeter, Target};
use oxydesk_core::layout::{Layout, OutputId};
use oxydesk_core::ports::{
    CaptureSource, ClipboardBackend, DesktopPorts, EncoderFactory, FrameEncoder, InputSink,
    PointerButton,
};
use oxydesk_core::wake::Wake;
use oxydesk_core::{CursorShape, CursorUpdate};
use oxydesk_core::{Frame, FrameData, PixelFormat, XkbLayout};

/// Creates the encoder of one output of the login screen (the composition root provides the same encoder as for a
/// desktop).
pub type GreeterEncoder = Arc<dyn Fn(OutputId) -> Box<dyn FrameEncoder> + Send + Sync>;

pub type SharedGreeter = Arc<Mutex<Greeter>>;

/// Position and size of the first screen in the desktop, shared by the capture (which learns the layout) and the input
/// (which turns pointer positions into places on the form).
type Screen = Arc<Mutex<(i32, i32, u32, u32)>>;

/// Called with the user name, the password and the layout of the client when the form is submitted.
pub type Submit = Box<dyn Fn(String, String, Option<XkbLayout>) + Send>;

type Rgb = [u8; 3];

// Colours of the site (site/style.css, dark scheme): warm greys, an orange accent and a violet glow.
const BACKGROUND_TOP: Rgb = [44, 37, 55];
const BACKGROUND_BOTTOM: Rgb = [12, 11, 10];
const CARD: Rgb = [29, 26, 24];
const CARD_BORDER: Rgb = [44, 40, 37];
const FIELD: Rgb = [20, 18, 17];
const FIELD_IDLE: Rgb = [62, 57, 52];
const ACCENT: Rgb = [255, 138, 76];
const ACCENT_DEEP: Rgb = [217, 72, 15];
const TEXT: Rgb = [241, 236, 230];
const MUTED: Rgb = [163, 155, 147];
const ERROR: Rgb = [255, 92, 96];

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
                let c = mix(base, [169, 139, 255], glow * 0.16);
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
/// Where the pieces of the form are on a screen of `width` x `height`, as (x, y, w, h) in pixels.
struct Form {
    s: f32,
    card: (f32, f32, f32, f32),
    fields: [(f32, f32, f32, f32); 2],
    button: (f32, f32, f32, f32),
}

fn form(width: u32, height: u32) -> Form {
    // Everything is drawn for a card of 400 x 440 units and scaled to the screen.
    let s = (height as f32 / 900.0)
        .clamp(0.6, 1.8)
        .min((width as f32 - 16.0) / 400.0)
        .min((height as f32 - 16.0) / 440.0)
        .max(0.05);
    let (card_w, card_h) = (400.0 * s, 440.0 * s);
    let left = (width as f32 - card_w) / 2.0;
    let top = (height as f32 - card_h) / 2.0;
    let pad = 32.0 * s;
    let inner = card_w - 2.0 * pad;
    let box_h = 44.0 * s;
    let mut y = top + 196.0 * s;
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
fn hit(width: u32, height: u32, x: i32, y: i32) -> Option<Target> {
    if width < 8 || height < 8 {
        return None;
    }
    let f = form(width, height);
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

pub fn render(greeter: &Greeter, width: u32, height: u32, primary: bool) -> Vec<u8> {
    let mut c = Canvas::new(width, height);
    if !primary || width < 8 || height < 8 {
        return c.px;
    }
    let Form { s, card, .. } = form(width, height);
    let (left, top, card_w, _) = card;
    let centre = left + card_w / 2.0;
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
    let form = form(width, height);
    for ((label, field), rect) in [("User name", Field::User), ("Password", Field::Password)]
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
            MUTED,
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
    }

    // The button.
    let button = form.button;
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
    screen: Screen,
}

impl GreeterCapture {
    pub fn new(state: SharedGreeter, width: u32, height: u32, screen: Screen) -> Self {
        Self {
            screen,
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
    /// Last position of the pointer in the desktop.
    pointer: (i32, i32),
}

impl GreeterInput {
    pub fn new(state: SharedGreeter, wake: Arc<Wake>, submit: Submit, screen: Screen) -> Self {
        Self {
            state,
            wake,
            submit,
            screen,
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
        if let Some(target) = hit(w, h, self.pointer.0 - sx, self.pointer.1 - sy) {
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
        )),
        encoders: vec![first],
        make_encoder: Some(make),
        input: Box::new(GreeterInput::new(state, wake, submit, screen)),
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
        // Red, not the orange of the accent (whose blue is lower than its green).
        let reddish = |p: &[u8]| {
            let (r, g, b) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
            r > 150 && r > g + 50 && b + 10 >= g
        };
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
    fn the_clickable_areas_are_where_the_form_is_drawn() {
        let g = Greeter::new("");
        for (w, h) in [(1280, 800), (1920, 1080), (640, 480)] {
            let f = form(w, h);
            let px = render(&g, w, h, true);
            let (bx, by) = centre(f.button);
            assert_eq!(hit(w, h, bx, by), Some(Target::Button));
            // The button is the orange one, the fields are dark.
            let p = pixel(&px, w, bx as u32 + (f.button.2 / 2.0) as u32 - 6, by as u32);
            assert!(p[0] > 200 && p[2] < 120, "{w}x{h}: {p:?}");
            for (i, field) in [Field::User, Field::Password].into_iter().enumerate() {
                let (x, y) = centre(f.fields[i]);
                assert_eq!(hit(w, h, x, y), Some(Target::Field(field)));
                let p = pixel(
                    &px,
                    w,
                    x as u32 + (f.fields[i].2 / 2.0) as u32 - 6,
                    y as u32,
                );
                assert_eq!(p, FIELD, "{w}x{h}");
            }
            assert_eq!(hit(w, h, 1, 1), None);
        }
        assert_eq!(hit(4, 4, 2, 2), None);
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
        );
        let f = form(1280, 800);
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
        );
        let f = form(1280, 800);
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
        );
        let button = centre(form(1280, 800).button);
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
        let mut cap = GreeterCapture::new(s.clone(), 1280, 800, shared.clone());
        let mut input = GreeterInput::new(
            s.clone(),
            Arc::new(Wake::default()),
            Box::new(|_, _, _| {}),
            shared,
        );
        let mut layout = Layout::side_by_side(1, 1920, 1080);
        layout.outputs[0].x = 500;
        layout.outputs[0].y = 40;
        cap.apply_layout(&layout).unwrap();
        let (x, y) = centre(form(1920, 1080).fields[0]);
        click_at(&mut input, (x + 500, y + 40));
        assert_eq!(s.lock().unwrap().focus(), Field::User);
    }

    #[test]
    fn the_capture_gives_a_frame_only_when_something_changed() {
        let s = state("alice");
        let mut cap = GreeterCapture::new(s.clone(), 640, 480, screen(640, 480));
        let first = cap.recv_latest(OutputId(0)).unwrap().expect("first frame");
        assert_eq!((first.width, first.height, first.stride), (640, 480, 2560));
        assert!(cap.recv_latest(OutputId(0)).unwrap().is_none());
        s.lock().unwrap().key(0x1E, true);
        assert!(cap.recv_latest(OutputId(0)).unwrap().is_some());
        assert!(cap.recv_latest(OutputId(1)).is_err());
    }

    #[test]
    fn the_login_screen_has_a_pointer_given_once() {
        let mut cap = GreeterCapture::new(state(""), 640, 480, screen(640, 480));
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
        let mut cap = GreeterCapture::new(s, 640, 480, screen(640, 480));
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
