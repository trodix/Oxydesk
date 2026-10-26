//! Languages of the login screen: which one a client gets, and the texts of each.
//!
//! The language follows the keyboard layout the client announces (a French keyboard gets French), unless the theme
//! fixes one (`language = fr`). A language that is not translated, or a client that announces no layout, gets English.

use crate::keyboard::XkbLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Lang {
    #[default]
    En,
    Fr,
    De,
    Es,
    It,
    Pt,
    Nl,
}

impl Lang {
    pub const ALL: [Lang; 7] = [
        Self::En,
        Self::Fr,
        Self::De,
        Self::Es,
        Self::It,
        Self::Pt,
        Self::Nl,
    ];

    /// The two-letter code (`fr`), also what a theme writes.
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Fr => "fr",
            Self::De => "de",
            Self::Es => "es",
            Self::It => "it",
            Self::Pt => "pt",
            Self::Nl => "nl",
        }
    }

    /// `fr`, `FR` or `fr_FR.UTF-8` (a locale name: only the language counts).
    pub fn parse(value: &str) -> Option<Self> {
        let code = value
            .split(['_', '-', '.', '@'])
            .next()?
            .to_ascii_lowercase();
        Self::ALL.into_iter().find(|l| l.code() == code)
    }

    /// The language of the people who type on this keyboard, if it says so.
    pub fn from_layout(layout: &XkbLayout) -> Option<Self> {
        match (layout.layout.as_str(), layout.variant.as_str()) {
            ("us" | "gb" | "ie", _) => Some(Self::En),
            ("fr", _) | ("ch", "fr") => Some(Self::Fr),
            ("de" | "at", _) | ("ch", _) => Some(Self::De),
            ("es" | "latam", _) => Some(Self::Es),
            ("it", _) => Some(Self::It),
            ("pt" | "br", _) => Some(Self::Pt),
            ("nl", _) => Some(Self::Nl),
            _ => None,
        }
    }
}

/// What the login screen says, apart from the texts a theme replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    Subtitle,
    UserName,
    Password,
    SignIn,
    SigningIn,
    CapsLock,
}

/// The text of `msg` in `lang`.
pub fn text(lang: Lang, msg: Msg) -> &'static str {
    use Lang::*;
    use Msg::*;
    match (msg, lang) {
        (Subtitle, En) => "Sign in to your desktop",
        (Subtitle, Fr) => "Connectez-vous à votre bureau",
        (Subtitle, De) => "Melden Sie sich an Ihrem Desktop an",
        (Subtitle, Es) => "Inicie sesión en su escritorio",
        (Subtitle, It) => "Accedi al tuo desktop",
        (Subtitle, Pt) => "Inicie sessão na sua área de trabalho",
        (Subtitle, Nl) => "Meld u aan op uw bureaublad",
        (UserName, En) => "User name",
        (UserName, Fr) => "Nom d'utilisateur",
        (UserName, De) => "Benutzername",
        (UserName, Es) => "Nombre de usuario",
        (UserName, It) => "Nome utente",
        (UserName, Pt) => "Nome de utilizador",
        (UserName, Nl) => "Gebruikersnaam",
        (Password, En) => "Password",
        (Password, Fr) => "Mot de passe",
        (Password, De) => "Passwort",
        (Password, Es) => "Contraseña",
        (Password, It) => "Password",
        (Password, Pt) => "Palavra-passe",
        (Password, Nl) => "Wachtwoord",
        (SignIn, En) => "Sign in",
        (SignIn, Fr) => "Se connecter",
        (SignIn, De) => "Anmelden",
        (SignIn, Es) => "Iniciar sesión",
        (SignIn, It) => "Accedi",
        (SignIn, Pt) => "Iniciar sessão",
        (SignIn, Nl) => "Aanmelden",
        (SigningIn, En) => "Signing in\u{2026}",
        (SigningIn, Fr) => "Connexion\u{2026}",
        (SigningIn, De) => "Anmeldung l\u{e4}uft\u{2026}",
        (SigningIn, Es) => "Iniciando sesión\u{2026}",
        (SigningIn, It) => "Accesso in corso\u{2026}",
        (SigningIn, Pt) => "A iniciar sessão\u{2026}",
        (SigningIn, Nl) => "Aanmelden\u{2026}",
        (CapsLock, En) => "Caps Lock is on",
        (CapsLock, Fr) => "Verrouillage majuscules activé",
        (CapsLock, De) => "Feststelltaste ist aktiv",
        (CapsLock, Es) => "Bloq Mayús está activado",
        (CapsLock, It) => "Blocco maiuscole attivo",
        (CapsLock, Pt) => "Caps Lock ativo",
        (CapsLock, Nl) => "Caps Lock staat aan",
    }
}

/// Why a login did not go through, as shown under the button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    WrongCredentials,
    /// Too many failures: the minutes to wait.
    Blocked(u64),
    SessionFailed,
    /// Any other text, shown as it is.
    Text(String),
}

