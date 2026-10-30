//! Color themes for the window and its terminals, and the saved choice.
//!
//! Themes come from three places: the built-in ones, Ghostty's theme files (wherever Ghostty
//! keeps them, plus `~/.config/rex-gpui/themes`), and files imported from the theme menu.
//! A theme sets the terminals' default colors. A server that sends a `theme` of its own,
//! from the Rex config or `set_theme`, still overrides the colors it names for that terminal.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde_json::json;

/// Colors for the window chrome.
#[derive(Clone, Copy)]
pub struct Ui {
    pub window: u32,
    pub card_border: u32,
    pub card_border_focused: u32,
    pub tab_active: u32,
    pub tab_active_border: u32,
    pub hover: u32,
    pub text: u32,
    pub text_dim: u32,
    pub text_faint: u32,
    /// Menus and dialogs.
    pub menu: u32,
    pub accent: u32,
    pub danger: u32,
}

/// A terminal's default colors.
#[derive(Clone)]
pub struct TermColors {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub selection: u32,
    /// The text color inside a selection, when the theme sets one.
    pub selection_fg: Option<u32>,
    /// The 16 ANSI colors: 8 normal, then 8 bright.
    pub ansi: [u32; 16],
}

#[derive(Clone, PartialEq)]
pub enum Source {
    BuiltIn,
    /// A Ghostty theme file.
    File(PathBuf),
}

#[derive(Clone)]
pub struct AppTheme {
    pub name: Cow<'static, str>,
    pub source: Source,
    pub ui: Ui,
    pub term: TermColors,
}

