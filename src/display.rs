//! Display / compositor integration helpers.
//!
//! Currently a single concern: discovering the user's display scale so
//! the first-run welcome dialog can pre-fill a sensible
//! `annotation_size_factor`. Hyprland is queried directly via
//! `hyprctl monitors -j` because GTK's integer `scale_factor` doesn't
//! expose fractional scales (1.25×, 1.5×) reliably.

use std::process::Command;
use std::sync::OnceLock;

/// The fractional scale the screenshot being edited was captured at,
/// resolved once and cached. `grim` hands us capture-native (device)
/// pixels with no scale metadata, so to map them back to logical
/// ("1×") pixels we need the compositor's fractional output scale —
/// GTK's `scale_factor()` only exposes the rounded-up integer
/// (a 1.07× output reports `2`, which would render the image at
/// roughly half size).
///
/// Resolution order: an explicit `input_scale` config override wins;
/// otherwise the focused Hyprland monitor's fractional `scale`.
/// Returns `None` when neither is available (not Hyprland, no
/// override) — callers then fall back to GTK's integer `scale_factor`,
/// which reproduces the pre-fractional behaviour exactly.
pub fn capture_scale() -> Option<f32> {
    static CACHE: OnceLock<Option<f32>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let input_scale = crate::APP_CONFIG.read().input_scale();
        if input_scale != 1.0 {
            Some(input_scale)
        } else {
            detect_hyprland_scale()
        }
    })
}

/// Try to read the focused monitor's scale from Hyprland. Returns
/// `None` when not running under Hyprland or when `hyprctl` is missing
/// / failing — callers should fall back to `1.0`.
///
/// We deliberately avoid pulling in `serde_json` for this one-shot
/// lookup; the JSON we care about is structurally trivial and the
/// inline parser below tracks just enough state to match each `"scale"`
/// to its surrounding monitor block's `"focused"` flag.
pub fn detect_hyprland_scale() -> Option<f32> {
    // Cheap pre-check: when this env var is missing we're not running
    // under Hyprland and shelling out would only produce noise.
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let output = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?;
    parse_scale(text)
}

/// The Hyprland monitor currently marked `focused` in `hyprctl monitors -j`.
///
/// Scroll capture uses `name` (the connector, e.g. `DP-3`) to pin its
/// layer-shell overlay, its screencopy source and its virtual pointer to the
/// same output. `None` outside Hyprland or when `hyprctl` fails.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct HyprlandMonitor {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f32,
    #[serde(default)]
    pub transform: i32,
    #[serde(default)]
    pub focused: bool,
}

pub fn hyprland_focused_monitor() -> Option<HyprlandMonitor> {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let output = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_focused_monitor(std::str::from_utf8(&output.stdout).ok()?)
}

/// The Hyprland monitor with connector `name`, e.g. `DP-3`.
///
/// The focused monitor can change while an overlay is up; anything that
/// has already committed to an output should resolve it by name.
pub fn hyprland_monitor_named(name: &str) -> Option<HyprlandMonitor> {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let output = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let monitors: Vec<HyprlandMonitor> =
        serde_json::from_str(std::str::from_utf8(&output.stdout).ok()?).ok()?;
    monitors.into_iter().find(|monitor| monitor.name == name)
}

