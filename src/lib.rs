//! Canonical user configuration for the ARGVUS desktop.
//!
//! The document stores logical user preferences. Generated files and native
//! application overrides remain projections outside this file.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub mod project;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
const LOCK_FILE: &str = "data/internal/config.lock";
const BACKUP_FILE: &str = "data/backups/config.json.bak";

const KNOWN_SECTIONS: [&str; 13] = [
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

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration directory is unavailable")]
    MissingConfigDirectory,
    #[error("invalid configuration JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("configuration I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub type ConfigResult<T> = Result<T, ConfigError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigDocument {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(flatten)]
    pub sections: BTreeMap<String, Value>,
}

impl Default for ConfigDocument {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            sections: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    Appearance,
    Desktop,
}

impl ConfigScope {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "appearance" | "theme" => Some(Self::Appearance),
            "desktop" | "all" => Some(Self::Desktop),
            _ => None,
        }
    }

    pub fn sections(self) -> &'static [&'static str] {
        match self {
            Self::Appearance => &["appearance", "layout", "effects", "fonts", "control_panel"],
            Self::Desktop => &[
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
            ],
        }
    }
}

impl ConfigDocument {
    pub fn load(path: &Path) -> ConfigResult<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)?;
        let mut document: Self = serde_json::from_str(&text)?;
        migrate_legacy_sections(&mut document)?;
        document.validate()?;
        Ok(document)
    }

    pub fn load_effective(path: &Path) -> ConfigResult<Self> {
        let mut document = Self::load(path)?;
        apply_defaults(&mut document);
        Ok(document)
    }

    pub fn validate(&self) -> ConfigResult<()> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ConfigError::Invalid(format!(
                "unsupported schema_version {} (expected {})",
                self.schema_version, CURRENT_SCHEMA_VERSION
            )));
        }
        for (section, value) in &self.sections {
            if !KNOWN_SECTIONS.contains(&section.as_str()) && section != "extensions" {
                return Err(ConfigError::Invalid(format!("unknown section: {section}")));
            }
            if !value.is_object() {
                return Err(ConfigError::Invalid(format!(
                    "section {section} must be a JSON object"
                )));
            }
        }
        validate_appearance(self)?;
        validate_layout(self)?;
        validate_effects(self)?;
        validate_fonts(self)?;
        validate_hyprland(self)?;
        validate_displays(self)?;
        validate_control_panel(self)?;
        validate_default_apps(self)?;
        validate_keyboard_shortcuts(self)?;
        validate_power(self)?;
        validate_session(self)?;
        Ok(())
    }

    pub fn get(&self, pointer: &str) -> Option<&Value> {
        if pointer.is_empty() || pointer == "/" {
            return None;
        }
        self.get_ref(pointer)
    }

    fn get_ref(&self, pointer: &str) -> Option<&Value> {
        let mut current: Option<&Value> = None;
        let object = &self.sections;
        let components = pointer
            .trim_start_matches('/')
            .split('/')
            .collect::<Vec<_>>();
        for (index, raw) in components.iter().enumerate() {
            let key = raw.replace("~1", "/").replace("~0", "~");
            if index == 0 {
                current = object.get(&key);
            } else {
                current = current?.get(&key);
            }
        }
        current
    }

    pub fn set(&mut self, pointer: &str, value: Value) -> ConfigResult<()> {
        let components = pointer_components(pointer)?;
        if components.len() == 1 {
            if !KNOWN_SECTIONS.contains(&components[0].as_str()) && components[0] != "extensions" {
                return Err(ConfigError::Invalid(format!(
                    "unknown section: {}",
                    components[0]
                )));
            }
            if !value.is_object() {
                return Err(ConfigError::Invalid(format!(
                    "section {} must be an object",
                    components[0]
                )));
            }
            self.sections.insert(components[0].clone(), value);
        } else {
            let section = components[0].clone();
            if !KNOWN_SECTIONS.contains(&section.as_str()) && section != "extensions" {
                return Err(ConfigError::Invalid(format!("unknown section: {section}")));
            }
            let root = self
                .sections
                .entry(section)
                .or_insert_with(|| Value::Object(Map::new()));
            set_nested(root, &components[1..], value)?;
        }
        self.validate()
    }

    pub fn unset(&mut self, pointer: &str) -> ConfigResult<()> {
        let components = pointer_components(pointer)?;
        if components.len() == 1 {
            self.sections.remove(&components[0]);
        } else if let Some(section) = self.sections.get_mut(&components[0]) {
            unset_nested(section, &components[1..]);
        }
        self.validate()
    }

    pub fn scoped(&self, scope: ConfigScope) -> Self {
        let sections = scope
            .sections()
            .iter()
            .filter_map(|name| {
                self.sections
                    .get(*name)
                    .map(|value| ((*name).to_owned(), value.clone()))
            })
            .collect();
        Self {
            schema_version: self.schema_version,
            sections,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    pub path: PathBuf,
    lock_path: PathBuf,
}

impl ConfigStore {
    pub fn from_environment() -> ConfigResult<Self> {
        let config_home = env::var_os("ARGVUS_CONFIG_HOME")
            .or_else(|| env::var_os("XDG_CONFIG_HOME"))
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or(ConfigError::MissingConfigDirectory)?;
        let directory = config_home.join("argvus");
        Ok(Self {
            path: directory.join("config.json"),
            lock_path: directory.join(LOCK_FILE),
        })
    }

    pub fn load(&self) -> ConfigResult<ConfigDocument> {
        ConfigDocument::load(&self.path)
    }

    pub fn load_effective(&self) -> ConfigResult<ConfigDocument> {
        ConfigDocument::load_effective(&self.path)
    }

    /// Create or complete the canonical document without replacing explicit
    /// values. This is the only initialization path used by session recovery.
    pub fn ensure(&self) -> ConfigResult<ConfigDocument> {
        let lock = self.open_lock()?;
        lock_file(&lock)?;
        let _lock_guard = LockGuard(&lock);
        let was_missing = !self.path.exists();
        let mut document = if !was_missing {
            self.load()?
        } else {
            ConfigDocument::default()
        };
        let before = document.clone();
        let root = self
            .path
            .parent()
            .ok_or(ConfigError::MissingConfigDirectory)?;
        let imported = import_legacy_values(&mut document, root)?;
        let moved = migrate_legacy_layout(root)?;
        apply_defaults(&mut document);
        document.validate()?;
        if document != before || imported || moved || was_missing {
            self.save_locked(&document)?;
        }
        Ok(document)
    }

    pub fn save(&self, document: &ConfigDocument) -> ConfigResult<()> {
        document.validate()?;
        let lock = self.open_lock()?;
        lock_file(&lock)?;
        let _lock_guard = LockGuard(&lock);
        self.save_locked(document)
    }

    fn open_lock(&self) -> ConfigResult<File> {
        let directory = self
            .path
            .parent()
            .ok_or(ConfigError::MissingConfigDirectory)?;
        fs::create_dir_all(directory)?;
        if let Some(lock_parent) = self.lock_path.parent() {
            fs::create_dir_all(lock_parent)?;
        }
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)?)
    }

    fn save_locked(&self, document: &ConfigDocument) -> ConfigResult<()> {
        let directory = self
            .path
            .parent()
            .ok_or(ConfigError::MissingConfigDirectory)?;
        fs::create_dir_all(directory)?;
        if self.path.exists() {
            let backup = directory.join(BACKUP_FILE);
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&self.path, backup)?;
        }
        let temporary = self
            .path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        let contents = serde_json::to_vec_pretty(document)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&contents)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        fs::rename(&temporary, &self.path)?;
        Ok(())
    }

    pub fn update(&self, pointer: &str, value: Value) -> ConfigResult<ConfigDocument> {
        self.modify(|document| document.set(pointer, value))
    }

    pub fn unset(&self, pointer: &str) -> ConfigResult<ConfigDocument> {
        self.modify(|document| document.unset(pointer))
    }

    /// Apply a complete JSON patch under one canonical lock. The patch object
    /// maps JSON pointers to their replacement values; `null` is a real JSON
    /// value and is therefore never treated as an implicit deletion.
    pub fn patch(&self, values: &Map<String, Value>) -> ConfigResult<ConfigDocument> {
        self.modify(|document| {
            for (pointer, value) in values {
                document.set(pointer, value.clone())?;
            }
            Ok(())
        })
    }

    /// Atomically apply the theme-owned appearance fields while preserving
    /// explicit user overrides recorded in the canonical document.
    pub fn apply_theme(
        &self,
        theme: &str,
        accent: Option<&str>,
        gtk_mode: Option<&str>,
        wallpaper: Option<&str>,
        reset_custom_wallpaper: bool,
    ) -> ConfigResult<ConfigDocument> {
        self.modify(|document| {
            document.set("/appearance/theme", Value::String(theme.to_owned()))?;
            document.set(
                "/layout/variant",
                Value::String(theme_layout_variant(theme).to_owned()),
            )?;
            let accent_custom = document
                .get("/appearance/accent_custom")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !accent_custom {
                if let Some(accent) = accent {
                    document.set("/appearance/accent", Value::String(accent.to_owned()))?;
                }
                document.set("/appearance/accent_custom", Value::Bool(false))?;
            }
            if let Some(mode) = gtk_mode {
                document.set("/appearance/gtk_mode", Value::String(mode.to_owned()))?;
            }
            let wallpaper_custom = document
                .get("/appearance/wallpaper_custom")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if reset_custom_wallpaper {
                document.set("/appearance/wallpaper_custom", Value::Bool(false))?;
            }
            if (reset_custom_wallpaper || !wallpaper_custom)
                && let Some(wallpaper) = wallpaper
            {
                document.set("/appearance/wallpaper", Value::String(wallpaper.to_owned()))?;
            }
            Ok(())
        })
    }

    /// Perform a read-modify-write transaction under the same lock used by
    /// `save`. Callers must use this for every canonical mutation so two UI
    /// processes cannot overwrite each other's sections with stale documents.
    pub fn modify<F>(&self, operation: F) -> ConfigResult<ConfigDocument>
    where
        F: FnOnce(&mut ConfigDocument) -> ConfigResult<()>,
    {
        self.modify_if_changed(|document| {
            operation(document)?;
            Ok(true)
        })
    }

    fn modify_if_changed<F>(&self, operation: F) -> ConfigResult<ConfigDocument>
    where
        F: FnOnce(&mut ConfigDocument) -> ConfigResult<bool>,
    {
        let lock = self.open_lock()?;
        lock_file(&lock)?;
        let _lock_guard = LockGuard(&lock);
        let mut document = self.load()?;
        let before = document.clone();
        if operation(&mut document)? && document != before {
            self.save_locked(&document)?;
        }
        Ok(document)
    }

    pub fn migrate_legacy(&self) -> ConfigResult<ConfigDocument> {
        let root = self
            .path
            .parent()
            .ok_or(ConfigError::MissingConfigDirectory)?;
        let result = self.modify_if_changed(|document| import_legacy_values(document, root));
        migrate_legacy_layout(root)?;
        result
    }
}

