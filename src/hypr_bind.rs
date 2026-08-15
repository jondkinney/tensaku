//! Register the scroll-capture shortcut as a Hyprland keybind.
//!
//! Two halves, both best-effort:
//!
//! * **Live** — apply the bind to the running compositor so it works
//!   immediately, without a reload. Hyprland's newer Lua config can't be
//!   poked with `hyprctl keyword` ("keyword can't work with non-legacy
//!   parsers. Use eval.") so we drive it through `hyprctl eval` and the
//!   Lua `hl.bind` / `hl.unbind` API. A legacy `hyprctl keyword bind`
//!   fallback covers older `.conf`-parser Hyprland.
//!
//! * **Persistent** — survive reboot and `omarchy-refresh-hyprland`.
//!   Tensaku isn't a daemon, so it can't re-apply the bind at runtime
//!   the way the cohort apps (vernier / hyprcorrect) do. Instead it
//!   drops a self-contained Lua file into Omarchy's
//!   `~/.local/state/omarchy/toggles/hypr/`, which stock `hyprland.lua`
//!   auto-requires on every start (`require("default.hypr.toggles")` →
//!   `require_all.files`) and which `omarchy-refresh-hyprland` leaves
//!   untouched. The file re-binds the chord every Hyprland start.
//!
//! Input chords are in the recorder's canonical `CTRL+SHIFT+ALT+SUPER+KEY`
//! form (see [`crate::chord_capture`]).

use std::path::PathBuf;
use std::process::Command;

use crate::omarchy_wrapper::is_omarchy;

/// Basename of the managed drop-in. Tensaku owns this file outright —
/// it's rewritten on every change and removed when the shortcut is
/// cleared.
const DROPIN_NAME: &str = "tensaku.lua";

/// Fixed bind description shown in `hyprctl binds` / Omarchy's keybinding
/// menu.
const DESCRIPTION: &str = "Tensaku scroll capture";

/// What [`register`] managed to do, so the caller can tell the user
/// whether the shortcut is live now, only persisted for next login, or
/// neither.
#[derive(Debug, Default, Clone, Copy)]
pub struct RegisterOutcome {
    /// The bind was applied to the running Hyprland session.
    pub live: bool,
    /// The persistent drop-in was written (it'll apply on next start).
    pub persisted: bool,
}

/// The command a bind fires. Prefer a stable, dedicated launcher from
/// `$PATH`: unlike a Cargo profile's `target/debug` or `target/release`
/// executable, that path remains correct when the active build changes.
/// Packaged installs normally have no dedicated launcher, so they retain the
/// absolute path of the running executable as a reliable fallback.
pub fn exec_command() -> String {
    exec_command_from(
        std::env::var_os("PATH").as_deref(),
        std::env::current_exe().ok(),
    )
}

fn exec_command_from(path: Option<&std::ffi::OsStr>, current_exe: Option<PathBuf>) -> String {
    build_exec_command(find_on_path("tensaku-scroll-capture", path), current_exe)
}

fn build_exec_command(launcher: Option<PathBuf>, current_exe: Option<PathBuf>) -> String {
    if let Some(launcher) = launcher {
        return shell_quote(&launcher.to_string_lossy());
    }

    let exe = current_exe
        .map(|p| shell_quote(&p.to_string_lossy()))
        .unwrap_or_else(|| "tensaku".to_string());
    format!("{exe} --scroll-capture")
}

