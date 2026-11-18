//! Every keyboard shortcut, once: its action, its name in the keybindings file, its default
//! keys, and how the shortcuts overlay describes it. Bindings in
//! `~/.config/rex-gpui/keybindings.json` replace the defaults, action by action:
//!
//! ```json
//! {
//!   "split_right": "alt-d",
//!   "copy": ["ctrl-shift-c", "ctrl-insert"],
//!   "quit": null
//! }
//! ```
//!
//! A key given to one action is taken from any other action that had it by default. `null`
//! or `[]` leaves an action without keys.

use std::path::PathBuf;
use std::sync::OnceLock;

use gpui::{App, KeyBinding, Keystroke, actions};
use serde_json::Value;

pub struct ActionInfo {
    pub group: &'static str,
    /// The action's name in the keybindings file.
    pub name: &'static str,
    pub defaults: &'static [&'static str],
    /// What the shortcuts overlay says it does.
    pub what: &'static str,
    bind: fn(&str) -> KeyBinding,
}

macro_rules! keymap {
    ($($group:literal => [$($action:ident $name:literal [$($keys:literal),*] $what:literal;)*])*) => {
        actions!(rex, [$($($action,)*)*]);

        pub const ACTIONS: &[ActionInfo] = &[$($(ActionInfo {
            group: $group,
            name: $name,
            defaults: &[$($keys),*],
            what: $what,
            bind: |keys| KeyBinding::new(keys, $action, None),
        },)*)*];
    };
}

keymap! {
    "Tabs and panes" => [
        NewWindow "new_tab" ["ctrl-shift-t"] "New tab";
        NextWindow "next_tab" ["ctrl-tab"] "Next tab";
        PreviousWindow "previous_tab" ["ctrl-shift-tab"] "Previous tab";
        SplitRight "split_right" ["ctrl-shift-d"] "Split right";
        SplitDown "split_down" ["ctrl-shift-e"] "Split down";
        NextPane "next_pane" ["ctrl-shift-o"] "Next pane";
        FocusLeft "focus_left" ["ctrl-shift-left"] "Pane to the left";
        FocusRight "focus_right" ["ctrl-shift-right"] "Pane to the right";
        FocusUp "focus_up" ["ctrl-shift-up"] "Pane above";
        FocusDown "focus_down" ["ctrl-shift-down"] "Pane below";
        ToggleZoom "zoom_pane" ["ctrl-shift-z"] "Zoom pane";
        ClosePane "close_pane" ["ctrl-shift-w"] "Close pane";
    ]
    "Sessions" => [
        NewSession "new_session" ["ctrl-shift-n"] "New session";
        NextSession "next_session" ["ctrl-shift-s"] "Next session";
        RenameSession "rename_session" ["ctrl-shift-r"] "Rename session";
        QuickSwitch "quick_switch" ["ctrl-shift-p"] "Go to a session or tab";
    ]
    "Terminal" => [
        Copy "copy" ["ctrl-shift-c"] "Copy the selection";
        Paste "paste" ["ctrl-shift-v"] "Paste";
        Find "find" ["ctrl-shift-f"] "Search the screen and scrollback";
        ScrollPageUp "scroll_page_up" ["shift-pageup"] "Scroll back a page";
        ScrollPageDown "scroll_page_down" ["shift-pagedown"] "Scroll forward a page";
        IncreaseFontSize "increase_font_size" ["ctrl-=", "ctrl-+"] "Bigger text";
        DecreaseFontSize "decrease_font_size" ["ctrl--"] "Smaller text";
        ResetFontSize "reset_font_size" ["ctrl-0"] "Default text size";
    ]
    "Window" => [
        ToggleSidebar "toggle_sidebar" ["ctrl-shift-b"] "Show or hide the sidebar";
        ToggleShortcuts "show_shortcuts" ["ctrl-shift-k"] "Show these shortcuts";
        Quit "quit" ["ctrl-shift-q"] "Quit";
    ]
}

pub fn path() -> PathBuf {
    crate::settings::dir().join("keybindings.json")
}

/// The keys in effect, and what was wrong with the keybindings file.
pub struct Keymap {
    /// Each action's keys, in the order of `ACTIONS`.
    pub keys: Vec<Vec<String>>,
    pub problems: Vec<String>,
}

static KEYMAP: OnceLock<Keymap> = OnceLock::new();

/// The keys in effect, once `bind_keys` has run.
pub fn current() -> &'static Keymap {
    KEYMAP.get_or_init(|| resolve(None))
}

/// Bind the default keys, with the keybindings file's changes.
pub fn bind_keys(cx: &mut App) {
    let user = match std::fs::read_to_string(path()) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(value) => Some(value),
            Err(err) => {
                let problem = format!("{} isn't valid JSON: {err}", path().display());
                let mut keymap = resolve(None);
                keymap.problems.push(problem);
                return bind(keymap, cx);
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            log::warn!("reading {}: {err}", path().display());
            None
        }
    };
    bind(resolve(user.as_ref()), cx);
}

