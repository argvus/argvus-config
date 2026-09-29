//! Typed canonical-configuration projections.
//!
//! This module owns the structured part of projection.  Component-specific
//! theme/effect adapters remain external processes until their public payload
//! contracts can be moved without duplicating asset ownership.

use crate::{ConfigDocument, ConfigError, ConfigResult, ConfigStore};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const MANIFEST_SCHEMA_VERSION: u32 = 2;
const SECTIONS: [&str; 13] = [
    "appearance",
    "layout",
    "effects",
    "fonts",
    "control_panel",
    "default_apps",
    "keyboard_shortcuts",
    "hyprland",
    "displays",
    "power",
    "session",
    "calendar",
    "removable_devices",
];

#[derive(Debug, Clone)]
pub struct ArgvusPaths {
    pub root: PathBuf,
    pub canonical_config: PathBuf,
    pub data: PathBuf,
    pub generated: PathBuf,
    pub internal: PathBuf,
    pub backups: PathBuf,
    pub projection_manifest: PathBuf,
}

impl ArgvusPaths {
    pub fn from_store(store: &ConfigStore) -> ConfigResult<Self> {
        let root = store
            .path
            .parent()
            .ok_or(ConfigError::MissingConfigDirectory)?
            .to_path_buf();
        let state_home = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .ok_or(ConfigError::MissingConfigDirectory)?;
        let state_root = state_home.join("argvus");
        Ok(Self {
            canonical_config: store.path.clone(),
            data: root.join("data"),
            generated: root.join("data/generated"),
            internal: root.join("data/internal"),
            backups: root.join("data/backups"),
            projection_manifest: state_root.join("config-projection.json"),
            root,
        })
    }
}

pub fn project(store: &ConfigStore, force: bool) -> ConfigResult<String> {
    let document = store.ensure()?;
    let paths = ArgvusPaths::from_store(store)?;
    fs::create_dir_all(&paths.data)?;
    fs::create_dir_all(&paths.generated)?;
    fs::create_dir_all(&paths.internal)?;

    let previous_manifest = if !force {
        read_manifest(&paths.projection_manifest)
    } else {
        None
    };
    let mut section_hashes = serde_json::Map::new();
    let mut changed_sections = Vec::new();
    for section in SECTIONS {
        let hash = section_hash(&document, section)?;
        if previous_manifest
            .as_ref()
            .and_then(|manifest| manifest.get("sections"))
            .and_then(|sections| sections.get(section))
            .and_then(Value::as_str)
            != Some(hash.as_str())
        {
            changed_sections.push(section);
        }
        section_hashes.insert(section.to_owned(), Value::String(hash));
    }

    let theme_hash = value_hash(document.get("/appearance/theme"))?;
    let accent_hash = value_hash(document.get("/appearance/accent"))?;
    let appearance_runtime_changed = previous_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("appearance_runtime"))
        .map(|runtime| {
            runtime.get("theme").and_then(Value::as_str) != Some(theme_hash.as_str())
                || runtime.get("accent").and_then(Value::as_str) != Some(accent_hash.as_str())
        })
        .unwrap_or(true);

    let mut affected_consumers = Vec::new();
    if changed_sections.contains(&"appearance") {
        affected_consumers.extend(["theme", "accent", "wallpaper", "surfaces"]);
        if appearance_runtime_changed {
            affected_consumers.push("hyprland");
        }
    }
    if changed_sections.contains(&"effects") {
        affected_consumers.extend(["surfaces", "hyprland"]);
    }
    if changed_sections
        .iter()
        .any(|section| ["layout", "hyprland", "displays"].contains(section))
    {
        affected_consumers.push("hyprland");
    }
    if changed_sections.contains(&"fonts") {
        affected_consumers.extend(["terminal", "surfaces"]);
    }
    if changed_sections.contains(&"control_panel") {
        affected_consumers.push("control-panel");
    }
    if changed_sections.contains(&"power") {
        affected_consumers.push("hypridle");
    }
    if changed_sections.iter().any(|section| {
        [
            "default_apps",
            "keyboard_shortcuts",
            "session",
            "removable_devices",
        ]
        .contains(section)
    }) {
        affected_consumers.push("session");
    }
    affected_consumers.sort_unstable();
    affected_consumers.dedup();

    project_hypridle(&document, &paths)?;
    project_json_section(
        &document,
        "/default_apps",
        &paths.data.join("control-center/defaults.json"),
    )?;
    project_json_section(
        &document,
        "/removable_devices",
        &paths.data.join("removable-devices/config.json"),
    )?;
    project_language(&document, &paths)?;
    project_control_panel(&document, &paths)?;
    project_scalar(
        &document,
        "/fonts/legacy",
        &paths.generated.join("fonts.conf"),
    )?;
    project_bool_marker(
        &document,
        "/power/keep_awake",
        &paths.data.join("power/keep-awake"),
    )?;
    project_input(&document, &paths)?;
    project_keyboard(&document, &paths)?;
    project_displays(&document, &paths)?;
    project_calendar(&document, &paths)?;
    run_component_adapters(&document, &paths, &changed_sections)?;
    remove_legacy_markers(&paths.root)?;

    let manifest = serde_json::json!({
        "schema_version": MANIFEST_SCHEMA_VERSION,
        "canonical_hash": file_hash(&paths.canonical_config)?,
        "sections": section_hashes,
        "appearance_runtime": {"theme": theme_hash, "accent": accent_hash},
        "changed_sections": changed_sections,
        "affected_consumers": affected_consumers,
        "reload_required": !affected_consumers.is_empty(),
    });
    atomic_write(
        &paths.projection_manifest,
        &format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    )?;
    Ok(format!(
        "argvus-config: projected canonical configuration into {}",
        paths.root.display()
    ))
}

