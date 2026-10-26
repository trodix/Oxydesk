//! The login screen shown to a client that connects without a password (as xrdp does): a user name and a password to
//! type, filled in advance with the name the client sent. Pure state: the keys go in, a submission comes out.

use crate::greeter_i18n::Notice;
use crate::keyboard::XkbLayout;
use crate::keymap::char_for;

/// Longest user name and password the form accepts, in characters.
pub const MAX_USER: usize = 64;
pub const MAX_PASSWORD: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    User,
    Password,
}

/// What a click of the pointer can land on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Field(Field),
    Button,
}

/// What a key did that the caller must act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing more than a possible change of the form (see `revision`).
    None,
    /// Enter on a complete form: the login to try.
    Submit { user: String, password: String },
}

#[derive(Debug, Clone)]
pub struct Greeter {
    user: String,
    password: String,
    focus: Field,
    message: Option<Notice>,
    busy: bool,
    layout: XkbLayout,
    shift: u8,
    ctrl: u8,
    altgr: bool,
    caps: bool,
    revision: u64,
}

const SHIFTS: [u16; 2] = [0x2A, 0x36];
const CTRLS: [u16; 2] = [0x1D, 0xE01D];
const ALTGR: u16 = 0xE038;
const CAPS: u16 = 0x3A;

