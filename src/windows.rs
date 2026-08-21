//! The windows a capture can snap to.
//!
//! Region selection is fiddly when what you actually want is "that
//! window". Hyprland already knows where every window is, so ask it
//! rather than make the user trace an outline that the compositor
//! could have given exactly.
//!
//! Best-effort throughout, like [`crate::hypr`]: off Hyprland, or when
//! `hyprctl` isn't on PATH, the list comes back empty and the caller
//! falls back to dragging a region by hand.

use serde::Deserialize;
use std::process::Command;

/// A window the selection can snap to, in logical screen coordinates —
/// the same space the overlay's pointer events arrive in.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowTarget {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// For the hint line, so the user can tell which window is about to
    /// be taken when two overlap.
    pub title: String,
}

impl WindowTarget {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    pub fn area(&self) -> f64 {
        self.width * self.height
    }
}

#[derive(Deserialize)]
struct HyprWorkspace {
    id: i64,
}

#[derive(Deserialize)]
struct HyprClient {
    at: (i32, i32),
    size: (i32, i32),
    title: String,
    workspace: HyprWorkspace,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    mapped: bool,
}

#[derive(Deserialize)]
struct HyprMonitorWorkspace {
    id: i64,
}

#[derive(Deserialize)]
struct HyprMonitor {
    #[serde(rename = "activeWorkspace")]
    active_workspace: HyprMonitorWorkspace,
    focused: bool,
}

/// Every window currently visible on the focused monitor, front-most
/// first, or an empty list when the compositor can't be asked.
pub fn visible_windows() -> Vec<WindowTarget> {
    let Some(workspace) = focused_workspace() else {
        return Vec::new();
    };
    let Some(json) = hyprctl_json("clients") else {
        return Vec::new();
    };
    parse_clients(&json, workspace)
}

/// The active workspace of the focused monitor. Windows on other
/// workspaces are real to Hyprland but invisible on screen, and
/// snapping to one would select a rectangle of whatever is actually
/// showing there.
fn focused_workspace() -> Option<i64> {
    let json = hyprctl_json("monitors")?;
    let monitors: Vec<HyprMonitor> = serde_json::from_str(&json).ok()?;
    monitors
        .iter()
        .find(|m| m.focused)
        .map(|m| m.active_workspace.id)
}

fn hyprctl_json(command: &str) -> Option<String> {
    let output = Command::new("hyprctl")
        .args(["-j", command])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Windows on `workspace` that are actually on screen, ordered
/// smallest-first.
///
/// Smallest-first is what makes a dialog over its parent selectable:
/// the first window containing the pointer wins, and the small one has
/// to be asked about before the large one it sits on.
fn parse_clients(json: &str, workspace: i64) -> Vec<WindowTarget> {
    let Ok(clients) = serde_json::from_str::<Vec<HyprClient>>(json) else {
        return Vec::new();
    };
    let mut targets: Vec<WindowTarget> = clients
        .into_iter()
        .filter(|c| c.workspace.id == workspace && c.mapped && !c.hidden)
        .filter(|c| c.size.0 > 0 && c.size.1 > 0)
        .map(|c| WindowTarget {
            x: c.at.0 as f64,
            y: c.at.1 as f64,
            width: c.size.0 as f64,
            height: c.size.1 as f64,
            title: c.title,
        })
        .collect();
    targets.sort_by(|a, b| {
        a.area()
            .partial_cmp(&b.area())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    targets
}

/// The window under `(x, y)`, or `None` over bare desktop.
pub fn window_at(windows: &[WindowTarget], x: f64, y: f64) -> Option<&WindowTarget> {
    windows.iter().find(|w| w.contains(x, y))
}

#[cfg(test)]
mod tests {
    use super::{parse_clients, window_at};

    const CLIENTS: &str = r#"[
        {"at":[0,0],"size":[1920,1080],"title":"Editor","workspace":{"id":1},"mapped":true,"hidden":false},
        {"at":[400,300],"size":[600,400],"title":"Dialog","workspace":{"id":1},"mapped":true,"hidden":false},
        {"at":[0,0],"size":[800,600],"title":"Elsewhere","workspace":{"id":7},"mapped":true,"hidden":false},
        {"at":[10,10],"size":[300,200],"title":"Unmapped","workspace":{"id":1},"mapped":false,"hidden":false}
    ]"#;

    /// Only what is on screen: another workspace's windows are real to
    /// the compositor and invisible to the user.
    #[test]
    fn other_workspaces_and_unmapped_windows_are_skipped() {
        let windows = parse_clients(CLIENTS, 1);
        let titles: Vec<&str> = windows.iter().map(|w| w.title.as_str()).collect();
        assert_eq!(titles, vec!["Dialog", "Editor"]);
    }

    /// A dialog over its parent is what you meant to pick, so the
    /// smaller window has to be asked about first.
    #[test]
    fn the_smaller_window_wins_where_they_overlap() {
        let windows = parse_clients(CLIENTS, 1);
        assert_eq!(window_at(&windows, 500.0, 400.0).unwrap().title, "Dialog");
        // Outside the dialog, the window underneath it answers.
        assert_eq!(window_at(&windows, 100.0, 100.0).unwrap().title, "Editor");
    }

    #[test]
    fn bare_desktop_has_no_window() {
        let windows = parse_clients(CLIENTS, 1);
        assert!(window_at(&windows, 5000.0, 5000.0).is_none());
    }

    /// A compositor that answers with something unexpected leaves the
    /// user dragging a region by hand, not staring at a panic.
    #[test]
    fn unparseable_output_is_an_empty_list() {
        assert!(parse_clients("not json", 1).is_empty());
        assert!(parse_clients("{}", 1).is_empty());
    }
}