static BUILT_IN: &[AppTheme] = &[
    AppTheme {
        name: Cow::Borrowed("Rex Dark"),
        source: Source::BuiltIn,
        ui: Ui {
            window: 0x161518, card_border: 0x2c2a2f, card_border_focused: 0x4a4750, tab_active: 0x29272c,
            tab_active_border: 0x3b3940, hover: 0x232126, text: 0xe8e6ea, text_dim: 0x9b98a0,
            text_faint: 0x6c6971, menu: 0x1f1e22, accent: 0x3d5afe, danger: 0xe5484d,
        },
        term: TermColors {
            foreground: 0xe6e3e8, background: 0x1f1e22, cursor: 0xe6e3e8, selection: 0x45475a, selection_fg: None,
            ansi: [
                0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
                0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
            ],
        },
    },
    AppTheme {
        name: Cow::Borrowed("Catppuccin Mocha"),
        source: Source::BuiltIn,
        ui: Ui {
            window: 0x11111b, card_border: 0x313244, card_border_focused: 0x585b70, tab_active: 0x313244,
            tab_active_border: 0x45475a, hover: 0x1e1e2e, text: 0xcdd6f4, text_dim: 0xa6adc8,
            text_faint: 0x6c7086, menu: 0x181825, accent: 0x89b4fa, danger: 0xf38ba8,
        },
        term: TermColors {
            foreground: 0xcdd6f4, background: 0x1e1e2e, cursor: 0xf5e0dc, selection: 0x45475a, selection_fg: None,
            ansi: [
                0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
                0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
            ],
        },
    },
    AppTheme {
        name: Cow::Borrowed("Tokyo Night"),
        source: Source::BuiltIn,
        ui: Ui {
            window: 0x16161e, card_border: 0x292e42, card_border_focused: 0x414868, tab_active: 0x292e42,
            tab_active_border: 0x3b4261, hover: 0x1f2335, text: 0xc0caf5, text_dim: 0x9aa5ce,
            text_faint: 0x565f89, menu: 0x1a1b26, accent: 0x7aa2f7, danger: 0xf7768e,
        },
        term: TermColors {
            foreground: 0xc0caf5, background: 0x1a1b26, cursor: 0xc0caf5, selection: 0x33467c, selection_fg: None,
            ansi: [
                0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
                0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
            ],
        },
    },
    AppTheme {
        name: Cow::Borrowed("Dracula"),
        source: Source::BuiltIn,
        ui: Ui {
            window: 0x21222c, card_border: 0x343746, card_border_focused: 0x6272a4, tab_active: 0x343746,
            tab_active_border: 0x44475a, hover: 0x2b2d3a, text: 0xf8f8f2, text_dim: 0xbfbfbf,
            text_faint: 0x6272a4, menu: 0x282a36, accent: 0xbd93f9, danger: 0xff5555,
        },
        term: TermColors {
            foreground: 0xf8f8f2, background: 0x282a36, cursor: 0xf8f8f2, selection: 0x44475a, selection_fg: None,
            ansi: [
                0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
                0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
            ],
        },
    },
    AppTheme {
        name: Cow::Borrowed("Gruvbox Dark"),
        source: Source::BuiltIn,
        ui: Ui {
            window: 0x1d2021, card_border: 0x3c3836, card_border_focused: 0x665c54, tab_active: 0x3c3836,
            tab_active_border: 0x504945, hover: 0x32302f, text: 0xebdbb2, text_dim: 0xbdae93,
            text_faint: 0x7c6f64, menu: 0x282828, accent: 0x83a598, danger: 0xfb4934,
        },
        term: TermColors {
            foreground: 0xebdbb2, background: 0x282828, cursor: 0xebdbb2, selection: 0x504945, selection_fg: None,
            ansi: [
                0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
                0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
            ],
        },
    },
    AppTheme {
        name: Cow::Borrowed("Nord"),
        source: Source::BuiltIn,
        ui: Ui {
            window: 0x242933, card_border: 0x3b4252, card_border_focused: 0x4c566a, tab_active: 0x3b4252,
            tab_active_border: 0x434c5e, hover: 0x2e3440, text: 0xeceff4, text_dim: 0xd8dee9,
            text_faint: 0x7b88a1, menu: 0x2e3440, accent: 0x88c0d0, danger: 0xbf616a,
        },
        term: TermColors {
            foreground: 0xd8dee9, background: 0x2e3440, cursor: 0xd8dee9, selection: 0x434c5e, selection_fg: None,
            ansi: [
                0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
                0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
            ],
        },
    },
    AppTheme {
        name: Cow::Borrowed("Solarized Dark"),
        source: Source::BuiltIn,
        ui: Ui {
            window: 0x00212b, card_border: 0x073642, card_border_focused: 0x586e75, tab_active: 0x073642,
            tab_active_border: 0x0d4654, hover: 0x02303b, text: 0x93a1a1, text_dim: 0x839496,
            text_faint: 0x586e75, menu: 0x002b36, accent: 0x268bd2, danger: 0xdc322f,
        },
        term: TermColors {
            foreground: 0x839496, background: 0x002b36, cursor: 0x93a1a1, selection: 0x073642, selection_fg: None,
            ansi: [
                0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
            ],
        },
    },
    AppTheme {
        name: Cow::Borrowed("One Light"),
        source: Source::BuiltIn,
        ui: Ui {
            window: 0xe5e5e6, card_border: 0xd0d0d3, card_border_focused: 0xa0a1a7, tab_active: 0xfafafa,
            tab_active_border: 0xc9c9cc, hover: 0xdadadc, text: 0x383a42, text_dim: 0x696c77,
            text_faint: 0x9d9fa6, menu: 0xf2f2f3, accent: 0x4078f2, danger: 0xe45649,
        },
        term: TermColors {
            foreground: 0x383a42, background: 0xfafafa, cursor: 0x526fff, selection: 0xd8dde8, selection_fg: None,
            ansi: [
                0x383a42, 0xe45649, 0x50a14f, 0xc18401, 0x4078f2, 0xa626a4, 0x0184bc, 0xa0a1a7,
                0x696c77, 0xe45649, 0x50a14f, 0xc18401, 0x4078f2, 0xa626a4, 0x0184bc, 0xffffff,
            ],
        },
    },
];

struct Registry {
    themes: Vec<Arc<AppTheme>>,
    current: Arc<AppTheme>,
}

static REGISTRY: RwLock<Option<Registry>> = RwLock::new(None);

fn with_registry<T>(f: impl FnOnce(&Registry) -> T) -> T {
    let guard = REGISTRY.read().unwrap();
    match guard.as_ref() {
        Some(registry) => f(registry),
        None => {
            drop(guard);
            load();
            f(REGISTRY.read().unwrap().as_ref().unwrap())
        }
    }
}