impl Greeter {
    /// `user`: the name the client sent, if any; the cursor starts on the password then.
    pub fn new(user: &str) -> Self {
        let user: String = user.chars().take(MAX_USER).collect();
        Self {
            focus: if user.is_empty() {
                Field::User
            } else {
                Field::Password
            },
            user,
            password: String::new(),
            message: None,
            busy: false,
            layout: XkbLayout::new("us", ""),
            shift: 0,
            ctrl: 0,
            altgr: false,
            caps: false,
            revision: 0,
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    /// Number of characters typed in the password (the screen shows one star each).
    pub fn password_len(&self) -> usize {
        self.password.chars().count()
    }

    pub fn focus(&self) -> Field {
        self.focus
    }

    pub fn message(&self) -> Option<&Notice> {
        self.message.as_ref()
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn caps_lock(&self) -> bool {
        self.caps
    }

    /// Grows with every change of what the screen shows.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The keyboard of the client: it decides the characters typed, and the language of the screen.
    pub fn set_layout(&mut self, layout: &XkbLayout) {
        if self.layout != *layout {
            self.layout = layout.clone();
            self.revision += 1;
        }
    }

    pub fn layout(&self) -> &XkbLayout {
        &self.layout
    }

    /// The login is being tried: the form is frozen.
    pub fn start_login(&mut self) {
        self.busy = true;
        self.message = None;
        self.revision += 1;
    }

    /// The login was refused: the password is cleared and typed again.
    pub fn login_failed(&mut self, notice: Notice) {
        self.busy = false;
        self.password.clear();
        self.focus = Field::Password;
        self.message = Some(notice);
        self.revision += 1;
    }

    fn type_char(&mut self, c: char) {
        match self.focus {
            Field::User if self.user.chars().count() < MAX_USER => self.user.push(c),
            Field::Password if self.password.chars().count() < MAX_PASSWORD => {
                self.password.push(c)
            }
            _ => return,
        }
        self.message = None;
        self.revision += 1;
    }

    fn erase(&mut self) {
        let field = match self.focus {
            Field::User => &mut self.user,
            Field::Password => &mut self.password,
        };
        if field.pop().is_some() {
            self.message = None;
            self.revision += 1;
        }
    }

    fn move_focus(&mut self, to: Field) {
        if self.focus != to {
            self.focus = to;
            self.revision += 1;
        }
    }

    /// The login to try, if the form is complete.
    fn submit(&self) -> Action {
        if self.user.is_empty() || self.password.is_empty() {
            return Action::None;
        }
        Action::Submit {
            user: self.user.clone(),
            password: self.password.clone(),
        }
    }

    /// A left click of the pointer. A field takes the cursor; the button submits a complete form, otherwise it moves the
    /// cursor to what is missing.
    pub fn click(&mut self, target: Target) -> Action {
        if self.busy {
            return Action::None;
        }
        match target {
            Target::Field(field) => self.move_focus(field),
            Target::Button => {
                if self.user.is_empty() {
                    self.move_focus(Field::User);
                } else if self.password.is_empty() {
                    self.move_focus(Field::Password);
                } else {
                    return self.submit();
                }
            }
        }
        Action::None
    }

    /// A key of the client. Releases only matter for the modifiers.
    pub fn key(&mut self, scancode: u16, pressed: bool) -> Action {
        if SHIFTS.contains(&scancode) {
            self.shift = if pressed {
                self.shift.saturating_add(1)
            } else {
                self.shift.saturating_sub(1)
            };
            return Action::None;
        }
        if CTRLS.contains(&scancode) {
            self.ctrl = if pressed {
                self.ctrl.saturating_add(1)
            } else {
                self.ctrl.saturating_sub(1)
            };
            return Action::None;
        }
        if scancode == ALTGR {
            self.altgr = pressed;
            return Action::None;
        }
        if !pressed || self.busy {
            return Action::None;
        }
        match scancode {
            CAPS => {
                self.caps = !self.caps;
                self.revision += 1;
            }
            0x0E => self.erase(),
            0x01 => {
                if !self.password.is_empty() {
                    self.password.clear();
                    self.revision += 1;
                }
            }
            // With two fields, tab and shift-tab go the same way.
            0x0F => self.move_focus(match self.focus {
                Field::User => Field::Password,
                Field::Password => Field::User,
            }),
            0xE048 => self.move_focus(Field::User),
            0xE050 => self.move_focus(Field::Password),
            0x1C | 0xE01C => {
                if self.user.is_empty() {
                    self.move_focus(Field::User);
                } else if self.focus == Field::User {
                    self.move_focus(Field::Password);
                } else {
                    return self.submit();
                }
            }
            _ => {
                if self.ctrl > 0 && !self.altgr {
                    return Action::None;
                }
                if let Some(c) = char_for(
                    &self.layout,
                    scancode,
                    self.shift > 0,
                    self.altgr,
                    self.caps,
                ) {
                    self.type_char(c);
                }
            }
        }
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_keys(g: &mut Greeter, codes: &[u16]) {
        for &c in codes {
            g.key(c, true);
            g.key(c, false);
        }
    }

    const A: u16 = 0x1E;
    const B: u16 = 0x30;
    const ENTER: u16 = 0x1C;
    const TAB: u16 = 0x0F;
    const BACKSPACE: u16 = 0x0E;

    #[test]
    fn without_a_name_the_cursor_starts_on_the_user() {
        let g = Greeter::new("");
        assert_eq!(g.focus(), Field::User);
        assert_eq!(g.user(), "");
    }

    #[test]
    fn the_name_sent_by_the_client_is_filled_in_and_the_cursor_is_on_the_password() {
        let g = Greeter::new("alice");
        assert_eq!(g.user(), "alice");
        assert_eq!(g.focus(), Field::Password);
    }

    #[test]
    fn a_long_name_is_cut() {
        assert_eq!(
            Greeter::new(&"x".repeat(MAX_USER + 10)).user().len(),
            MAX_USER
        );
    }

    #[test]
    fn typing_a_name_then_a_password_and_enter_submits() {
        let mut g = Greeter::new("");
        type_keys(&mut g, &[A, B]);
        assert_eq!(g.user(), "ab");
        assert_eq!(g.key(ENTER, true), Action::None); // moves on to the password
        assert_eq!(g.focus(), Field::Password);
        type_keys(&mut g, &[B, A]);
        assert_eq!(g.password_len(), 2);
        assert_eq!(
            g.key(ENTER, true),
            Action::Submit {
                user: "ab".into(),
                password: "ba".into()
            }
        );
    }

    #[test]
    fn clicking_a_field_moves_the_cursor_there() {
        let mut g = Greeter::new("alice");
        assert_eq!(g.focus(), Field::Password);
        let before = g.revision();
        assert_eq!(g.click(Target::Field(Field::User)), Action::None);
        assert_eq!(g.focus(), Field::User);
        assert!(g.revision() > before);
        let same = g.revision();
        g.click(Target::Field(Field::User));
        assert_eq!(g.revision(), same);
    }

    #[test]
    fn the_button_submits_a_complete_form_and_points_at_what_is_missing() {
        let mut g = Greeter::new("");
        g.click(Target::Field(Field::Password));
        assert_eq!(g.click(Target::Button), Action::None);
        assert_eq!(g.focus(), Field::User);
        let mut g = Greeter::new("alice");
        g.click(Target::Field(Field::User));
        assert_eq!(g.click(Target::Button), Action::None);
        assert_eq!(g.focus(), Field::Password);
        type_keys(&mut g, &[0x1E]);
        assert_eq!(
            g.click(Target::Button),
            Action::Submit {
                user: "alice".into(),
                password: "a".into()
            }
        );
    }

    #[test]
    fn clicks_do_nothing_while_signing_in() {
        let mut g = Greeter::new("alice");
        type_keys(&mut g, &[0x1E]);
        g.start_login();
        assert_eq!(g.click(Target::Button), Action::None);
        g.click(Target::Field(Field::User));
        assert_eq!(g.focus(), Field::Password);
    }

    #[test]
    fn enter_on_an_empty_password_submits_nothing() {
        let mut g = Greeter::new("alice");
        assert_eq!(g.key(ENTER, true), Action::None);
    }

    #[test]
    fn enter_without_a_name_goes_to_the_name() {
        let mut g = Greeter::new("");
        g.key(TAB, true);
        assert_eq!(g.focus(), Field::Password);
        assert_eq!(g.key(ENTER, true), Action::None);
        assert_eq!(g.focus(), Field::User);
    }

    #[test]
    fn tab_and_the_arrows_move_between_the_fields() {
        let mut g = Greeter::new("alice");
        g.key(TAB, true);
        assert_eq!(g.focus(), Field::User);
        g.key(TAB, true);
        assert_eq!(g.focus(), Field::Password);
        g.key(0xE048, true);
        assert_eq!(g.focus(), Field::User);
        g.key(0xE050, true);
        assert_eq!(g.focus(), Field::Password);
        // Shift-tab goes back.
        g.key(0x2A, true);
        g.key(TAB, true);
        assert_eq!(g.focus(), Field::User);
        g.key(0x2A, false);
    }

    #[test]
    fn backspace_erases_and_escape_clears_the_password() {
        let mut g = Greeter::new("ab");
        g.key(BACKSPACE, true);
        assert_eq!(g.user(), "ab"); // the cursor is on the password
        type_keys(&mut g, &[A, B, A]);
        g.key(BACKSPACE, true);
        assert_eq!(g.password_len(), 2);
        g.key(0x01, true);
        assert_eq!(g.password_len(), 0);
    }

    #[test]
    fn shift_makes_capitals_and_caps_lock_toggles() {
        let mut g = Greeter::new("");
        g.key(0x2A, true);
        type_keys(&mut g, &[A]);
        g.key(0x2A, false);
        g.key(0x3A, true);
        g.key(0x3A, false);
        assert!(g.caps_lock());
        type_keys(&mut g, &[A]);
        assert_eq!(g.user(), "AA");
    }

    #[test]
    fn control_combinations_type_nothing_but_altgr_does() {
        let mut g = Greeter::new("");
        g.key(0x1D, true);
        type_keys(&mut g, &[A]);
        assert_eq!(g.user(), "");
        // AltGr arrives with a control on Windows.
        g.key(0xE038, true);
        g.set_layout(&XkbLayout::new("fr", ""));
        type_keys(&mut g, &[0x0B]);
        assert_eq!(g.user(), "@");
        g.key(0xE038, false);
        g.key(0x1D, false);
    }

    #[test]
    fn the_layout_of_the_client_decides_the_characters() {
        let mut g = Greeter::new("");
        g.set_layout(&XkbLayout::new("fr", ""));
        type_keys(&mut g, &[0x10]); // q on a US keyboard
        assert_eq!(g.user(), "a");
    }

    #[test]
    fn a_refused_login_clears_the_password_and_shows_why() {
        let mut g = Greeter::new("alice");
        type_keys(&mut g, &[A]);
        g.start_login();
        assert!(g.busy());
        // The form is frozen while the login is tried.
        type_keys(&mut g, &[B]);
        assert_eq!(g.password_len(), 1);
        g.login_failed(Notice::WrongCredentials);
        assert!(!g.busy());
        assert_eq!(g.password_len(), 0);
        assert_eq!(g.message(), Some(&Notice::WrongCredentials));
        assert_eq!(g.focus(), Field::Password);
        // Typing again removes the message.
        type_keys(&mut g, &[A]);
        assert_eq!(g.message(), None);
    }

    #[test]
    fn the_revision_moves_with_every_visible_change_only() {
        let mut g = Greeter::new("alice");
        let r = g.revision();
        g.key(A, false); // a release: nothing
        g.key(0x01, true); // escape on an empty password: nothing
        g.key(BACKSPACE, true); // nothing to erase
        assert_eq!(g.revision(), r);
        type_keys(&mut g, &[A]);
        assert!(g.revision() > r);
    }

    #[test]
    fn the_fields_are_bounded() {
        let mut g = Greeter::new("");
        for _ in 0..MAX_USER + 5 {
            type_keys(&mut g, &[A]);
        }
        assert_eq!(g.user().len(), MAX_USER);
        g.key(TAB, true);
        for _ in 0..MAX_PASSWORD + 5 {
            type_keys(&mut g, &[A]);
        }
        assert_eq!(g.password_len(), MAX_PASSWORD);
    }
}