fn theme_layout_variant(theme: &str) -> &'static str {
    if theme.ends_with("-float") {
        "float"
    } else {
        "sticky"
    }
}

fn import_legacy_values(document: &mut ConfigDocument, root: &Path) -> ConfigResult<bool> {
    let mut changed = false;
    changed |= import_line(document, "/appearance/theme", &root.join(".active-theme"))?;
    let imported_accent = import_line(document, "/appearance/accent", &root.join(".accent-color"))?;
    changed |= imported_accent;
    if imported_accent && root.join(".accent-custom").is_file() {
        document.set("/appearance/accent_custom", Value::Bool(true))?;
        changed = true;
    }
    changed |= import_line(document, "/appearance/gtk_mode", &root.join(".gtk-mode"))?;
    changed |= import_line(
        document,
        "/appearance/wallpaper",
        &root.join(".wallpaper-custom"),
    )?;
    changed |= import_raw(document, "/layout/spaces_legacy", &root.join(".spaces"))?;
    changed |= import_raw(document, "/layout/borders_legacy", &root.join(".borders"))?;
    changed |= import_fonts(document, &root.join("fonts.conf"))?;
    changed |= import_hyprland(document, root)?;
    changed |= import_displays(document, root)?;
    changed |= import_defaults(document, &root.join("defaults.json"))?;
    changed |= import_keyboard_shortcuts(document, root)?;
    changed |= import_language(document, &root.join("language"))?;
    changed |= import_power(document, root)?;
    changed |= import_removable_devices(document, root)?;
    Ok(changed)
}

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

