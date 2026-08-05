// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use baseview::MouseButton;

pub(crate) const SCROLL_LINES_TO_POINTS: f32 = 10.0; // ~10 pixels/line (baseview→egui heuristic)

/// Maps baseview mouse buttons → egui.
pub(crate) fn map_mouse_button(button: MouseButton) -> Option<egui::PointerButton> {
    match button {
        MouseButton::Left => Some(egui::PointerButton::Primary),
        MouseButton::Right => Some(egui::PointerButton::Secondary),
        MouseButton::Middle => Some(egui::PointerButton::Middle),
        _ => None,
    }
}

/// Maps baseview keyboard modifiers → egui.
///
/// On Linux, `META` (Super/Win) is mapped to both `mac_cmd` and `command`,
/// maintaining compatibility with egui's cross-platform shortcuts.
pub(crate) fn map_modifiers(mods: keyboard_types::Modifiers) -> egui::Modifiers {
    egui::Modifiers {
        alt: mods.contains(keyboard_types::Modifiers::ALT),
        ctrl: mods.contains(keyboard_types::Modifiers::CONTROL),
        shift: mods.contains(keyboard_types::Modifiers::SHIFT),
        mac_cmd: mods.contains(keyboard_types::Modifiers::META),
        command: mods.contains(keyboard_types::Modifiers::META)
            || mods.contains(keyboard_types::Modifiers::CONTROL),
    }
}

/// Converts a baseview keyboard event into an `egui::Event::Key`.
///
/// Maps navigation keys (arrows, Tab, Enter, etc.), letters (a-z), digits (0-9)
/// and space. Unmapped keys (F-keys, special keys) return `None` and are ignored.
pub(crate) fn map_keyboard_event(key_event: &keyboard_types::KeyboardEvent) -> Option<egui::Event> {
    let pressed = match key_event.state {
        keyboard_types::KeyState::Down => true,
        keyboard_types::KeyState::Up => false,
    };

    let modifiers = map_modifiers(key_event.modifiers);

    let egui_key = match &key_event.key {
        keyboard_types::Key::ArrowDown => Some(egui::Key::ArrowDown),
        keyboard_types::Key::ArrowLeft => Some(egui::Key::ArrowLeft),
        keyboard_types::Key::ArrowRight => Some(egui::Key::ArrowRight),
        keyboard_types::Key::ArrowUp => Some(egui::Key::ArrowUp),
        keyboard_types::Key::Escape => Some(egui::Key::Escape),
        keyboard_types::Key::Enter => Some(egui::Key::Enter),
        keyboard_types::Key::Tab => Some(egui::Key::Tab),
        keyboard_types::Key::Backspace => Some(egui::Key::Backspace),
        keyboard_types::Key::Delete => Some(egui::Key::Delete),
        keyboard_types::Key::Home => Some(egui::Key::Home),
        keyboard_types::Key::End => Some(egui::Key::End),
        keyboard_types::Key::PageUp => Some(egui::Key::PageUp),
        keyboard_types::Key::PageDown => Some(egui::Key::PageDown),
        keyboard_types::Key::Character(s) => {
            if s.len() == 1 {
                // SAFETY: len() == 1 guarantees that next() does not return None.
                let c = s.chars().next().unwrap_or('\0').to_ascii_lowercase();
                match c {
                    'a' => Some(egui::Key::A),
                    'b' => Some(egui::Key::B),
                    'c' => Some(egui::Key::C),
                    'd' => Some(egui::Key::D),
                    'e' => Some(egui::Key::E),
                    'f' => Some(egui::Key::F),
                    'g' => Some(egui::Key::G),
                    'h' => Some(egui::Key::H),
                    'i' => Some(egui::Key::I),
                    'j' => Some(egui::Key::J),
                    'k' => Some(egui::Key::K),
                    'l' => Some(egui::Key::L),
                    'm' => Some(egui::Key::M),
                    'n' => Some(egui::Key::N),
                    'o' => Some(egui::Key::O),
                    'p' => Some(egui::Key::P),
                    'q' => Some(egui::Key::Q),
                    'r' => Some(egui::Key::R),
                    's' => Some(egui::Key::S),
                    't' => Some(egui::Key::T),
                    'u' => Some(egui::Key::U),
                    'v' => Some(egui::Key::V),
                    'w' => Some(egui::Key::W),
                    'x' => Some(egui::Key::X),
                    'y' => Some(egui::Key::Y),
                    'z' => Some(egui::Key::Z),
                    '0' => Some(egui::Key::Num0),
                    '1' => Some(egui::Key::Num1),
                    '2' => Some(egui::Key::Num2),
                    '3' => Some(egui::Key::Num3),
                    '4' => Some(egui::Key::Num4),
                    '5' => Some(egui::Key::Num5),
                    '6' => Some(egui::Key::Num6),
                    '7' => Some(egui::Key::Num7),
                    '8' => Some(egui::Key::Num8),
                    '9' => Some(egui::Key::Num9),
                    ' ' => Some(egui::Key::Space),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    };

    egui_key.map(|key| egui::Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers,
    })
}
