//! Per-user incidental UI state that survives across launches. Explicit
//! Preferences values are canonical in `config.toml`; the matching optional
//! fields here remain only long enough to migrate state written by older
//! Tensaku builds. This file lives in the XDG state dir
//! (`~/.local/state/tensaku/state.toml` on Linux).

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hex_color::HexColor;
use serde::{Deserialize, Serialize};
use toml_edit::{Decor, DocumentMut, Item, Key, RawString};
use xdg::BaseDirectories;

use crate::style::{Color, Size};
use crate::tools::{ArrowStyle, BlurStyle, HighlighterStyle, TextBackground, Tools};

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PersistedState {
    pub last_color: Option<HexColor>,
    #[serde(default)]
    pub saved_custom_colors: Vec<HexColor>,
    /// Spotlight overlay darkness (0.10–0.90). None = use the
    /// 50% default (detent value).
    #[serde(default)]
    pub spotlight_darkness: Option<f32>,
    /// Highlighter stroke opacity (0.10–1.00). None = use the
    /// 40% default.
    #[serde(default)]
    pub highlighter_opacity: Option<f32>,
    #[serde(default)]
    pub spotlight_magnification: Option<f32>,
    /// Legacy Preferences value; migrated to `config.toml` at startup.
    #[serde(default)]
    pub annotation_size_factor: Option<f32>,
    /// Crop tool's "Snap to edges" preference (the bottom-left
    /// checkbox while cropping). `None` means "use default" — true.
    #[serde(default)]
    pub snap_to_edges: Option<bool>,
    /// Saved-default size per tool. Keyed by `Tools` (serializes to
    /// the lowercase tool name); `None` for a missing entry means
    /// "use the global Size::Medium default". Updated only by the
    /// size slider's right-click → "Save as default" — the slider's
    /// live value isn't persisted on every drag.
    #[serde(default)]
    pub size_per_tool: HashMap<Tools, Size>,
    /// Last-chosen arrow geometry (Standard / Pointy / Curved / Double).
    /// Auto-saved on every selection so re-opening the Arrow tool
    /// picks up where the user left off.
    #[serde(default)]
    pub arrow_style: Option<ArrowStyle>,
    /// Last-chosen blur algorithm (Gaussian / Pixelate). Same
    /// auto-save semantics as `arrow_style`.
    #[serde(default)]
    pub blur_style: Option<BlurStyle>,
    /// Last-chosen text background style (Plain / Rounded). Same
    /// auto-save semantics as `arrow_style` — re-opening the Text
    /// tool restores the user's last choice.
    #[serde(default)]
    pub text_background: Option<TextBackground>,
    /// Last-chosen highlighter style (TextLocked / Normal). Same
    /// auto-save semantics: cycling via toolbar or double-tap
    /// persists, restoring the user's preference on next launch.
    #[serde(default)]
    pub highlighter_style: Option<HighlighterStyle>,
    /// Legacy Preferences value; migrated to `config.toml` at startup.
    #[serde(default)]
    pub keybinds: Option<HashMap<Tools, String>>,
    /// Legacy Preferences value; migrated to `config.toml` at startup.
    #[serde(default)]
    pub close_on_esc: Option<bool>,
    /// Legacy Preferences value; migrated to `config.toml` at startup.
    #[serde(default)]
    pub close_on_copy: Option<bool>,
    /// Legacy Preferences value; migrated to `config.toml` at startup.
    #[serde(default)]
    pub close_on_save: Option<bool>,
    /// Legacy Preferences value; migrated to `config.toml` at startup.
    #[serde(default)]
    pub hide_default_palette: Option<bool>,
    /// Saved-default number of Chaikin post-stroke smoothing passes
    /// for the brush tool. `None` = "never explicitly saved"; callers
    /// fall back to the config / built-in default (5).
    #[serde(default)]
    pub brush_post_smooth_iterations: Option<usize>,
    /// Per-tool saved-default fill state (true = filled, false =
    /// outline). Keyed by `Tools`; only Rectangle / Ellipse currently
    /// honor this. A missing entry means "no saved default — use the
    /// `default-fill-shapes` config value".
    #[serde(default)]
    pub fill_per_tool: HashMap<Tools, bool>,
    /// Legacy Preferences value; migrated to `config.toml` at startup.
    #[serde(default)]
    pub sticky_session_defaults: Option<bool>,
    /// Pixel width of the layer panel (Paned start_child slot). `None`
    /// means "user hasn't dragged the divider yet" — falls back to the
    /// in-code default. Persisted so re-opening the app puts the
    /// divider where the user last left it.
    #[serde(default)]
    pub layer_panel_width: Option<f32>,
    /// The keyboard chord recorded in Preferences to trigger
    /// scroll-capture mode, in the canonical `CTRL+SHIFT+ALT+SUPER+KEY`
    /// form (see `chord_capture`). `None` means no shortcut is set. This
    /// is purely the recorder's display/source-of-truth; the actual bind
    /// lives in Hyprland (registered via `hypr_bind`).
    #[serde(default)]
    pub scroll_capture_shortcut: Option<String>,
    /// The last region a scroll capture actually ran on, as
    /// `[x, y, w, h]` in overlay-logical (monitor) pixels. Restored in
    /// the overlay via the `scroll-capture-restore-region-shortcut`
    /// key. Incidental UI state, not a preference — hence state.toml.
    #[serde(default)]
    pub scroll_capture_last_region: Option<[f64; 4]>,
    /// Legacy Preferences value; migrated to `config.toml` at startup.
    #[serde(default)]
    pub park_pointer_during_manual_scroll_capture: Option<bool>,
}