fn pointer_components(pointer: &str) -> ConfigResult<Vec<String>> {
    if !pointer.starts_with('/') {
        return Err(ConfigError::Invalid(format!(
            "JSON pointer must start with '/': {pointer}"
        )));
    }
    let components = pointer
        .trim_start_matches('/')
        .split('/')
        .filter(|component| !component.is_empty())
        .map(|component| component.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>();
    if components.is_empty() {
        return Err(ConfigError::Invalid("empty JSON pointer".into()));
    }
    Ok(components)
}

fn set_nested(root: &mut Value, components: &[String], value: Value) -> ConfigResult<()> {
    let mut current = root;
    for component in &components[..components.len().saturating_sub(1)] {
        let object = current
            .as_object_mut()
            .ok_or_else(|| ConfigError::Invalid("intermediate value must be an object".into()))?;
        current = object
            .entry(component)
            .or_insert_with(|| Value::Object(Map::new()));
    }
    current
        .as_object_mut()
        .ok_or_else(|| ConfigError::Invalid("target parent must be an object".into()))?
        .insert(components.last().cloned().unwrap_or_default(), value);
    Ok(())
}

fn unset_nested(root: &mut Value, components: &[String]) {
    if components.is_empty() {
        return;
    }
    if components.len() == 1 {
        if let Some(object) = root.as_object_mut() {
            object.remove(&components[0]);
        }
        return;
    }
    if let Some(next) = root.get_mut(&components[0]) {
        unset_nested(next, &components[1..]);
    }
}

fn apply_defaults(document: &mut ConfigDocument) {
    let appearance = document
        .sections
        .entry("appearance".into())
        .or_insert_with(|| Value::Object(Map::new()));
    let object = appearance.as_object_mut().expect("appearance is object");
    object
        .entry("accent")
        .or_insert_with(|| Value::String("#3590BD".into()));
    object
        .entry("accent_custom")
        .or_insert_with(|| Value::Bool(false));
    object
        .entry("theme")
        .or_insert_with(|| Value::String("argvus-dark".into()));
    object
        .entry("wallpaper")
        .or_insert_with(|| Value::String("/usr/share/backgrounds/argvus/argvus-dark.jxl".into()));
    object
        .entry("gtk_mode")
        .or_insert_with(|| Value::String("dark".into()));
    let theme = object
        .get("theme")
        .and_then(Value::as_str)
        .unwrap_or("argvus-dark")
        .to_owned();

    let layout = document
        .sections
        .entry("layout".into())
        .or_insert_with(|| Value::Object(Map::new()));
    let layout = layout.as_object_mut().expect("layout is object");
    layout
        .entry("variant")
        .or_insert_with(|| Value::String(theme_layout_variant(&theme).into()));

    let effects = document
        .sections
        .entry("effects".into())
        .or_insert_with(|| Value::Object(Map::new()));
    let effects = effects.as_object_mut().expect("effects is object");
    effects
        .entry("animations")
        .or_insert_with(|| Value::Bool(true));
    for (surface, transparency) in [
        ("taskbar", 90),
        ("launchers", 50),
        ("widget-telemetry", 90),
        ("control-panel", 90),
        ("terminal", 50),
        ("control-center", 50),
    ] {
        effects
            .entry(format!("transparency_{surface}_enabled"))
            .or_insert_with(|| Value::Bool(true));
        effects
            .entry(format!("transparency_{surface}_value"))
            .or_insert_with(|| Value::Number(transparency.into()));
        effects
            .entry(format!("blur_{surface}_enabled"))
            .or_insert_with(|| Value::Bool(true));
    }
    effects
        .entry("blur_global_value")
        .or_insert_with(|| Value::Number(50.into()));
    effects
        .entry("transparency_control-center_enabled")
        .or_insert_with(|| Value::Bool(true));
    effects
        .entry("transparency_control-center_value")
        .or_insert_with(|| Value::Number(50.into()));
    effects
        .entry("blur_control-center_enabled")
        .or_insert_with(|| Value::Bool(true));

    let default_apps = document
        .sections
        .entry("default_apps".into())
        .or_insert_with(|| Value::Object(Map::new()));
    let default_apps = default_apps
        .as_object_mut()
        .expect("default_apps is object");
    for (category, application) in [
        ("terminal", "argvus-terminal"),
        ("file_manager", "spf"),
        ("text_editor", "mousepad"),
        ("terminal_editor", "vim"),
        ("browser", "firefox"),
        ("image_viewer", "imv"),
        ("pdf_viewer", "zathura"),
        ("video_player", "mpv"),
        ("audio_player", "audacious"),
        ("archive", "xarchiver"),
        ("launcher", "rofi"),
    ] {
        default_apps
            .entry(category)
            .or_insert_with(|| Value::String(application.into()));
    }

    let keyboard_shortcuts = document
        .sections
        .entry("keyboard_shortcuts".into())
        .or_insert_with(|| Value::Object(Map::new()));
    let keyboard_shortcuts = keyboard_shortcuts
        .as_object_mut()
        .expect("keyboard_shortcuts is object");
    for (config_key, shortcut) in manifest_shortcut_defaults() {
        keyboard_shortcuts.entry(config_key).or_insert(shortcut);
    }

    let hyprland = document
        .sections
        .entry("hyprland".into())
        .or_insert_with(|| Value::Object(Map::new()));
    let hyprland = hyprland.as_object_mut().expect("hyprland is object");
    let input = hyprland
        .entry("input")
        .or_insert_with(|| Value::Object(Map::new()));
    let input = input.as_object_mut().expect("hyprland.input is object");
    let mouse = input
        .entry("mouse")
        .or_insert_with(|| Value::Object(Map::new()));
    let mouse = mouse
        .as_object_mut()
        .expect("hyprland.input.mouse is object");
    for (key, value) in [
        ("accel_profile", Value::String("flat".into())),
        ("left_handed", Value::Bool(false)),
        ("natural_scroll", Value::Bool(false)),
        ("scroll_factor", Value::from(1.0)),
        ("sensitivity", Value::from(0.0)),
    ] {
        mouse.entry(key).or_insert(value);
    }
    let touchpad = input
        .entry("touchpad")
        .or_insert_with(|| Value::Object(Map::new()));
    let touchpad = touchpad
        .as_object_mut()
        .expect("hyprland.input.touchpad is object");
    for (key, value) in [
        ("disable_while_typing", Value::Bool(true)),
        ("natural_scroll", Value::Bool(false)),
        ("tap_and_drag", Value::Bool(true)),
        ("tap_to_click", Value::Bool(true)),
        ("two_finger_right_click", Value::Bool(false)),
    ] {
        touchpad.entry(key).or_insert(value);
    }

    let power = document
        .sections
        .entry("power".into())
        .or_insert_with(|| Value::Object(Map::new()));
    let power = power.as_object_mut().expect("power is object");
    power.entry("lock_minutes").or_insert(15.into());
    power.entry("screen_off_minutes").or_insert(30.into());
}

fn validate_appearance(document: &ConfigDocument) -> ConfigResult<()> {
    let Some(appearance) = document.sections.get("appearance") else {
        return Ok(());
    };
    let object = appearance.as_object().expect("validated section object");
    if let Some(theme) = object.get("theme")
        && theme.as_str().is_none_or(|value| value.trim().is_empty())
    {
        return Err(ConfigError::Invalid(
            "appearance.theme must be a non-empty string".into(),
        ));
    }
    if let Some(accent) = object.get("accent") {
        let valid = accent.as_str().is_some_and(|value| {
            let value = value.strip_prefix('#').unwrap_or(value);
            value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        if !valid {
            return Err(ConfigError::Invalid(
                "appearance.accent must be #RRGGBB".into(),
            ));
        }
    }
    if let Some(accent_custom) = object.get("accent_custom")
        && !accent_custom.is_boolean()
    {
        return Err(ConfigError::Invalid(
            "appearance.accent_custom must be a boolean".into(),
        ));
    }
    for key in ["wallpaper_custom"] {
        if object.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(ConfigError::Invalid(format!(
                "appearance.{key} must be a boolean"
            )));
        }
    }
    if let Some(mode) = object.get("gtk_mode")
        && !mode
            .as_str()
            .is_some_and(|value| matches!(value, "light" | "dark" | "auto" | "sticky"))
    {
        return Err(ConfigError::Invalid(
            "appearance.gtk_mode is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_layout(document: &ConfigDocument) -> ConfigResult<()> {
    validate_numeric_section(document, "layout", 0, 100)?;
    if let Some(variant) = document.get("/layout/variant").and_then(Value::as_str)
        && !matches!(variant, "sticky" | "float")
    {
        return Err(ConfigError::Invalid(
            "layout.variant must be sticky or float".into(),
        ));
    }
    Ok(())
}

fn validate_effects(document: &ConfigDocument) -> ConfigResult<()> {
    let Some(effects) = document.sections.get("effects") else {
        return Ok(());
    };
    for (key, value) in effects.as_object().expect("validated section object") {
        if value.is_boolean() {
            continue;
        }
        let Some(number) = value.as_i64() else {
            return Err(ConfigError::Invalid(format!(
                "effects.{key} must be a boolean or an integer between 0 and 100"
            )));
        };
        if !(0..=100).contains(&number) {
            return Err(ConfigError::Invalid(format!(
                "effects.{key} must be between 0 and 100"
            )));
        }
    }
    Ok(())
}

fn validate_fonts(document: &ConfigDocument) -> ConfigResult<()> {
    let Some(value) = document.sections.get("fonts") else {
        return Ok(());
    };
    if let Some(targets) = value.get("targets") {
        let targets = targets
            .as_object()
            .ok_or_else(|| ConfigError::Invalid("fonts.targets must be an object".into()))?;
        for (target, selection) in targets {
            let selection = selection.as_object().ok_or_else(|| {
                ConfigError::Invalid(format!("fonts.targets.{target} must be an object"))
            })?;
            if selection
                .get("family")
                .is_some_and(|value| !value.is_string())
                || selection
                    .get("style")
                    .is_some_and(|value| !value.is_string())
            {
                return Err(ConfigError::Invalid(format!(
                    "fonts.targets.{target}.family and style must be strings"
                )));
            }
            if let Some(size) = selection.get("size") {
                let size = size.as_u64().ok_or_else(|| {
                    ConfigError::Invalid(format!("fonts.targets.{target}.size must be an integer"))
                })?;
                if !(8..=32).contains(&size) {
                    return Err(ConfigError::Invalid(format!(
                        "fonts.targets.{target}.size must be between 8 and 32"
                    )));
                }
            }
        }
    }
    if let Some(rendering) = value.get("rendering") {
        let rendering = rendering
            .as_object()
            .ok_or_else(|| ConfigError::Invalid("fonts.rendering must be an object".into()))?;
        for key in ["antialias", "custom_dpi"] {
            if rendering.get(key).is_some_and(|value| !value.is_boolean()) {
                return Err(ConfigError::Invalid(format!(
                    "fonts.rendering.{key} must be a boolean"
                )));
            }
        }
        for key in ["hinting", "subpixel"] {
            if rendering.get(key).is_some_and(|value| !value.is_string()) {
                return Err(ConfigError::Invalid(format!(
                    "fonts.rendering.{key} must be a string"
                )));
            }
        }
        if let Some(dpi) = rendering.get("dpi") {
            let dpi = dpi.as_u64().ok_or_else(|| {
                ConfigError::Invalid("fonts.rendering.dpi must be an integer".into())
            })?;
            if !(72..=240).contains(&dpi) {
                return Err(ConfigError::Invalid(
                    "fonts.rendering.dpi must be between 72 and 240".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_hyprland(document: &ConfigDocument) -> ConfigResult<()> {
    let Some(value) = document.sections.get("hyprland") else {
        return Ok(());
    };
    for key in ["keybindings", "input", "keyboard"] {
        if value.get(key).is_some_and(|value| !value.is_object()) {
            return Err(ConfigError::Invalid(format!(
                "hyprland.{key} must be an object"
            )));
        }
    }
    Ok(())
}

fn validate_displays(document: &ConfigDocument) -> ConfigResult<()> {
    let Some(value) = document.sections.get("displays") else {
        return Ok(());
    };
    if value
        .get("primary_monitor")
        .is_some_and(|value| !value.is_string() && !value.is_null())
    {
        return Err(ConfigError::Invalid(
            "displays.primary_monitor must be a string or null".into(),
        ));
    }
    if value
        .get("monitors")
        .is_some_and(|value| !value.is_object())
        || value
            .get("workspaces")
            .is_some_and(|value| !value.is_object())
    {
        return Err(ConfigError::Invalid(
            "displays.monitors and workspaces must be objects".into(),
        ));
    }
    Ok(())
}

fn validate_default_apps(document: &ConfigDocument) -> ConfigResult<()> {
    if let Some(value) = document.sections.get("default_apps") {
        let object = value
            .as_object()
            .ok_or_else(|| ConfigError::Invalid("default_apps must be an object".into()))?;
        const CATEGORIES: [&str; 11] = [
            "terminal",
            "file_manager",
            "text_editor",
            "terminal_editor",
            "browser",
            "image_viewer",
            "pdf_viewer",
            "video_player",
            "audio_player",
            "archive",
            "launcher",
        ];
        for (category, application) in object {
            if !CATEGORIES.contains(&category.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "default_apps contains unknown category: {category}"
                )));
            }
            if !application.is_string() {
                return Err(ConfigError::Invalid(format!(
                    "default_apps.{category} must be a string"
                )));
            }
        }
    }
    Ok(())
}

fn validate_keyboard_shortcuts(document: &ConfigDocument) -> ConfigResult<()> {
    let Some(value) = document.sections.get("keyboard_shortcuts") else {
        return Ok(());
    };
    let object = value
        .as_object()
        .ok_or_else(|| ConfigError::Invalid("keyboard_shortcuts must be an object".into()))?;
    for (key, shortcut) in object {
        if !key.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        }) {
            return Err(ConfigError::Invalid(format!(
                "keyboard_shortcuts.{key} is not a stable config key"
            )));
        }
        if !shortcut.is_null() && !shortcut.is_string() {
            return Err(ConfigError::Invalid(format!(
                "keyboard_shortcuts.{key} must be a string or null"
            )));
        }
    }
    Ok(())
}

fn validate_power(document: &ConfigDocument) -> ConfigResult<()> {
    let Some(power) = document.sections.get("power") else {
        return Ok(());
    };
    for key in ["screen_off_minutes", "lock_minutes"] {
        if let Some(value) = power.get(key)
            && value.as_u64().is_none()
        {
            return Err(ConfigError::Invalid(format!(
                "power.{key} must be an integer"
            )));
        }
    }
    for key in ["keep_awake"] {
        if power.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(ConfigError::Invalid(format!("power.{key} must be boolean")));
        }
    }
    for key in ["lid_battery", "lid_ac", "power_button"] {
        if power.get(key).is_some_and(|value| !value.is_string()) {
            return Err(ConfigError::Invalid(format!(
                "power.{key} must be a string"
            )));
        }
    }
    Ok(())
}

fn validate_session(document: &ConfigDocument) -> ConfigResult<()> {
    if let Some(value) = document.sections.get("session")
        && value
            .get("language")
            .is_some_and(|language| !language.is_string())
    {
        return Err(ConfigError::Invalid(
            "session.language must be a string".into(),
        ));
    }
    Ok(())
}

fn validate_numeric_section(
    document: &ConfigDocument,
    section: &str,
    minimum: i64,
    maximum: i64,
) -> ConfigResult<()> {
    let Some(value) = document.sections.get(section) else {
        return Ok(());
    };
    for (key, value) in value.as_object().expect("validated section object") {
        if key.ends_with("_legacy") {
            continue;
        }
        if let Some(number) = value.as_i64()
            && !(minimum..=maximum).contains(&number)
        {
            return Err(ConfigError::Invalid(format!(
                "{section}.{key} must be between {minimum} and {maximum}"
            )));
        }
    }
    Ok(())
}

fn validate_control_panel(document: &ConfigDocument) -> ConfigResult<()> {
    let Some(value) = document.sections.get("control_panel") else {
        return Ok(());
    };
    let Some(cards) = value.get("cards") else {
        return Ok(());
    };
    if !cards.is_array() {
        return Err(ConfigError::Invalid(
            "control_panel.cards must be an array".into(),
        ));
    }
    Ok(())
}

fn import_line(document: &mut ConfigDocument, pointer: &str, path: &Path) -> ConfigResult<bool> {
    if document.get(pointer).is_some() || !path.is_file() {
        return Ok(false);
    }
    let value = fs::read_to_string(path)?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if value.is_empty() {
        return Ok(false);
    }
    document.set(pointer, Value::String(value))?;
    Ok(true)
}

fn import_raw(document: &mut ConfigDocument, pointer: &str, path: &Path) -> ConfigResult<bool> {
    if document.get(pointer).is_some() || !path.is_file() {
        return Ok(false);
    }
    let value = fs::read_to_string(path)?;
    document.set(pointer, Value::String(value))?;
    Ok(true)
}

fn import_fonts(document: &mut ConfigDocument, path: &Path) -> ConfigResult<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let legacy = fs::read_to_string(path)?;
    let state = legacy
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect::<BTreeMap<_, _>>();
    let mut changed = false;

    if document.get("/fonts/legacy").is_none() {
        document.set("/fonts/legacy", Value::String(legacy.clone()))?;
        changed = true;
    }

    if document.get("/fonts/targets").is_none() {
        let mut targets = Map::new();
        for target in [
            "taskbar",
            "sysinfo",
            "control_panel",
            "system",
            "apps",
            "terminal",
            "browser",
        ] {
            let mut selection = Map::new();
            let family_key = format!("{target}_family");
            if let Some(value) = state.get(family_key.as_str()) {
                selection.insert("family".into(), Value::String((*value).into()));
            }
            let style_key = format!("{target}_style");
            if let Some(value) = state.get(style_key.as_str()) {
                selection.insert("style".into(), Value::String((*value).into()));
            }
            let size_key = format!("{target}_size");
            if let Some(value) = state.get(size_key.as_str())
                && let Ok(value) = (*value).parse::<u16>()
            {
                selection.insert("size".into(), value.into());
            }
            if !selection.is_empty() {
                targets.insert(target.into(), Value::Object(selection));
            }
        }
        if !targets.is_empty() {
            document.set("/fonts/targets", Value::Object(targets))?;
            changed = true;
        }
    }

    if document.get("/fonts/rendering").is_none() {
        let mut rendering = Map::new();
        if let Some(value) = state.get("antialias") {
            rendering.insert("antialias".into(), Value::Bool(*value == "true"));
        }
        for key in ["hinting", "subpixel"] {
            if let Some(value) = state.get(key) {
                rendering.insert(key.into(), Value::String((*value).into()));
            }
        }
        if let Some(value) = state.get("custom_dpi") {
            rendering.insert("custom_dpi".into(), Value::Bool(*value == "true"));
        }
        if let Some(value) = state.get("dpi")
            && let Ok(value) = (*value).parse::<u16>()
        {
            rendering.insert("dpi".into(), value.into());
        }
        if !rendering.is_empty() {
            document.set("/fonts/rendering", Value::Object(rendering))?;
            changed = true;
        }
    }

    Ok(changed)
}

fn import_defaults(document: &mut ConfigDocument, path: &Path) -> ConfigResult<bool> {
    if document.get("/default_apps").is_some() {
        return Ok(false);
    }
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(false);
    };
    let value: Value = serde_json::from_str(&contents).map_err(ConfigError::InvalidJson)?;
    let Some(object) = value.as_object() else {
        return Ok(false);
    };
    const CATEGORIES: [&str; 11] = [
        "terminal",
        "file_manager",
        "text_editor",
        "terminal_editor",
        "browser",
        "image_viewer",
        "pdf_viewer",
        "video_player",
        "audio_player",
        "archive",
        "launcher",
    ];
    let migrated = object
        .iter()
        .filter(|(key, value)| CATEGORIES.contains(&key.as_str()) && value.is_string())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    document.set("/default_apps", Value::Object(migrated))?;
    Ok(true)
}

fn import_keyboard_shortcuts(document: &mut ConfigDocument, root: &Path) -> ConfigResult<bool> {
    if document.get("/keyboard_shortcuts").is_some() {
        return Ok(false);
    }
    let old_value = if let Some(value) = document.get("/hyprland/keybindings") {
        Some(value.clone())
    } else {
        parse_keybindings_toml(&root.join("keybindings.toml"))?
    };
    let Some(value) = old_value else {
        return Ok(false);
    };
    let shortcuts = legacy_keybindings_to_shortcuts(&value);
    document.set("/keyboard_shortcuts", shortcuts)?;
    document.unset("/hyprland/keybindings")?;
    Ok(true)
}

fn migrate_legacy_sections(document: &mut ConfigDocument) -> ConfigResult<()> {
    if document.get("/default_apps").is_none()
        && let Some(value) = document.sections.remove("defaults")
    {
        document.set("/default_apps", value)?;
    } else {
        document.sections.remove("defaults");
    }
    if document.get("/keyboard_shortcuts").is_none()
        && let Some(value) = document
            .sections
            .get_mut("hyprland")
            .and_then(Value::as_object_mut)
            .and_then(|object| object.remove("keybindings"))
    {
        document.set(
            "/keyboard_shortcuts",
            legacy_keybindings_to_shortcuts(&value),
        )?;
    }
    if let Some(hyprland) = document.sections.get_mut("hyprland") {
        hyprland
            .as_object_mut()
            .expect("hyprland is object")
            .remove("keybindings");
    }
    Ok(())
}

fn legacy_keybindings_to_shortcuts(value: &Value) -> Value {
    let mut shortcuts = Map::new();
    if let Some(entries) = value.as_object() {
        for (id, binding) in entries {
            let config_key = config_key_for_binding(id);
            let shortcut = binding
                .get("enabled")
                .and_then(Value::as_bool)
                .filter(|enabled| !enabled)
                .map(|_| Value::Null)
                .or_else(|| binding.get("keys").cloned())
                .unwrap_or(Value::Null);
            shortcuts.insert(config_key, shortcut);
        }
    }
    Value::Object(shortcuts)
}

fn config_key_for_binding(id: &str) -> String {
    match id {
        "window.close" => "close_window",
        "window.drag_mouse" => "drag_window__floating_window_only",
        "window.maximize" => "maximize_window__toggle",
        "app.browser" => "open_browser",
        "system.about" => "open_about",
        "appearance.theme" => "open_theme_selector",
        _ => return id.replace(['.', '-'], "_"),
    }
    .into()
}

fn manifest_shortcut_defaults() -> BTreeMap<String, Value> {
    let path = env::var_os("ARGVUS_SYSTEM_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/argvus"))
        .join("hyprland/keybindings.json");
    let Ok(contents) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(manifest) = serde_json::from_str::<Value>(&contents) else {
        return BTreeMap::new();
    };
    let mut defaults = BTreeMap::new();
    let Some(bindings) = manifest.get("bindings").and_then(Value::as_array) else {
        return defaults;
    };
    for binding in bindings {
        let Some(id) = binding.get("id").and_then(Value::as_str) else {
            continue;
        };
        let key = binding
            .get("config_key")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| config_key_for_binding(id));
        if let Some(shortcut) = binding.get("keys") {
            defaults.insert(key, shortcut.clone());
        }
    }
    defaults
}

fn migrate_legacy_layout(root: &Path) -> ConfigResult<bool> {
    let data = root.join("data");
    fs::create_dir_all(data.join("internal"))?;
    fs::create_dir_all(data.join("backups"))?;
    let mut moved = false;
    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by_key(|entry| {
        if entry.file_name() == "generated" {
            0
        } else {
            1
        }
    });
    for entry in entries {
        let source = entry.path();
        let name = entry.file_name();
        if name == "config.json" || name == "data" {
            continue;
        }
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            ".active-theme" | ".gtk-mode" | ".wallpaper-custom" | ".spaces" | ".borders"
        ) {
            continue;
        }
        let relative = match name.as_ref() {
            ".config.lock" => PathBuf::from("data/internal/config.lock"),
            "config.json.bak" => PathBuf::from("data/backups/config.json.bak"),
            "defaults.json" => PathBuf::from("data/control-center/defaults.json"),
            "keybindings.toml" => PathBuf::from("data/hypr/keybindings.toml"),
            "input.toml" => PathBuf::from("data/hypr/input.toml"),
            ".idle-timeout" | ".keep-awake" => PathBuf::from(format!("data/power/{name}")),
            "fonts.conf" => PathBuf::from("data/generated/fonts.conf"),
            "generated" => PathBuf::from("data/generated"),
            "control-center" => PathBuf::from("data/control-center"),
            "state" => PathBuf::from("data/state"),
            "hypr" => PathBuf::from("data/hypr"),
            "waybar" => PathBuf::from("data/taskbar/waybar"),
            "rofi" => PathBuf::from("data/launcher/rofi"),
            "quickshell" => PathBuf::from("data/control-panel/quickshell"),
            "terminal" | "kitty" | "foot" => PathBuf::from(format!("data/terminal/{name}")),
            _ => PathBuf::from(format!("data/legacy/{name}")),
        };
        let destination = root.join(relative);
        if matches!(name.as_ref(), "generated" | "control-center") && source.is_dir() {
            merge_legacy_tree(&source, &destination, &data.join("legacy"))?;
        } else if destination.exists() {
            let fallback = root.join("data/legacy").join(name.as_ref());
            if fallback.exists() {
                continue;
            }
            fs::create_dir_all(fallback.parent().expect("legacy parent"))?;
            fs::rename(&source, fallback)?;
        } else {
            fs::create_dir_all(destination.parent().expect("migration parent"))?;
            fs::rename(&source, destination)?;
        }
        moved = true;
    }
    Ok(moved)
}

