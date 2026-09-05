use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use clap::Parser;
use hex_color::HexColor;
use relm4::SharedState;

use serde::Deserialize;
use serde::de::Deserializer;
use thiserror::Error;
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};
use xdg::{BaseDirectories, BaseDirectoriesError};

use crate::{
    style::Color,
    tools::{Highlighters, Tools},
};

use tensaku_cli::command_line::{
    Action as CommandLineAction, CommandLine, Fullscreen, Resize, ScrollCaptureTest,
};

pub static APP_CONFIG: SharedState<Configuration> = SharedState::new();

#[derive(Error, Debug)]
enum ConfigurationFileError {
    #[error("XDG context error: {0}")]
    Xdg(#[from] BaseDirectoriesError),

    #[error("Error reading file: {0}")]
    ReadFile(#[from] io::Error),

    #[error("Decoding toml failed: {0}")]
    TomlDecoding(#[from] toml::de::Error),
}

#[derive(Error, Debug)]
pub(crate) enum ConfigurationWriteError {
    #[error("no writable Tensaku configuration path is available")]
    MissingPath,

    #[error("error reading or writing the configuration file: {0}")]
    Io(#[from] io::Error),

    #[error("decoding the configuration file for editing failed: {0}")]
    TomlEditing(#[from] toml_edit::TomlError),

    #[error("the [{0}] configuration entry is not a table")]
    SectionIsNotATable(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
enum GeneralValue {
    Bool(bool),
    Float(f32),
    Str(String),
}

impl GeneralValue {
    fn into_toml_value(self) -> Value {
        match self {
            Self::Bool(value) => Value::from(value),
            // Converting f32 directly to f64 exposes binary precision noise in
            // the user-facing file (for example 1.7 became
            // 1.7000000476837158). Reparse f32's shortest round-trippable
            // decimal representation before handing it to toml_edit.
            Self::Float(value) => {
                Value::from(value.to_string().parse::<f64>().unwrap_or(f64::from(value)))
            }
            Self::Str(value) => Value::from(value),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ConfigUpdates {
    general: Vec<(&'static str, GeneralValue)>,
    keybinds: Vec<(&'static str, String)>,
}

impl ConfigUpdates {
    fn is_empty(&self) -> bool {
        self.general.is_empty() && self.keybinds.is_empty()
    }
}

pub struct Configuration {
    /// The exact file used for this process: the path supplied through
    /// `--config`, or the XDG default. Preferences that are backed by
    /// config.toml write through this path rather than state.toml.
    config_file_path: Option<PathBuf>,
    man: bool,
    license: bool,
    install_desktop: bool,
    doctor: bool,
    install_omarchy_wrapper: bool,
    wire_omarchy: bool,
    wire_capture_key: Option<String>,
    input_filename: Option<String>,
    /// Detected input encoding for this document, never persisted as a preference.
    source_format: crate::image_export::ImageFormat,
    output_filename: Option<String>,
    fullscreen: Option<Fullscreen>,
    resize: Option<Resize>,
    floating_hack: bool,
    early_exit: bool,
    early_exit_save_as: bool,
    corner_roundness: f32,
    initial_tool: Tools,
    copy_command: Option<String>,
    annotation_size_factor: f32,
    /// True when the factor came from config.toml, legacy preference
    /// migration, the CLI, or a successful Preferences save. The welcome
    /// dialog uses presence rather than comparing the value to the 1.0
    /// built-in default.
    annotation_size_factor_is_explicit: bool,
    save_after_copy: bool,
    auto_copy: bool,
    actions_on_enter: Vec<Action>,
    actions_on_escape: Vec<Action>,
    actions_on_right_click: Vec<Action>,
    color_palette: ColorPalette,
    default_hide_toolbars: bool,
    focus_toggles_toolbars: bool,
    default_fill_shapes: bool,
    font: FontConfiguration,
    primary_highlighter: Highlighters,
    disable_notifications: bool,
    profile_startup: bool,
    no_window_decoration: bool,
    brush_smooth_history_size: usize,
    /// Brush post-stroke smoothing level (0..=8). 0 disables. 1–2
    /// are pure Chaikin corner-cutting (light smoothing). 3+ adds
    /// Ramer–Douglas–Peucker simplification
    /// before Chaikin's, with tolerance scaling per level — the
    /// upper end of the slider produces visibly stylized arcs.
    /// Default is 5: flattens free-hand mousing jitter while
    /// keeping intentional curvature.
    brush_post_smooth_iterations: usize,
    keybinds: Keybinds,
    zoom_factor: f32,
    pan_step_size: f32,
    text_move_length: f32,
    input_scale: f32,
    title: Option<String>,
    app_id: Option<String>,
    /// User preference: when true, clicking any existing annotation
    /// selects it regardless of which drawing tool is active (the
    /// Pointer grabs whatever was clicked). When false, a click only
    /// selects an annotation whose owning tool matches the active tool
    /// — otherwise it falls through and starts a new annotation.
    /// Default is true. Read by `PointerTool::should_pass_through_body_hit`.
    /// User preference: when true, pressing Esc on the canvas adds an
    /// `Action::Exit` to whatever `actions_on_escape` already runs, so
    /// the app closes. Default is false — Esc does nothing
    /// app-globally, leaving each tool's own Esc handling intact (Crop
    /// cancel, Text exit-edit, in-progress shape abort, etc.).
    close_on_esc: bool,
    /// Close the window after a copy-to-clipboard. The direct
    /// `close-on-copy` config key wins over the legacy `early-exit`
    /// fallback; CLI `--early-exit` wins over both for that invocation.
    close_on_copy: bool,
    /// Close the window after a save-to-file, with the same precedence as
    /// `close_on_copy` through `close-on-save`.
    close_on_save: bool,
    /// Whether to hide the default 10-color palette in the color
    /// picker. When true, the popover's left column disappears and
    /// the 1–9, 0 number-key shortcuts pick from the first column
    /// of saved-custom colors instead.
    hide_default_palette: bool,
    /// User preference: when true, in-session per-tool adjustments
    /// (size slider, fill toggle, highlighter opacity, brush
    /// smoothness) survive tool switches — only a fresh app launch
    /// resets to the saved defaults. When false (default), the saved
    /// defaults snap back on every tool switch. Spotlight darkness
    /// is unaffected — it's already global per-session.
    sticky_session_defaults: bool,
    /// Whether crop-driven content-size changes resize the editor window
    /// around the cropped content. This is config.toml-backed and defaults
    /// to true.
    resize_window_to_content_on_crop: bool,
    /// Clip annotations to the current image/crop instead of growing it.
    fixed_canvas: bool,
    /// Keyboard chord that toggles the layer panel. Parsed at the
    /// keypress site by `parse_shortcut`; format is e.g. "ctrl+l",
    /// "ctrl+shift+t", "f7". Unrecognised values silently fall back
    /// to "no binding" — the toolbar button still works.
    layer_panel_shortcut: String,
    /// Key pressed inside the scroll-capture overlay (before capture
    /// starts) to reselect the previously captured region. Same
    /// `parse_shortcut` format as `layer_panel_shortcut`. Empty
    /// disables the binding.
    scroll_capture_restore_region_shortcut: String,
    scroll_capture_test: Option<ScrollCaptureTest>,
    scroll_capture: bool,
    /// Command-line only: choose a region / window / the whole screen,
    /// then edit it. See `crate::region_capture`.
    capture: bool,
    auto_scroll_test: bool,
}

pub struct Keybinds {
    shortcuts: HashMap<char, Tools>,
}

const PREFERENCE_KEYBINDS: [(Tools, &str); 12] = [
    (Tools::Pointer, "pointer"),
    (Tools::Crop, "crop"),
    (Tools::Brush, "brush"),
    (Tools::Line, "line"),
    (Tools::Arrow, "arrow"),
    (Tools::Rectangle, "rectangle"),
    (Tools::Ellipse, "ellipse"),
    (Tools::Text, "text"),
    (Tools::Marker, "marker"),
    (Tools::Blur, "blur"),
    (Tools::Highlighter, "highlight"),
    (Tools::Spotlight, "spotlight"),
];

impl Keybinds {
    pub fn get_tool(&self, key: char) -> Option<Tools> {
        self.shortcuts.get(&key).copied()
    }

    pub fn shortcuts(&self) -> &HashMap<char, Tools> {
        &self.shortcuts
    }

    /// Update a single keybind, only if it is valid
    fn update_keybind(&mut self, key: Option<String>, tool: Tools) {
        if let Some(key_str) = key {
            // The Preferences dialog permits displacing a tool without
            // immediately assigning it another key. An explicit empty string
            // records that unbound state; an absent config key still means
            // "use the built-in/default binding" for hand-written partial
            // config files.
            if key_str.is_empty() {
                self.shortcuts.retain(|_, value| *value != tool);
                return;
            }
            if let Some(validated_key) = Self::validate_keybind(&key_str, tool) {
                self.shortcuts.retain(|_, v| *v != tool);
                self.shortcuts.insert(validated_key, tool);
            }
        }
    }

    /// A shortcut keybinding is only valid if it is one char
    fn validate_keybind(key: &str, tool: Tools) -> Option<char> {
        if let Some(key) = single_keybind_char(key) {
            Some(key)
        } else {
            eprintln!(
                "Warning: Invalid keybind: '{} = {}'. Keybinds must be single characters. Using default keybind instead.",
                tool, key
            );
            None
        }
    }

    /// Merge keybindings with default
    /// Only replaces defaults if they are set
    fn merge(&mut self, file_keybinds: KeybindsFile) {
        self.update_keybind(file_keybinds.pointer, Tools::Pointer);
        self.update_keybind(file_keybinds.crop, Tools::Crop);
        self.update_keybind(file_keybinds.brush, Tools::Brush);
        self.update_keybind(file_keybinds.line, Tools::Line);
        self.update_keybind(file_keybinds.arrow, Tools::Arrow);
        self.update_keybind(file_keybinds.rectangle, Tools::Rectangle);
        self.update_keybind(file_keybinds.ellipse, Tools::Ellipse);
        self.update_keybind(file_keybinds.text, Tools::Text);
        self.update_keybind(file_keybinds.marker, Tools::Marker);
        self.update_keybind(file_keybinds.blur, Tools::Blur);
        self.update_keybind(file_keybinds.highlight, Tools::Highlighter);
        self.update_keybind(file_keybinds.spotlight, Tools::Spotlight);
    }
}

fn single_keybind_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(key), None) => Some(key),
        _ => None,
    }
}

impl Default for Keybinds {
    fn default() -> Self {
        let mut shortcuts = HashMap::new();
        shortcuts.insert('v', Tools::Pointer);
        shortcuts.insert('x', Tools::Crop);
        shortcuts.insert('z', Tools::Brush);
        shortcuts.insert('s', Tools::Line);
        shortcuts.insert('a', Tools::Arrow);
        shortcuts.insert('r', Tools::Rectangle);
        shortcuts.insert('e', Tools::Ellipse);
        shortcuts.insert('t', Tools::Text);
        shortcuts.insert('c', Tools::Marker);
        shortcuts.insert('b', Tools::Blur);
        shortcuts.insert('w', Tools::Highlighter);
        shortcuts.insert('g', Tools::Spotlight);

        Self { shortcuts }
    }
}

#[derive(Default)]
pub struct FontConfiguration {
    family: Option<String>,
    style: Option<String>,
    fallback: Vec<String>,
}

impl FontConfiguration {
    pub fn family(&self) -> Option<&str> {
        self.family.as_deref()
    }

    pub fn style(&self) -> Option<&str> {
        self.style.as_deref()
    }

    pub fn fallback(&self) -> &[String] {
        &self.fallback
    }

    fn merge(&mut self, file_font: FontFile) {
        if let Some(v) = file_font.family {
            self.family = Some(v);
        }
        if let Some(v) = file_font.style {
            self.style = Some(v);
        }
        if let Some(v) = file_font.fallback {
            self.fallback = v
        }
    }
}

pub struct ColorPalette {
    palette: Vec<Color>,
    custom: Vec<Color>,
}

impl ColorPalette {
    pub fn palette(&self) -> &[Color] {
        &self.palette
    }

    fn merge(&mut self, file_palette: ColorPaletteFile) {
        if let Some(v) = file_palette.palette {
            self.palette = v.into_iter().map(Color::from).collect();
        }
        if let Some(v) = file_palette.custom {
            self.custom = v.into_iter().map(Color::from).collect();
        }
    }
}

// remain compatible with old config with fullscreen=true/false
#[derive(Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "kebab-case")]
enum FullscreenCompat {
    Bool(bool),
    Mode(Fullscreen),
}

fn de_fullscreen_mode<'de, D>(d: D) -> Result<Option<Fullscreen>, D::Error>
where
    D: Deserializer<'de>,
{
    match FullscreenCompat::deserialize(d)? {
        FullscreenCompat::Bool(true) => Ok(Some(Fullscreen::CurrentScreen)),
        FullscreenCompat::Bool(false) => Ok(None),
        FullscreenCompat::Mode(m) => Ok(Some(m)),
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Hand the finished image to a pinned window and step the editor
    /// aside. See `crate::pin`.
    Pin,
    SaveToClipboard,
    SaveToFile,
    SaveToFileAs,
    CopyFilepathToClipboard,
    Exit,
}

impl From<CommandLineAction> for Action {
    fn from(action: CommandLineAction) -> Self {
        match action {
            CommandLineAction::SaveToClipboard => Self::SaveToClipboard,
            CommandLineAction::SaveToFile => Self::SaveToFile,
            CommandLineAction::SaveToFileAs => Self::SaveToFileAs,
            CommandLineAction::CopyFilepathToClipboard => Self::CopyFilepathToClipboard,
            CommandLineAction::Exit => Self::Exit,
        }
    }
}

/// Search `$PATH` for the `wl-copy` binary. Returns true if found.
/// Standalone helper so the auto-fallback in `merge` stays a
/// single readable conditional.
fn wl_copy_in_path() -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join("wl-copy").is_file())
}

fn replace_value_preserving_decor(item: &mut Item, replacement: Value) {
    if let Some(existing) = item.as_value_mut() {
        let decor = existing.decor().clone();
        let mut replacement = replacement;
        *replacement.decor_mut() = decor;
        *existing = replacement;
    } else {
        *item = Item::Value(replacement);
    }
}

fn update_table_value(table: &mut dyn TableLike, key: &str, replacement: Value) {
    if let Some(item) = table.get_mut(key) {
        replace_value_preserving_decor(item, replacement);
    } else {
        table.insert(key, Item::Value(replacement));
    }
}

fn update_document(
    document: &mut DocumentMut,
    updates: ConfigUpdates,
) -> Result<(), ConfigurationWriteError> {
    if !updates.general.is_empty() {
        if document.get("general").is_none() {
            document["general"] = Item::Table(Table::new());
        }
        let general = document["general"]
            .as_table_like_mut()
            .ok_or(ConfigurationWriteError::SectionIsNotATable("general"))?;
        for (key, value) in updates.general {
            update_table_value(general, key, value.into_toml_value());
        }
    }

    if !updates.keybinds.is_empty() {
        if document.get("keybinds").is_none() {
            document["keybinds"] = Item::Table(Table::new());
        }
        let keybinds = document["keybinds"]
            .as_table_like_mut()
            .ok_or(ConfigurationWriteError::SectionIsNotATable("keybinds"))?;
        for (key, value) in updates.keybinds {
            update_table_value(keybinds, key, Value::from(value));
        }
    }
    Ok(())
}

static CONFIG_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let original_permissions = fs::metadata(path).ok().map(|meta| meta.permissions());
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");

    for _ in 0..100 {
        let id = CONFIG_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(
            ".{file_name}.tensaku-tmp-{}-{id}",
            std::process::id()
        ));
        let mut temporary = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };

        let write_result = (|| {
            if let Some(permissions) = original_permissions.clone() {
                temporary.set_permissions(permissions)?;
            }
            temporary.write_all(contents)?;
            temporary.sync_all()?;
            drop(temporary);
            fs::rename(&temporary_path, path)
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        return write_result;
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a temporary config file",
    ))
}

fn resolve_config_write_path(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            // canonicalize handles existing relative targets, nested symlinks,
            // and loop detection. A dangling link cannot be canonicalized, so
            // resolve exactly its one declared target; atomic_write will create
            // the target's parent if needed without replacing the link itself.
            match fs::canonicalize(path) {
                Ok(target) => Ok(target),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let target = fs::read_link(path)?;
                    if target.is_absolute() {
                        Ok(target)
                    } else {
                        Ok(path
                            .parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .unwrap_or_else(|| Path::new("."))
                            .join(target))
                    }
                }
                Err(error) => Err(error),
            }
        }
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(error),
    }
}

/// Apply a set of config changes as one comment-preserving, atomic
/// transaction. The file is read fresh for each write so unrelated edits made
/// since startup survive.
fn write_config_updates(
    path: &Path,
    updates: ConfigUpdates,
) -> Result<(), ConfigurationWriteError> {
    if updates.is_empty() {
        return Ok(());
    }
    let write_path = resolve_config_write_path(path)?;
    let content = match fs::read_to_string(&write_path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut document = if content.trim().is_empty() {
        DocumentMut::new()
    } else {
        content.parse::<DocumentMut>()?
    };
    update_document(&mut document, updates)?;
    atomic_write(&write_path, document.to_string().as_bytes())?;
    Ok(())
}

impl Configuration {
    pub fn load() {
        // parse commandline options and exit if error
        let command_line = match CommandLine::try_parse() {
            Ok(cmd) => cmd,
            Err(e) => e.exit(),
        };

        // Resolve the path once and keep it with the live configuration.
        // Preferences must write to the same custom `--config` file that was
        // read, rather than silently falling back to the XDG default.
        let config_file_path = ConfigurationFile::active_path(&command_line.config);

        // read configuration file and exit on error
        let mut file = match ConfigurationFile::try_read(&command_line.config) {
            Ok(c) => c,
            Err(ConfigurationFileError::ReadFile(e)) if e.kind() == io::ErrorKind::NotFound => {
                eprintln!("config file not found");
                None
            }
            Err(e) => {
                eprintln!("Error reading config file: {e}");

                // swallow broken pipes
                let _ = std::io::stdout().lock().flush();
                let _ = std::io::stderr().lock().flush();

                // exit
                std::process::exit(3);
            }
        };

        // Migrate the old state.toml-backed Preferences as one transaction.
        // Missing config keys inherit legacy values; explicitly configured
        // keys win. If the config write fails, the in-memory merge below still
        // uses the legacy values for this run and state remains intact so the
        // migration can retry next launch.
        if !is_utility_invocation(&command_line) {
            let legacy = crate::state::load_legacy_preferences();
            if !legacy.is_empty() {
                let updates = apply_legacy_preferences(
                    file.get_or_insert_with(ConfigurationFile::default),
                    &legacy,
                );
                let migration = if updates.is_empty() {
                    Ok(())
                } else {
                    config_file_path
                        .as_deref()
                        .ok_or(ConfigurationWriteError::MissingPath)
                        .and_then(|path| write_config_updates(path, updates))
                };
                match migration {
                    Ok(()) => {
                        if let Err(error) = crate::state::clear_legacy_preferences() {
                            eprintln!(
                                "Warning: config preferences migrated, but legacy state could not be cleared: {error}"
                            );
                        }
                    }
                    Err(error) => eprintln!(
                        "Warning: could not migrate Preferences to config.toml; using legacy state for this run: {error}"
                    ),
                }
            }
        }

        {
            let mut config = APP_CONFIG.write();
            config.config_file_path = config_file_path;
            config.merge(file, command_line);
        }

        // Brush post-stroke smoothing iterations: if the user has
        // saved a default via the slider's right-click menu, fold
        // that on top of the config / built-in default so the brush
        // tool sees a single live value.
        if let Some(v) = crate::state::load_brush_post_smooth_iterations() {
            APP_CONFIG.write().set_brush_post_smooth_iterations(v);
        }
    }
    fn merge_general(&mut self, general: ConfigurationFileGeneral) {
        // `early-exit` predates the per-action Preferences. Treat it as a
        // compatibility fallback; explicit direct keys in the same config
        // take precedence even when they are false.
        let close_on_copy = general.close_on_copy.or(general.early_exit);
        let close_on_save = general.close_on_save.or(general.early_exit);
        if let Some(v) = general.fullscreen {
            self.fullscreen = Some(v);
        }
        if let Some(v) = general.resize {
            self.resize = Some(v);
        }
        if let Some(v) = general.floating_hack {
            self.floating_hack = v;
        }
        if let Some(v) = general.early_exit {
            self.early_exit = v;
        }
        if let Some(v) = general.early_exit_save_as {
            self.early_exit_save_as = v;
        }
        if let Some(v) = general.corner_roundness {
            self.corner_roundness = v;
        }
        if let Some(v) = general.initial_tool {
            self.initial_tool = v;
        }
        if let Some(v) = general.copy_command {
            self.copy_command = Some(v);
        }
        if let Some(v) = general.output_filename {
            self.output_filename = Some(v);
        }
        if let Some(v) = general.annotation_size_factor {
            self.annotation_size_factor = v;
            self.annotation_size_factor_is_explicit = true;
        }
        if let Some(v) = general.save_after_copy {
            self.save_after_copy = v;
        }
        if let Some(v) = general.auto_copy {
            self.auto_copy = v;
        }
        if let Some(v) = general.actions_on_enter {
            self.actions_on_enter = v;
        }
        if let Some(v) = general.actions_on_escape {
            self.actions_on_escape = v;
        }
        if let Some(v) = general.actions_on_right_click {
            self.actions_on_right_click = v;
        }
        if let Some(v) = general.default_hide_toolbars {
            self.default_hide_toolbars = v;
        }
        if let Some(v) = general.focus_toggles_toolbars {
            self.focus_toggles_toolbars = v;
        }
        if let Some(v) = general.default_fill_shapes {
            self.default_fill_shapes = v;
        }
        if let Some(v) = general.primary_highlighter {
            self.primary_highlighter = v;
        }
        if let Some(v) = general.disable_notifications {
            self.disable_notifications = v;
        }
        if let Some(v) = general.no_window_decoration {
            self.no_window_decoration = v;
        }
        if let Some(v) = general.brush_smooth_history_size {
            self.brush_smooth_history_size = v;
        }
        if let Some(v) = general.brush_post_smooth_iterations {
            self.brush_post_smooth_iterations = v;
        }
        if let Some(v) = general.zoom_factor {
            self.zoom_factor = v;
        }
        if let Some(v) = general.pan_step_size {
            self.pan_step_size = v;
        }
        if let Some(v) = general.text_move_length {
            self.text_move_length = v;
        }
        if let Some(v) = general.input_scale {
            self.input_scale = v;
        }
        if let Some(v) = general.title {
            self.title = Some(v);
        }
        if let Some(v) = general.app_id {
            self.app_id = Some(v);
        }
        if let Some(v) = general.layer_panel_shortcut {
            self.layer_panel_shortcut = v;
        }
        if let Some(v) = general.scroll_capture_restore_region_shortcut {
            self.scroll_capture_restore_region_shortcut = v;
        }
        if let Some(v) = general.resize_window_to_content_on_crop {
            self.resize_window_to_content_on_crop = v;
        }
        if let Some(v) = general.fixed_canvas {
            self.fixed_canvas = v;
        }
        if let Some(v) = general.close_on_esc {
            self.close_on_esc = v;
        }
        if let Some(v) = close_on_copy {
            self.close_on_copy = v;
        }
        if let Some(v) = close_on_save {
            self.close_on_save = v;
        }
        if let Some(v) = general.hide_default_palette {
            self.hide_default_palette = v;
        }
        if let Some(v) = general.sticky_session_defaults {
            self.sticky_session_defaults = v;
        }
        // --- deprecated options ---
        if let Some(v) = general.right_click_copy
            && v
            && !self
                .actions_on_right_click
                .contains(&Action::SaveToClipboard)
        {
            self.actions_on_right_click
                .insert(0, Action::SaveToClipboard);
        }
        if let Some(v) = general.action_on_enter {
            self.actions_on_enter.insert(0, v);
        }
        // ---
    }
    fn merge(&mut self, file: Option<ConfigurationFile>, command_line: CommandLine) {
        self.input_filename = command_line.filename.or(command_line.input);

        // overwrite with all specified values from config file
        if let Some(file) = file {
            if let Some(general) = file.general {
                self.merge_general(general);
            }
            if let Some(v) = file.color_palette {
                self.color_palette.merge(v);
            }
            if let Some(v) = file.font {
                self.font.merge(v);
            }
            if let Some(v) = file.keybinds {
                self.keybinds.merge(v);
            }
        }

        // overwrite with all specified values from command line
        if let Some(v) = command_line.fullscreen {
            self.fullscreen = Some(v);
        }
        if let Some(v) = command_line.resize {
            self.resize = Some(v);
        }
        if command_line.floating_hack {
            self.floating_hack = command_line.floating_hack;
        }
        if command_line.man {
            self.man = command_line.man;
        }
        if command_line.license {
            self.license = command_line.license;
        }
        if command_line.install_desktop {
            self.install_desktop = true;
        }
        if command_line.doctor {
            self.doctor = true;
        }
        if command_line.install_omarchy_wrapper {
            self.install_omarchy_wrapper = true;
        }
        if command_line.wire_omarchy {
            self.wire_omarchy = true;
        }
        if let Some(key) = &command_line.wire_capture_key {
            self.wire_capture_key = Some(key.clone());
        }
        if command_line.early_exit {
            self.early_exit = command_line.early_exit;
            // CLI has higher precedence than config, including explicit
            // per-action false values.
            self.close_on_copy = true;
            self.close_on_save = true;
        }
        if command_line.early_exit_save_as {
            self.early_exit_save_as = command_line.early_exit_save_as;
        }
        if let Some(v) = command_line.corner_roundness {
            self.corner_roundness = v;
        }
        if command_line.default_hide_toolbars {
            self.default_hide_toolbars = command_line.default_hide_toolbars;
        }
        if command_line.focus_toggles_toolbars {
            self.focus_toggles_toolbars = command_line.focus_toggles_toolbars
        }
        if command_line.default_fill_shapes {
            self.default_fill_shapes = command_line.default_fill_shapes;
        }
        if let Some(v) = command_line.initial_tool {
            self.initial_tool = v.into();
        }
        if let Some(v) = command_line.copy_command {
            self.copy_command = Some(v);
        }
        if let Some(v) = command_line.output_filename {
            self.output_filename = Some(v);
        }
        if let Some(v) = command_line.annotation_size_factor {
            self.annotation_size_factor = v;
            self.annotation_size_factor_is_explicit = true;
        }
        if command_line.save_after_copy {
            self.save_after_copy = command_line.save_after_copy;
        }
        if command_line.auto_copy {
            self.auto_copy = command_line.auto_copy;
        }
        if let Some(v) = command_line.actions_on_enter {
            self.actions_on_enter = v.iter().cloned().map(Into::into).collect();
        }
        if let Some(v) = command_line.actions_on_escape {
            self.actions_on_escape = v.iter().cloned().map(Into::into).collect();
        }
        if let Some(v) = command_line.actions_on_right_click {
            self.actions_on_right_click = v.iter().cloned().map(Into::into).collect();
        }
        if let Some(v) = command_line.font_family {
            self.font.family = Some(v);
        }
        if let Some(v) = command_line.font_style {
            self.font.style = Some(v);
        }
        if let Some(v) = command_line.primary_highlighter {
            self.primary_highlighter = v.into();
        }
        if command_line.disable_notifications {
            self.disable_notifications = command_line.disable_notifications;
        }
        if command_line.profile_startup {
            self.profile_startup = command_line.profile_startup;
        }
        if command_line.no_window_decoration {
            self.no_window_decoration = command_line.no_window_decoration;
        }
        if let Some(v) = command_line.brush_smooth_history_size {
            self.brush_smooth_history_size = v;
        }
        if let Some(v) = command_line.brush_post_smooth_iterations {
            self.brush_post_smooth_iterations = v;
        }
        if let Some(v) = command_line.zoom_factor {
            self.zoom_factor = v;
        }
        if let Some(v) = command_line.pan_step_size {
            self.pan_step_size = v;
        }
        if let Some(v) = command_line.text_move_length {
            self.text_move_length = v;
        }
        if let Some(v) = command_line.input_scale {
            self.input_scale = v;
        }
        if let Some(v) = command_line.title {
            self.title = Some(v);
        }
        if let Some(v) = command_line.app_id {
            self.app_id = Some(v);
        }
        if let Some(v) = command_line.scroll_capture_test {
            self.scroll_capture_test = Some(v);
        }
        if command_line.capture {
            self.capture = true;
        }
        if command_line.scroll_capture {
            self.scroll_capture = true;
        }
        if command_line.auto_scroll_test {
            self.auto_scroll_test = true;
        }

        // --- deprecated options ---
        if command_line.right_click_copy
            && !self
                .actions_on_right_click
                .contains(&Action::SaveToClipboard)
        {
            self.actions_on_right_click
                .insert(0, Action::SaveToClipboard);
        }
        if let Some(v) = command_line.action_on_enter {
            self.actions_on_enter.insert(0, v.into());
        }
        // ---

        // Wayland clipboard persistence fallback. GTK4 sets clipboard
        // contents via a `wl_data_source` that satty itself must
        // serve from — when satty exits the offer dies and the
        // pasted content disappears (unless a clipboard manager is
        // running and grabbed it in time, which isn't guaranteed).
        // `wl-copy` forks a tiny daemon that holds the data
        // independently, so the clipboard survives satty's exit.
        // Only auto-applied when (a) we're on Wayland, (b) the user
        // hasn't already set a copy_command, and (c) wl-copy is
        // somewhere on PATH. Image MIME type is set explicitly so
        // paste targets see image/png.
        if self.copy_command.is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_some()
            && wl_copy_in_path()
        {
            self.copy_command = Some("wl-copy --type image/png".to_string());
        }
    }

    pub fn man(&self) -> bool {
        self.man
    }

    pub fn license(&self) -> bool {
        self.license
    }

    pub fn install_desktop(&self) -> bool {
        self.install_desktop
    }

    pub fn doctor(&self) -> bool {
        self.doctor
    }

    pub fn install_omarchy_wrapper(&self) -> bool {
        self.install_omarchy_wrapper
    }

    pub fn wire_omarchy(&self) -> bool {
        self.wire_omarchy
    }

    pub fn wire_capture_key(&self) -> Option<&str> {
        self.wire_capture_key.as_deref()
    }

    pub fn early_exit_save_as(&self) -> bool {
        self.early_exit_save_as
    }

    pub fn corner_roundness(&self) -> f32 {
        self.corner_roundness
    }

    pub fn initial_tool(&self) -> Tools {
        self.initial_tool
    }

    pub fn copy_command(&self) -> Option<&String> {
        self.copy_command.as_ref()
    }

    pub fn fullscreen(&self) -> Option<Fullscreen> {
        self.fullscreen
    }

    pub fn resize(&self) -> Option<Resize> {
        self.resize
    }

    pub fn floating_hack(&self) -> bool {
        self.floating_hack
    }

    pub fn output_filename(&self) -> Option<&String> {
        self.output_filename.as_ref()
    }

    pub fn input_filename(&self) -> &str {
        match self.input_filename {
            Some(ref v) => v,
            None => "",
        }
    }

    pub fn source_format(&self) -> crate::image_export::ImageFormat {
        self.source_format
    }

    pub(crate) fn set_source_format(&mut self, format: crate::image_export::ImageFormat) {
        self.source_format = format;
    }

    pub(crate) fn set_input_filename(&mut self, filename: String) {
        self.input_filename = Some(filename);
    }

    pub fn annotation_size_factor(&self) -> f32 {
        self.annotation_size_factor
    }

    pub fn annotation_size_factor_is_explicit(&self) -> bool {
        self.annotation_size_factor_is_explicit
    }

    pub(crate) fn save_annotation_size_factor(
        &mut self,
        value: f32,
    ) -> Result<(), ConfigurationWriteError> {
        self.write_updates(ConfigUpdates {
            general: vec![("annotation-size-factor", GeneralValue::Float(value))],
            keybinds: Vec::new(),
        })?;
        self.annotation_size_factor = value;
        self.annotation_size_factor_is_explicit = true;
        Ok(())
    }

    pub fn save_after_copy(&self) -> bool {
        self.save_after_copy
    }

    pub fn auto_copy(&self) -> bool {
        self.auto_copy
    }

    pub fn actions_on_enter(&self) -> Vec<Action> {
        self.actions_on_enter.clone()
    }

    pub fn actions_on_escape(&self) -> Vec<Action> {
        self.actions_on_escape.clone()
    }

    pub fn actions_on_right_click(&self) -> Vec<Action> {
        self.actions_on_right_click.clone()
    }

    pub fn color_palette(&self) -> &ColorPalette {
        &self.color_palette
    }

    pub fn default_hide_toolbars(&self) -> bool {
        self.default_hide_toolbars
    }

    pub fn layer_panel_shortcut(&self) -> &str {
        &self.layer_panel_shortcut
    }

    pub fn focus_toggles_toolbars(&self) -> bool {
        self.focus_toggles_toolbars
    }

    pub fn default_fill_shapes(&self) -> bool {
        self.default_fill_shapes
    }

    pub fn primary_highlighter(&self) -> Highlighters {
        self.primary_highlighter
    }

    pub fn disable_notifications(&self) -> bool {
        self.disable_notifications
    }

    pub fn profile_startup(&self) -> bool {
        self.profile_startup
    }

    pub fn no_window_decoration(&self) -> bool {
        self.no_window_decoration
    }

    pub fn font(&self) -> &FontConfiguration {
        &self.font
    }

    pub fn brush_smooth_history_size(&self) -> usize {
        self.brush_smooth_history_size
    }

    pub fn brush_post_smooth_iterations(&self) -> usize {
        self.brush_post_smooth_iterations
    }

    /// Live in-memory override for the brush post-smoothing iteration
    /// count. The brush tool reads this on every EndDrag, so updates
    /// take effect on the next stroke without restart.
    pub fn set_brush_post_smooth_iterations(&mut self, value: usize) {
        self.brush_post_smooth_iterations = value;
    }

    pub fn keybinds(&self) -> &Keybinds {
        &self.keybinds
    }

    pub(crate) fn save_keybinds(
        &mut self,
        shortcuts: HashMap<char, Tools>,
    ) -> Result<(), ConfigurationWriteError> {
        let keybinds = PREFERENCE_KEYBINDS
            .into_iter()
            .map(|(tool, config_key)| {
                let key = shortcuts
                    .iter()
                    .find_map(|(key, bound_tool)| (*bound_tool == tool).then(|| key.to_string()))
                    .unwrap_or_default();
                (config_key, key)
            })
            .collect();
        self.write_updates(ConfigUpdates {
            general: Vec::new(),
            keybinds,
        })?;
        self.keybinds.shortcuts = shortcuts;
        Ok(())
    }

    pub fn close_on_esc(&self) -> bool {
        self.close_on_esc
    }

    pub(crate) fn save_close_on_esc(&mut self, value: bool) -> Result<(), ConfigurationWriteError> {
        self.write_general_bool("close-on-esc", value)?;
        self.close_on_esc = value;
        Ok(())
    }

    pub fn close_on_copy(&self) -> bool {
        self.close_on_copy
    }

    pub(crate) fn save_close_on_copy(
        &mut self,
        value: bool,
    ) -> Result<(), ConfigurationWriteError> {
        self.write_general_bool("close-on-copy", value)?;
        self.close_on_copy = value;
        Ok(())
    }

    pub fn close_on_save(&self) -> bool {
        self.close_on_save
    }

    pub(crate) fn save_close_on_save(
        &mut self,
        value: bool,
    ) -> Result<(), ConfigurationWriteError> {
        self.write_general_bool("close-on-save", value)?;
        self.close_on_save = value;
        Ok(())
    }

    pub fn hide_default_palette(&self) -> bool {
        self.hide_default_palette
    }

    pub(crate) fn save_hide_default_palette(
        &mut self,
        value: bool,
    ) -> Result<(), ConfigurationWriteError> {
        self.write_general_bool("hide-default-palette", value)?;
        self.hide_default_palette = value;
        Ok(())
    }

    pub fn sticky_session_defaults(&self) -> bool {
        self.sticky_session_defaults
    }

    pub(crate) fn save_sticky_session_defaults(
        &mut self,
        value: bool,
    ) -> Result<(), ConfigurationWriteError> {
        self.write_general_bool("sticky-session-defaults", value)?;
        self.sticky_session_defaults = value;
        Ok(())
    }

    pub fn resize_window_to_content_on_crop(&self) -> bool {
        self.resize_window_to_content_on_crop
    }

    pub fn fixed_canvas(&self) -> bool {
        self.fixed_canvas
    }

    pub(crate) fn save_fixed_canvas(&mut self, value: bool) -> Result<(), ConfigurationWriteError> {
        self.write_general_bool("fixed-canvas", value)?;
        self.fixed_canvas = value;
        Ok(())
    }

    pub(crate) fn save_resize_window_to_content_on_crop(
        &mut self,
        value: bool,
    ) -> Result<(), ConfigurationWriteError> {
        self.write_general_bool("resize-window-to-content-on-crop", value)?;
        self.resize_window_to_content_on_crop = value;
        Ok(())
    }

    fn write_updates(&self, updates: ConfigUpdates) -> Result<(), ConfigurationWriteError> {
        let path = self
            .config_file_path
            .as_deref()
            .ok_or(ConfigurationWriteError::MissingPath)?;
        write_config_updates(path, updates)
    }

    fn write_general_bool(
        &self,
        key: &'static str,
        value: bool,
    ) -> Result<(), ConfigurationWriteError> {
        self.write_updates(ConfigUpdates {
            general: vec![(key, GeneralValue::Bool(value))],
            keybinds: Vec::new(),
        })
    }

    pub fn scroll_capture_restore_region_shortcut(&self) -> &str {
        &self.scroll_capture_restore_region_shortcut
    }

    pub(crate) fn save_scroll_capture_restore_region_shortcut(
        &mut self,
        value: String,
    ) -> Result<(), ConfigurationWriteError> {
        self.write_updates(ConfigUpdates {
            general: vec![(
                "scroll-capture-restore-region-shortcut",
                GeneralValue::Str(value.clone()),
            )],
            keybinds: Vec::new(),
        })?;
        self.scroll_capture_restore_region_shortcut = value;
        Ok(())
    }

    pub fn zoom_factor(&self) -> f32 {
        self.zoom_factor
    }

    pub fn pan_step_size(&self) -> f32 {
        self.pan_step_size
    }

    pub fn text_move_length(&self) -> f32 {
        self.text_move_length
    }

    pub fn input_scale(&self) -> f32 {
        self.input_scale
    }

    pub fn title(&self) -> Option<&String> {
        self.title.as_ref()
    }

    pub fn app_id(&self) -> Option<&String> {
        self.app_id.as_ref()
    }

    pub fn scroll_capture_test(&self) -> Option<&ScrollCaptureTest> {
        self.scroll_capture_test.as_ref()
    }

    pub fn scroll_capture(&self) -> bool {
        self.scroll_capture
    }

    pub fn capture(&self) -> bool {
        self.capture
    }

    pub fn auto_scroll_test(&self) -> bool {
        self.auto_scroll_test
    }
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            config_file_path: None,
            man: false,
            license: false,
            install_desktop: false,
            doctor: false,
            install_omarchy_wrapper: false,
            wire_omarchy: false,
            wire_capture_key: None,
            input_filename: Some(String::new()),
            source_format: crate::image_export::ImageFormat::default(),
            output_filename: None,
            fullscreen: None,
            resize: None,
            floating_hack: false,
            early_exit: false,
            early_exit_save_as: false,
            corner_roundness: 12.0,
            initial_tool: Tools::Pointer,
            copy_command: None,
            annotation_size_factor: 1.0,
            annotation_size_factor_is_explicit: false,
            save_after_copy: false,
            auto_copy: false,
            actions_on_enter: vec![],
            // Default to NO automatic Exit on Esc — the "Close on Esc"
            // config-backed preference gates that
            // behavior so a fresh install doesn't kill the window on
            // an accidental Esc. Users who explicitly set
            // `actions_on_escape = ["exit", ...]` in their config.toml
            // still get the original behavior (config wins).
            actions_on_escape: vec![],
            actions_on_right_click: vec![],
            color_palette: ColorPalette::default(),
            default_hide_toolbars: false,
            focus_toggles_toolbars: false,
            default_fill_shapes: false,
            font: FontConfiguration::default(),
            primary_highlighter: Highlighters::Block,
            disable_notifications: false,
            profile_startup: false,
            no_window_decoration: false,
            brush_smooth_history_size: 0, // default to 0, no history
            // Level 5 = 2 Chaikin passes + RDP at ~5px tolerance,
            // which smooths typical free-hand strokes aggressively
            // enough to flatten the inevitable mousing jitter while
            // still preserving intentional curvature. Picked as the
            // built-in after side-by-side comparisons at levels 2–6.
            brush_post_smooth_iterations: 5,
            keybinds: Keybinds::default(),
            zoom_factor: 1.1,
            pan_step_size: 50.,
            text_move_length: 50.0,
            input_scale: 1.0,
            title: None,
            app_id: None,
            // Default matches the historical Preferences fallback.
            close_on_esc: false,
            close_on_copy: false,
            close_on_save: false,
            hide_default_palette: false,
            sticky_session_defaults: false,
            resize_window_to_content_on_crop: true,
            fixed_canvas: false,
            layer_panel_shortcut: "ctrl+l".into(),
            scroll_capture_restore_region_shortcut: "r".into(),
            scroll_capture_test: None,
            scroll_capture: false,
            capture: false,
            auto_scroll_test: false,
        }
    }
}

impl Default for ColorPalette {
    fn default() -> Self {
        // 10-color curated palette. Order matches the keyboard
        // shortcuts (1..9, 0) so users can pick any palette color from
        // the keyboard without thinking. Red leads (the most-reached-for
        // annotation color); black + white anchor the tail since they
        // are picked least often.
        Self {
            palette: vec![
                Color::red(),
                Color::orange(),
                Color::yellow(),
                Color::green(),
                Color::teal(),
                Color::royal_blue(),
                Color::purple(),
                Color::pink(),
                Color::black(),
                Color::white(),
            ],
            custom: vec![],
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct ConfigurationFile {
    general: Option<ConfigurationFileGeneral>,
    color_palette: Option<ColorPaletteFile>,
    font: Option<FontFile>,
    keybinds: Option<KeybindsFile>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct KeybindsFile {
    pointer: Option<String>,
    crop: Option<String>,
    brush: Option<String>,
    line: Option<String>,
    arrow: Option<String>,
    rectangle: Option<String>,
    ellipse: Option<String>,
    text: Option<String>,
    marker: Option<String>,
    blur: Option<String>,
    highlight: Option<String>,
    spotlight: Option<String>,
}

impl KeybindsFile {
    fn get(&self, tool: Tools) -> Option<&String> {
        match tool {
            Tools::Pointer => self.pointer.as_ref(),
            Tools::Crop => self.crop.as_ref(),
            Tools::Brush => self.brush.as_ref(),
            Tools::Line => self.line.as_ref(),
            Tools::Arrow => self.arrow.as_ref(),
            Tools::Rectangle => self.rectangle.as_ref(),
            Tools::Ellipse => self.ellipse.as_ref(),
            Tools::Text => self.text.as_ref(),
            Tools::Marker => self.marker.as_ref(),
            Tools::Blur => self.blur.as_ref(),
            Tools::Highlighter => self.highlight.as_ref(),
            Tools::Spotlight => self.spotlight.as_ref(),
        }
    }

    fn set(&mut self, tool: Tools, key: String) {
        *match tool {
            Tools::Pointer => &mut self.pointer,
            Tools::Crop => &mut self.crop,
            Tools::Brush => &mut self.brush,
            Tools::Line => &mut self.line,
            Tools::Arrow => &mut self.arrow,
            Tools::Rectangle => &mut self.rectangle,
            Tools::Ellipse => &mut self.ellipse,
            Tools::Text => &mut self.text,
            Tools::Marker => &mut self.marker,
            Tools::Blur => &mut self.blur,
            Tools::Highlighter => &mut self.highlight,
            Tools::Spotlight => &mut self.spotlight,
        } = Some(key);
    }
}

fn migrate_general_value<T: Copy>(
    slot: &mut Option<T>,
    legacy: Option<T>,
    key: &'static str,
    into_value: impl FnOnce(T) -> GeneralValue,
    updates: &mut ConfigUpdates,
) {
    if slot.is_none()
        && let Some(value) = legacy
    {
        *slot = Some(value);
        updates.general.push((key, into_value(value)));
    }
}

fn apply_legacy_preferences(
    file: &mut ConfigurationFile,
    legacy: &crate::state::LegacyPreferences,
) -> ConfigUpdates {
    let mut updates = ConfigUpdates::default();
    let general = file
        .general
        .get_or_insert_with(ConfigurationFileGeneral::default);
    migrate_general_value(
        &mut general.annotation_size_factor,
        legacy.annotation_size_factor,
        "annotation-size-factor",
        GeneralValue::Float,
        &mut updates,
    );
    migrate_general_value(
        &mut general.close_on_esc,
        legacy.close_on_esc,
        "close-on-esc",
        GeneralValue::Bool,
        &mut updates,
    );
    migrate_general_value(
        &mut general.close_on_copy,
        legacy.close_on_copy,
        "close-on-copy",
        GeneralValue::Bool,
        &mut updates,
    );
    migrate_general_value(
        &mut general.close_on_save,
        legacy.close_on_save,
        "close-on-save",
        GeneralValue::Bool,
        &mut updates,
    );
    migrate_general_value(
        &mut general.hide_default_palette,
        legacy.hide_default_palette,
        "hide-default-palette",
        GeneralValue::Bool,
        &mut updates,
    );
    migrate_general_value(
        &mut general.sticky_session_defaults,
        legacy.sticky_session_defaults,
        "sticky-session-defaults",
        GeneralValue::Bool,
        &mut updates,
    );
    migrate_general_value(
        &mut general.park_pointer_during_manual_scroll_capture,
        legacy.park_pointer_during_manual_scroll_capture,
        "park-pointer-during-manual-scroll-capture",
        GeneralValue::Bool,
        &mut updates,
    );
    migrate_general_value(
        &mut general.resize_window_to_content_on_crop,
        legacy.keep_window_size_on_crop.map(|keep| !keep),
        "resize-window-to-content-on-crop",
        GeneralValue::Bool,
        &mut updates,
    );

    if let Some(legacy_keybinds) = legacy.keybinds.as_ref() {
        let keybinds = file.keybinds.get_or_insert_with(KeybindsFile::default);
        // Reserve every valid character explicitly present in config before
        // filling any missing tools. Otherwise a legacy binding processed
        // later in `Keybinds::merge` could overwrite an earlier explicit
        // config binding that uses the same character.
        let explicit_config_keys: HashSet<char> = PREFERENCE_KEYBINDS
            .iter()
            .filter_map(|(tool, _)| keybinds.get(*tool))
            .filter_map(|key| single_keybind_char(key))
            .collect();
        for (tool, config_key) in PREFERENCE_KEYBINDS {
            if keybinds.get(tool).is_none() {
                // A missing tool in the legacy whole-map meant it was
                // intentionally unbound. Persist an empty string so reload
                // does not resurrect that tool's built-in binding. A legacy
                // character already claimed by explicit config is likewise
                // migrated as unbound so explicit config always wins,
                // independent of the fixed tool merge order.
                let key = legacy_keybinds
                    .get(&tool)
                    .filter(|key| {
                        single_keybind_char(key)
                            .is_none_or(|key| !explicit_config_keys.contains(&key))
                    })
                    .cloned()
                    .unwrap_or_default();
                keybinds.set(tool, key.clone());
                updates.keybinds.push((config_key, key));
            }
        }
    }

    updates
}

fn is_utility_invocation(command_line: &CommandLine) -> bool {
    command_line.man
        || command_line.license
        || command_line.install_desktop
        || command_line.doctor
        || command_line.install_omarchy_wrapper
        || command_line.wire_omarchy
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct FontFile {
    family: Option<String>,
    style: Option<String>,
    fallback: Option<Vec<String>>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct ConfigurationFileGeneral {
    #[serde(deserialize_with = "de_fullscreen_mode", default)]
    fullscreen: Option<Fullscreen>,
    resize: Option<Resize>,
    floating_hack: Option<bool>,
    early_exit: Option<bool>,
    early_exit_save_as: Option<bool>,
    corner_roundness: Option<f32>,
    initial_tool: Option<Tools>,
    copy_command: Option<String>,
    annotation_size_factor: Option<f32>,
    save_after_copy: Option<bool>,
    auto_copy: Option<bool>,
    output_filename: Option<String>,
    actions_on_enter: Option<Vec<Action>>,
    actions_on_escape: Option<Vec<Action>>,
    actions_on_right_click: Option<Vec<Action>>,
    default_hide_toolbars: Option<bool>,
    focus_toggles_toolbars: Option<bool>,
    default_fill_shapes: Option<bool>,
    primary_highlighter: Option<Highlighters>,
    disable_notifications: Option<bool>,
    no_window_decoration: Option<bool>,
    brush_smooth_history_size: Option<usize>,
    brush_post_smooth_iterations: Option<usize>,
    zoom_factor: Option<f32>,
    pan_step_size: Option<f32>,
    text_move_length: Option<f32>,
    input_scale: Option<f32>,
    title: Option<String>,
    app_id: Option<String>,
    layer_panel_shortcut: Option<String>,
    scroll_capture_restore_region_shortcut: Option<String>,
    /// Removed. Selecting an annotation no longer has a mode: a large
    /// annotation is grabbed by its border and its interior belongs to
    /// whichever drawing tool is armed, while the Pointer tool selects
    /// anywhere.
    ///
    /// Still accepted so a config written before the removal keeps
    /// loading — the file format denies unknown fields, so dropping it
    /// outright would turn an existing config into a parse error. The
    /// value is ignored.
    #[allow(dead_code)]
    select_any_annotation: Option<bool>,
    /// Removed: the compositor already applies the user's natural-
    /// scrolling choice, and flipping it again here second-guessed a
    /// decision they made for the whole desktop.
    ///
    /// Still accepted for the same reason as the key above — the
    /// format denies unknown fields, so dropping it would turn an
    /// existing config into a parse error. The value is ignored.
    #[allow(dead_code)]
    invert_scrolling: Option<bool>,
    close_on_esc: Option<bool>,
    close_on_copy: Option<bool>,
    close_on_save: Option<bool>,
    hide_default_palette: Option<bool>,
    sticky_session_defaults: Option<bool>,
    /// Deprecated: manual scroll capture no longer moves the pointer at
    /// all, and automatic capture always parks it exactly once. Still
    /// parsed (`deny_unknown_fields`) so configs that set it keep
    /// loading; the value is ignored.
    #[allow(dead_code)]
    park_pointer_during_manual_scroll_capture: Option<bool>,
    /// Resize the editor window around crop-only content-size changes.
    /// This positive spelling is shared with the Preferences checkbox.
    resize_window_to_content_on_crop: Option<bool>,
    fixed_canvas: Option<bool>,

    // --- deprecated options ---
    right_click_copy: Option<bool>,
    action_on_enter: Option<Action>,
    // ---
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct ColorPaletteFile {
    palette: Option<Vec<HexColor>>,
    custom: Option<Vec<HexColor>>,
}

impl ConfigurationFile {
    fn active_path(specified_path: &Option<String>) -> Option<PathBuf> {
        let config_home = BaseDirectories::with_prefix(env!("CARGO_PKG_NAME")).get_config_home();
        Self::active_path_from_config_home(specified_path.as_deref(), config_home.as_deref())
    }

    /// Resolve a write target without touching the filesystem. In particular,
    /// utility invocations may call this while the XDG application directory
    /// does not exist yet; only an actual Preferences write or migration
    /// creates it.
    fn active_path_from_config_home(
        specified_path: Option<&str>,
        prefixed_config_home: Option<&Path>,
    ) -> Option<PathBuf> {
        specified_path
            .map(PathBuf::from)
            .or_else(|| prefixed_config_home.map(|home| home.join("config.toml")))
    }

    fn try_read(
        specified_path: &Option<String>,
    ) -> Result<Option<ConfigurationFile>, ConfigurationFileError> {
        match specified_path {
            None => Self::try_read_xdg(),
            Some(p) => Self::try_read_path(p),
        }
    }

    fn try_read_xdg() -> Result<Option<ConfigurationFile>, ConfigurationFileError> {
        let dirs = BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"));
        match dirs.get_config_file("config.toml") {
            Some(path) => Self::try_read_path(path),
            None => Ok(None),
        }
    }

    fn try_read_path<P: AsRef<Path>>(
        path: P,
    ) -> Result<Option<ConfigurationFile>, ConfigurationFileError> {
        let content = fs::read_to_string(path)?;
        Ok(Some(toml::from_str::<ConfigurationFile>(&content)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::LegacyPreferences;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_config_path(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "tensaku-{test_name}-{}-{nonce}",
                std::process::id()
            ))
            .join("custom.toml")
    }

    fn command_line(extra: &[&str]) -> CommandLine {
        let mut args = vec!["tensaku", "--filename", "-"];
        args.extend_from_slice(extra);
        CommandLine::try_parse_from(args).expect("test command line should parse")
    }

    fn parsed_config(content: &str) -> ConfigurationFile {
        toml::from_str(content).expect("test config should parse")
    }

    #[test]
    fn fixed_canvas_persists_and_reloads_without_changing_other_preferences() {
        let path = temporary_config_path("fixed-canvas");
        let mut config = Configuration {
            config_file_path: Some(path.clone()),
            ..Configuration::default()
        };
        assert!(!config.fixed_canvas());
        config.save_fixed_canvas(true).unwrap();
        assert!(config.fixed_canvas());
        let file = parsed_config(&fs::read_to_string(&path).unwrap());
        let mut reloaded = Configuration::default();
        reloaded.merge(Some(file), command_line(&[]));
        assert!(reloaded.fixed_canvas());
        assert!(reloaded.resize_window_to_content_on_crop());
        config.save_fixed_canvas(false).unwrap();
        let file = parsed_config(&fs::read_to_string(&path).unwrap());
        reloaded.merge(Some(file), command_line(&[]));
        assert!(!reloaded.fixed_canvas());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn crop_window_resize_defaults_on_and_parses_positive_config_key() {
        assert!(Configuration::default().resize_window_to_content_on_crop());

        let file: ConfigurationFile =
            toml::from_str("[general]\nresize-window-to-content-on-crop = false\n")
                .expect("canonical crop setting should parse");
        assert_eq!(
            file.general
                .and_then(|general| general.resize_window_to_content_on_crop),
            Some(false)
        );
    }

    #[test]
    fn canonical_preferences_parse_and_merge_into_runtime() {
        let file = parsed_config(
            "[general]\n\
             annotation-size-factor = 1.7\n\
             invert-scrolling = false\n\
             select-any-annotation = false\n\
             close-on-esc = true\n\
             close-on-copy = true\n\
             close-on-save = true\n\
             hide-default-palette = true\n\
             sticky-session-defaults = true\n\
             park-pointer-during-manual-scroll-capture = false\n\
             resize-window-to-content-on-crop = false\n",
        );
        let mut config = Configuration::default();
        config.merge(Some(file), command_line(&[]));

        assert_eq!(config.annotation_size_factor(), 1.7);
        assert!(config.annotation_size_factor_is_explicit());
        assert!(config.close_on_esc());
        assert!(config.close_on_copy());
        assert!(config.close_on_save());
        assert!(config.hide_default_palette());
        assert!(config.sticky_session_defaults());
        assert!(!config.resize_window_to_content_on_crop());
    }

    #[test]
    fn direct_close_keys_beat_config_early_exit_and_cli_beats_both() {
        let content = "[general]\nearly-exit = true\nclose-on-copy = false\n";
        let mut config = Configuration::default();
        config.merge(Some(parsed_config(content)), command_line(&[]));
        assert!(!config.close_on_copy());
        assert!(config.close_on_save());

        let mut cli_config = Configuration::default();
        cli_config.merge(
            Some(parsed_config(content)),
            command_line(&["--early-exit"]),
        );
        assert!(cli_config.close_on_copy());
        assert!(cli_config.close_on_save());
    }

    #[test]
    fn cli_annotation_factor_beats_config_and_marks_value_explicit() {
        let mut config = Configuration::default();
        assert!(!config.annotation_size_factor_is_explicit());
        config.merge(
            Some(parsed_config("[general]\nannotation-size-factor = 1.5\n")),
            command_line(&["--annotation-size-factor", "2.5"]),
        );
        assert_eq!(config.annotation_size_factor(), 2.5);
        assert!(config.annotation_size_factor_is_explicit());
    }

    #[test]
    fn migration_fills_only_absent_config_keys_and_inverts_crop_legacy_value() {
        let mut file = parsed_config(
            "[general]\n\
             invert-scrolling = true\n\
             close-on-copy = false\n\
             resize-window-to-content-on-crop = true\n\
             [keybinds]\n\
             pointer = \"p\"\n",
        );
        let legacy = LegacyPreferences {
            annotation_size_factor: Some(1.4),
            close_on_copy: Some(true),
            close_on_save: Some(true),
            keep_window_size_on_crop: Some(true),
            keybinds: Some(HashMap::from([
                (Tools::Pointer, "v".to_string()),
                (Tools::Crop, "c".to_string()),
            ])),
            ..LegacyPreferences::default()
        };

        let updates = apply_legacy_preferences(&mut file, &legacy);
        let general = file.general.as_ref().expect("general table should exist");
        assert_eq!(general.close_on_copy, Some(false));
        assert_eq!(general.close_on_save, Some(true));
        assert_eq!(general.resize_window_to_content_on_crop, Some(true));
        assert_eq!(general.annotation_size_factor, Some(1.4));
        let keybinds = file.keybinds.as_ref().expect("keybind table should exist");
        assert_eq!(keybinds.pointer.as_deref(), Some("p"));
        assert_eq!(keybinds.crop.as_deref(), Some("c"));
        assert_eq!(keybinds.spotlight.as_deref(), Some(""));
        assert!(
            !updates
                .general
                .iter()
                .any(|(key, _)| *key == "invert-scrolling")
        );
        assert!(
            !updates
                .general
                .iter()
                .any(|(key, _)| *key == "resize-window-to-content-on-crop")
        );
    }

    #[test]
    fn explicit_config_keybind_wins_a_legacy_character_collision() {
        let mut file = parsed_config("[keybinds]\npointer = \"a\"\n");
        let legacy = LegacyPreferences {
            keybinds: Some(HashMap::from([(Tools::Arrow, "a".to_string())])),
            ..LegacyPreferences::default()
        };

        let updates = apply_legacy_preferences(&mut file, &legacy);
        let keybinds = file.keybinds.as_ref().expect("keybind table should exist");
        assert_eq!(keybinds.pointer.as_deref(), Some("a"));
        assert_eq!(keybinds.arrow.as_deref(), Some(""));
        assert!(
            updates
                .keybinds
                .iter()
                .any(|(key, value)| *key == "arrow" && value.is_empty())
        );

        let mut runtime = Configuration::default();
        runtime.merge(Some(file), command_line(&[]));
        assert_eq!(runtime.keybinds().get_tool('a'), Some(Tools::Pointer));
        assert!(
            !runtime
                .keybinds()
                .shortcuts()
                .values()
                .any(|tool| *tool == Tools::Arrow)
        );
    }

    #[test]
    fn crop_legacy_value_is_inverted_when_canonical_key_is_absent() {
        for (legacy_keep, expected_resize) in [(true, false), (false, true)] {
            let mut file = ConfigurationFile::default();
            let updates = apply_legacy_preferences(
                &mut file,
                &LegacyPreferences {
                    keep_window_size_on_crop: Some(legacy_keep),
                    ..LegacyPreferences::default()
                },
            );
            assert_eq!(
                file.general
                    .and_then(|general| general.resize_window_to_content_on_crop),
                Some(expected_resize)
            );
            assert_eq!(
                updates.general,
                vec![(
                    "resize-window-to-content-on-crop",
                    GeneralValue::Bool(expected_resize)
                )]
            );
        }
    }

    #[test]
    fn general_bool_writer_preserves_comments_and_unrelated_content() {
        let path = temporary_config_path("preserve-config");
        let parent = path.parent().expect("temporary path should have a parent");
        fs::create_dir_all(parent).expect("temporary directory should be created");
        fs::write(
            &path,
            "# top-level comment\n\
             [general]\n\
             floating-hack = true # unrelated inline comment\n\
             resize-window-to-content-on-crop = true # crop inline comment\n\
             \n\
             [font]\n\
             family = \"Roboto\"\n",
        )
        .expect("fixture should be written");

        write_config_updates(
            &path,
            ConfigUpdates {
                general: vec![
                    (
                        "resize-window-to-content-on-crop",
                        GeneralValue::Bool(false),
                    ),
                    ("annotation-size-factor", GeneralValue::Float(1.7)),
                ],
                keybinds: vec![("crop", "".to_string())],
            },
        )
        .expect("existing setting should be updated");

        let output = fs::read_to_string(&path).expect("updated config should be readable");
        assert!(output.contains("# top-level comment"));
        assert!(output.contains("floating-hack = true # unrelated inline comment"));
        assert!(output.contains("# crop inline comment"));
        assert!(output.contains("[font]"));
        assert!(output.contains("family = \"Roboto\""));
        assert!(output.contains("annotation-size-factor = 1.7"));
        assert!(!output.contains("1.7000000476837158"));
        let document = output
            .parse::<DocumentMut>()
            .expect("updated config should remain valid TOML");
        assert_eq!(
            document["general"]["resize-window-to-content-on-crop"].as_bool(),
            Some(false)
        );
        assert_eq!(
            document["general"]["annotation-size-factor"].as_float(),
            Some(1.7)
        );
        assert_eq!(document["keybinds"]["crop"].as_str(), Some(""));

        fs::remove_dir_all(parent).expect("temporary directory should be removed");
    }

    #[test]
    fn writer_updates_inline_general_and_keybind_tables() {
        let path = temporary_config_path("inline-tables");
        let parent = path.parent().expect("temporary path should have a parent");
        fs::create_dir_all(parent).expect("temporary directory should be created");
        fs::write(
            &path,
            "general = { floating-hack = true, resize-window-to-content-on-crop = true } # general comment\n\
             keybinds = { pointer = \"v\", crop = \"x\" } # keybind comment\n",
        )
        .expect("fixture should be written");

        write_config_updates(
            &path,
            ConfigUpdates {
                general: vec![(
                    "resize-window-to-content-on-crop",
                    GeneralValue::Bool(false),
                )],
                keybinds: vec![("crop", String::new())],
            },
        )
        .expect("inline tables should be writable");

        let output = fs::read_to_string(&path).expect("updated config should be readable");
        assert!(output.contains("general = {"));
        assert!(output.contains("# general comment"));
        assert!(output.contains("# keybind comment"));
        let document = output
            .parse::<DocumentMut>()
            .expect("updated config should remain valid TOML");
        assert_eq!(document["general"]["floating-hack"].as_bool(), Some(true));
        assert_eq!(
            document["general"]["resize-window-to-content-on-crop"].as_bool(),
            Some(false)
        );
        assert_eq!(document["keybinds"]["pointer"].as_str(), Some("v"));
        assert_eq!(document["keybinds"]["crop"].as_str(), Some(""));

        fs::remove_dir_all(parent).expect("temporary directory should be removed");
    }

    #[test]
    fn fresh_xdg_write_path_is_resolved_without_creating_it() {
        let marker = temporary_config_path("fresh-xdg");
        let root = marker
            .parent()
            .expect("temporary path should have a parent")
            .to_path_buf();
        let prefixed_config_home = root.join("xdg-config").join("tensaku");
        let expected = prefixed_config_home.join("config.toml");

        let resolved =
            ConfigurationFile::active_path_from_config_home(None, Some(&prefixed_config_home));
        assert_eq!(resolved.as_deref(), Some(expected.as_path()));
        assert!(!prefixed_config_home.exists());

        let mut config = Configuration {
            config_file_path: resolved,
            ..Configuration::default()
        };
        config
            .save_resize_window_to_content_on_crop(false)
            .expect("first Preferences save should create the XDG config");
        assert!(expected.is_file());

        fs::remove_dir_all(root).expect("temporary directory should be removed");
    }

    #[test]
    fn configuration_writer_uses_active_custom_config_path() {
        let path = temporary_config_path("active-config");
        let mut config = Configuration {
            config_file_path: Some(path.clone()),
            ..Configuration::default()
        };

        config
            .save_resize_window_to_content_on_crop(false)
            .expect("missing custom config should be created");
        assert!(!config.resize_window_to_content_on_crop());

        let output = fs::read_to_string(&path).expect("custom config should be readable");
        let document = output
            .parse::<DocumentMut>()
            .expect("custom config should be valid TOML");
        assert_eq!(
            document["general"]["resize-window-to-content-on-crop"].as_bool(),
            Some(false)
        );

        fs::remove_dir_all(path.parent().expect("temporary path should have a parent"))
            .expect("temporary directory should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn configuration_writer_preserves_a_relative_config_symlink() {
        use std::os::unix::fs::symlink;

        let marker = temporary_config_path("symlink-config");
        let root = marker
            .parent()
            .expect("temporary path should have a parent")
            .to_path_buf();
        let link_dir = root.join("active");
        let target_dir = root.join("dotfiles");
        let link_path = link_dir.join("config.toml");
        let target_path = target_dir.join("tensaku.toml");
        fs::create_dir_all(&link_dir).expect("link directory should be created");
        fs::create_dir_all(&target_dir).expect("target directory should be created");
        fs::write(
            &target_path,
            "# managed dotfile\n[general]\ninvert-scrolling = true\n",
        )
        .expect("target fixture should be written");
        symlink("../dotfiles/tensaku.toml", &link_path)
            .expect("relative config symlink should be created");

        write_config_updates(
            &link_path,
            ConfigUpdates {
                general: vec![("invert-scrolling", GeneralValue::Bool(false))],
                keybinds: Vec::new(),
            },
        )
        .expect("preference should be written through the symlink");

        assert!(
            fs::symlink_metadata(&link_path)
                .expect("link should still exist")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(&link_path).expect("link target should remain readable"),
            PathBuf::from("../dotfiles/tensaku.toml")
        );
        let output = fs::read_to_string(&target_path).expect("target should be updated");
        assert!(output.contains("# managed dotfile"));
        assert!(output.contains("invert-scrolling = false"));

        fs::remove_dir_all(root).expect("temporary directory should be removed");
    }

    #[test]
    fn invalid_toml_is_unchanged_when_a_preference_save_fails() {
        let path = temporary_config_path("invalid-config");
        let parent = path.parent().expect("temporary path should have a parent");
        fs::create_dir_all(parent).expect("temporary directory should be created");
        let original = b"[general\nthis is not toml\n";
        fs::write(&path, original).expect("invalid fixture should be written");
        let mut config = Configuration {
            config_file_path: Some(path.clone()),
            ..Configuration::default()
        };

        // Saving the non-default value must fail AND leave the
        // runtime on the default it started from.
        assert!(config.save_close_on_esc(true).is_err());
        assert!(!config.close_on_esc());
        assert_eq!(
            fs::read(&path).expect("fixture should remain readable"),
            original
        );

        fs::remove_dir_all(parent).expect("temporary directory should be removed");
    }

    #[test]
    fn failed_migration_keeps_legacy_value_effective_for_the_run() {
        let path = temporary_config_path("failed-migration");
        let parent = path.parent().expect("temporary path should have a parent");
        fs::create_dir_all(parent).expect("temporary directory should be created");
        fs::write(&path, "[broken\n").expect("invalid fixture should be written");
        // A value that differs from the runtime default, so the
        // assertions below can't pass on the default alone.
        let legacy = LegacyPreferences {
            close_on_esc: Some(true),
            ..LegacyPreferences::default()
        };
        let mut file = ConfigurationFile::default();
        let updates = apply_legacy_preferences(&mut file, &legacy);

        assert!(write_config_updates(&path, updates).is_err());
        let mut runtime = Configuration::default();
        runtime.merge(Some(file), command_line(&[]));
        assert!(runtime.close_on_esc());
        assert_eq!(legacy.close_on_esc, Some(true));

        fs::remove_dir_all(parent).expect("temporary directory should be removed");
    }

    #[test]
    fn utility_invocations_do_not_trigger_preference_migration() {
        let doctor = CommandLine::try_parse_from(["tensaku", "--doctor"])
            .expect("doctor command should parse");
        assert!(is_utility_invocation(&doctor));
        assert!(!is_utility_invocation(&command_line(&[])));
    }

    #[test]
    fn saved_unbound_keybind_remains_unbound_after_reload() {
        let path = temporary_config_path("unbound-keybind");
        let mut shortcuts = Keybinds::default().shortcuts;
        shortcuts.retain(|_, tool| *tool != Tools::Crop);
        let mut writer = Configuration {
            config_file_path: Some(path.clone()),
            ..Configuration::default()
        };
        writer
            .save_keybinds(shortcuts)
            .expect("keybinds should save");

        let file = ConfigurationFile::try_read_path(&path)
            .expect("saved config should parse")
            .expect("saved config should exist");
        let mut reloaded = Configuration::default();
        reloaded.merge(Some(file), command_line(&[]));
        assert!(
            !reloaded
                .keybinds()
                .shortcuts()
                .values()
                .any(|tool| *tool == Tools::Crop)
        );

        fs::remove_dir_all(path.parent().expect("temporary path should have a parent"))
            .expect("temporary directory should be removed");
    }
}
