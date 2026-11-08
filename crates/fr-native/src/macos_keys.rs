//! macOS virtual keycodes (`kVK_*`, `HIToolbox` `Events.h`) to USB HID usages
//! (page 0x07). Physical key identity only: layout, dead keys and committed
//! text never enter this table. Pure data, compiled on every target so its
//! tests run in ordinary CI.
#![forbid(unsafe_code)]

/// A modifier key reported by `NSEventTypeFlagsChanged`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Modifier {
    /// HID usage (0xE0..=0xE7).
    pub usage: u16,
    /// `NX_DEVICE*KEYMASK` bit for this side of the keyboard.
    pub device_mask: u64,
    /// Device-independent `NSEventModifierFlag*` bit.
    pub flag: u64,
}

/// All device-dependent modifier bits (`NX_DEVICE*KEYMASK`).
pub const DEVICE_MASKS: u64 = 0x207f;
/// `NSEventModifierFlagCapsLock`.
pub const CAPS_LOCK_FLAG: u64 = 1 << 16;
/// `kVK_CapsLock`.
pub const CAPS_LOCK: u16 = 0x39;

pub const fn modifier(keycode: u16) -> Option<Modifier> {
    const SHIFT: u64 = 1 << 17;
    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    let (usage, device_mask, flag) = match keycode {
        0x3b => (0xe0, 0x0001, CONTROL),
        0x38 => (0xe1, 0x0002, SHIFT),
        0x3a => (0xe2, 0x0020, OPTION),
        0x37 => (0xe3, 0x0008, COMMAND),
        0x3e => (0xe4, 0x2000, CONTROL),
        0x3c => (0xe5, 0x0004, SHIFT),
        0x3d => (0xe6, 0x0040, OPTION),
        0x36 => (0xe7, 0x0010, COMMAND),
        _ => return None,
    };
    Some(Modifier {
        usage,
        device_mask,
        flag,
    })
}

