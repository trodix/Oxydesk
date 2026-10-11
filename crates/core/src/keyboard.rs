//! Client keyboard layout: Windows keyboard layout identifier (KLID, announced in the RDP client core data)
//! → XKB layout of the session.
//!
//! RDP sends physical keys (scancodes): the character produced depends on the layout of the session. For
//! AZERTY, QWERTZ… to give the same characters as on the client, the session adopts the client's layout.

/// XKB layout and variant (`setxkbmap -layout fr -variant oss`); an empty variant is the default one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XkbLayout {
    pub layout: String,
    pub variant: String,
}

impl XkbLayout {
    pub fn new(layout: &str, variant: &str) -> Self {
        Self {
            layout: layout.into(),
            variant: variant.into(),
        }
    }
}

/// (language identifier, layout, variant). The language identifier is the low word of the KLID.
const TABLE: &[(u16, &str, &str)] = &[
    (0x0409, "us", ""),
    (0x0809, "gb", ""),
    (0x1009, "ca", "fr"), // Canada, English: the multilingual standard layout is the common one
    (0x0C09, "us", ""),   // Australia
    (0x1409, "us", ""),   // New Zealand
    (0x1809, "ie", ""),
    (0x040C, "fr", ""),
    (0x080C, "be", ""),
    (0x0C0C, "ca", "fr"),
    (0x100C, "ch", "fr"),
    (0x140C, "fr", ""), // Luxembourg
    (0x0407, "de", ""),
    (0x0807, "ch", "de"),
    (0x0C07, "at", ""),
    (0x1007, "de", ""), // Luxembourg
    (0x0813, "be", ""),
    (0x0413, "us", "intl"), // Dutch: the usual Windows layout is US international
    (0x0410, "it", ""),
    (0x0810, "ch", "it"),
    (0x040A, "es", ""),
    (0x0C0A, "es", ""),
    (0x080A, "latam", ""),
    (0x0816, "pt", ""),
    (0x0416, "br", ""),
    (0x0406, "dk", ""),
    (0x041D, "se", ""),
    (0x081D, "se", ""),
    (0x0414, "no", ""),
    (0x0814, "no", ""),
    (0x040B, "fi", ""),
    (0x0415, "pl", ""),
    (0x0405, "cz", ""),
    (0x041B, "sk", ""),
    (0x040E, "hu", ""),
    (0x0418, "ro", ""),
    (0x0402, "bg", ""),
    (0x041A, "hr", ""),
    (0x0424, "si", ""),
    (0x041F, "tr", ""),
    (0x0408, "gr", ""),
    (0x0419, "ru", ""),
    (0x0422, "ua", ""),
    (0x0425, "ee", ""),
    (0x0426, "lv", ""),
    (0x0427, "lt", ""),
    (0x040F, "is", ""),
    (0x040D, "il", ""),
    (0x0401, "ara", ""),
    (0x0411, "jp", ""),
    (0x0412, "kr", ""),
];

/// XKB layout matching a Windows KLID, `None` when it is unknown or not announced (0): the session keeps
/// its own layout.
pub fn klid_to_xkb(klid: u32) -> Option<XkbLayout> {
    let lang = (klid & 0xFFFF) as u16;
    TABLE
        .iter()
        .find(|(l, _, _)| *l == lang)
        .map(|(_, layout, variant)| XkbLayout::new(layout, variant))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_layouts() {
        assert_eq!(klid_to_xkb(0x0409), Some(XkbLayout::new("us", "")));
        assert_eq!(klid_to_xkb(0x040C), Some(XkbLayout::new("fr", "")));
        assert_eq!(klid_to_xkb(0x0407), Some(XkbLayout::new("de", "")));
        assert_eq!(klid_to_xkb(0x0807), Some(XkbLayout::new("ch", "de")));
        assert_eq!(klid_to_xkb(0x100C), Some(XkbLayout::new("ch", "fr")));
    }

    #[test]
    fn device_word_is_ignored() {
        assert_eq!(klid_to_xkb(0xF0C0_040C), Some(XkbLayout::new("fr", "")));
    }

    #[test]
    fn unknown_or_absent_keeps_the_session_layout() {
        assert_eq!(klid_to_xkb(0), None);
        assert_eq!(klid_to_xkb(0x7777), None);
    }

    #[test]
    fn language_ids_are_unique() {
        for (i, (a, ..)) in TABLE.iter().enumerate() {
            assert!(TABLE[i + 1..].iter().all(|(b, ..)| a != b), "{a:#06x}");
        }
    }
}
