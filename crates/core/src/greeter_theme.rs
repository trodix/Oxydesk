//! Look of the login screen: the settings of a theme file, parsed and validated without any I/O.
//!
//! A theme file (`greeter-theme = FILE` in the server configuration) holds `key = value` lines, with the same syntax
//! as the configuration file: `#` or `;` start a comment, a value may be wrapped in double quotes, and an unknown key
//! is an error. Colours are `#rgb` or `#rrggbb`. Paths are relative to the directory of the theme file. Every key is
//! optional; the defaults are the look of the built-in login screen.
//!
//! | key | value |
//! |---|---|
//! | `background` | one colour, or two (top and bottom of a gradient) |
//! | `background-glow` | `yes`/`no`: the soft glow above the centre of the default background |
//! | `background-image` | PNG shown behind the form, scaled to cover the screen |
//! | `logo` | PNG shown at the top of the card instead of the badge with the initial |
//! | `logo-size` | height of the logo, in units of the card (default 76) |
//! | `card`, `card-border`, `field`, `field-border` | colours of the card and of the input fields |
//! | `accent`, `accent-deep` | colours of the button, of the focused field and of the caret |
//! | `text`, `muted`, `error`, `button-text` | colours of the texts |
//! | `language` | `auto` (the language of the client's keyboard) or a code such as `fr` |
//! | `title`, `subtitle`, `user-label`, `password-label`, `button-label`, `busy-label`, `footer` | texts, for every language |
//! | `title.fr`, `footer.de`, ... | the same texts for one language only (they win over the plain key) |
//! | `font`, `font-bold` | TTF or OTF files (`font-bold` defaults to `font`) |
//! | `card-width`, `card-radius`, `field-radius` | geometry, in units of the card (400 wide by default) |

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::greeter_i18n::{self, Lang, Msg};

pub type Rgb = [u8; 3];

/// The texts a theme can replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TextKey {
    Title,
    Subtitle,
    UserLabel,
    PasswordLabel,
    ButtonLabel,
    BusyLabel,
    Footer,
}

impl TextKey {
    const ALL: [(&'static str, TextKey); 7] = [
        ("title", Self::Title),
        ("subtitle", Self::Subtitle),
        ("user-label", Self::UserLabel),
        ("password-label", Self::PasswordLabel),
        ("button-label", Self::ButtonLabel),
        ("busy-label", Self::BusyLabel),
        ("footer", Self::Footer),
    ];

    /// `title` or `title.fr`: the text, and the language it is for (`None`: all of them).
    fn parse(key: &str) -> Option<(Self, Option<Lang>)> {
        let (name, lang) = match key.split_once('.') {
            Some((name, lang)) => (name, Some(Lang::parse(lang).filter(|l| l.code() == lang)?)),
            None => (key, None),
        };
        Self::ALL
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, k)| (*k, lang))
    }

    /// What the built-in login screen says there, in `lang`.
    fn built_in(self, lang: Lang) -> &'static str {
        match self {
            Self::Title => "Oxydesk",
            Self::Subtitle => greeter_i18n::text(lang, Msg::Subtitle),
            Self::UserLabel => greeter_i18n::text(lang, Msg::UserName),
            Self::PasswordLabel => greeter_i18n::text(lang, Msg::Password),
            Self::ButtonLabel => greeter_i18n::text(lang, Msg::SignIn),
            Self::BusyLabel => greeter_i18n::text(lang, Msg::SigningIn),
            Self::Footer => "",
        }
    }
}

/// Longest text accepted for a title, a label or a footer, in characters.
const MAX_TEXT: usize = 120;

/// The settings of a login screen theme.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeSpec {
    pub background_top: Rgb,
    pub background_bottom: Rgb,
    pub background_glow: bool,
    pub background_image: Option<PathBuf>,
    pub logo: Option<PathBuf>,
    pub logo_size: f32,
    pub card: Rgb,
    pub card_border: Rgb,
    pub field: Rgb,
    pub field_border: Rgb,
    pub accent: Rgb,
    pub accent_deep: Rgb,
    pub text: Rgb,
    pub muted: Rgb,
    pub error: Rgb,
    pub button_text: Rgb,
    /// Language of the screen; `None`: the one of the client's keyboard.
    pub language: Option<Lang>,
    /// The texts the theme sets, by language (`None`: every language).
    pub texts: BTreeMap<(TextKey, Option<Lang>), String>,
    pub font: Option<PathBuf>,
    pub font_bold: Option<PathBuf>,
    pub card_width: f32,
    pub card_radius: f32,
    pub field_radius: f32,
}