pub fn current() -> Arc<AppTheme> {
    with_registry(|r| r.current.clone())
}

pub fn ui() -> Ui {
    with_registry(|r| r.current.ui)
}

/// Every theme, built-in ones first, then files by name.
pub fn all() -> Vec<Arc<AppTheme>> {
    with_registry(|r| r.themes.clone())
}

/// Make the theme named `name` current and save the choice. Returns false for an unknown name.
pub fn select(name: &str) -> bool {
    let mut guard = REGISTRY.write().unwrap();
    let Some(registry) = guard.as_mut() else { return false };
    let Some(theme) = registry.themes.iter().find(|t| t.name == name).cloned() else { return false };
    registry.current = theme;
    drop(guard);
    if let Err(err) = crate::settings::set("theme", json!(name)) {
        log::warn!("saving the theme: {err:#}");
    }
    true
}

/// Directories to read Ghostty theme files from, in order; a later one's file wins over an
/// earlier one's with the same name.
fn theme_dirs() -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
    let mut dirs: Vec<PathBuf> = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
        .split(':')
        .rev()
        .map(|d| Path::new(d).join("ghostty/themes"))
        .collect();
    dirs.push("/snap/ghostty/current/share/ghostty/themes".into());
    dirs.push(data.join("ghostty/themes"));
    dirs.push(crate::settings::config_home().join("ghostty/themes"));
    dirs.push(user_theme_dir());
    dirs
}

/// Where imported themes go: `~/.config/rex-gpui/themes`.
pub fn user_theme_dir() -> PathBuf {
    crate::settings::dir().join("themes")
}

/// Read every theme and pick the current one: `$REX_THEME`, else the saved choice, else the
/// theme in Ghostty's own config, else the first built-in one.
pub fn load() {
    let mut themes: Vec<Arc<AppTheme>> = BUILT_IN.iter().cloned().map(Arc::new).collect();
    let mut files: std::collections::BTreeMap<String, AppTheme> = Default::default();
    for dir in theme_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            match parse_ghostty(&path) {
                Some(theme) => {
                    files.insert(theme.name.to_lowercase(), theme);
                }
                None => log::debug!("skipping theme file {}", path.display()),
            }
        }
    }
    let built_in: Vec<String> = themes.iter().map(|t| t.name.to_lowercase()).collect();
    themes.extend(files.into_values().filter(|t| !built_in.contains(&t.name.to_lowercase())).map(Arc::new));

    let wanted = std::env::var("REX_THEME")
        .ok()
        .or_else(|| crate::settings::get("theme").and_then(|v| v.as_str().map(str::to_string)))
        .or_else(ghostty_config_theme);
    let current = wanted
        .and_then(|name| themes.iter().find(|t| t.name.eq_ignore_ascii_case(&name)).cloned())
        .unwrap_or_else(|| themes[0].clone());
    *REGISTRY.write().unwrap() = Some(Registry { themes, current });
}

/// Copy Ghostty theme files into the user theme directory and read the themes again.
/// Returns the name of the first theme that was imported.
pub fn import(paths: &[PathBuf]) -> anyhow::Result<String> {
    let dir = user_theme_dir();
    std::fs::create_dir_all(&dir)?;
    let mut first = None;
    for path in paths {
        let theme = parse_ghostty(path)
            .ok_or_else(|| anyhow::anyhow!("{} isn't a Ghostty theme file", path.display()))?;
        std::fs::copy(path, dir.join(theme.name.as_ref()))?;
        first.get_or_insert(theme.name.to_string());
    }
    let current = current().name.to_string();
    load();
    select(&current);
    first.ok_or_else(|| anyhow::anyhow!("no file chosen"))
}

/// The `theme` in Ghostty's config. Its `light:X,dark:Y` form picks the dark one.
fn ghostty_config_theme() -> Option<String> {
    let text = std::fs::read_to_string(crate::settings::config_home().join("ghostty/config")).ok()?;
    let value = text.lines().rev().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "theme").then(|| value.trim().trim_matches('"').to_string())
    })?;
    Some(match value.split(',').find_map(|part| part.trim().strip_prefix("dark:")) {
        Some(dark) => dark.trim().to_string(),
        None => value,
    })
}