fn read_manifest(path: &Path) -> Option<Value> {
    let value: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    (value.get("schema_version").and_then(Value::as_u64) == Some(MANIFEST_SCHEMA_VERSION as u64))
        .then_some(value)
}

fn section_hash(document: &ConfigDocument, section: &str) -> ConfigResult<String> {
    value_hash(document.sections.get(section))
}

fn value_hash(value: Option<&Value>) -> ConfigResult<String> {
    let bytes = serde_json::to_vec(value.unwrap_or(&Value::Null))?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

fn file_hash(path: &Path) -> ConfigResult<String> {
    if !path.is_file() {
        return Ok(String::new());
    }
    value_hash(Some(&Value::String(fs::read_to_string(path)?)))
}

fn atomic_write(path: &Path, contents: &str) -> ConfigResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn project_scalar(document: &ConfigDocument, pointer: &str, path: &Path) -> ConfigResult<()> {
    match document.get(pointer) {
        Some(Value::Null) | None => {
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Some(value) => atomic_write(
            path,
            &format!("{}\n", value.as_str().unwrap_or(&value.to_string())),
        )?,
    }
    Ok(())
}

fn project_json_section(document: &ConfigDocument, pointer: &str, path: &Path) -> ConfigResult<()> {
    match document.get(pointer) {
        Some(Value::Null) | None => Ok(()),
        Some(value) => atomic_write(path, &format!("{}\n", serde_json::to_string_pretty(value)?)),
    }
}

fn project_bool_marker(document: &ConfigDocument, pointer: &str, path: &Path) -> ConfigResult<()> {
    match document.get(pointer).and_then(Value::as_bool) {
        Some(true) => atomic_write(path, "enabled\n"),
        Some(false) => atomic_write(path, "disabled\n"),
        None => {
            if path.exists() {
                fs::remove_file(path)?;
            }
            Ok(())
        }
    }
}

fn project_language(document: &ConfigDocument, paths: &ArgvusPaths) -> ConfigResult<()> {
    if let Some(language) = document.get("/session/language").and_then(Value::as_str) {
        atomic_write(
            &paths.internal.join("language"),
            &format!("{}\n", language.replace('-', "_")),
        )
    } else if paths.internal.join("language").exists() {
        fs::remove_file(paths.internal.join("language"))?;
        Ok(())
    } else {
        Ok(())
    }
}

fn project_control_panel(document: &ConfigDocument, paths: &ArgvusPaths) -> ConfigResult<()> {
    if let Some(cards) = document
        .get("/control_panel/cards")
        .and_then(Value::as_array)
    {
        let disabled = cards
            .iter()
            .filter_map(|card| {
                (card.get("enabled").and_then(Value::as_bool) == Some(false))
                    .then(|| card.get("id").cloned())
                    .flatten()
            })
            .collect::<Vec<_>>();
        let order = cards
            .iter()
            .filter_map(|card| card.get("id").cloned())
            .collect::<Vec<_>>();
        atomic_write(
            &paths.data.join("control-panel/cards.json"),
            &format!(
                "{}\n",
                serde_json::json!({"disabled": disabled, "order": order})
            ),
        )?;
    }
    if let Some(enabled) = document
        .get("/control_panel/enabled")
        .and_then(Value::as_bool)
    {
        atomic_write(
            &paths.data.join("control-panel/state"),
            if enabled { "enabled\n" } else { "disabled\n" },
        )?;
    }
    Ok(())
}

fn project_input(document: &ConfigDocument, paths: &ArgvusPaths) -> ConfigResult<()> {
    let Some(input) = document.get("/hyprland/input").and_then(Value::as_object) else {
        return Ok(());
    };
    let mouse = input.get("mouse").and_then(Value::as_object);
    let touchpad = input.get("touchpad").and_then(Value::as_object);
    let toml = format!(
        "[mouse]\nsensitivity = {}\naccel_profile = \"{}\"\nnatural_scroll = {}\nscroll_factor = {}\nleft_handed = {}\n\n[touchpad]\nnatural_scroll = {}\ntap_to_click = {}\ntap_and_drag = {}\ntwo_finger_right_click = {}\ndisable_while_typing = {}\n",
        object_value(mouse, "sensitivity").unwrap_or(&Value::from(0)),
        object_value(mouse, "accel_profile")
            .and_then(Value::as_str)
            .unwrap_or(""),
        object_value(mouse, "natural_scroll").unwrap_or(&Value::Bool(false)),
        object_value(mouse, "scroll_factor").unwrap_or(&Value::from(1)),
        object_value(mouse, "left_handed").unwrap_or(&Value::Bool(false)),
        object_value(touchpad, "natural_scroll").unwrap_or(&Value::Bool(false)),
        object_value(touchpad, "tap_to_click").unwrap_or(&Value::Bool(true)),
        object_value(touchpad, "tap_and_drag").unwrap_or(&Value::Bool(true)),
        object_value(touchpad, "two_finger_right_click").unwrap_or(&Value::Bool(true)),
        object_value(touchpad, "disable_while_typing").unwrap_or(&Value::Bool(true))
    );
    atomic_write(&paths.data.join("hypr/input.toml"), &toml)
}

fn object_value<'a>(
    object: Option<&'a serde_json::Map<String, Value>>,
    key: &str,
) -> Option<&'a Value> {
    object.and_then(|value| value.get(key))
}