impl Default for ThemeSpec {
    /// The built-in look: colours of the site (warm greys, an orange accent and a violet glow).
    fn default() -> Self {
        Self {
            background_top: [44, 37, 55],
            background_bottom: [12, 11, 10],
            background_glow: true,
            background_image: None,
            logo: None,
            logo_size: 76.0,
            card: [29, 26, 24],
            card_border: [44, 40, 37],
            field: [20, 18, 17],
            field_border: [62, 57, 52],
            accent: [255, 138, 76],
            accent_deep: [217, 72, 15],
            text: [241, 236, 230],
            muted: [163, 155, 147],
            error: [255, 92, 96],
            button_text: [255, 255, 255],
            language: None,
            texts: BTreeMap::new(),
            font: None,
            font_bold: None,
            card_width: 400.0,
            card_radius: 18.0,
            field_radius: 8.0,
        }
    }
}

impl ThemeSpec {
    /// The language of the screen for a client whose keyboard speaks `keyboard`.
    pub fn lang(&self, keyboard: Option<Lang>) -> Lang {
        self.language.or(keyboard).unwrap_or_default()
    }

    /// The text shown for `key` in `lang`: the theme's own for that language, else its text for all languages, else the
    /// built-in one. The footer is empty unless the theme sets it.
    pub fn text(&self, key: TextKey, lang: Lang) -> &str {
        self.texts
            .get(&(key, Some(lang)))
            .or_else(|| self.texts.get(&(key, None)))
            .map_or_else(|| key.built_in(lang), String::as_str)
    }