/// Merge an old component directory into its new data-owned location.
///
/// New files win on collision because they are the current layout. Conflicting
/// legacy files are retained under data/legacy instead of being discarded.
fn merge_legacy_tree(source: &Path, destination: &Path, legacy_root: &Path) -> ConfigResult<bool> {
    fs::create_dir_all(destination)?;
    let component_name = source
        .file_name()
        .ok_or_else(|| ConfigError::Invalid("legacy component has no name".into()))?;
    let component_legacy_root = legacy_root.join(component_name);
    let mut changed = false;

    for entry in fs::read_dir(source)?.collect::<Result<Vec<_>, io::Error>>()? {
        let source_entry = entry.path();
        let entry_name = entry.file_name();
        let destination_entry = destination.join(&entry_name);

        if destination_entry.is_dir() && source_entry.is_dir() {
            changed |=
                merge_legacy_tree(&source_entry, &destination_entry, &component_legacy_root)?;
            continue;
        }

        if destination_entry.exists() {
            fs::create_dir_all(&component_legacy_root)?;
            let mut legacy_entry = component_legacy_root.join(&entry_name);
            let mut suffix = 1u32;
            while legacy_entry.exists() {
                legacy_entry = component_legacy_root
                    .join(format!("{}.legacy-{suffix}", entry_name.to_string_lossy()));
                suffix += 1;
            }
            fs::rename(&source_entry, legacy_entry)?;
        } else {
            fs::rename(&source_entry, destination_entry)?;
        }
        changed = true;
    }

    if fs::read_dir(source)?.next().is_none() {
        fs::remove_dir(source)?;
    }
    Ok(changed)
}

