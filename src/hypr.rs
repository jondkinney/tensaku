//! Best-effort Hyprland IPC for making a window resize *stick*.
//!
//! On Wayland a client cannot persist a self-chosen floating size: the
//! compositor treats `set_default_size` / `set_size_request` as transient
//! requests and reverts to the window's STORED floating size on the next
//! configure — notably when the user moves the window. That stored size is
//! set at map time and updated only by a user drag, so a programmatic
//! resize-on-crop visibly snaps back the first time the window is moved.
//!
//! Hyprland is the one exception: when *it* performs the resize (via its own
//! dispatch), it updates the stored size, so the new size survives a move.
//! This module sends that dispatch over Hyprland's IPC socket, targeting our
//! own toplevel by PID (no window-address lookup needed).
//!
//! Everything here is strictly best-effort. Off Hyprland, or if the socket /
//! dispatch API isn't what we expect (the Lua dispatch API is new and still
//! evolving — `hl.dsp.window.resize` on 0.55), [`resize_self`] returns `false`
//! and the caller falls back to the portable `set_default_size` path. It never
//! panics and never blocks for more than the socket timeouts below.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Cap on each socket operation so a wedged compositor can't stall the UI
/// thread. The real round-trip is sub-millisecond; this is only a safety net.
const IPC_TIMEOUT: Duration = Duration::from_millis(100);

/// Ask Hyprland to resize OUR window (matched by PID) to exactly `w × h`
/// logical px. Returns `true` only when Hyprland acknowledges with `ok`.
///
/// Best-effort: a missing env var, an unreachable socket, or any non-`ok`
/// reply yields `false` so the caller can fall back to the portable resize.
/// The compositor floors the width at the window's natural minimum (the
/// single-row toolbar width), which is exactly what we want for a narrow crop.
pub fn resize_self(w: i32, h: i32) -> bool {
    if w <= 0 || h <= 0 {
        return false;
    }
    let Some(sig) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") else {
        return false;
    };
    let sig = sig.to_string_lossy();
    let pid = std::process::id();
    let paths = socket_paths(&sig);
    // Two dispatch syntaxes, newest first, for resilience across Hyprland
    // versions (the dispatch API changed under us mid-development):
    //   * 0.50+ Lua object API — the `dispatch` verb is evaluated as
    //     `return hl.dispatch(<text>)`, so we pass a dispatcher OBJECT.
    //   * legacy string dispatcher (`resizewindowpixel exact W H,<window>`)
    //     used by older builds.
    // Both use `pid:N` to target our own toplevel and an absolute size. We try
    // the modern form first (what current Hyprland uses); only if it isn't
    // acknowledged do we try the legacy form — so older Hyprland ALSO gets the
    // stored-size update (and thus the hold-across-move behaviour) instead of
    // silently dropping to the portable fallback. On a version that honours
    // neither, every attempt returns false and the caller's `set_default_size`
    // path takes over. (On modern Hyprland the legacy line is never sent — the
    // first form already succeeded.)
    let commands = [
        format!(
            "dispatch hl.dsp.window.resize({{ x = {w}, y = {h}, relative = false, window = \"pid:{pid}\" }})"
        ),
        format!("dispatch resizewindowpixel exact {w} {h},pid:{pid}"),
    ];
    commands
        .iter()
        .any(|cmd| paths.iter().any(|path| dispatch_ok(path, cmd)))
}

/// Candidate IPC socket paths, newest layout first. Hyprland ≥ 0.40 keeps the
/// socket under `$XDG_RUNTIME_DIR/hypr/<sig>/`; older builds used `/tmp/hypr/`.
fn socket_paths(sig: &str) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        paths.push(format!("{}/hypr/{sig}/.socket.sock", rt.to_string_lossy()));
    }
    paths.push(format!("/tmp/hypr/{sig}/.socket.sock"));
    paths
}

/// Send one IPC command and report whether the reply was exactly `ok`.
/// Hyprland writes a short reply then closes the connection, so a read to EOF
/// gets the whole response.
fn dispatch_ok(path: &str, cmd: &str) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(IPC_TIMEOUT));
    let _ = stream.set_read_timeout(Some(IPC_TIMEOUT));
    if stream.write_all(cmd.as_bytes()).is_err() {
        return false;
    }
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    resp.trim() == "ok"
}