    /// Sets a text for every language.
    pub fn with_text(mut self, key: TextKey, text: &str) -> Self {
        self.texts.insert((key, None), text.to_owned());
        self
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ThemeError(pub String);

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ThemeError {}

/// `#rgb` or `#rrggbb`.
pub fn parse_colour(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    if !hex.is_ascii() {
        return None;
    }
    let digit = |s: &str| u8::from_str_radix(s, 16).ok();
    match hex.len() {
        3 => {
            let d = |i: usize| digit(&hex[i..=i]).map(|v| v * 17);
            Some([d(0)?, d(1)?, d(2)?])
        }
        6 => Some([digit(&hex[0..2])?, digit(&hex[2..4])?, digit(&hex[4..6])?]),
        _ => None,
    }
}

/// Parses a theme file. `source` names the file in error messages; `base` is the directory its paths are relative to.
pub fn parse(text: &str, source: &str, base: &Path) -> Result<ThemeSpec, ThemeError> {
    let mut spec = ThemeSpec::default();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let at = format!("{source}:{}", i + 1);
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| ThemeError(format!("{at}: expected `key = value`, got `{line}`")))?;
        let key = key.trim();
        let value = value.trim();
        let unquoted = value.strip_prefix('"').and_then(|v| v.strip_suffix('"'));
        let value = unquoted.unwrap_or(value);
        // An empty text (`title = ""`) hides it; any other key needs a value.
        let text_key = TextKey::parse(key);
        let blank_text = unquoted.is_some() && text_key.is_some();
        if value.is_empty() && !blank_text {
            return Err(ThemeError(format!("{at}: `{key}` has no value")));
        }
        let err = |what: &str| ThemeError(format!("{at}: `{key}`: `{value}` ({what})"));
        let colour = || parse_colour(value).ok_or_else(|| err("expected #rgb or #rrggbb"));
        let path = || {
            let p = Path::new(value);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(p)
            }
        };
        let number = |min: f32, max: f32| {
            value
                .parse::<f32>()
                .ok()
                .filter(|n| n.is_finite() && (min..=max).contains(n))
                .ok_or_else(|| err(&format!("expected a number from {min} to {max}")))
        };
        let words = || {
            if value.chars().count() > MAX_TEXT {
                Err(err(&format!("at most {MAX_TEXT} characters")))
            } else {
                Ok(value.to_owned())
            }
        };
        match key {
            "background" => {
                let mut parts = value.split_whitespace();
                let top = parts.next().and_then(parse_colour);
                let bottom = parts.next().map(parse_colour);
                match (top, bottom, parts.next()) {
                    (Some(top), None, None) => {
                        spec.background_top = top;
                        spec.background_bottom = top;
                    }
                    (Some(top), Some(Some(bottom)), None) => {
                        spec.background_top = top;
                        spec.background_bottom = bottom;
                    }
                    _ => return Err(err("expected one or two colours")),
                }
            }
            "background-glow" => {
                spec.background_glow = match value.to_ascii_lowercase().as_str() {
                    "yes" | "true" | "on" | "1" => true,
                    "no" | "false" | "off" | "0" => false,
                    _ => return Err(err("yes|no")),
                }
            }
            "background-image" => spec.background_image = Some(path()),
            "logo" => spec.logo = Some(path()),
            "logo-size" => spec.logo_size = number(16.0, 200.0)?,
            "card" => spec.card = colour()?,
            "card-border" => spec.card_border = colour()?,
            "field" => spec.field = colour()?,
            "field-border" => spec.field_border = colour()?,
            "accent" => spec.accent = colour()?,
            "accent-deep" => spec.accent_deep = colour()?,
            "text" => spec.text = colour()?,
            "muted" => spec.muted = colour()?,
            "error" => spec.error = colour()?,
            "button-text" => spec.button_text = colour()?,
            "language" => {
                spec.language = if value.eq_ignore_ascii_case("auto") {
                    None
                } else {
                    Some(
                        Lang::parse(value)
                            .ok_or_else(|| err("auto, or one of en fr de es it pt nl"))?,
                    )
                }
            }
            "font" => spec.font = Some(path()),
            "font-bold" => spec.font_bold = Some(path()),
            "card-width" => spec.card_width = number(280.0, 640.0)?,
            "card-radius" => spec.card_radius = number(0.0, 40.0)?,
            "field-radius" => spec.field_radius = number(0.0, 22.0)?,
            _ => match text_key {
                Some(k) => {
                    spec.texts.insert(k, words()?);
                }
                None => return Err(ThemeError(format!("{at}: unknown key `{key}`"))),
            },
        }
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Result<ThemeSpec, ThemeError> {
        parse(text, "theme", Path::new("/etc/oxydesk"))
    }

    #[test]
    fn an_empty_theme_is_the_built_in_look() {
        assert_eq!(parsed("").unwrap(), ThemeSpec::default());
        assert_eq!(
            parsed("# only a comment\n; another\n\n").unwrap(),
            ThemeSpec::default()
        );
    }