fn project_keyboard(document: &ConfigDocument, paths: &ArgvusPaths) -> ConfigResult<()> {
    let Some(keyboard) = document
        .get("/hyprland/keyboard")
        .and_then(Value::as_object)
    else {
        return Ok(());
    };
    let quote =
        |key: &str| serde_json::to_string(keyboard.get(key).and_then(Value::as_str).unwrap_or(""));
    atomic_write(
        &paths.generated.join("hypr/input.lua"),
        &format!(
            "-- Generated by argvus-config. Do not edit.\nreturn {{ kb_layout = {}, kb_variant = {}, kb_options = {}, }}\n",
            quote("layout")?,
            quote("variant")?,
            quote("options")?
        ),
    )?;
    project_shortcuts(document, paths)
}

fn project_shortcuts(document: &ConfigDocument, paths: &ArgvusPaths) -> ConfigResult<()> {
    let Some(shortcuts) = document
        .get("/keyboard_shortcuts")
        .and_then(Value::as_object)
    else {
        return Ok(());
    };
    let mut lua = String::from("-- Generated by argvus-config. Do not edit.\nreturn {\n");
    let mut toml = String::new();
    for (key, shortcut) in shortcuts {
        let (keys, enabled) = match shortcut {
            Value::String(value) => (Some(value.as_str()), true),
            Value::Null => (None, false),
            _ => {
                return Err(ConfigError::Invalid(format!(
                    "keyboard_shortcuts.{key} must be a string or null"
                )));
            }
        };
        lua.push_str(&format!("  [{}] = {{", serde_json::to_string(key)?));
        if let Some(keys) = keys {
            lua.push_str(&format!(" keys = {},", serde_json::to_string(keys)?));
        }
        lua.push_str(&format!(" enabled = {enabled} }},\n"));
        toml.push_str(&format!("[keybindings.{}]\n", key.replace(['.', '-'], "_")));
        toml.push_str(&format!("enabled = {enabled}\n"));
        if let Some(keys) = keys {
            toml.push_str(&format!("keys = {}\n", serde_json::to_string(keys)?));
        }
        toml.push('\n');
    }
    lua.push_str("}\n");
    atomic_write(&paths.generated.join("hypr/keybindings.lua"), &lua)?;
    atomic_write(&paths.data.join("hypr/keybindings.toml"), &toml)
}

