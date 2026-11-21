//! Global hotkeys via RegisterHotKey on a dedicated message thread. They live in the engine
//! so a busy or crashed UI never costs a clip.

use crossbeam_channel::Sender;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub vk: u32,
}

impl Hotkey {
    /// Parses strings like "F9", "Alt+F10", "Ctrl+Shift+S".
    pub fn parse(s: &str) -> Option<Self> {
        let mut modifiers = 0u32;
        let mut vk = None;
        for part in s.split('+').map(|p| p.trim().to_ascii_uppercase()) {
            match part.as_str() {
                "CTRL" | "CONTROL" => modifiers |= MOD_CONTROL.0,
                "ALT" => modifiers |= MOD_ALT.0,
                "SHIFT" => modifiers |= MOD_SHIFT.0,
                "WIN" => modifiers |= MOD_WIN.0,
                key => vk = Some(parse_key(key)?),
            }
        }
        Some(Self { modifiers, vk: vk? })
    }
}

fn parse_key(k: &str) -> Option<u32> {
    if let Some(n) = k.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        return (1..=24).contains(&n).then_some(0x70 + n - 1);
    }
    let b = k.as_bytes();
    if b.len() == 1 && (b[0].is_ascii_uppercase() || b[0].is_ascii_digit()) {
        return Some(b[0] as u32);
    }
    match k {
        "HOME" => Some(0x24),
        "END" => Some(0x23),
        "INSERT" | "INS" => Some(0x2D),
        "PAUSE" => Some(0x13),
        "PRINTSCREEN" | "PRTSC" => Some(0x2C),
        "SCROLLLOCK" => Some(0x91),
        "PAGEUP" => Some(0x21),
        "PAGEDOWN" => Some(0x22),
        _ => None,
    }
}

/// Registers `bindings` (id, hotkey) and forwards presses as ids. Returns the ids that
/// failed to register (usually: another app owns that combination).
pub fn spawn(bindings: Vec<(i32, Hotkey)>, tx: Sender<i32>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("hotkeys".into())
        .spawn(move || {
            for (id, hk) in &bindings {
                let mods = HOT_KEY_MODIFIERS(hk.modifiers | MOD_NOREPEAT.0);
                if let Err(e) = unsafe { RegisterHotKey(None, *id, mods, hk.vk) } {
                    tracing::error!(?hk, "hotkey registration failed (in use by another app?): {e}");
                }
            }
            let mut msg = MSG::default();
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                if msg.message == WM_HOTKEY {
                    let _ = tx.send(msg.wParam.0 as i32);
                }
            }
        })
        .expect("spawn hotkey thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_hotkeys() {
        assert_eq!(Hotkey::parse("F9"), Some(Hotkey { modifiers: 0, vk: 0x78 }));
        assert_eq!(
            Hotkey::parse("alt+f10"),
            Some(Hotkey {
                modifiers: MOD_ALT.0,
                vk: 0x79
            })
        );
        assert_eq!(
            Hotkey::parse("Ctrl+Shift+S"),
            Some(Hotkey {
                modifiers: MOD_CONTROL.0 | MOD_SHIFT.0,
                vk: b'S' as u32
            })
        );
        assert_eq!(Hotkey::parse("Ctrl+"), None);
    }
}