fn state_path() -> Option<PathBuf> {
    let dirs = BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"));
    dirs.place_state_file("state.toml").ok()
}

pub fn load() -> PersistedState {
    let Some(path) = state_path() else {
        return PersistedState::default();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return PersistedState::default();
    };
    toml::from_str(&content).unwrap_or_default()
}

fn save(state: &PersistedState) {
    let Some(path) = state_path() else { return };
    let Ok(s) = toml::to_string(state) else {
        return;
    };
    let _ = fs::write(path, s);
}

/// Preference values written by older Tensaku builds before config.toml
/// became their canonical store. Startup consumes this as one migration
/// snapshot; incidental editor state remains in `PersistedState`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LegacyPreferences {
    pub annotation_size_factor: Option<f32>,
    pub keybinds: Option<HashMap<Tools, String>>,
    pub close_on_esc: Option<bool>,
    pub close_on_copy: Option<bool>,
    pub close_on_save: Option<bool>,
    pub hide_default_palette: Option<bool>,
    pub sticky_session_defaults: Option<bool>,
    pub park_pointer_during_manual_scroll_capture: Option<bool>,
    pub keep_window_size_on_crop: Option<bool>,
}

impl LegacyPreferences {
    pub(crate) fn is_empty(&self) -> bool {
        self.annotation_size_factor.is_none()
            && self.keybinds.is_none()
            && self.close_on_esc.is_none()
            && self.close_on_copy.is_none()
            && self.close_on_save.is_none()
            && self.hide_default_palette.is_none()
            && self.sticky_session_defaults.is_none()
            && self.park_pointer_during_manual_scroll_capture.is_none()
            && self.keep_window_size_on_crop.is_none()
    }
}

pub(crate) fn load_legacy_preferences() -> LegacyPreferences {
    let state = load();
    LegacyPreferences {
        annotation_size_factor: state.annotation_size_factor,
        keybinds: state.keybinds,
        close_on_esc: state.close_on_esc,
        close_on_copy: state.close_on_copy,
        close_on_save: state.close_on_save,
        hide_default_palette: state.hide_default_palette,
        sticky_session_defaults: state.sticky_session_defaults,
        park_pointer_during_manual_scroll_capture: state.park_pointer_during_manual_scroll_capture,
        keep_window_size_on_crop: load_legacy_keep_window_size_on_crop(),
    }
}

