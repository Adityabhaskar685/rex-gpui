//! Settings kept between runs, in `~/.config/rex-gpui/settings.json`.

use std::path::PathBuf;

use serde_json::{Value, json};

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
}

pub fn dir() -> PathBuf {
    config_home().join("rex-gpui")
}

fn path() -> PathBuf {
    dir().join("settings.json")
}

fn read() -> Value {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

pub fn get(key: &str) -> Option<Value> {
    read().get(key).cloned().filter(|v| !v.is_null())
}

pub fn set(key: &str, value: Value) -> anyhow::Result<()> {
    let mut settings = read();
    settings[key] = value;
    std::fs::create_dir_all(dir())?;
    std::fs::write(path(), serde_json::to_string_pretty(&settings)? + "\n")?;
    Ok(())
}
