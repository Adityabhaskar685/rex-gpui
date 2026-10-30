//! Translate GPUI keystrokes into the `client.key` event shape.
//!
//! Rex wants the physical key as a W3C `code` name and leaves the escape-sequence encoding,
//! including the Kitty keyboard protocol, to the server. GPUI on Linux reports the key's
//! symbol instead, and for shifted symbols such as `!` it clears `shift`, so we map each
//! symbol back to its key on a US layout and restore the shift.

use gpui::{Keystroke, Modifiers};
use serde_json::{Value, json};

/// `(code, unshifted character, symbol implies shift)` for a GPUI key name.
fn physical_key(key: &str) -> Option<(&'static str, Option<char>, bool)> {
    const LETTERS: [&str; 26] = [
        "key_a", "key_b", "key_c", "key_d", "key_e", "key_f", "key_g", "key_h", "key_i", "key_j",
        "key_k", "key_l", "key_m", "key_n", "key_o", "key_p", "key_q", "key_r", "key_s", "key_t",
        "key_u", "key_v", "key_w", "key_x", "key_y", "key_z",
    ];
    const DIGITS: [&str; 10] = [
        "digit_0", "digit_1", "digit_2", "digit_3", "digit_4", "digit_5", "digit_6", "digit_7",
        "digit_8", "digit_9",
    ];
    const FKEYS: [&str; 24] = [
        "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14",
        "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24",
    ];

    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_lowercase() {
            return Some((LETTERS[(c as u8 - b'a') as usize], Some(c), false));
        }
        if c.is_ascii_digit() {
            return Some((DIGITS[(c as u8 - b'0') as usize], Some(c), false));
        }
        let (code, base, shifted) = match c {
            '`' => ("backquote", '`', false),
            '~' => ("backquote", '`', true),
            '-' => ("minus", '-', false),
            '_' => ("minus", '-', true),
            '=' => ("equal", '=', false),
            '+' => ("equal", '=', true),
            '[' => ("bracket_left", '[', false),
            '{' => ("bracket_left", '[', true),
            ']' => ("bracket_right", ']', false),
            '}' => ("bracket_right", ']', true),
            '\\' => ("backslash", '\\', false),
            '|' => ("backslash", '\\', true),
            ';' => ("semicolon", ';', false),
            ':' => ("semicolon", ';', true),
            '\'' => ("quote", '\'', false),
            '"' => ("quote", '\'', true),
            ',' => ("comma", ',', false),
            '<' => ("comma", ',', true),
            '.' => ("period", '.', false),
            '>' => ("period", '.', true),
            '/' => ("slash", '/', false),
            '?' => ("slash", '/', true),
            '!' => ("digit_1", '1', true),
            '@' => ("digit_2", '2', true),
            '#' => ("digit_3", '3', true),
            '$' => ("digit_4", '4', true),
            '%' => ("digit_5", '5', true),
            '^' => ("digit_6", '6', true),
            '&' => ("digit_7", '7', true),
            '*' => ("digit_8", '8', true),
            '(' => ("digit_9", '9', true),
            ')' => ("digit_0", '0', true),
            _ => return None,
        };
        return Some((code, Some(base), shifted));
    }

    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<usize>().ok()) {
        return FKEYS
            .get(n.wrapping_sub(1))
            .map(|code| (*code, None, false));
    }
    let code = match key {
        "enter" => "enter",
        "escape" => "escape",
        "backspace" => "backspace",
        "tab" => "tab",
        "space" => return Some(("space", Some(' '), false)),
        "delete" => "delete",
        "insert" => "insert",
        "home" => "home",
        "end" => "end",
        "pageup" => "page_up",
        "pagedown" => "page_down",
        "left" => "arrow_left",
        "right" => "arrow_right",
        "up" => "arrow_up",
        "down" => "arrow_down",
        "shift" => "shift_left",
        "control" => "control_left",
        "alt" => "alt_left",
        "platform" | "super" => "meta_left",
        "menu" => "context_menu",
        _ => return None,
    };
    Some((code, None, false))
}

/// The `key` object for `client.key`, or `None` for a key Rex has no name for.
pub fn client_key(keystroke: &Keystroke, repeat: bool) -> Option<Value> {
    let (code, unshifted, implied_shift) = match physical_key(&keystroke.key) {
        Some(found) => found,
        // A non-ASCII symbol from another layout: send it as text on an unidentified key.
        None if keystroke.key_char.is_some() => ("unidentified", None, false),
        None => return None,
    };
    let m = &keystroke.modifiers;
    let shift = m.shift || implied_shift;
    let mods = mods_string(&Modifiers { shift, ..*m });
    let text = match keystroke.key_char.as_deref() {
        // Ctrl and super combinations type nothing; the server encodes them from the key.
        Some(text) if !m.control && !m.platform => text,
        _ if code == "space" && !m.control => " ",
        _ => "",
    };
    // The layout spent shift to produce the text, such as `A` or `!`.
    let consumed = if shift && !text.is_empty() {
        "shift"
    } else {
        ""
    };
    Some(json!({
        "key": code,
        "mods": mods,
        "consumed_mods": consumed,
        "text": text,
        "unshifted_codepoint": unshifted.map_or(0, |c| c as u32),
        "composing": false,
        "action": if repeat { "repeat" } else { "press" },
    }))
}

/// Modifiers in Rex's form: names joined with `+`, in the order shift, ctrl, alt, super.
pub fn mods_string(m: &Modifiers) -> String {
    let mut mods = Vec::new();
    if m.shift {
        mods.push("shift");
    }
    if m.control {
        mods.push("ctrl");
    }
    if m.alt {
        mods.push("alt");
    }
    if m.platform {
        mods.push("super");
    }
    mods.join("+")
}