/// Read a Ghostty theme file: `key = value` lines such as `background = #1e1e2e` and
/// `palette = 4=#89b4fa`. The window's colors are derived from the terminal's.
fn parse_ghostty(path: &Path) -> Option<AppTheme> {
    let text = std::fs::read_to_string(path).ok()?;
    let name = path.file_name()?.to_str()?.trim_end_matches(".conf").to_string();
    let (mut background, mut foreground, mut cursor, mut selection, mut selection_fg) = (None, None, None, None, None);
    let mut ansi: [Option<u32>; 16] = [None; 16];
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match key.trim() {
            "background" => background = hex(value),
            "foreground" => foreground = hex(value),
            "cursor-color" => cursor = hex(value),
            "selection-background" => selection = hex(value),
            "selection-foreground" => selection_fg = hex(value),
            "palette" => {
                if let Some((index, color)) = value.split_once('=') {
                    if let (Ok(index), Some(color)) = (index.trim().parse::<usize>(), hex(color)) {
                        if index < 16 {
                            ansi[index] = Some(color);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let (background, foreground) = (background?, foreground?);
    // Ghostty's own defaults for any of the 16 colors a theme leaves out.
    let fallback = BUILT_IN[0].term.ansi;
    let ansi: [u32; 16] = std::array::from_fn(|i| ansi[i].unwrap_or(fallback[i]));
    let term = TermColors {
        foreground,
        background,
        cursor: cursor.unwrap_or(foreground),
        selection: selection.unwrap_or_else(|| mix(background, foreground, 0.25)),
        selection_fg,
        ansi,
    };
    Some(AppTheme { name: name.into(), source: Source::File(path.to_path_buf()), ui: derive_ui(&term), term })
}

/// Chrome colors that sit well around a terminal with these colors.
fn derive_ui(term: &TermColors) -> Ui {
    let (bg, fg) = (term.background, term.foreground);
    let dark = luminance(bg) < 0.5;
    Ui {
        window: if dark { mix(bg, 0x000000, 0.28) } else { mix(bg, 0x000000, 0.07) },
        card_border: mix(bg, fg, 0.12),
        card_border_focused: mix(bg, fg, 0.3),
        tab_active: mix(bg, fg, if dark { 0.08 } else { 0.0 }),
        tab_active_border: mix(bg, fg, 0.18),
        hover: mix(bg, fg, 0.05),
        text: fg,
        text_dim: mix(fg, bg, 0.3),
        text_faint: mix(fg, bg, 0.55),
        menu: mix(bg, fg, 0.03),
        accent: term.ansi[4],
        danger: term.ansi[1],
    }
}

fn hex(value: &str) -> Option<u32> {
    let value = value.trim().trim_start_matches('#');
    (value.len() == 6).then(|| u32::from_str_radix(value, 16).ok()).flatten()
}

/// `a` moved `t` of the way towards `b`.
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let channel = |shift: u32| {
        let (x, y) = (((a >> shift) & 0xff) as f32, ((b >> shift) & 0xff) as f32);
        ((x + (y - x) * t).round() as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

fn luminance(c: u32) -> f32 {
    let channel = |shift: u32| ((c >> shift) & 0xff) as f32 / 255.;
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_ghostty_theme() {
        let dir = std::env::temp_dir().join(format!("rex-gpui-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Test Night");
        std::fs::write(
            &path,
            "# comment\npalette = 0=#000000\npalette = 4=#3366ff\nbackground = #101010\nforeground = #e0e0e0\nselection-background = #333333\n",
        )
        .unwrap();
        let theme = parse_ghostty(&path).unwrap();
        assert_eq!(theme.name, "Test Night");
        assert_eq!(theme.term.background, 0x101010);
        assert_eq!(theme.term.ansi[0], 0x000000);
        assert_eq!(theme.term.ansi[4], 0x3366ff);
        assert_eq!(theme.term.cursor, 0xe0e0e0);
        assert_eq!(theme.ui.accent, 0x3366ff);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