fn bind(keymap: Keymap, cx: &mut App) {
    for problem in &keymap.problems {
        log::warn!("keybindings: {problem}");
    }
    let bindings = ACTIONS
        .iter()
        .zip(&keymap.keys)
        .flat_map(|(action, keys)| keys.iter().map(|k| (action.bind)(k)))
        .collect::<Vec<_>>();
    cx.bind_keys(bindings);
    let _ = KEYMAP.set(keymap);
}

/// A key as a comparable value, or `None` when GPUI can't read it. A binding can be a
/// sequence of keys, separated by spaces.
fn normalize(keys: &str) -> Option<String> {
    let parsed = keys
        .split_whitespace()
        .map(|k| Keystroke::parse(k).ok().map(|k| k.unparse()))
        .collect::<Option<Vec<_>>>()?;
    (!parsed.is_empty()).then(|| parsed.join(" "))
}

/// The defaults with `user`'s changes: a map from action names to a key, a list of keys,
/// or null.
fn resolve(user: Option<&Value>) -> Keymap {
    let mut keys: Vec<Vec<String>> = ACTIONS
        .iter()
        .map(|a| a.defaults.iter().map(|k| k.to_string()).collect())
        .collect();
    let mut problems = Vec::new();
    let mut overridden = vec![false; ACTIONS.len()];

    match user {
        None => {}
        Some(Value::Object(map)) => {
            for (name, value) in map {
                let Some(index) = ACTIONS.iter().position(|a| a.name == name) else {
                    problems.push(format!("there's no action called \"{name}\""));
                    continue;
                };
                let given: Vec<&Value> = match value {
                    Value::Null => Vec::new(),
                    Value::Array(list) => list.iter().collect(),
                    other => vec![other],
                };
                let mut valid = Vec::new();
                for key in given {
                    match key.as_str().filter(|k| normalize(k).is_some()) {
                        Some(k) => valid.push(k.to_string()),
                        None => problems.push(format!("\"{name}\": {key} isn't a key")),
                    }
                }
                keys[index] = valid;
                overridden[index] = true;
            }
        }
        Some(_) => problems.push("the file should hold an object of action names".into()),
    }

    // A key the file gives to one action no longer runs another action by default.
    let claimed: Vec<String> = (0..ACTIONS.len())
        .filter(|&i| overridden[i])
        .flat_map(|i| keys[i].iter().filter_map(|k| normalize(k)))
        .collect();
    for (i, list) in keys.iter_mut().enumerate() {
        if !overridden[i] {
            list.retain(|k| normalize(k).is_none_or(|k| !claimed.contains(&k)));
        }
    }
    Keymap { keys, problems }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn keys_of(keymap: &Keymap, name: &str) -> Vec<String> {
        let index = ACTIONS.iter().position(|a| a.name == name).unwrap();
        keymap.keys[index].clone()
    }

    #[test]
    fn every_default_is_a_key_and_names_are_unique() {
        for action in ACTIONS {
            for keys in action.defaults {
                assert!(normalize(keys).is_some(), "{keys}");
            }
            assert_eq!(ACTIONS.iter().filter(|a| a.name == action.name).count(), 1);
        }
        assert!(resolve(None).problems.is_empty());
    }

    #[test]
    fn the_file_replaces_an_actions_keys() {
        let keymap = resolve(Some(&json!({
            "split_right": "alt-d",
            "copy": ["ctrl-shift-c", "ctrl-insert"],
            "quit": null,
        })));
        assert!(keymap.problems.is_empty(), "{:?}", keymap.problems);
        assert_eq!(keys_of(&keymap, "split_right"), ["alt-d"]);
        assert_eq!(keys_of(&keymap, "copy"), ["ctrl-shift-c", "ctrl-insert"]);
        assert!(keys_of(&keymap, "quit").is_empty());
        assert_eq!(keys_of(&keymap, "new_tab"), ["ctrl-shift-t"]);
    }

    #[test]
    fn a_reused_key_leaves_its_default_action() {
        let keymap = resolve(Some(&json!({"zoom_pane": "Ctrl-Shift-D"})));
        assert!(keys_of(&keymap, "split_right").is_empty());
        assert_eq!(keys_of(&keymap, "zoom_pane"), ["Ctrl-Shift-D"]);
    }

    #[test]
    fn reports_mistakes_and_keeps_the_rest() {
        let keymap = resolve(Some(&json!({
            "no_such_thing": "ctrl-x",
            "find": ["ctrl-f", 5, "ctrl-a-b"],
        })));
        assert_eq!(keymap.problems.len(), 3, "{:?}", keymap.problems);
        assert_eq!(keys_of(&keymap, "find"), ["ctrl-f"]);
        assert!(!resolve(Some(&json!([]))).problems.is_empty());
    }
}