/// The global cursor position in logical (layout) pixels, from
/// Hyprland. `None` outside Hyprland or when `hyprctl` fails — callers
/// treat that as "cannot watch the cursor" and skip whatever depended
/// on knowing where it is.
pub fn hyprland_cursor_position() -> Option<(f64, f64)> {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let output = Command::new("hyprctl")
        .args(["cursorpos", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value =
        serde_json::from_str(std::str::from_utf8(&output.stdout).ok()?).ok()?;
    Some((value.get("x")?.as_f64()?, value.get("y")?.as_f64()?))
}

/// Parse `hyprctl monitors -j` and return the focused monitor, falling back
/// to the first one when none is marked focused.
fn parse_focused_monitor(text: &str) -> Option<HyprlandMonitor> {
    let monitors: Vec<HyprlandMonitor> = serde_json::from_str(text).ok()?;
    monitors
        .iter()
        .find(|monitor| monitor.focused)
        .or_else(|| monitors.first())
        .cloned()
}

/// Logical (CSS-px) size of the focused Hyprland monitor. Used as the
/// window-size cap when GTK can't yet resolve which monitor a surface
/// is on (on Wayland `monitor_at_surface` returns nothing until the
/// compositor sends the surface's first output-enter, which is after
/// the window-sizing code needs the answer). Returns `None` when not
/// under Hyprland or `hyprctl` fails — callers fall back further.
/// Cached variant of [`hyprland_focused_logical_size`] for hot paths
/// (the canvas resize handler calls it on every relayout). A monitor
/// layout change mid-session won't be re-read until restart — fine
/// for its only use as a window-size clamp.
pub fn hyprland_focused_logical_size_cached() -> Option<(i32, i32)> {
    static CACHE: OnceLock<Option<(i32, i32)>> = OnceLock::new();
    *CACHE.get_or_init(hyprland_focused_logical_size)
}

pub fn hyprland_focused_logical_size() -> Option<(i32, i32)> {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let output = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_focused_logical_size(std::str::from_utf8(&output.stdout).ok()?)
}

/// Walk `hyprctl monitors -j` and return the focused monitor's
/// mode divided by its scale — its logical size — with width/height
/// swapped for 90°/270° transforms (odd transform values). Falls back
/// to the first monitor seen when none is marked focused.
fn parse_focused_logical_size(text: &str) -> Option<(i32, i32)> {
    let mut first: Option<(i32, i32)> = None;

    for chunk in text.split('}') {
        let mut width: Option<f32> = None;
        let mut height: Option<f32> = None;
        let mut scale: Option<f32> = None;
        let mut transform = 0i64;
        let mut focused = false;

        for raw in chunk.lines() {
            let line = raw.trim().trim_end_matches(',');
            if let Some(value) = line.strip_prefix("\"width\":") {
                width = value.trim().parse::<f32>().ok();
            } else if let Some(value) = line.strip_prefix("\"height\":") {
                height = value.trim().parse::<f32>().ok();
            } else if let Some(value) = line.strip_prefix("\"scale\":") {
                scale = value.trim().parse::<f32>().ok();
            } else if let Some(value) = line.strip_prefix("\"transform\":") {
                transform = value.trim().parse::<i64>().unwrap_or(0);
            } else if let Some(value) = line.strip_prefix("\"focused\":") {
                focused = value.trim() == "true";
            }
        }

        if let (Some(w), Some(h), Some(s)) = (width, height, scale)
            && w > 0.0
            && h > 0.0
            && s > 0.0
        {
            let (w, h) = if transform % 2 != 0 { (h, w) } else { (w, h) };
            let logical = ((w / s).round() as i32, (h / s).round() as i32);
            if first.is_none() {
                first = Some(logical);
            }
            if focused {
                return Some(logical);
            }
        }
    }
    first
}

/// Walk the `hyprctl monitors -j` output and return the focused
/// monitor's `"scale"`. Falls back to the first scale seen if nothing
/// is marked focused (edge states on multi-monitor setups).
///
/// We split on `}` so each chunk is one monitor's key/value lines —
/// hyprctl's pretty-printed output uses `},{` as the inter-monitor
/// boundary, which doesn't match a clean per-line `}` heuristic.
fn parse_scale(text: &str) -> Option<f32> {
    let mut first_scale: Option<f32> = None;

    for chunk in text.split('}') {
        let mut scale: Option<f32> = None;
        let mut focused = false;

        for raw in chunk.lines() {
            let line = raw.trim().trim_end_matches(',');
            if let Some(value) = line.strip_prefix("\"scale\":") {
                scale = value.trim().parse::<f32>().ok();
            } else if let Some(value) = line.strip_prefix("\"focused\":") {
                focused = value.trim() == "true";
            }
        }

        if let Some(s) = scale {
            if first_scale.is_none() {
                first_scale = Some(s);
            }
            if focused {
                return Some(s);
            }
        }
    }
    first_scale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_focused_monitor_from_hyprctl_json() {
        // Real `hyprctl monitors -j` shape (nested workspace objects included).
        let text = r#"[{
            "id": 0, "name": "DP-3", "width": 6144, "height": 3456, "x": 0, "y": 0,
            "activeWorkspace": {"id": 1, "name": "1"},
            "specialWorkspace": {"id": 0, "name": ""},
            "reserved": [0, 26, 0, 0], "scale": 2, "transform": 0, "focused": false
        },{
            "id": 1, "name": "HDMI-A-1", "width": 2560, "height": 1440, "x": 3072, "y": 0,
            "activeWorkspace": {"id": 2, "name": "2"},
            "specialWorkspace": {"id": 0, "name": ""},
            "reserved": [0, 0, 0, 0], "scale": 1, "transform": 0, "focused": true
        }]"#;
        let monitor = parse_focused_monitor(text).expect("monitor");
        assert_eq!(monitor.name, "HDMI-A-1");
        assert_eq!((monitor.x, monitor.y), (3072, 0));
        assert_eq!(monitor.scale, 1.0);
        let none_focused = text.replace("\"focused\": true", "\"focused\": false");
        assert_eq!(parse_focused_monitor(&none_focused).unwrap().name, "DP-3");
        assert!(parse_focused_monitor("not json").is_none());
    }

    #[test]
    fn parses_focused_monitor_scale() {
        // Trimmed shape of the real `hyprctl monitors -j` output —
        // two monitors, second one focused with a different scale.
        let json = r#"[{
    "id": 0,
    "name": "DP-1",
    "scale": 1.00,
    "focused": false
},{
    "id": 1,
    "name": "DP-3",
    "scale": 2.00,
    "focused": true
}]"#;
        assert_eq!(parse_scale(json), Some(2.0));
    }

    #[test]
    fn falls_back_to_first_when_nothing_focused() {
        let json = r#"[{
    "scale": 1.50,
    "focused": false
}]"#;
        assert_eq!(parse_scale(json), Some(1.5));
    }

    #[test]
    fn returns_none_on_unrelated_input() {
        assert_eq!(parse_scale("not json"), None);
    }

    #[test]
    fn logical_size_uses_focused_monitor_and_divides_by_scale() {
        let json = r#"[{
    "id": 0,
    "width": 1920,
    "height": 1080,
    "scale": 1.00,
    "transform": 0,
    "focused": false
},{
    "id": 1,
    "width": 6144,
    "height": 3456,
    "scale": 2.00,
    "transform": 0,
    "focused": true
}]"#;
        assert_eq!(parse_focused_logical_size(json), Some((3072, 1728)));
    }

    #[test]
    fn logical_size_swaps_axes_for_rotated_transform() {
        let json = r#"[{
    "width": 2560,
    "height": 1440,
    "scale": 1.00,
    "transform": 1,
    "focused": true
}]"#;
        assert_eq!(parse_focused_logical_size(json), Some((1440, 2560)));
    }

    #[test]
    fn logical_size_falls_back_to_first_when_nothing_focused() {
        let json = r#"[{
    "width": 3840,
    "height": 2160,
    "scale": 1.50,
    "transform": 0,
    "focused": false
}]"#;
        assert_eq!(parse_focused_logical_size(json), Some((2560, 1440)));
    }

    #[test]
    fn logical_size_returns_none_on_unrelated_input() {
        assert_eq!(parse_focused_logical_size("not json"), None);
    }
}