fn import_language(document: &mut ConfigDocument, path: &Path) -> ConfigResult<bool> {
    if document.get("/session/language").is_some() {
        return Ok(false);
    }
    let Ok(value) = fs::read_to_string(path) else {
        return Ok(false);
    };
    let language = value.trim();
    if language.is_empty() {
        return Ok(false);
    }
    document.set(
        "/session/language",
        Value::String(language.replace('_', "-")),
    )?;
    Ok(true)
}

fn import_power(document: &mut ConfigDocument, root: &Path) -> ConfigResult<bool> {
    let mut changed = false;
    if document.get("/power/keep_awake").is_none()
        && let Ok(value) = fs::read_to_string(root.join(".keep-awake"))
        && value.trim() == "enabled"
    {
        document.set("/power/keep_awake", Value::Bool(true))?;
        changed = true;
    }
    let path = root.join("hypr/hypridle.conf");
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(changed);
    };
    let mut timeout = None;
    let mut action = String::new();
    for line in contents.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("timeout =") {
            timeout = value.trim().parse::<u64>().ok();
        } else if let Some(value) = line.strip_prefix("on-timeout =") {
            action = value.trim().to_ascii_lowercase();
        } else if line == "}" {
            if let Some(seconds) = timeout.take() {
                let key = if action.contains("dpms") && action.contains("off") {
                    "screen_off_minutes"
                } else if action.contains("lock") {
                    "lock_minutes"
                } else {
                    ""
                };
                if !key.is_empty() {
                    let pointer = format!("/power/{key}");
                    if document.get(&pointer).is_none() {
                        document.set(&pointer, Value::from(seconds / 60))?;
                        changed = true;
                    }
                }
            }
            action.clear();
        }
    }
    Ok(changed)
}

