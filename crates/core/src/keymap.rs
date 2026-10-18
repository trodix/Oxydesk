//! Characters typed on the client's keyboard before any session exists (the login screen): RDP sends physical
//! keys (scancodes), so the character depends on the layout announced by the client.

use crate::keyboard::XkbLayout;

/// Scancodes (set 1) of the keys that type a character, in the order of the rows of `LAYOUTS`.
const KEYS: [u16; 48] = [
    0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C,
    0x0D, // 1 2 3 ... = (top row)
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, // q w e ... ]
    0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, // a s d ... `
    0x2B, // \
    0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, // z x c ... /
    0x56, // the key between left shift and z on European keyboards
];

/// Per key: the characters without modifier, with shift and with AltGr, in this order. A missing one is `\0` or
/// absent. Dead keys (accents that wait for a letter) type nothing.
type Row = [&'static str; 48];

const US: Row = [
    "1!", "2@", "3#", "4$", "5%", "6^", "7&", "8*", "9(", "0)", "-_", "=+", //
    "qQ", "wW", "eE", "rR", "tT", "yY", "uU", "iI", "oO", "pP", "[{", "]}", //
    "aA", "sS", "dD", "fF", "gG", "hH", "jJ", "kK", "lL", ";:", "'\"", "`~",  //
    "\\|", //
    "zZ", "xX", "cC", "vV", "bB", "nN", "mM", ",<", ".>", "/?", //
    "",
];

const GB: Row = [
    "1!", "2\"", "3£", "4$€", "5%", "6^", "7&", "8*", "9(", "0)", "-_", "=+", //
    "qQ", "wW", "eE", "rR", "tT", "yY", "uU", "iI", "oO", "pP", "[{", "]}", //
    "aA", "sS", "dD", "fF", "gG", "hH", "jJ", "kK", "lL", ";:", "'@", "`¬|", //
    "#~",  //
    "zZ", "xX", "cC", "vV", "bB", "nN", "mM", ",<", ".>", "/?", //
    "\\|",
];

const FR: Row = [
    "&1", "é2~", "\"3#", "'4{", "(5[", "-6|", "è7`", "_8\\", "ç9^", "à0@", ")°]", "=+}", //
    "aA", "zZ", "eE€", "rR", "tT", "yY", "uU", "iI", "oO", "pP", "", "$£¤", //
    "qQ", "sS", "dD", "fF", "gG", "hH", "jJ", "kK", "lL", "mM", "ù%", "²",  //
    "*µ", //
    "wW", "xX", "cC", "vV", "bB", "nN", ",?", ";.", ":/", "!§", //
    "<>",
];

const DE: Row = [
    "1!", "2\"²", "3§³", "4$", "5%", "6&", "7/{", "8([", "9)]", "0=}", "ß?\\", "", //
    "qQ@", "wW", "eE€", "rR", "tT", "zZ", "uU", "iI", "oO", "pP", "üÜ", "+*~", //
    "aA", "sS", "dD", "fF", "gG", "hH", "jJ", "kK", "lL", "öÖ", "äÄ", "",   //
    "#'", //
    "yY", "xX", "cC", "vV", "bB", "nN", "mMµ", ",;", ".:", "-_", //
    "<>|",
];

fn row(layout: &XkbLayout) -> &'static Row {
    match layout.layout.as_str() {
        "gb" | "ie" => &GB,
        "fr" => &FR,
        "de" | "at" => &DE,
        _ => &US,
    }
}

/// The character typed by `scancode` on `layout`, with shift and AltGr as given (`caps`: caps lock, which only
/// changes letters). `None` for a key that types nothing. Layouts that are not listed type as a US keyboard.
pub fn char_for(
    layout: &XkbLayout,
    scancode: u16,
    shift: bool,
    altgr: bool,
    caps: bool,
) -> Option<char> {
    if scancode == 0x39 {
        return Some(' ');
    }
    let index = KEYS.iter().position(|&k| k == scancode)?;
    let mut chars = row(layout)[index].chars();
    let normal = chars.next();
    let shifted = chars.next();
    let alt = chars.next();
    let pick = if altgr {
        alt
    } else {
        let letter = normal.is_some_and(char::is_alphabetic);
        if shift != (caps && letter) {
            shifted
        } else {
            normal
        }
    };
    pick.filter(|&c| c != '\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(name: &str) -> XkbLayout {
        XkbLayout::new(name, "")
    }

    #[test]
    fn a_us_keyboard_types_letters_digits_and_symbols() {
        let us = l("us");
        assert_eq!(char_for(&us, 0x1E, false, false, false), Some('a'));
        assert_eq!(char_for(&us, 0x1E, true, false, false), Some('A'));
        assert_eq!(char_for(&us, 0x03, true, false, false), Some('@'));
        assert_eq!(char_for(&us, 0x39, false, false, false), Some(' '));
        assert_eq!(char_for(&us, 0x01, false, false, false), None); // escape
        assert_eq!(char_for(&us, 0x56, false, false, false), None);
    }

    #[test]
    fn an_azerty_keyboard_moves_the_letters_and_needs_shift_for_digits() {
        let fr = l("fr");
        assert_eq!(char_for(&fr, 0x10, false, false, false), Some('a'));
        assert_eq!(char_for(&fr, 0x1E, false, false, false), Some('q'));
        assert_eq!(char_for(&fr, 0x02, false, false, false), Some('&'));
        assert_eq!(char_for(&fr, 0x02, true, false, false), Some('1'));
        assert_eq!(char_for(&fr, 0x0B, false, true, false), Some('@'));
        assert_eq!(char_for(&fr, 0x1A, false, false, false), None); // dead key
    }

    #[test]
    fn a_qwertz_keyboard_swaps_y_and_z() {
        let de = l("de");
        assert_eq!(char_for(&de, 0x15, false, false, false), Some('z'));
        assert_eq!(char_for(&de, 0x2C, false, false, false), Some('y'));
        assert_eq!(char_for(&de, 0x10, false, true, false), Some('@'));
    }

    #[test]
    fn the_british_keyboard_differs_from_the_us_one() {
        let gb = l("gb");
        assert_eq!(char_for(&gb, 0x04, true, false, false), Some('£'));
        assert_eq!(char_for(&gb, 0x2B, false, false, false), Some('#'));
    }

    #[test]
    fn caps_lock_changes_letters_only() {
        let us = l("us");
        assert_eq!(char_for(&us, 0x1E, false, false, true), Some('A'));
        assert_eq!(char_for(&us, 0x1E, true, false, true), Some('a'));
        assert_eq!(char_for(&us, 0x02, false, false, true), Some('1'));
        let fr = l("fr");
        assert_eq!(char_for(&fr, 0x02, false, false, true), Some('&'));
    }

    #[test]
    fn an_unlisted_layout_types_as_a_us_keyboard() {
        assert_eq!(char_for(&l("cz"), 0x10, false, false, false), Some('q'));
    }

    #[test]
    fn every_layout_has_a_row_per_key() {
        for r in [&US, &GB, &FR, &DE] {
            assert_eq!(r.len(), KEYS.len());
        }
    }
}