/// Remove only fields now owned by config.toml, leaving incidental editor
/// state and the Hyprland-managed global scroll chord untouched. This is
/// called only after the config migration transaction succeeds (or when the
/// corresponding values were already explicitly present in config.toml).
pub(crate) fn clear_legacy_preferences() -> io::Result<()> {
    let Some(path) = state_path() else {
        return Ok(());
    };
    clear_legacy_preferences_at_path(&path)
}

const LEGACY_PREFERENCE_KEYS: [&str; 11] = [
    "annotation-size-factor",
    "keybinds",
    "invert-scrolling",
    "select-any-annotation",
    "close-on-esc",
    "close-on-copy",
    "close-on-save",
    "hide-default-palette",
    "sticky-session-defaults",
    "park-pointer-during-manual-scroll-capture",
    "keep-window-size-on-crop",
];

fn clear_legacy_preferences_at_path(path: &Path) -> io::Result<()> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut document = content
        .parse::<DocumentMut>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    remove_legacy_keys_preserving_leading_comments(&mut document);

    atomic_write_state(path, document.to_string().as_bytes())
}

fn raw_string(raw: Option<&RawString>) -> &str {
    raw.and_then(RawString::as_str).unwrap_or_default()
}

fn leading_prefix(key: &Key, item: &Item) -> String {
    match item {
        Item::Table(table) => raw_string(table.decor().prefix()).to_string(),
        Item::ArrayOfTables(tables) => tables
            .get(0)
            .map(|table| raw_string(table.decor().prefix()).to_string())
            .unwrap_or_else(|| raw_string(key.leaf_decor().prefix()).to_string()),
        _ => raw_string(key.leaf_decor().prefix()).to_string(),
    }
}

fn prepend_decor_prefix(decor: &mut Decor, prefix: &str) {
    let existing = raw_string(decor.prefix());
    decor.set_prefix(format!("{prefix}{existing}"));
}

fn prepend_item_prefix(document: &mut DocumentMut, key_name: &str, prefix: &str) {
    let Some((mut key, item)) = document.as_table_mut().get_key_value_mut(key_name) else {
        return;
    };
    match item {
        Item::Table(table) => prepend_decor_prefix(table.decor_mut(), prefix),
        Item::ArrayOfTables(tables) => {
            if let Some(table) = tables.get_mut(0) {
                prepend_decor_prefix(table.decor_mut(), prefix);
            } else {
                prepend_decor_prefix(key.leaf_decor_mut(), prefix);
            }
        }
        _ => prepend_decor_prefix(key.leaf_decor_mut(), prefix),
    }
}

fn remove_legacy_keys_preserving_leading_comments(document: &mut DocumentMut) {
    let ordered_keys: Vec<String> = document.iter().map(|(key, _)| key.to_string()).collect();
    let mut pending_prefix = String::new();

    for key_name in ordered_keys {
        if LEGACY_PREFERENCE_KEYS.contains(&key_name.as_str()) {
            if let Some((key, item)) = document.as_table_mut().remove_entry(&key_name) {
                pending_prefix.push_str(&leading_prefix(&key, &item));
            }
        } else if !pending_prefix.is_empty() {
            prepend_item_prefix(document, &key_name, &pending_prefix);
            pending_prefix.clear();
        }
    }

    if !pending_prefix.is_empty() {
        let trailing = raw_string(Some(document.trailing()));
        document.set_trailing(format!("{pending_prefix}{trailing}"));
    }
}

static STATE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn atomic_write_state(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let original_permissions = fs::metadata(path).ok().map(|meta| meta.permissions());
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.toml");

    for _ in 0..100 {
        let id = STATE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        "could not allocate a temporary state file",
    ))
}

pub fn save_last_color(color: Color) {
    let mut state = load();
    state.last_color = Some(HexColor::rgba(color.r, color.g, color.b, color.a));
    save(&state);
}

pub fn load_last_color() -> Option<Color> {
    load().last_color.map(Color::from)
}