/// HID usage for a macOS virtual keycode, or `None` for keys without one
/// (Fn) and codes no keyboard sends.
#[allow(clippy::too_many_lines, reason = "one arm per key: a data table")]
pub const fn usage(keycode: u16) -> Option<u16> {
    if let Some(m) = modifier(keycode) {
        return Some(m.usage);
    }
    Some(match keycode {
        0x00 => 0x04, // A
        0x0b => 0x05, // B
        0x08 => 0x06, // C
        0x02 => 0x07, // D
        0x0e => 0x08, // E
        0x03 => 0x09, // F
        0x05 => 0x0a, // G
        0x04 => 0x0b, // H
        0x22 => 0x0c, // I
        0x26 => 0x0d, // J
        0x28 => 0x0e, // K
        0x25 => 0x0f, // L
        0x2e => 0x10, // M
        0x2d => 0x11, // N
        0x1f => 0x12, // O
        0x23 => 0x13, // P
        0x0c => 0x14, // Q
        0x0f => 0x15, // R
        0x01 => 0x16, // S
        0x11 => 0x17, // T
        0x20 => 0x18, // U
        0x09 => 0x19, // V
        0x0d => 0x1a, // W
        0x07 => 0x1b, // X
        0x10 => 0x1c, // Y
        0x06 => 0x1d, // Z
        0x12 => 0x1e, // 1
        0x13 => 0x1f, // 2
        0x14 => 0x20, // 3
        0x15 => 0x21, // 4
        0x17 => 0x22, // 5
        0x16 => 0x23, // 6
        0x1a => 0x24, // 7
        0x1c => 0x25, // 8
        0x19 => 0x26, // 9
        0x1d => 0x27, // 0
        0x24 => 0x28, // Return
        0x35 => 0x29, // Escape
        0x33 => 0x2a, // Delete (backspace)
        0x30 => 0x2b, // Tab
        0x31 => 0x2c, // Space
        0x1b => 0x2d, // - _
        0x18 => 0x2e, // = +
        0x21 => 0x2f, // [ {
        0x1e => 0x30, // ] }
        0x2a => 0x31, // \ |
        0x29 => 0x33, // ; :
        0x27 => 0x34, // ' "
        0x32 => 0x35, // ` ~
        0x2b => 0x36, // , <
        0x2f => 0x37, // . >
        0x2c => 0x38, // / ?
        0x39 => 0x39, // Caps Lock
        0x7a => 0x3a, // F1
        0x78 => 0x3b, // F2
        0x63 => 0x3c, // F3
        0x76 => 0x3d, // F4
        0x60 => 0x3e, // F5
        0x61 => 0x3f, // F6
        0x62 => 0x40, // F7
        0x64 => 0x41, // F8
        0x65 => 0x42, // F9
        0x6d => 0x43, // F10
        0x67 => 0x44, // F11
        0x6f => 0x45, // F12
        0x72 => 0x49, // Help: the Insert position on PC-layout keyboards
        0x73 => 0x4a, // Home
        0x74 => 0x4b, // Page Up
        0x75 => 0x4c, // Forward Delete
        0x77 => 0x4d, // End
        0x79 => 0x4e, // Page Down
        0x7c => 0x4f, // Right Arrow
        0x7b => 0x50, // Left Arrow
        0x7d => 0x51, // Down Arrow
        0x7e => 0x52, // Up Arrow
        0x47 => 0x53, // Keypad Clear: the Num Lock position
        0x4b => 0x54, // Keypad /
        0x43 => 0x55, // Keypad *
        0x4e => 0x56, // Keypad -
        0x45 => 0x57, // Keypad +
        0x4c => 0x58, // Keypad Enter
        0x53 => 0x59, // Keypad 1
        0x54 => 0x5a, // Keypad 2
        0x55 => 0x5b, // Keypad 3
        0x56 => 0x5c, // Keypad 4
        0x57 => 0x5d, // Keypad 5
        0x58 => 0x5e, // Keypad 6
        0x59 => 0x5f, // Keypad 7
        0x5b => 0x60, // Keypad 8
        0x5c => 0x61, // Keypad 9
        0x52 => 0x62, // Keypad 0
        0x41 => 0x63, // Keypad .
        0x0a => 0x64, // ISO section: the non-US backslash position
        0x6e => 0x65, // Contextual menu: Application
        0x51 => 0x67, // Keypad =
        0x69 => 0x68, // F13
        0x6b => 0x69, // F14
        0x71 => 0x6a, // F15
        0x6a => 0x6b, // F16
        0x40 => 0x6c, // F17
        0x4f => 0x6d, // F18
        0x50 => 0x6e, // F19
        0x5a => 0x6f, // F20
        0x4a => 0x7f, // Mute
        0x48 => 0x80, // Volume Up
        0x49 => 0x81, // Volume Down
        0x5f => 0x85, // JIS keypad comma
        0x5e => 0x87, // JIS underscore (International1)
        0x5d => 0x89, // JIS yen (International3)
        0x68 => 0x90, // JIS kana (LANG1)
        0x66 => 0x91, // JIS eisu (LANG2)
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::input::PhysicalKey;

    #[test]
    fn every_mapped_usage_is_an_admitted_physical_key_and_unique() {
        let mut seen = [None::<u16>; 256];
        let mut mapped = 0;
        for keycode in 0..=0x7f_u16 {
            let Some(usage) = usage(keycode) else {
                continue;
            };
            mapped += 1;
            assert!(
                PhysicalKey::new(usage).is_some(),
                "{keycode:#x} -> {usage:#x}"
            );
            let slot = &mut seen[usize::from(usage)];
            assert_eq!(
                *slot, None,
                "usage {usage:#x} from {keycode:#x} and {slot:?}"
            );
            *slot = Some(keycode);
        }
        // Letters, digits, punctuation, F1-F20, navigation, keypad, modifiers.
        assert_eq!(mapped, 119);
        assert_eq!(usage(0x3f), None, "Fn has no HID keyboard usage");
        assert_eq!(usage(0x80), None);
    }

    #[test]
    fn known_positions_map_by_physical_location() {
        // kVK_ANSI_A, kVK_ANSI_Q, kVK_ANSI_Z sit where HID A, Q, Z sit.
        assert_eq!(usage(0x00), Some(0x04));
        assert_eq!(usage(0x0c), Some(0x14));
        assert_eq!(usage(0x06), Some(0x1d));
        assert_eq!(usage(0x33), Some(0x2a), "Delete is backspace");
        assert_eq!(usage(0x75), Some(0x4c), "Forward Delete is Delete");
        assert_eq!(usage(0x37), Some(0xe3), "Command is left GUI");
        assert_eq!(usage(0x36), Some(0xe7));
    }

    #[test]
    fn modifier_masks_are_distinct_device_bits() {
        let mut all = 0;
        for keycode in [0x3b, 0x38, 0x3a, 0x37, 0x3e, 0x3c, 0x3d, 0x36] {
            let m = modifier(keycode).unwrap();
            assert_eq!(all & m.device_mask, 0);
            all |= m.device_mask;
            assert_eq!(m.device_mask & !DEVICE_MASKS, 0);
        }
        assert_eq!(all, DEVICE_MASKS);
        assert_eq!(
            modifier(CAPS_LOCK),
            None,
            "Caps Lock toggles; it is not held"
        );
    }
}