fn find_on_path(program: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    path.and_then(|path| {
        std::env::split_paths(path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./".contains(&byte))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Refresh an already-recorded shortcut with the current stable launch
/// command. This migrates drop-ins that captured a one-off Cargo profile path
/// without requiring the user to record the chord again.
pub fn reconcile_saved_shortcut() {
    if let Some(chord) = crate::state::load_scroll_capture_shortcut() {
        let _ = register(&chord);
    }
}

/// Register `chord` (e.g. `"SUPER+SHIFT+S"`) so it launches scroll
/// capture: live in the running session and persisted for future starts.
/// Best-effort — see [`RegisterOutcome`].
pub fn register(chord: &str) -> RegisterOutcome {
    let cmd = exec_command();
    RegisterOutcome {
        live: register_live(chord, &cmd),
        persisted: persist(chord, &cmd),
    }
}

/// Remove `chord`'s bind: from the running session and from the
/// persistent drop-in. Best-effort and idempotent.
pub fn unregister(chord: &str) {
    unregister_live(chord);
    if let Some(path) = dropin_path() {
        let _ = std::fs::remove_file(path);
    }
}

// ===== live registration ==============================================

/// Apply the bind to the running compositor. Tries the Lua `eval` API
/// first (required by Hyprland's non-legacy config parser), then the
/// legacy `keyword` form for older builds. Returns whether either stuck.
fn register_live(chord: &str, cmd: &str) -> bool {
    if !in_hyprland() {
        return false;
    }
    let lua = format!(
        "hl.unbind(\"{chord}\"); hl.bind(\"{chord}\", hl.dsp.exec_cmd(\"{cmd}\"), {{description = \"{desc}\"}})",
        chord = lua_escape(&lua_chord(chord)),
        cmd = lua_escape(cmd),
        desc = lua_escape(DESCRIPTION),
    );
    if hyprctl_ok(&["eval", &lua]) {
        return true;
    }
    // Legacy `.conf`-parser Hyprland: `keyword` works, `eval`/`hl` don't.
    let (mods, key) = keyword_chord(chord);
    let _ = hyprctl_ok(&["keyword", "unbind", &format!("{mods}, {key}")]);
    hyprctl_ok(&["keyword", "bind", &format!("{mods}, {key}, exec, {cmd}")])
}

/// Drop the bind from the running compositor (both syntaxes).
fn unregister_live(chord: &str) {
    if !in_hyprland() {
        return;
    }
    let lua = format!("hl.unbind(\"{}\")", lua_escape(&lua_chord(chord)));
    if hyprctl_ok(&["eval", &lua]) {
        return;
    }
    let (mods, key) = keyword_chord(chord);
    let _ = hyprctl_ok(&["keyword", "unbind", &format!("{mods}, {key}")]);
}

// ===== persistent drop-in =============================================

/// Write the managed Lua drop-in so the bind survives reboot and
/// `omarchy-refresh-hyprland`. Omarchy-only (the toggles dir is an
/// Omarchy mechanism); returns whether the file was written.
fn persist(chord: &str, cmd: &str) -> bool {
    let Some(path) = dropin_path() else {
        return false;
    };
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let contents = dropin_contents(chord, cmd);
    std::fs::write(&path, contents).is_ok()
}

/// The body of the managed drop-in: re-bind the chord on every Hyprland
/// start. Uses Omarchy's `o.bind` helper (available here because the
/// toggles file is required after `default.hypr.omarchy` loads it), with
/// an `hl.unbind` first so it cleanly overrides any prior bind on the
/// same combo — matching Omarchy's documented override pattern.
fn dropin_contents(chord: &str, cmd: &str) -> String {
    let lua_chord = lua_escape(&lua_chord(chord));
    format!(
        "-- Managed by Tensaku — do NOT edit by hand.\n\
         -- Regenerated when you change the scroll-capture shortcut in\n\
         -- Tensaku's Preferences, and removed when you clear it.\n\
         -- Auto-loaded every Hyprland start via Omarchy's\n\
         -- require(\"default.hypr.toggles\"); survives omarchy-refresh-hyprland.\n\
         hl.unbind(\"{lua_chord}\")\n\
         o.bind(\"{lua_chord}\", \"{desc}\", \"{cmd}\")\n",
        desc = lua_escape(DESCRIPTION),
        cmd = lua_escape(cmd),
    )
}

/// `~/.local/state/omarchy/toggles/hypr/tensaku.lua`, or `None` when this
/// isn't an Omarchy session or the state dir can't be resolved.
fn dropin_path() -> Option<PathBuf> {
    if !is_omarchy() {
        return None;
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(
        base.join("omarchy")
            .join("toggles")
            .join("hypr")
            .join(DROPIN_NAME),
    )
}

// ===== chord formatting ===============================================

/// Convert the recorder's `CTRL+SHIFT+ALT+SUPER+KEY` chord into
/// Hyprland's Lua bind string `"MOD + MOD + KEY"` (with the key mapped to
/// its XKB keysym name).
fn lua_chord(chord: &str) -> String {
    let (mods, key) = split_chord(chord);
    let mut parts: Vec<String> = mods.iter().map(|m| (*m).to_string()).collect();
    parts.push(hypr_key(key));
    parts.join(" + ")
}

/// The legacy `keyword` form: `("MOD MOD", "Key")`.
fn keyword_chord(chord: &str) -> (String, String) {
    let (mods, key) = split_chord(chord);
    (mods.join(" "), hypr_key(key))
}

/// Split a chord into its modifier tokens and the trigger key (the last
/// `+`-separated token).
fn split_chord(chord: &str) -> (Vec<&str>, &str) {
    let mut tokens: Vec<&str> = chord.split('+').filter(|t| !t.is_empty()).collect();
    let key = tokens.pop().unwrap_or("");
    (tokens, key)
}

/// Map a recorder key token to the XKB keysym name Hyprland's bind
/// parser expects. Single letters / function keys / unknown keysym names
/// pass through unchanged (Hyprland matches them case-insensitively).
fn hypr_key(token: &str) -> String {
    match token {
        "ESC" => "Escape",
        "ENTER" => "Return",
        "TAB" => "Tab",
        "BACKSPACE" => "BackSpace",
        "DELETE" => "Delete",
        "SPACE" => "Space",
        "UP" => "Up",
        "DOWN" => "Down",
        "LEFT" => "Left",
        "RIGHT" => "Right",
        "PLUS" => "plus",
        "MINUS" => "minus",
        "EQUAL" => "equal",
        other => other,
    }
    .to_string()
}

/// Escape a string for embedding in a double-quoted Lua literal.
fn lua_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ===== hyprctl plumbing ===============================================

fn in_hyprland() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

/// Run `hyprctl <args>` and report whether it acknowledged with `ok`.
///
/// Checking the output text (not just the exit code) is essential:
/// `hyprctl keyword` on a Lua config prints "keyword can't work with
/// non-legacy parsers. Use eval." and still **exits 0**, so an exit-code
/// check would read that failure as success.
fn hyprctl_ok(args: &[&str]) -> bool {
    Command::new("hyprctl")
        .args(args)
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .eq_ignore_ascii_case("ok")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_chord_joins_with_spaces_and_maps_key() {
        assert_eq!(lua_chord("SUPER+SHIFT+S"), "SUPER + SHIFT + S");
        assert_eq!(lua_chord("CTRL+ENTER"), "CTRL + Return");
        assert_eq!(lua_chord("ALT+SPACE"), "ALT + Space");
        assert_eq!(lua_chord("F5"), "F5");
    }

    #[test]
    fn keyword_chord_space_joins_mods() {
        assert_eq!(
            keyword_chord("SUPER+SHIFT+S"),
            ("SUPER SHIFT".to_string(), "S".to_string())
        );
        assert_eq!(
            keyword_chord("CTRL+ESC"),
            ("CTRL".to_string(), "Escape".to_string())
        );
    }

    #[test]
    fn lua_escape_handles_quotes_and_backslashes() {
        assert_eq!(lua_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn dropin_contents_rebinds_and_overrides() {
        let body = dropin_contents("SUPER+SHIFT+S", "/usr/bin/tensaku --scroll-capture");
        assert!(body.contains("hl.unbind(\"SUPER + SHIFT + S\")"));
        assert!(body.contains(
            "o.bind(\"SUPER + SHIFT + S\", \"Tensaku scroll capture\", \"/usr/bin/tensaku --scroll-capture\")"
        ));
    }

    #[test]
    fn stable_launcher_wins_over_cargo_profile_executable() {
        assert_eq!(
            build_exec_command(
                Some(PathBuf::from(
                    "/home/user/.local/bin/tensaku-scroll-capture"
                )),
                Some(PathBuf::from("/checkout/target/debug/tensaku")),
            ),
            "/home/user/.local/bin/tensaku-scroll-capture"
        );
    }

    #[test]
    fn current_executable_is_the_packaged_fallback() {
        assert_eq!(
            build_exec_command(None, Some(PathBuf::from("/usr/bin/tensaku"))),
            "/usr/bin/tensaku --scroll-capture"
        );
    }

    #[test]
    fn launcher_paths_are_shell_quoted() {
        assert_eq!(
            build_exec_command(
                Some(PathBuf::from(
                    "/home/user/path with ' quote/tensaku-scroll-capture",
                )),
                None,
            ),
            "'/home/user/path with '\\'' quote/tensaku-scroll-capture'"
        );
    }
}