/// Resolve the startup annotation color. Persisted last-color wins;
/// otherwise red. Shared between the toolbar (swatch preview) and
/// sketch_board (drawing style) so the very first stroke after launch
/// matches the swatch the user sees — `Style::default()` would
/// otherwise resolve color to whatever the palette's first entry is,
/// which is independent of (and can disagree with) the user's
/// previously-chosen color.
pub fn initial_color() -> Color {
    load_last_color().unwrap_or_else(Color::red)
}

/// Load saved-custom colors as a sparse slot list — `None` entries
/// represent empty placeholder slots the user has intentionally left
/// (e.g. by dragging a color away from its position). Empty slots are
/// encoded on disk as `HexColor::rgba(0, 0, 0, 0)` (fully transparent
/// black), which is unreachable through the color chooser UI in normal
/// use; if a user does manage to save a fully-transparent black color,
/// it'll round-trip as an empty slot — acceptable since that color
/// would render as nothing visible anyway.
pub fn load_custom_colors() -> Vec<Option<Color>> {
    load()
        .saved_custom_colors
        .into_iter()
        .map(hex_to_slot)
        .collect()
}

/// Add `color` to the persisted saved-custom list. Fills the first
/// `None` slot if one exists (so explicit gaps left by drag-aways get
/// reused first); otherwise appends a new slot at the end. Returns the
/// new list so callers can update their in-memory mirror without a
/// separate re-load.
pub fn append_custom_color(color: Color) -> Vec<Option<Color>> {
    let mut slots = load_custom_colors();
    if let Some(empty_idx) = slots.iter().position(Option::is_none) {
        slots[empty_idx] = Some(color);
    } else {
        slots.push(Some(color));
    }
    save_custom_colors(&slots);
    slots
}

fn hex_to_slot(hc: HexColor) -> Option<Color> {
    if hc.r == 0 && hc.g == 0 && hc.b == 0 && hc.a == 0 {
        None
    } else {
        Some(Color::from(hc))
    }
}

fn slot_to_hex(slot: Option<Color>) -> HexColor {
    match slot {
        Some(c) => HexColor::rgba(c.r, c.g, c.b, c.a),
        None => HexColor::rgba(0, 0, 0, 0),
    }
}

pub fn load_layer_panel_width() -> Option<f32> {
    load().layer_panel_width
}

pub fn save_layer_panel_width(value: f32) {
    let mut state = load();
    state.layer_panel_width = Some(value);
    save(&state);
}

/// The recorded scroll-capture shortcut chord (canonical
/// `CTRL+SHIFT+ALT+SUPER+KEY` form), or `None` when unset.
pub fn load_scroll_capture_shortcut() -> Option<String> {
    load().scroll_capture_shortcut
}

/// Persist (or clear, with `None`) the scroll-capture shortcut chord.
/// Called from the Preferences recorder row when the user records a new
/// chord or clears it.
pub fn save_scroll_capture_shortcut(chord: Option<String>) {
    let mut state = load();
    state.scroll_capture_shortcut = chord;
    save(&state);
}

/// The region (`[x, y, w, h]`, overlay-logical px) of the most recent
/// scroll capture, or `None` when none has run yet.
pub fn load_scroll_capture_last_region() -> Option<[f64; 4]> {
    load().scroll_capture_last_region
}

/// Remember the region a scroll capture just started on, so the next
/// overlay session can reselect it via the restore-region key.
pub fn save_scroll_capture_last_region(region: [f64; 4]) {
    let mut state = load();
    state.scroll_capture_last_region = Some(region);
    save(&state);
}

pub fn load_spotlight_darkness() -> Option<f32> {
    load().spotlight_darkness
}

pub fn save_spotlight_darkness(value: f32) {
    let mut state = load();
    state.spotlight_darkness = Some(value);
    save(&state);
}

pub fn load_highlighter_opacity() -> Option<f32> {
    load().highlighter_opacity
}

pub fn load_spotlight_magnification() -> Option<f32> {
    load().spotlight_magnification
}