fn import_removable_devices(document: &mut ConfigDocument, root: &Path) -> ConfigResult<bool> {
    if document.get("/removable_devices").is_some() {
        return Ok(false);
    }
    let path = root.join("removable-devices/config.json");
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(false);
    };
    let value: Value =
        serde_json::from_str(&strip_json_comments(&contents)).map_err(ConfigError::InvalidJson)?;
    if !value.is_object() {
        return Ok(false);
    }
    document.set("/removable_devices", value)?;
    Ok(true)
}

fn strip_json_comments(contents: &str) -> String {
    let mut output = String::with_capacity(contents.len());
    let mut chars = contents.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(character) = chars.next() {
        if in_string {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            output.push(character);
        } else if character == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for comment_character in chars.by_ref() {
                if comment_character == '\n' {
                    output.push('\n');
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn import_hyprland(document: &mut ConfigDocument, root: &Path) -> ConfigResult<bool> {
    let mut changed = false;
    if document.get("/hyprland/input").is_none()
        && let Some(value) = parse_input_toml(&root.join("input.toml"))?
    {
        document.set("/hyprland/input", value)?;
        changed = true;
    }
    if document.get("/hyprland/keyboard").is_none()
        && let Some(value) = parse_keyboard_lua(&root.join("generated/hypr/input.lua"))?
    {
        document.set("/hyprland/keyboard", value)?;
        changed = true;
    }
    Ok(changed)
}

fn import_displays(document: &mut ConfigDocument, root: &Path) -> ConfigResult<bool> {
    if document.get("/displays").is_some() {
        return Ok(false);
    }
    let Ok(contents) = fs::read_to_string(root.join("generated/hypr/monitors.lua")) else {
        return Ok(false);
    };
    let mut monitors = Map::new();
    let mut workspaces: Map<String, Value> = Map::new();
    let mut primary_monitor = None;

    for body in lua_call_bodies(&contents, "hl.monitor") {
        let fields = lua_fields(&body);
        let Some(name) = fields.get("output").and_then(Value::as_str) else {
            continue;
        };
        let mut monitor = Map::new();
        for key in [
            "mode",
            "position",
            "scale",
            "transform",
            "vrr",
            "mirror",
            "bitdepth",
            "disabled",
            "supports_hdr",
            "hdr",
            "sdrbrightness",
            "sdrsaturation",
        ] {
            if let Some(value) = fields.get(key) {
                let canonical = match key {
                    "supports_hdr" => "hdr",
                    "sdrbrightness" => "sdr_brightness",
                    "sdrsaturation" => "sdr_saturation",
                    _ => key,
                };
                monitor.insert(canonical.into(), value.clone());
            }
        }
        monitors.insert(name.to_string(), Value::Object(monitor));
    }

    for body in lua_call_bodies(&contents, "hl.workspace_rule") {
        let fields = lua_fields(&body);
        let (Some(workspace), Some(monitor)) = (
            fields.get("workspace").and_then(Value::as_str),
            fields.get("monitor").and_then(Value::as_str),
        ) else {
            continue;
        };
        let Ok(workspace) = workspace.parse::<u64>() else {
            continue;
        };
        let ids = workspaces
            .entry(monitor.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(ids) = ids.as_array_mut()
            && !ids.iter().any(|value| value.as_u64() == Some(workspace))
        {
            ids.push(Value::Number(workspace.into()));
        }
        if workspace == 1 && fields.get("default") == Some(&Value::Bool(true)) {
            primary_monitor = Some(Value::String(monitor.to_string()));
        }
    }

    if monitors.is_empty() && workspaces.is_empty() {
        return Ok(false);
    }
    for ids in workspaces.values_mut() {
        if let Some(ids) = ids.as_array_mut() {
            ids.sort_by_key(Value::as_u64);
        }
    }
    let mut displays = Map::new();
    displays.insert(
        "primary_monitor".into(),
        primary_monitor.unwrap_or(Value::Null),
    );
    displays.insert("monitors".into(), Value::Object(monitors));
    displays.insert("workspaces".into(), Value::Object(workspaces));
    document.set("/displays", Value::Object(displays))?;
    Ok(true)
}

fn lua_call_bodies(contents: &str, call: &str) -> Vec<String> {
    let mut bodies = Vec::new();
    let mut offset = 0;
    while let Some(relative) = contents[offset..].find(&format!("{call}({{")) {
        let start = offset + relative + call.len() + 2;
        let Some(end) = contents[start..].find("})") else {
            break;
        };
        bodies.push(contents[start..start + end].to_string());
        offset = start + end + 2;
    }
    bodies
}

fn lua_fields(body: &str) -> Map<String, Value> {
    body.split(',')
        .filter_map(|field| {
            let (key, raw) = field.split_once('=')?;
            Some((key.trim().to_string(), parse_lua_value(raw.trim())?))
        })
        .collect()
}

fn parse_lua_value(raw: &str) -> Option<Value> {
    parse_scalar(raw).or_else(|| {
        raw.parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
    })
}

fn parse_keybindings_toml(path: &Path) -> ConfigResult<Option<Value>> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut bindings = Map::new();
    let mut current = None;
    for line in contents.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix("[keybindings.")
            && let Some(section) = section.strip_suffix(']')
        {
            current = Some(section.trim_matches('"').to_string());
            bindings.insert(current.clone().unwrap(), Value::Object(Map::new()));
            continue;
        }
        let Some(id) = current.as_ref() else {
            continue;
        };
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let Some(value) = parse_scalar(raw.trim()) else {
            continue;
        };
        if let Some(object) = bindings.get_mut(id).and_then(Value::as_object_mut) {
            object.insert(key.trim().to_string(), value);
        }
    }
    Ok((!bindings.is_empty()).then_some(Value::Object(bindings)))
}

fn parse_input_toml(path: &Path) -> ConfigResult<Option<Value>> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut sections = Map::new();
    let mut current = None;
    for line in contents.lines() {
        let line = line.trim();
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
            && matches!(section, "mouse" | "touchpad")
        {
            current = Some(section.to_string());
            sections.insert(section.to_string(), Value::Object(Map::new()));
            continue;
        }
        let Some(section) = current.as_ref() else {
            continue;
        };
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let Some(value) = parse_scalar(raw.trim()) else {
            continue;
        };
        if let Some(object) = sections.get_mut(section).and_then(Value::as_object_mut) {
            object.insert(key.trim().to_string(), value);
        }
    }
    Ok((!sections.is_empty()).then_some(Value::Object(sections)))
}

fn parse_keyboard_lua(path: &Path) -> ConfigResult<Option<Value>> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut keyboard = Map::new();
    for key in ["kb_layout", "kb_variant", "kb_options"] {
        let Some(value) = contents.lines().find_map(|line| {
            let value = line.trim().strip_prefix(&format!("{key} ="))?.trim();
            let value = value.strip_suffix(',').unwrap_or(value);
            let value = value.strip_prefix('"')?.strip_suffix('"')?;
            Some(value.to_string())
        }) else {
            continue;
        };
        keyboard.insert(
            match key {
                "kb_layout" => "layout",
                "kb_variant" => "variant",
                _ => "options",
            }
            .into(),
            Value::String(value),
        );
    }
    Ok((!keyboard.is_empty()).then_some(Value::Object(keyboard)))
}

fn parse_scalar(raw: &str) -> Option<Value> {
    serde_json::from_str(raw).ok().or_else(|| {
        raw.parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
    })
}

fn lock_file(file: &File) -> io::Result<()> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

struct LockGuard<'a>(&'a File);

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_nested_values_and_validates_accent() {
        let mut document = ConfigDocument::default();
        document
            .set("/appearance/accent", Value::String("#AABBCC".into()))
            .unwrap();
        assert_eq!(
            document.get("/appearance/accent").and_then(Value::as_str),
            Some("#AABBCC")
        );
        assert!(
            document
                .set("/appearance/accent", Value::String("bad".into()))
                .is_err()
        );
    }

    #[test]
    fn scope_contains_only_requested_sections() {
        let mut document = ConfigDocument::default();
        document
            .set("/appearance/theme", Value::String("argvus-dark".into()))
            .unwrap();
        document
            .set("/default_apps/browser", Value::String("firefox".into()))
            .unwrap();
        let scoped = document.scoped(ConfigScope::Appearance);
        assert!(scoped.sections.contains_key("appearance"));
        assert!(!scoped.sections.contains_key("defaults"));
    }

    #[test]
    fn validates_structured_font_ranges() {
        let mut document = ConfigDocument::default();
        document
            .set(
                "/fonts/targets/apps",
                serde_json::json!({
                    "family": "IBM Plex Mono",
                    "style": "Regular",
                    "size": 14
                }),
            )
            .unwrap();
        assert!(
            document
                .set("/fonts/rendering/dpi", Value::Number(300.into()))
                .is_err()
        );
    }

    #[test]
    fn legacy_migration_does_not_replace_canonical_values() {
        let directory = env::temp_dir().join(format!(
            "argvus-config-test-{}-migration",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        let config_directory = directory.join("argvus");
        fs::create_dir_all(&config_directory).unwrap();
        fs::write(config_directory.join(".active-theme"), "argvus-light\n").unwrap();
        fs::write(config_directory.join(".accent-color"), "#112233\n").unwrap();
        let store = ConfigStore {
            path: config_directory.join("config.json"),
            lock_path: config_directory.join(LOCK_FILE),
        };

        store
            .update("/appearance/theme", Value::String("argvus-dark".into()))
            .unwrap();
        store.migrate_legacy().unwrap();

        let document = store.load().unwrap();
        assert_eq!(
            document.get("/appearance/theme").and_then(Value::as_str),
            Some("argvus-dark")
        );
        assert_eq!(
            document.get("/appearance/accent").and_then(Value::as_str),
            Some("#112233")
        );
        assert!(document.get("/appearance/accent_custom").is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bootstrap_migration_marks_only_legacy_explicit_accents() {
        let directory = env::temp_dir().join(format!(
            "argvus-config-test-{}-accent-marker",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        let config_directory = directory.join("argvus");
        fs::create_dir_all(&config_directory).unwrap();
        fs::write(config_directory.join(".accent-color"), "#ABCDEF\n").unwrap();
        fs::write(config_directory.join(".accent-custom"), "1\n").unwrap();
        let store = ConfigStore {
            path: config_directory.join("config.json"),
            lock_path: config_directory.join(LOCK_FILE),
        };

        store.ensure().unwrap();
        let document = store.load().unwrap();
        assert_eq!(
            document.get("/appearance/accent").and_then(Value::as_str),
            Some("#ABCDEF")
        );
        assert_eq!(
            document.get("/appearance/accent_custom"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            document.get("/appearance/theme").and_then(Value::as_str),
            Some("argvus-dark")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn save_keeps_previous_valid_configuration_as_backup() {
        let directory =
            env::temp_dir().join(format!("argvus-config-test-{}-backup", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let config_directory = directory.join("argvus");
        let store = ConfigStore {
            path: config_directory.join("config.json"),
            lock_path: config_directory.join(LOCK_FILE),
        };
        let mut first = ConfigDocument::default();
        first
            .set("/appearance/theme", Value::String("argvus-dark".into()))
            .unwrap();
        store.save(&first).unwrap();

        let mut second = first.clone();
        second
            .set("/appearance/theme", Value::String("argvus-light".into()))
            .unwrap();
        store.save(&second).unwrap();

        let backup =
            fs::read_to_string(config_directory.join("data/backups/config.json.bak")).unwrap();
        let backup_document: ConfigDocument = serde_json::from_str(&backup).unwrap();
        assert_eq!(
            backup_document
                .get("/appearance/theme")
                .and_then(Value::as_str),
            Some("argvus-dark")
        );
        assert_eq!(
            store
                .load()
                .unwrap()
                .get("/appearance/theme")
                .and_then(Value::as_str),
            Some("argvus-light")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn canonical_update_preserves_unrelated_sections() {
        let directory = env::temp_dir().join(format!(
            "argvus-config-test-{}-preserve",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        let config_directory = directory.join("argvus");
        let store = ConfigStore {
            path: config_directory.join("config.json"),
            lock_path: config_directory.join(LOCK_FILE),
        };

        store
            .update("/power/lock_minutes", Value::Number(15.into()))
            .unwrap();
        store
            .update(
                "/appearance/theme",
                Value::String("argvus-dark-float".into()),
            )
            .unwrap();

        let document = store.load().unwrap();
        assert_eq!(
            document.get("/power/lock_minutes"),
            Some(&Value::Number(15.into()))
        );
        assert_eq!(
            document.get("/appearance/theme").and_then(Value::as_str),
            Some("argvus-dark-float")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn new_profiles_receive_effect_defaults_without_overwriting_values() {
        let empty = ConfigDocument::default();
        let path = env::temp_dir().join(format!(
            "argvus-config-defaults-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let effective = ConfigDocument::load_effective(&path).unwrap();
        let effects = effective.sections.get("effects").unwrap();
        assert_eq!(effects.get("animations"), Some(&Value::Bool(true)));
        assert_eq!(
            effects.get("transparency_taskbar_value"),
            Some(&Value::Number(90.into()))
        );
        assert_eq!(
            effects.get("transparency_terminal_value"),
            Some(&Value::Number(50.into()))
        );
        assert_eq!(
            effects.get("transparency_launchers_value"),
            Some(&Value::Number(50.into()))
        );
        assert_eq!(
            effects.get("transparency_control-center_value"),
            Some(&Value::Number(50.into()))
        );
        assert_eq!(
            effects.get("blur_global_value"),
            Some(&Value::Number(50.into()))
        );

        let mut existing = empty;
        existing
            .set(
                "/effects/transparency_control-center_value",
                Value::Number(80.into()),
            )
            .unwrap();
        let existing_path = path.with_extension("existing.json");
        fs::write(&existing_path, serde_json::to_vec(&existing).unwrap()).unwrap();
        let preserved = ConfigDocument::load_effective(&existing_path).unwrap();
        assert_eq!(
            preserved.get("/effects/transparency_control-center_value"),
            Some(&Value::Number(80.into()))
        );
        let _ = fs::remove_file(existing_path);
    }

    #[test]
    fn ensure_materializes_defaults_and_preserves_explicit_false_and_zero() {
        let directory =
            env::temp_dir().join(format!("argvus-config-test-{}-ensure", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let config_directory = directory.join("argvus");
        let store = ConfigStore {
            path: config_directory.join("config.json"),
            lock_path: config_directory.join(LOCK_FILE),
        };

        store.ensure().unwrap();
        assert!(store.path.is_file());
        let document = store.load().unwrap();
        assert_eq!(
            document.get("/appearance/theme").and_then(Value::as_str),
            Some("argvus-dark")
        );
        assert_eq!(
            document.get("/effects/animations"),
            Some(&Value::Bool(true))
        );

        store
            .set_explicit_for_test("/effects/animations", Value::Bool(false))
            .unwrap();
        store
            .set_explicit_for_test(
                "/effects/transparency_terminal_value",
                Value::Number(0.into()),
            )
            .unwrap();
        store.ensure().unwrap();
        let preserved = store.load().unwrap();
        assert_eq!(
            preserved.get("/effects/animations"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            preserved.get("/effects/transparency_terminal_value"),
            Some(&Value::Number(0.into()))
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ensure_migrates_component_directories_into_data_idempotently() {
        let directory = env::temp_dir().join(format!(
            "argvus-config-test-{}-component-migration",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        let config_directory = directory.join("argvus");
        fs::create_dir_all(config_directory.join("generated")).unwrap();
        fs::create_dir_all(config_directory.join("control-center")).unwrap();
        fs::create_dir_all(config_directory.join("data/generated")).unwrap();
        fs::create_dir_all(config_directory.join("data/control-center")).unwrap();
        fs::write(
            config_directory.join("generated/theme-effective.conf"),
            b"legacy-theme",
        )
        .unwrap();
        fs::write(
            config_directory.join("control-center/foot.ini"),
            b"legacy-foot",
        )
        .unwrap();
        fs::write(
            config_directory.join("data/generated/current.conf"),
            b"current-generated",
        )
        .unwrap();
        fs::write(
            config_directory.join("data/control-center/current.ini"),
            b"current-control-center",
        )
        .unwrap();

        let store = ConfigStore {
            path: config_directory.join("config.json"),
            lock_path: config_directory.join(LOCK_FILE),
        };
        store.ensure().unwrap();

        assert!(!config_directory.join("generated").exists());
        assert!(!config_directory.join("control-center").exists());
        assert_eq!(
            fs::read(config_directory.join("data/generated/theme-effective.conf")).unwrap(),
            b"legacy-theme"
        );
        assert_eq!(
            fs::read(config_directory.join("data/control-center/foot.ini")).unwrap(),
            b"legacy-foot"
        );

        let first_tree = fs::read_dir(config_directory.join("data"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        store.ensure().unwrap();
        let second_tree = fs::read_dir(config_directory.join("data"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(first_tree, second_tree);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ensure_does_not_replace_malformed_configuration() {
        let directory = env::temp_dir().join(format!(
            "argvus-config-test-{}-malformed",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        let config_directory = directory.join("argvus");
        fs::create_dir_all(&config_directory).unwrap();
        let path = config_directory.join("config.json");
        fs::write(&path, b"{ invalid\n").unwrap();
        let store = ConfigStore {
            path: path.clone(),
            lock_path: config_directory.join(LOCK_FILE),
        };

        assert!(store.ensure().is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "{ invalid\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_theme_is_one_transaction_and_preserves_explicit_overrides() {
        let directory =
            env::temp_dir().join(format!("argvus-config-test-{}-theme", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let config_directory = directory.join("argvus");
        let store = ConfigStore {
            path: config_directory.join("config.json"),
            lock_path: config_directory.join(LOCK_FILE),
        };
        store.ensure().unwrap();
        let mut patch = Map::new();
        patch.insert("/appearance/accent".into(), Value::String("#ABCDEF".into()));
        patch.insert("/appearance/accent_custom".into(), Value::Bool(true));
        patch.insert(
            "/appearance/wallpaper".into(),
            Value::String("/tmp/custom.jxl".into()),
        );
        patch.insert("/appearance/wallpaper_custom".into(), Value::Bool(true));
        store.patch(&patch).unwrap();
        store
            .apply_theme(
                "universe",
                Some("#EEEEEE"),
                Some("dark"),
                Some("/usr/share/backgrounds/universe.jxl"),
                false,
            )
            .unwrap();
        let document = store.load().unwrap();
        assert_eq!(
            document.get("/appearance/theme").and_then(Value::as_str),
            Some("universe")
        );
        assert_eq!(
            document.get("/layout/variant").and_then(Value::as_str),
            Some("sticky")
        );
        assert_eq!(
            document.get("/appearance/accent").and_then(Value::as_str),
            Some("#ABCDEF")
        );
        assert_eq!(
            document
                .get("/appearance/wallpaper")
                .and_then(Value::as_str),
            Some("/tmp/custom.jxl")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    impl ConfigStore {
        fn set_explicit_for_test(&self, pointer: &str, value: Value) -> ConfigResult<()> {
            self.update(pointer, value).map(|_| ())
        }
    }
}