fn project_displays(document: &ConfigDocument, paths: &ArgvusPaths) -> ConfigResult<()> {
    let Some(displays) = document.get("/displays").and_then(Value::as_object) else {
        return Ok(());
    };
    let mut output = String::from("-- Generated by argvus-config. Do not edit.\n");
    if let Some(monitors) = displays.get("monitors").and_then(Value::as_object) {
        for (output_name, monitor) in monitors {
            output.push_str(&format!(
                "hl.monitor({{ output = {}",
                serde_json::to_string(output_name)?
            ));
            if monitor.get("disabled").and_then(Value::as_bool) == Some(true) {
                output.push_str(", disabled = true");
            }
            for key in ["mode", "position", "scale", "transform"] {
                if let Some(value) = monitor.get(key) {
                    output.push_str(&format!(", {key} = {}", lua_value(value)?));
                }
            }
            output.push_str(" })\n");
        }
    }
    atomic_write(&paths.generated.join("hypr/monitors.lua"), &output)
}

fn project_calendar(document: &ConfigDocument, paths: &ArgvusPaths) -> ConfigResult<()> {
    let Some(calendar) = document.get("/calendar").and_then(Value::as_object) else {
        return Ok(());
    };
    let config_home = paths
        .root
        .parent()
        .ok_or(ConfigError::MissingConfigDirectory)?;
    let text = format!(
        "[calendar]\nweek_start = {}\nshow_events = {}\ndefault_event_duration_minutes = {}\ndefault_reminder_minutes = {}\nsync_interval_minutes = {}\n",
        scalar(calendar, "week_start", 1),
        scalar(calendar, "show_events", true),
        scalar(calendar, "default_event_duration_minutes", 60),
        scalar(calendar, "default_reminder_minutes", 10),
        scalar(calendar, "sync_interval_minutes", 15)
    );
    atomic_write(
        &config_home.join("argvus-taskbar-calendar/config.toml"),
        &text,
    )
}

fn scalar(object: &serde_json::Map<String, Value>, key: &str, default: impl Into<Value>) -> Value {
    object.get(key).cloned().unwrap_or_else(|| default.into())
}
fn lua_value(value: &Value) -> ConfigResult<String> {
    Ok(match value {
        Value::String(value) => serde_json::to_string(value)?,
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        _ => serde_json::to_string(value)?,
    })
}