pub fn save_spotlight_magnification(value: f32) {
    let mut state = load();
    state.spotlight_magnification = Some(value);
    save(&state);
}

pub fn save_highlighter_opacity(value: f32) {
    let mut state = load();
    state.highlighter_opacity = Some(value);
    save(&state);
}

/// "Snap to edges" toggle for the crop tool. `None` falls back to
/// the default (true) — callers handle the unwrap to keep the
/// reader honest about the missing-state case.
pub fn load_snap_to_edges() -> Option<bool> {
    load().snap_to_edges
}

pub fn save_snap_to_edges(value: bool) {
    let mut state = load();
    state.snap_to_edges = Some(value);
    save(&state);
}

#[derive(Deserialize)]
struct LegacyCropWindowState {
    #[serde(rename = "keep-window-size-on-crop")]
    keep_window_size_on_crop: Option<bool>,
}

fn parse_legacy_keep_window_size_on_crop(content: &str) -> Option<bool> {
    toml::from_str::<LegacyCropWindowState>(content)
        .ok()
        .and_then(|state| state.keep_window_size_on_crop)
}

/// Read the short-lived inverse crop-window preference so startup can migrate
/// it to config.toml. It is deliberately absent from `PersistedState`, so any
/// subsequent state save removes the obsolete key rather than persisting two
/// competing sources of truth.
pub fn load_legacy_keep_window_size_on_crop() -> Option<bool> {
    let path = state_path()?;
    let content = fs::read_to_string(path).ok()?;
    parse_legacy_keep_window_size_on_crop(&content)
}

/// Read this tool's saved-default size, if the user has explicitly
/// saved one via the size slider's right-click → "Save as default".
pub fn load_size_for_tool(tool: Tools) -> Option<Size> {
    load().size_per_tool.get(&tool).copied()
}

/// Persist `size` as the default for `tool`. Future launches and
/// future tool switches into `tool` will start at this size.
pub fn save_size_for_tool(tool: Tools, size: Size) {
    let mut state = load();
    state.size_per_tool.insert(tool, size);
    save(&state);
}

pub fn load_arrow_style() -> Option<ArrowStyle> {
    load().arrow_style
}

pub fn save_arrow_style(style: ArrowStyle) {
    let mut state = load();
    state.arrow_style = Some(style);
    save(&state);
}

pub fn load_blur_style() -> Option<BlurStyle> {
    load().blur_style
}

pub fn save_blur_style(style: BlurStyle) {
    let mut state = load();
    state.blur_style = Some(style);
    save(&state);
}

pub fn load_text_background() -> Option<TextBackground> {
    load().text_background
}

pub fn save_text_background(bg: TextBackground) {
    let mut state = load();
    state.text_background = Some(bg);
    save(&state);
}

pub fn load_highlighter_style() -> Option<HighlighterStyle> {
    load().highlighter_style
}

pub fn save_highlighter_style(style: HighlighterStyle) {
    let mut state = load();
    state.highlighter_style = Some(style);
    save(&state);
}

/// Saved-default number of post-stroke Chaikin smoothing passes for
/// the brush. `None` = use config / built-in default.
pub fn load_brush_post_smooth_iterations() -> Option<usize> {
    load().brush_post_smooth_iterations
}

pub fn save_brush_post_smooth_iterations(value: usize) {
    let mut state = load();
    state.brush_post_smooth_iterations = Some(value);
    save(&state);
}

/// Read this tool's saved-default fill state, if any. Returns `None`
/// when the user has never persisted a fill default for the tool —
/// callers fall back to `APP_CONFIG.default_fill_shapes()`.
pub fn load_fill_for_tool(tool: Tools) -> Option<bool> {
    load().fill_per_tool.get(&tool).copied()
}

/// Persist `fill` as the saved-default fill state for `tool`. Future
/// launches and future entries into `tool` will start at this fill.
pub fn save_fill_for_tool(tool: Tools, fill: bool) {
    let mut state = load();
    state.fill_per_tool.insert(tool, fill);
    save(&state);
}