impl Notice {
    pub fn text(&self, lang: Lang) -> String {
        use Lang::*;
        match (self, lang) {
            (Self::Text(t), _) => t.clone(),
            (Self::WrongCredentials, En) => "Wrong user name or password".into(),
            (Self::WrongCredentials, Fr) => "Nom d'utilisateur ou mot de passe incorrect".into(),
            (Self::WrongCredentials, De) => "Falscher Benutzername oder falsches Passwort".into(),
            (Self::WrongCredentials, Es) => "Nombre de usuario o contraseña incorrectos".into(),
            (Self::WrongCredentials, It) => "Nome utente o password errati".into(),
            (Self::WrongCredentials, Pt) => "Nome de utilizador ou palavra-passe incorretos".into(),
            (Self::WrongCredentials, Nl) => "Onjuiste gebruikersnaam of wachtwoord".into(),
            (Self::Blocked(n), En) => format!("Too many failures: try again in {n} min"),
            (Self::Blocked(n), Fr) => format!("Trop d'échecs : réessayez dans {n} min"),
            (Self::Blocked(n), De) => {
                format!("Zu viele Fehlversuche: erneut versuchen in {n} Min.")
            }
            (Self::Blocked(n), Es) => format!("Demasiados fallos: inténtelo de nuevo en {n} min"),
            (Self::Blocked(n), It) => format!("Troppi errori: riprova tra {n} min"),
            (Self::Blocked(n), Pt) => {
                format!("Demasiadas falhas: tente novamente dentro de {n} min")
            }
            (Self::Blocked(n), Nl) => {
                format!("Te veel mislukte pogingen: probeer het over {n} min opnieuw")
            }
            (Self::SessionFailed, En) => "The session cannot be started".into(),
            (Self::SessionFailed, Fr) => "La session ne peut pas être démarrée".into(),
            (Self::SessionFailed, De) => "Die Sitzung kann nicht gestartet werden".into(),
            (Self::SessionFailed, Es) => "No se puede iniciar la sesión".into(),
            (Self::SessionFailed, It) => "Impossibile avviare la sessione".into(),
            (Self::SessionFailed, Pt) => "Não foi possível iniciar a sessão".into(),
            (Self::SessionFailed, Nl) => "De sessie kan niet worden gestart".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_MSGS: [Msg; 6] = [
        Msg::Subtitle,
        Msg::UserName,
        Msg::Password,
        Msg::SignIn,
        Msg::SigningIn,
        Msg::CapsLock,
    ];

    #[test]
    fn a_language_is_named_by_its_code_or_a_locale() {
        for l in Lang::ALL {
            assert_eq!(Lang::parse(l.code()), Some(l));
        }
        assert_eq!(Lang::parse("FR"), Some(Lang::Fr));
        assert_eq!(Lang::parse("fr_CH.UTF-8"), Some(Lang::Fr));
        assert_eq!(Lang::parse("pt-BR"), Some(Lang::Pt));
        assert_eq!(Lang::parse("de@euro"), Some(Lang::De));
        for bad in ["", "klingon", "xx_FR", "_fr"] {
            assert_eq!(Lang::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_keyboard_tells_the_language() {
        let of = |l: &str, v: &str| Lang::from_layout(&XkbLayout::new(l, v));
        assert_eq!(of("us", ""), Some(Lang::En));
        assert_eq!(of("fr", ""), Some(Lang::Fr));
        assert_eq!(of("fr", "oss"), Some(Lang::Fr));
        assert_eq!(of("ch", "fr"), Some(Lang::Fr));
        assert_eq!(of("ch", "de"), Some(Lang::De));
        assert_eq!(of("ch", ""), Some(Lang::De));
        assert_eq!(of("de", ""), Some(Lang::De));
        assert_eq!(of("latam", ""), Some(Lang::Es));
        assert_eq!(of("br", ""), Some(Lang::Pt));
        assert_eq!(of("nl", ""), Some(Lang::Nl));
        assert_eq!(of("it", ""), Some(Lang::It));
        // A layout shared by several languages says nothing.
        assert_eq!(of("be", ""), None);
        assert_eq!(of("jp", ""), None);
    }

    #[test]
    fn every_language_has_every_text_and_the_texts_differ_from_english() {
        for l in Lang::ALL {
            for m in ALL_MSGS {
                assert!(!text(l, m).is_empty(), "{l:?} {m:?}");
            }
            for n in [
                Notice::WrongCredentials,
                Notice::Blocked(3),
                Notice::SessionFailed,
            ] {
                let t = n.text(l);
                assert!(!t.is_empty());
                if l != Lang::En {
                    assert_ne!(t, n.text(Lang::En), "{l:?} {n:?}");
                }
            }
        }
        // Italian shares "Password" with English; the others are translated.
        for l in Lang::ALL {
            if l != Lang::En && l != Lang::It {
                assert_ne!(text(l, Msg::Password), text(Lang::En, Msg::Password));
            }
        }
    }

    #[test]
    fn the_wait_is_in_the_message() {
        for l in Lang::ALL {
            assert!(Notice::Blocked(7).text(l).contains('7'), "{l:?}");
        }
        assert_eq!(Notice::Text("as is".into()).text(Lang::Fr), "as is");
    }
}