    #[test]
    fn colours_are_short_or_long_hex() {
        assert_eq!(parse_colour("#fff"), Some([255, 255, 255]));
        assert_eq!(parse_colour("#1a2B3c"), Some([0x1a, 0x2b, 0x3c]));
        for bad in ["fff", "#ff", "#ffff", "#ggg", "#12345g", "#é12", ""] {
            assert_eq!(parse_colour(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_keys_set_the_look() {
        let t = parsed(
            "accent = #00ff00\ntitle = \"ACME Corp\"\ncard-width = 480\nfooter = Authorised use only\n\
             background-glow = no\nlogo-size = 100\nbutton-label = Log in",
        )
        .unwrap();
        assert_eq!(t.accent, [0, 255, 0]);
        assert_eq!(t.text(TextKey::Title, Lang::En), "ACME Corp");
        assert_eq!(t.card_width, 480.0);
        assert_eq!(t.text(TextKey::Footer, Lang::Fr), "Authorised use only");
        assert!(!t.background_glow);
        assert_eq!(t.logo_size, 100.0);
        assert_eq!(t.text(TextKey::ButtonLabel, Lang::En), "Log in");
        // What is not named keeps the default.
        assert_eq!(t.card, ThemeSpec::default().card);
    }

    #[test]
    fn without_a_theme_the_texts_follow_the_language() {
        let t = ThemeSpec::default();
        assert_eq!(t.text(TextKey::ButtonLabel, Lang::En), "Sign in");
        assert_eq!(t.text(TextKey::ButtonLabel, Lang::Fr), "Se connecter");
        assert_eq!(t.text(TextKey::Title, Lang::De), "Oxydesk");
        assert_eq!(t.text(TextKey::Footer, Lang::De), "");
    }

    #[test]
    fn a_text_of_the_theme_for_one_language_wins_over_the_general_one() {
        let t = parsed("title = ACME\ntitle.fr = ACME France\nsubtitle.de = Willkommen").unwrap();
        assert_eq!(t.text(TextKey::Title, Lang::En), "ACME");
        assert_eq!(t.text(TextKey::Title, Lang::Fr), "ACME France");
        // Only the language that was named is changed; the others keep the translation.
        assert_eq!(t.text(TextKey::Subtitle, Lang::De), "Willkommen");
        assert_eq!(
            t.text(TextKey::Subtitle, Lang::Fr),
            "Connectez-vous à votre bureau"
        );
        assert!(parsed("title.xx = a").is_err());
        assert!(parsed("title.fr_FR = a").is_err());
        assert!(parsed("colour.fr = a").is_err());
    }

    #[test]
    fn the_language_is_fixed_by_the_theme_or_follows_the_keyboard() {
        let auto = parsed("language = auto").unwrap();
        assert_eq!(auto.lang(Some(Lang::Fr)), Lang::Fr);
        assert_eq!(auto.lang(None), Lang::En);
        let fixed = parsed("language = de").unwrap();
        assert_eq!(fixed.lang(Some(Lang::Fr)), Lang::De);
        assert_eq!(fixed.lang(None), Lang::De);
        assert!(parsed("language = klingon").is_err());
    }

    #[test]
    fn a_text_can_be_emptied_with_quotes_only() {
        let t = parsed("title = \"\"\nsubtitle = \"\"\nfooter = \"\"").unwrap();
        for key in [TextKey::Title, TextKey::Subtitle, TextKey::Footer] {
            assert_eq!(t.text(key, Lang::Fr), "");
        }
        assert!(parsed("title =").is_err());
        assert!(parsed("accent = \"\"").is_err());
    }

    #[test]
    fn the_background_is_a_colour_or_a_gradient() {
        let one = parsed("background = #102030").unwrap();
        assert_eq!(one.background_top, [0x10, 0x20, 0x30]);
        assert_eq!(one.background_bottom, [0x10, 0x20, 0x30]);
        let two = parsed("background = #102030 #405060").unwrap();
        assert_eq!(two.background_bottom, [0x40, 0x50, 0x60]);
        for bad in [
            "background = #102030 #405060 #708090",
            "background = red",
            "background = #102030 blue",
        ] {
            assert!(parsed(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn paths_are_relative_to_the_theme_file() {
        let t =
            parsed("logo = logo.png\nbackground-image = /srv/bg.png\nfont = fonts/a.ttf").unwrap();
        assert_eq!(t.logo, Some(PathBuf::from("/etc/oxydesk/logo.png")));
        assert_eq!(t.background_image, Some(PathBuf::from("/srv/bg.png")));
        assert_eq!(t.font, Some(PathBuf::from("/etc/oxydesk/fonts/a.ttf")));
    }

    #[test]
    fn mistakes_are_reported_with_their_line() {
        let cases = [
            ("colour = #fff", "theme:1: unknown key `colour`"),
            ("\naccent = orange", "theme:2: `accent`: `orange`"),
            ("accent", "expected `key = value`"),
            ("title =", "`title` has no value"),
            ("language = xx", "auto, or one of"),
            ("card-width = 10", "from 280 to 640"),
            ("card-width = wide", "expected a number"),
            ("card-radius = NaN", "expected a number"),
            ("background-glow = maybe", "yes|no"),
        ];
        for (text, expected) in cases {
            let e = parsed(text).unwrap_err().to_string();
            assert!(e.contains(expected), "{text}: {e}");
        }
        let long = format!("title = {}", "x".repeat(MAX_TEXT + 1));
        assert!(parsed(&long).unwrap_err().to_string().contains("at most"));
    }
}