/// Replace the persisted saved-custom slot list wholesale. Trailing
/// `None`s are trimmed (they carry no information — the user can
/// always grow the list again by dragging or saving a new color), but
/// mid-list `None`s are preserved so explicit gaps survive a restart.
pub fn save_custom_colors(slots: &[Option<Color>]) {
    let mut state = load();
    let mut trimmed: Vec<Option<Color>> = slots.to_vec();
    while matches!(trimmed.last(), Some(None)) {
        trimmed.pop();
    }
    state.saved_custom_colors = trimmed.into_iter().map(slot_to_hex).collect();
    save(&state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_state_path(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "tensaku-state-{test_name}-{}-{nonce}",
                std::process::id()
            ))
            .join("state.toml")
    }

    #[test]
    fn clearing_legacy_preferences_preserves_incidental_state_and_scroll_chord() {
        let path = temporary_state_path("preserve-comments");
        let parent = path.parent().expect("temporary path should have a parent");
        fs::create_dir_all(parent).expect("temporary directory should be created");
        fs::write(
            &path,
            "# state heading\n\
             annotation-size-factor = 1.5 # migrated\n\
             invert-scrolling = false\n\
             select-any-annotation = true\n\
             close-on-esc = false\n\
             close-on-copy = true\n\
             close-on-save = true\n\
             hide-default-palette = false\n\
             sticky-session-defaults = true\n\
             park-pointer-during-manual-scroll-capture = false\n\
             keep-window-size-on-crop = true # inverse legacy key\n\
             scroll-capture-shortcut = \"ALT+PRINT\" # keep chord\n\
             spotlight-darkness = 0.7\n\
             future-incidental-setting = \"preserve me\" # unknown key\n\
             \n\
             [keybinds]\n\
             pointer = \"p\"\n\
             \n\
             [future-plugin-state]\n\
             enabled = true # unknown table comment\n",
        )
        .expect("legacy fixture should be written");

        clear_legacy_preferences_at_path(&path).expect("legacy cleanup should succeed");

        let output = fs::read_to_string(&path).expect("cleaned state should be readable");
        assert!(output.contains("# state heading"));
        assert!(output.contains("scroll-capture-shortcut = \"ALT+PRINT\" # keep chord"));
        assert!(output.contains("spotlight-darkness = 0.7"));
        assert!(output.contains("future-incidental-setting = \"preserve me\" # unknown key"));
        assert!(output.contains("[future-plugin-state]"));
        assert!(output.contains("enabled = true # unknown table comment"));
        let document = output
            .parse::<DocumentMut>()
            .expect("cleaned state should remain valid TOML");
        for key in LEGACY_PREFERENCE_KEYS {
            assert!(document.get(key).is_none(), "legacy key remained: {key}");
        }
        assert_eq!(
            document["scroll-capture-shortcut"].as_str(),
            Some("ALT+PRINT")
        );
        assert_eq!(
            document["future-incidental-setting"].as_str(),
            Some("preserve me")
        );
        assert_eq!(
            document["future-plugin-state"]["enabled"].as_bool(),
            Some(true)
        );
        assert!(
            fs::read_dir(parent)
                .expect("temporary directory should remain readable")
                .all(|entry| !entry
                    .expect("directory entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .contains("tensaku-tmp"))
        );

        fs::remove_dir_all(parent).expect("temporary directory should be removed");
    }

    #[test]
    fn missing_legacy_crop_window_preference_has_nothing_to_migrate() {
        assert_eq!(parse_legacy_keep_window_size_on_crop(""), None);
    }

    #[test]
    fn legacy_crop_window_preference_is_read_but_not_reserialized() {
        let input = "keep-window-size-on-crop = true\nlast-color = \"#ff0000\"\n";
        assert_eq!(parse_legacy_keep_window_size_on_crop(input), Some(true));

        let state: PersistedState = toml::from_str(input).expect("legacy state should parse");
        let encoded = toml::to_string(&state).expect("preference should serialize");
        assert!(!encoded.contains("keep-window-size-on-crop"));
    }
}