fn project_hypridle(document: &ConfigDocument, paths: &ArgvusPaths) -> ConfigResult<()> {
    let lock_minutes = document
        .get("/power/lock_minutes")
        .and_then(Value::as_u64)
        .unwrap_or(15);
    let screen_minutes = document
        .get("/power/screen_off_minutes")
        .and_then(Value::as_u64)
        .unwrap_or(30);
    let mut text = String::from(
        "# ARGVUS hypridle configuration\n# Generated from config.json by argvus-config project. Do not edit.\n\ngeneral {\n  lock_cmd = sh /usr/share/argvus/power/sh/hypr-power-menu.sh --lock\n  before_sleep_cmd = sh /usr/share/argvus/power/sh/hypr-power-menu.sh --lock\n  after_sleep_cmd = hyprctl dispatch 'hl.dsp.dpms({ action = \"on\" })'\n}\n",
    );
    if lock_minutes > 0 {
        text.push_str(&format!("\nlistener {{\n  timeout = {}\n  on-timeout = sh /usr/share/argvus/power/sh/hypr-power-menu.sh --lock\n}}\n", lock_minutes * 60));
    }
    if screen_minutes > 0 {
        text.push_str(&format!("\nlistener {{\n  timeout = {}\n  on-timeout = hyprctl dispatch 'hl.dsp.dpms({{ action = \"off\" }})'\n  on-resume = hyprctl dispatch 'hl.dsp.dpms({{ action = \"on\" }})'\n}}\n", screen_minutes * 60));
    }
    atomic_write(&paths.data.join("hypr/hypridle.conf"), &text)
}

fn run_component_adapters(
    document: &ConfigDocument,
    paths: &ArgvusPaths,
    changed_sections: &[&str],
) -> ConfigResult<()> {
    let system_config = env::var_os("ARGVUS_SYSTEM_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/argvus"));
    let theme = document
        .get("/appearance/theme")
        .and_then(Value::as_str)
        .unwrap_or("argvus-dark");
    let theme_output = paths.generated.join("theme-effective.conf");
    let theme_qml = paths.generated.join(format!(
        "quickshell/argvus-control-panel/themes/{theme}/Theme.qml"
    ));
    let theme_transaction = env::var("ARGVUS_THEME_SWITCH").ok().as_deref() == Some("1");
    if !theme_transaction
        && (changed_sections.contains(&"appearance")
            || !theme_output.is_file()
            || !theme_qml.is_file())
    {
        run_adapter(
            &system_config.join("appearance/sh/theme-switch.sh"),
            &[theme],
            paths,
        )?;
    }
    let effects_output = paths.generated.join(format!("effects/{theme}.conf"));
    if !theme_transaction && (changed_sections.contains(&"effects") || !effects_output.is_file()) {
        run_adapter(
            &system_config.join("session/sh/effects-toggle.sh"),
            &["apply"],
            paths,
        )?;
    }
    Ok(())
}

fn run_adapter(path: &Path, arguments: &[&str], paths: &ArgvusPaths) -> ConfigResult<()> {
    if !path.is_file() {
        return Ok(());
    }
    let status = Command::new("sh")
        .arg(path)
        .args(arguments)
        .env("ARGVUS_NO_RUNTIME", "1")
        .env("ARGVUS_PROJECTING", "1")
        .env(
            "ARGVUS_CONFIG_HOME",
            paths.root.parent().unwrap_or(Path::new("")),
        )
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(ConfigError::Invalid(format!(
            "projection adapter {} exited with {status}",
            path.display()
        )))
    }
}

fn remove_legacy_markers(root: &Path) -> ConfigResult<()> {
    // These files are still consumed by the Lua compositor and shell
    // adapters. They are derived compatibility projections, not competing
    // sources of truth, so do not delete them until all consumers read data/.
    for name in [".accent-custom"] {
        let path = root.join(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}
