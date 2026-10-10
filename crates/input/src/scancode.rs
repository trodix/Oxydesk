//! RDP scancode (set 1, prefix 0xE000 = extended) → evdev code (linux/input-event-codes.h) conversion.

/// Returns the evdev code, or `None` for an unsupported key.
pub fn rdp_to_evdev(scancode: u16) -> Option<u16> {
    let code = scancode & 0xFF;
    if scancode & 0xE000 == 0xE000 {
        return match code {
            0x1C => Some(96),  // KP_ENTER
            0x1D => Some(97),  // RIGHT_CTRL
            0x35 => Some(98),  // KP_SLASH
            0x37 => Some(99),  // PRINT / SYSRQ
            0x38 => Some(100), // RIGHT_ALT
            0x47 => Some(102), // HOME
            0x48 => Some(103), // UP
            0x49 => Some(104), // PAGE_UP
            0x4B => Some(105), // LEFT
            0x4D => Some(106), // RIGHT
            0x4F => Some(107), // END
            0x50 => Some(108), // DOWN
            0x51 => Some(109), // PAGE_DOWN
            0x52 => Some(110), // INSERT
            0x53 => Some(111), // DELETE
            0x5B => Some(125), // LEFT_META
            0x5C => Some(126), // RIGHT_META
            0x5D => Some(127), // COMPOSE (menu)
            _ => None,
        };
    }
    // For 0x01..=0x58 set 1 coincides with evdev (ESC=1 … F12=88).
    (1..=0x58).contains(&code).then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_keys_are_identity() {
        assert_eq!(rdp_to_evdev(0x02), Some(2)); // '1'
        assert_eq!(rdp_to_evdev(0x1E), Some(30)); // 'A'
        assert_eq!(rdp_to_evdev(0x58), Some(88)); // F12
    }

    #[test]
    fn extended_keys_use_table() {
        assert_eq!(rdp_to_evdev(0xE048), Some(103)); // UP
        assert_eq!(rdp_to_evdev(0xE053), Some(111)); // DELETE
        assert_eq!(rdp_to_evdev(0xE05B), Some(125)); // META
    }

    #[test]
    fn unknown_is_none() {
        assert_eq!(rdp_to_evdev(0), None);
        assert_eq!(rdp_to_evdev(0xE0FF), None);
        assert_eq!(rdp_to_evdev(0x7F), None);
    }
}
