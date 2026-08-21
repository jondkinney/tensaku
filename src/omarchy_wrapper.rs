//! Omarchy screenshot-wrapper integration.
//!
//! Modern Omarchy runs `tensaku-edit` by default and packages Tensaku's
//! window rules, so a normal package install needs no wiring step. Tensaku
//! still ships the small `tensaku-edit` adapter because the editor receives a
//! positional image path while Tensaku expects `--filename`.
//!
//! This module retains two recovery/compatibility paths:
//!
//! - [`run`] — the explicit `--install-omarchy-wrapper` flag; installs,
//!   then reports and verifies the `$OMARCHY_SCREENSHOT_EDITOR` wiring.
//! - [`ensure_first_launch`] — silent and one-shot, on the first normal
//!   launch, when Omarchy is detected.
//!
//! The legacy [`wire`] path edits pre-Lua Omarchy/Hyprland configuration;
//! current Omarchy users should not need it.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use xdg::BaseDirectories;

use crate::desktop_install::xdg_data_home;
use crate::doctor::on_path;

/// The wrapper script, embedded so the install works from a `cargo
/// install`ed binary with no repo checkout present.
const WRAPPER: &str = include_str!("../assets/tensaku-edit");

/// Basename of the wrapper Omarchy is wired to invoke.
const WRAPPER_NAME: &str = "tensaku-edit";

/// Is this an Omarchy session? `$OMARCHY_PATH` is the canonical signal
/// Omarchy exports; the data-dir check is a fallback for a shell that
/// didn't inherit it.
pub(crate) fn is_omarchy() -> bool {
    let data_home = xdg_data_home().ok();
    is_omarchy_with(
        std::env::var_os("OMARCHY_PATH").as_deref(),
        data_home.as_deref(),
    )
}

/// Pure core of [`is_omarchy`], split out so it can be unit-tested
/// without mutating the process environment.
fn is_omarchy_with(omarchy_path: Option<&OsStr>, data_home: Option<&Path>) -> bool {
    omarchy_path.is_some_and(|p| !p.is_empty())
        || data_home.is_some_and(|d| d.join("omarchy").is_dir())
}

/// `~/.local/bin/tensaku-edit` — where Omarchy expects the editor
/// wrapper to live.
pub(crate) fn wrapper_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/bin").join(WRAPPER_NAME))
}

/// Write the wrapper and mark it executable. Returns its path.
fn install() -> Result<PathBuf> {
    let path = wrapper_path()?;
    let dir = path.parent().expect("wrapper path always has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(&path, WRAPPER).with_context(|| format!("write {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))?;
    Ok(path)
}

/// How `$OMARCHY_SCREENSHOT_EDITOR` relates to our wrapper.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wiring {
    /// Points at our wrapper — captures will open in Tensaku.
    Correct,
    /// Set, but to some other editor (carried for the warning message).
    Elsewhere(PathBuf),
    /// Not set at all.
    Unset,
}

/// Classify `$OMARCHY_SCREENSHOT_EDITOR` against `wrapper`. Pure (takes
/// the env value as an argument) so it can be unit-tested, and total: it
/// always returns a classification, never an error.
///
/// The value may be unset, carry trailing arguments, use a leading `~/`,
/// or name a path that doesn't exist yet — so we compare the first
/// whitespace token, expand a leading tilde, and fall back to plain path
/// equality when `canonicalize` can't resolve a (possibly missing) path.
pub(crate) fn classify_wiring(env_val: Option<OsString>, wrapper: &Path) -> Wiring {
    let raw = match env_val {
        Some(v) if !v.is_empty() => v,
        _ => return Wiring::Unset,
    };
    let value = raw.to_string_lossy();
    let Some(first) = value.split_whitespace().next() else {
        return Wiring::Unset;
    };

    // Env vars aren't tilde-expanded the way a shell expands them, so
    // handle a literal leading `~/` ourselves.
    let candidate = match first.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(first),
        },
        None => PathBuf::from(first),
    };
    // Modern Omarchy's healthy default is the bare command `tensaku-edit`,
    // not an absolute path. Resolve bare commands through PATH before
    // comparing them with the installed wrapper.
    let candidate = if candidate.components().count() == 1 {
        which(first).unwrap_or(candidate)
    } else {
        candidate
    };

    let same_path = match (
        std::fs::canonicalize(&candidate).ok(),
        std::fs::canonicalize(wrapper).ok(),
    ) {
        (Some(a), Some(b)) => a == b,
        // One or both paths don't exist yet — compare as written.
        _ => candidate == wrapper,
    };
    // Packaged, cargo-installed, and user-local copies are all valid
    // `tensaku-edit` adapters. Their paths may differ while still naming the
    // integration Omarchy expects.
    let same_wrapper_name = candidate.file_name() == Some(OsStr::new(WRAPPER_NAME))
        && wrapper.file_name() == Some(OsStr::new(WRAPPER_NAME))
        && candidate.is_file()
        && wrapper.is_file();

    if same_path || same_wrapper_name {
        Wiring::Correct
    } else {
        Wiring::Elsewhere(candidate)
    }
}

/// Print the steps to point `$OMARCHY_SCREENSHOT_EDITOR` at the wrapper.
fn print_wiring_help(wrapper: &Path) {
    println!("Point it at the wrapper to open captures in Tensaku — add to");
    println!("~/.config/hypr/envs.conf:");
    println!("  env = OMARCHY_SCREENSHOT_EDITOR,{}", wrapper.display());
    println!("then run: hyprctl reload");
}

/// `--install-omarchy-wrapper`: install the wrapper and report what
/// landed, then check the `$OMARCHY_SCREENSHOT_EDITOR` wiring.
pub fn run() -> Result<()> {
    let path = wrapper_path()?;
    let existed = path.exists();
    install()?;

    if existed {
        println!("Updated Omarchy screenshot wrapper (overwrote existing):");
    } else {
        println!("Installed Omarchy screenshot wrapper:");
    }
    println!("  {}", path.display());
    println!();

    if !is_omarchy() {
        println!(
            "Note: this doesn't look like an Omarchy session ($OMARCHY_PATH unset and no\n\
             ~/.local/share/omarchy). The wrapper is installed anyway."
        );
        println!();
    }

    match classify_wiring(std::env::var_os("OMARCHY_SCREENSHOT_EDITOR"), &path) {
        Wiring::Correct => {
            println!("OMARCHY_SCREENSHOT_EDITOR already points at the wrapper — you're set.");
        }
        Wiring::Elsewhere(other) => {
            println!(
                "OMARCHY_SCREENSHOT_EDITOR points at {}, not the Tensaku wrapper.",
                other.display()
            );
            print_wiring_help(&path);
        }
        Wiring::Unset if omarchy_capture_defaults_to_tensaku() => {
            println!(
                "Omarchy uses tensaku-edit by default — no OMARCHY_SCREENSHOT_EDITOR override is needed."
            );
        }
        Wiring::Unset => {
            println!("This older/custom Omarchy setup has no screenshot editor configured.");
            print_wiring_help(&path);
        }
    }

    if !on_path("tensaku") {
        println!();
        println!("Warning: `tensaku` isn't on $PATH, so the wrapper's `exec tensaku` will");
        println!("fail when Omarchy invokes it. Put Tensaku's install dir on $PATH.");
    }

    Ok(())
}

/// Install the wrapper silently on the first normal launch, when Omarchy
/// is detected, so a fresh Omarchy setup needs no manual step.
///
/// One-shot via a marker in the XDG state dir, and silent: this runs
/// during a GUI launch, so the verbose `--install-omarchy-wrapper` path
/// is where the wiring is reported. Best-effort throughout — wrapper
/// housekeeping must never break startup, so any failure is swallowed
/// (and left to retry next launch). Skips Flatpak (sandboxed) and never
/// overwrites a wrapper the user already has.
pub fn ensure_first_launch() {
    let _ = try_ensure_first_launch();
}

fn try_ensure_first_launch() -> Result<()> {
    // Sandboxed: ~/.local/bin isn't meaningful inside a Flatpak.
    if std::env::var_os("FLATPAK_ID").is_some() {
        return Ok(());
    }

    // Only auto-install on Omarchy — anywhere else the wrapper is inert.
    if !is_omarchy() {
        return Ok(());
    }

    // The marker means the one-time first-launch step is already done.
    // `place_state_file` also creates the state dir for the write below.
    let marker = BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"))
        .place_state_file("omarchy-wrapper-done")
        .context("locate the XDG state dir")?;
    if marker.exists() {
        return Ok(());
    }

    // Never clobber a wrapper the user (or a previous run) already placed,
    // and skip when a packaged wrapper (e.g. /usr/bin/tensaku-edit) already
    // provides it — a per-user copy would only shadow the system one.
    // --install-omarchy-wrapper is the explicit path for a reset.
    if !wrapper_path()?.exists() && !packaged_wrapper_exists() {
        install()?;
    }

    // Record completion last, so a failed install above is retried on the
    // next launch rather than marked done.
    std::fs::write(
        &marker,
        "Tensaku ran its one-time first-launch Omarchy wrapper install.\n",
    )
    .with_context(|| format!("write {}", marker.display()))?;
    Ok(())
}

/// Find an executable named `bin` on `$PATH`, returning its full path.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

/// Does the installed Omarchy capture script fall back to `tensaku-edit`
/// when no explicit `$OMARCHY_SCREENSHOT_EDITOR` override is present?
pub(crate) fn omarchy_capture_defaults_to_tensaku() -> bool {
    which("omarchy-capture-screenshot")
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|script| capture_script_defaults_to_tensaku(&script))
}

/// Pure parser for [`omarchy_capture_defaults_to_tensaku`]. Ignore comments
/// and whitespace so formatting-only changes in Omarchy do not break the
/// detection, while avoiding a false positive from documentation text.
fn capture_script_defaults_to_tensaku(script: &str) -> bool {
    script.lines().any(|line| {
        let line = line.trim_start();
        if line.starts_with('#') {
            return false;
        }
        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        compact.starts_with("SCREENSHOT_EDITOR=")
            && compact.contains("${OMARCHY_SCREENSHOT_EDITOR:-tensaku-edit}")
    })
}

/// The wrapper path to wire `$OMARCHY_SCREENSHOT_EDITOR` at: a packaged
/// `tensaku-edit` on `$PATH` (e.g. `/usr/bin/tensaku-edit`) wins; otherwise
/// ensure the per-user copy exists and use that. Wiring at a missing file
/// would be pointless, so this never returns a path that doesn't exist.
fn find_or_install_wrapper() -> Result<PathBuf> {
    if let Some(p) = which(WRAPPER_NAME) {
        return Ok(p);
    }
    let p = wrapper_path()?;
    if !p.exists() {
        install()?;
    }
    Ok(p)
}

/// The wrapper that is actually present, if any — a packaged `tensaku-edit`
/// on `$PATH` (e.g. `/usr/bin`) wins, else the per-user copy if it exists.
/// Read-only: unlike [`find_or_install_wrapper`] it installs nothing, so
/// `--doctor` can report the true state without side effects.
pub(crate) fn installed_wrapper() -> Option<PathBuf> {
    if let Some(p) = which(WRAPPER_NAME) {
        return Some(p);
    }
    match wrapper_path() {
        Ok(p) if p.exists() => Some(p),
        _ => None,
    }
}

/// Does a system install already provide the wrapper? A package (AUR /
/// `make install`) drops `tensaku-edit` into a system bindir on `$PATH`
/// (e.g. `/usr/bin`); a user-local copy would only shadow it. True when a
/// `tensaku-edit` on `$PATH` resolves to something other than our per-user
/// path.
fn packaged_wrapper_exists() -> bool {
    match (which(WRAPPER_NAME), wrapper_path()) {
        (Some(found), Ok(ours)) => found != ours,
        (Some(_), Err(_)) => true,
        _ => false,
    }
}

/// Legacy pre-Lua Omarchy's `$XDG_CONFIG_HOME/hypr/envs.conf`, falling back
/// to `~/.config/...`.
fn hypr_config_dir() -> Result<PathBuf> {
    let base = if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        PathBuf::from(dir)
    } else {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        PathBuf::from(home).join(".config")
    };
    Ok(base.join("hypr"))
}

/// Omarchy's personal-overrides file. Everything Tensaku writes goes
/// here: it is the one config file Omarchy never refreshes, so an
/// `omarchy refresh hyprland` can't drop our wiring.
fn hypr_local_lua() -> Result<PathBuf> {
    Ok(hypr_config_dir()?.join("local.lua"))
}

/// The config entry point, which has to `require` the overrides file.
fn hypr_main_lua() -> Result<PathBuf> {
    Ok(hypr_config_dir()?.join("hyprland.lua"))
}

/// Sibling `bindings.lua`, used only to detect conflicting inline binds.
fn hypr_bindings_lua() -> Result<PathBuf> {
    Ok(hypr_config_dir()?.join("bindings.lua"))
}

/// The canonical Hyprland env directive wiring the screenshot editor.
fn desired_env_line(wrapper: &str) -> String {
    format!("hl.env(\"OMARCHY_SCREENSHOT_EDITOR\", \"{wrapper}\")")
}

/// If `line` is an `env = OMARCHY_SCREENSHOT_EDITOR,<value>` directive,
/// return its `<value>` (trimmed). Comments (`#…`) don't match because
/// they don't start with `env`.
fn env_line_value(line: &str) -> Option<String> {
    let t = line.trim();
    if t.starts_with("--") {
        return None; // Lua comment
    }
    let rest = t.strip_prefix("hl.env")?.trim_start();
    let rest = rest.strip_prefix('(')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let rest = rest.strip_prefix("OMARCHY_SCREENSHOT_EDITOR")?;
    let rest = rest.strip_prefix('"')?.trim_start();
    let rest = rest.strip_prefix(',')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let value = rest.split('"').next()?;
    Some(value.to_string())
}

/// The `OMARCHY_SCREENSHOT_EDITOR` value configured in `local.lua`, if
/// any.
///
/// Unlike the live `$OMARCHY_SCREENSHOT_EDITOR` (which reflects the running
/// session and goes stale after `--wire-omarchy` until the next login), this
/// is the *persistent* override. First
/// matching directive wins, mirroring [`apply_env_line`]. Read-only and
/// best-effort: a missing or unreadable file reads as "not configured".
pub(crate) fn configured_editor() -> Option<OsString> {
    let path = hypr_local_lua().ok()?;
    let contents = std::fs::read_to_string(path).ok()?;
    contents
        .lines()
        .find_map(env_line_value)
        .map(OsString::from)
}

/// An inline `env OMARCHY_SCREENSHOT_EDITOR=<value>` prefix on an `exec`
/// bind, returned as `<value>`. This is the per-command form (NAME=value),
/// distinct from the envs.conf directive form (NAME,value).
fn inline_bind_editor_value(line: &str) -> Option<String> {
    let marker = "OMARCHY_SCREENSHOT_EDITOR=";
    let after = &line[line.find(marker)? + marker.len()..];
    match after.split_whitespace().next() {
        Some(v) if !v.is_empty() => Some(v.to_string()),
        _ => None,
    }
}

/// Outcome of reconciling envs.conf with the desired wiring.
#[derive(Debug, PartialEq, Eq)]
enum EnvLineOutcome {
    /// The correct line is already present — nothing to write.
    AlreadySet,
    /// An existing line pointed elsewhere and was rewritten.
    Updated(String),
    /// No line existed; one was appended.
    Inserted(String),
}

/// Reconcile `contents` (an envs.conf) with the desired wiring. Pure, so
/// the line-rewriting rules can be unit-tested without touching the disk.
fn apply_env_line(contents: &str, wrapper: &str) -> EnvLineOutcome {
    let desired = desired_env_line(wrapper);
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();

    if let Some(idx) = lines.iter().position(|l| env_line_value(l).is_some()) {
        if env_line_value(&lines[idx]).as_deref() == Some(wrapper) {
            return EnvLineOutcome::AlreadySet;
        }
        lines[idx] = desired;
        let mut out = lines.join("\n");
        if contents.ends_with('\n') {
            out.push('\n');
        }
        EnvLineOutcome::Updated(out)
    } else {
        let mut out = contents.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("-- Tensaku screenshot editor (set by `tensaku --wire-omarchy`).\n");
        out.push_str(&desired);
        out.push('\n');
        EnvLineOutcome::Inserted(out)
    }
}

/// `<path>.bak.<unix-seconds>`, matching Omarchy's backup convention.
fn backup_path(file: &Path) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    file.with_file_name(format!("{name}.bak.{secs}"))
}

/// Set the var in the running Hyprland session via `hyprctl keyword env`,
/// which propagates to processes Hyprland spawns afterward. No-op (with a
/// note) when not in a Hyprland session or `hyprctl` is unavailable.
fn apply_live(wrapper: &str) {
    if !in_hyprland() {
        println!("(not in a Hyprland session — this takes effect on next Hyprland start.)");
        return;
    }
    // `eval`, not `keyword`: a Lua-configured Hyprland rejects `keyword`
    // outright ("keyword can't work with non-legacy parsers"). And it
    // reports that failure on stdout while still exiting 0, so trusting
    // the exit status alone claims success for a command that did
    // nothing — check for hyprctl's "ok" instead.
    //
    // `.output()` (not `.status()`) also keeps hyprctl's own chatter out
    // of our report.
    let ok = std::process::Command::new("hyprctl")
        .arg("eval")
        .arg(desired_env_line(wrapper))
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "ok")
        .unwrap_or(false);
    if ok {
        println!("Applied to the running Hyprland session (effective immediately).");
    } else {
        println!("(couldn't apply live via hyprctl — takes effect on next Hyprland start.)");
    }
}

/// Warn if a screenshot bind sets `OMARCHY_SCREENSHOT_EDITOR` inline to
/// something other than `wrapper`: such a prefix overrides both envs.conf
/// and the live env, so the wiring wouldn't take effect for that bind. We
/// don't edit bindings.conf (by design) — just flag it.
fn warn_conflicting_binds(wrapper: &str) {
    let Ok(path) = hypr_bindings_lua() else {
        return;
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    let conflicts: Vec<String> = contents
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(inline_bind_editor_value)
        .filter(|v| v != wrapper)
        .collect();
    if let Some(first) = conflicts.first() {
        println!();
        println!(
            "Warning: {} screenshot bind(s) in {} set OMARCHY_SCREENSHOT_EDITOR inline",
            conflicts.len(),
            path.display()
        );
        println!("(e.g. → {first}), which overrides what was just set. Remove the inline");
        println!("`env OMARCHY_SCREENSHOT_EDITOR=…` prefix from those binds for it to apply.");
    }
}

/// True when we're in a Hyprland session with `hyprctl` available.
fn in_hyprland() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() && which("hyprctl").is_some()
}

/// Reload Hyprland config so newly-written window rules take effect for
/// the next window (rules, unlike `env =`, are re-applied on reload).
fn hypr_reload() {
    if in_hyprland() {
        let _ = std::process::Command::new("hyprctl").arg("reload").output();
    }
}

/// Surface any Hyprland config errors after a reload (best-effort).
fn report_config_errors() {
    if !in_hyprland() {
        return;
    }
    if let Ok(out) = std::process::Command::new("hyprctl")
        .arg("configerrors")
        .output()
    {
        let errs = String::from_utf8_lossy(&out.stdout);
        let errs = errs.trim();
        if !errs.is_empty() && !errs.to_lowercase().contains("no errors") {
            println!();
            println!("Note: Hyprland reports config issues after reload:");
            println!("{errs}");
        }
    }
}

/// Tensaku's Hyprland window class — what window rules match on. Matches
/// the desktop entry's `StartupWMClass`.
const WINDOW_CLASS: &str = "dev.tensaku.Tensaku";

/// `$XDG_CONFIG_HOME/hypr/hyprland.conf` (sibling of envs.conf).
/// Is there an uncommented `windowrule = <action>, match:class <our class>`?
fn has_class_rule(contents: &str, action: &str) -> bool {
    window_rule_calls(contents)
        .iter()
        .any(|call| call.contains(WINDOW_CLASS) && call.contains(action))
}

/// Every `o.window(...)` call in `contents`, comments stripped and each
/// call flattened onto one line.
///
/// Lua rules are routinely written across several lines, so scanning a
/// line at a time sees `o.window("dev.tensaku.Tensaku", {` and
/// `float = true,` as unrelated and concludes the rule is missing —
/// which appends a duplicate to a config that was already wired.
fn window_rule_calls(contents: &str) -> Vec<String> {
    let stripped = contents
        .lines()
        .map(|l| match l.find("--") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut calls = Vec::new();
    let mut rest = stripped.as_str();
    while let Some(idx) = rest.find("o.window") {
        let after = &rest[idx..];
        // Take to the paren that closes the call, so the whole rule
        // body is considered however it is laid out.
        let mut depth = 0i32;
        let mut end = after.len();
        for (i, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        calls.push(
            after[..end]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        );
        rest = &after[end..];
    }
    calls
}

/// The float/center/opacity rules that let Tensaku size its own window
/// and render it opaque.
fn window_rules_block() -> String {
    format!(
        "\n-- Tensaku: float + center its window. Tensaku sizes its own window\n\
         -- around the capture, so it must float with no fixed-size rule. The\n\
         -- `-floating-window` tag undoes an Omarchy default that would\n\
         -- otherwise pin a fixed size. Added by `tensaku --wire-omarchy`.\n\
         o.window(\"{WINDOW_CLASS}\", {{ tag = \"-floating-window\", float = true, center = true }})\n\
         -- Force full opacity. Omarchy tags every window `default-opacity` and\n\
         -- then applies `opacity 0.985 0.96`, so ~1.5%% of whatever sits behind\n\
         -- the window blends into it -- and an image editor is the one place\n\
         -- that must not happen. Over a dark window behind, Tensaku's canvas\n\
         -- background #2E3440 composites to #2D333F, which reads as banding or\n\
         -- ghosting across large flat areas of the canvas and looks like a\n\
         -- rendering fault in the image being edited, even though what gets\n\
         -- saved is untouched. Steam, qemu and retroarch opt out the same way.\n\
         o.window(\"{WINDOW_CLASS}\", {{ tag = \"-default-opacity\", opacity = \"1 1\" }})\n"
    )
}

/// `hyprland.lua` only loads `local.lua` if it says so. Return the new
/// contents when the `require` has to be added, `None` when it is
/// already there.
///
/// Omarchy's own template ends with this line, so in practice it is
/// present; adding it covers a hand-rolled config, and keeps our wiring
/// from silently doing nothing.
fn ensure_local_require(contents: &str) -> Option<String> {
    let present = contents.lines().any(|l| {
        let t = l.trim();
        !t.starts_with("--") && t.contains("require") && t.contains("hypr.local")
    });
    if present {
        return None;
    }
    let mut out = contents.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(
        "\n-- Personal overrides, including Tensaku's wiring. Added by\n\
         -- `tensaku --wire-omarchy`.\n\
         require(\"hypr.local\")\n",
    );
    Some(out)
}

/// How `local.lua` relates to Tensaku's window rules.
#[derive(Debug, PartialEq, Eq)]
enum WindowRuleOutcome {
    /// float + center for our class already present — nothing to add.
    AlreadyPresent,
    /// Rules appended; carries the new file contents.
    Appended(String),
}

/// Append Tensaku's window rules unless float + center for our class are
/// already present (so a hand-written setup isn't duplicated). Pure.
fn apply_window_rules(contents: &str) -> WindowRuleOutcome {
    if has_class_rule(contents, "float") && has_class_rule(contents, "-default-opacity") {
        return WindowRuleOutcome::AlreadyPresent;
    }
    let mut out = contents.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&window_rules_block());
    WindowRuleOutcome::Appended(out)
}

/// `--wire-omarchy`: point `$OMARCHY_SCREENSHOT_EDITOR` at the wrapper —
/// persistently in Hyprland's envs.conf, and live in the running session —
/// and float + center the Tensaku window. Ensures the wrapper exists
/// first; never edits keybinds.
pub fn wire() -> Result<()> {
    let main_lua = hypr_main_lua()?;
    if !main_lua.exists() {
        anyhow::bail!(
            "no {} — this expects a Lua-configured Hyprland (Omarchy 3+).",
            main_lua.display()
        );
    }
    let wrapper = find_or_install_wrapper()?;
    let wrapper_str = wrapper.to_string_lossy().into_owned();

    // Everything goes in local.lua: Omarchy never refreshes it, so an
    // `omarchy refresh hyprland` can't drop the wiring.
    let local = hypr_local_lua()?;
    let existing = std::fs::read_to_string(&local).unwrap_or_default();
    let mut updated = match apply_env_line(&existing, &wrapper_str) {
        EnvLineOutcome::AlreadySet => {
            println!("local.lua already wires OMARCHY_SCREENSHOT_EDITOR → {wrapper_str}");
            existing.clone()
        }
        EnvLineOutcome::Updated(new) | EnvLineOutcome::Inserted(new) => {
            println!("Setting OMARCHY_SCREENSHOT_EDITOR → {wrapper_str}");
            new
        }
    };

    // Float + center so Tensaku can size itself around the capture, and
    // full opacity so the window behind doesn't blend into the canvas.
    match apply_window_rules(&updated) {
        WindowRuleOutcome::AlreadyPresent => {
            println!("local.lua already floats, centers and un-dims the Tensaku window.");
        }
        WindowRuleOutcome::Appended(new) => {
            println!("Adding float + center + opacity rules for {WINDOW_CLASS}");
            updated = new;
        }
    }

    let changed = updated != existing;
    if changed {
        if local.exists() {
            let backup = backup_path(&local);
            std::fs::copy(&local, &backup)
                .with_context(|| format!("back up {}", local.display()))?;
            println!("Backed up {} → {}", local.display(), backup.display());
        } else if let Some(dir) = local.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        std::fs::write(&local, &updated).with_context(|| format!("write {}", local.display()))?;
        println!("Wrote {}", local.display());
    }

    // A local.lua nothing loads would be wiring that silently does nothing.
    let main_contents = std::fs::read_to_string(&main_lua)
        .with_context(|| format!("read {}", main_lua.display()))?;
    let mut reload_needed = changed;
    if let Some(new_main) = ensure_local_require(&main_contents) {
        let backup = backup_path(&main_lua);
        std::fs::copy(&main_lua, &backup)
            .with_context(|| format!("back up {}", main_lua.display()))?;
        println!("Backed up {} → {}", main_lua.display(), backup.display());
        std::fs::write(&main_lua, new_main)
            .with_context(|| format!("write {}", main_lua.display()))?;
        println!("Added `require(\"hypr.local\")` to {}", main_lua.display());
        reload_needed = true;
    }

    // Reload picks up both the window rules and, since they live in the
    // config now, the env directive.
    if reload_needed {
        hypr_reload();
        report_config_errors();
    }
    apply_live(&wrapper_str);

    warn_conflicting_binds(&wrapper_str);

    println!();
    println!("Done — your screenshot keys will open captures in Tensaku.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_line_value_parses_variants() {
        assert_eq!(
            env_line_value(r#"hl.env("OMARCHY_SCREENSHOT_EDITOR", "/usr/bin/tensaku-edit")"#)
                .as_deref(),
            Some("/usr/bin/tensaku-edit")
        );
        // Whitespace variations Lua allows.
        assert_eq!(
            env_line_value(r#"hl.env ( "OMARCHY_SCREENSHOT_EDITOR" ,  "/x" )"#).as_deref(),
            Some("/x")
        );
        assert_eq!(
            env_line_value(r#"   hl.env("OMARCHY_SCREENSHOT_EDITOR","/x")   "#).as_deref(),
            Some("/x")
        );
        // Comments and other vars don't match.
        assert!(env_line_value(r#"-- hl.env("OMARCHY_SCREENSHOT_EDITOR", "/x")"#).is_none());
        assert!(env_line_value(r#"hl.env("SOMETHING_ELSE", "/x")"#).is_none());
        // The legacy conf syntax is not Lua and must not be recognised,
        // or a stale envs.conf line would read as already wired.
        assert!(env_line_value("env = OMARCHY_SCREENSHOT_EDITOR,/x").is_none());
    }

    #[test]
    fn apply_env_line_inserts_when_absent() {
        match apply_env_line("-- Extra env variables\n", "/usr/bin/tensaku-edit") {
            EnvLineOutcome::Inserted(s) => {
                assert!(
                    s.contains(r#"hl.env("OMARCHY_SCREENSHOT_EDITOR", "/usr/bin/tensaku-edit")"#)
                );
                assert!(s.starts_with("-- Extra env variables\n"));
            }
            other => panic!("expected Inserted, got {other:?}"),
        }
    }

    #[test]
    fn apply_env_line_already_set_is_noop() {
        let contents = "hl.env(\"OMARCHY_SCREENSHOT_EDITOR\", \"/usr/bin/tensaku-edit\")\n";
        assert_eq!(
            apply_env_line(contents, "/usr/bin/tensaku-edit"),
            EnvLineOutcome::AlreadySet
        );
    }

    #[test]
    fn apply_env_line_updates_when_different() {
        let contents = "a\nhl.env(\"OMARCHY_SCREENSHOT_EDITOR\", \"/old/path\")\nb\n";
        match apply_env_line(contents, "/usr/bin/tensaku-edit") {
            EnvLineOutcome::Updated(s) => {
                assert!(
                    s.contains(r#"hl.env("OMARCHY_SCREENSHOT_EDITOR", "/usr/bin/tensaku-edit")"#)
                );
                assert!(!s.contains("/old/path"));
                assert!(s.starts_with("a\n") && s.trim_end().ends_with('b'));
            }
            other => panic!("expected Updated, got {other:?}"),
        }
    }

    #[test]
    fn inline_bind_value_extraction() {
        let bind = "bindd = , code:191, Screenshot, exec, env OMARCHY_SCREENSHOT_EDITOR=/home/u/.local/bin/tensaku-edit omarchy-capture-screenshot";
        assert_eq!(
            inline_bind_editor_value(bind).as_deref(),
            Some("/home/u/.local/bin/tensaku-edit")
        );
        // The config directive is not an inline bind value.
        assert!(inline_bind_editor_value(r#"hl.env("OMARCHY_SCREENSHOT_EDITOR", "/x")"#).is_none());
    }

    /// Rules written across several lines — the normal Lua layout, and
    /// what Omarchy's own config uses — must be recognised, or wiring an
    /// already-wired config appends a duplicate rule.
    #[test]
    fn multi_line_rules_are_recognised() {
        let c = "o.window(\"dev.tensaku.Tensaku\", {\n\
                 \u{20}\u{20}animation = \"none\",\n\
                 \u{20}\u{20}tag = \"-floating-window\",\n\
                 \u{20}\u{20}float = true,\n\
                 \u{20}\u{20}center = true,\n\
                 })\n\
                 o.window(\"dev.tensaku.Tensaku\", { tag = \"-default-opacity\", opacity = \"1 1\" })\n";
        assert!(has_class_rule(c, "float"));
        assert!(has_class_rule(c, "-default-opacity"));
        assert_eq!(apply_window_rules(c), WindowRuleOutcome::AlreadyPresent);
    }

    /// A rule for a different class must not satisfy ours, even when the
    /// two calls sit next to each other.
    #[test]
    fn another_class_rule_does_not_count() {
        let c = "o.window(\"org.other.App\", { float = true, tag = \"-default-opacity\" })\n";
        assert!(!has_class_rule(c, "float"));
        assert!(matches!(
            apply_window_rules(c),
            WindowRuleOutcome::Appended(_)
        ));
    }

    #[test]
    fn local_require_added_only_when_missing() {
        // Omarchy's template already ends with it.
        assert!(ensure_local_require("require(\"hypr.local\")\n").is_none());
        assert!(ensure_local_require("require('hypr.local')\n").is_none());
        // A commented-out one doesn't load anything.
        let out = ensure_local_require("-- require(\"hypr.local\")\n")
            .expect("commented require must not count");
        assert!(out.contains("require(\"hypr.local\")"));
        // Absent entirely.
        let out = ensure_local_require("require(\"hypr.monitors\")\n")
            .expect("expected the require to be added");
        assert!(out.contains("require(\"hypr.local\")"));
        assert!(out.starts_with("require(\"hypr.monitors\")\n"));
    }

    #[test]
    fn window_rules_appended_when_absent() {
        match apply_window_rules("-- my hypr config\n") {
            WindowRuleOutcome::Appended(s) => {
                assert!(s.contains(r#"o.window("dev.tensaku.Tensaku""#));
                assert!(s.contains("float = true"));
                assert!(s.contains("center = true"));
                assert!(s.contains(r#"tag = "-floating-window""#));
                assert!(s.contains(r#"tag = "-default-opacity""#));
                assert!(s.contains(r#"opacity = "1 1""#));
                assert!(s.starts_with("-- my hypr config\n"));
            }
            other => panic!("expected Appended, got {other:?}"),
        }
    }

    #[test]
    fn window_rules_already_present_is_noop() {
        let c = "o.window(\"dev.tensaku.Tensaku\", { float = true, center = true })\n\
                 o.window(\"dev.tensaku.Tensaku\", { tag = \"-default-opacity\", opacity = \"1 1\" })\n";
        assert_eq!(apply_window_rules(c), WindowRuleOutcome::AlreadyPresent);
    }

    /// A config wired before the opacity rule existed still needs it —
    /// float + center alone must not count as complete, or an existing
    /// install keeps the translucent window that ghosts whatever is
    /// behind it into the canvas.
    #[test]
    fn window_rules_without_opacity_are_topped_up() {
        let c = "o.window(\"dev.tensaku.Tensaku\", { float = true, center = true })\n";
        let WindowRuleOutcome::Appended(out) = apply_window_rules(c) else {
            panic!("expected the opacity rule to be appended");
        };
        assert!(out.contains(r#"tag = "-default-opacity""#));
        assert!(out.contains(r#"opacity = "1 1""#));
    }

    #[test]
    fn window_rules_commented_out_dont_count() {
        let c = "-- o.window(\"dev.tensaku.Tensaku\", { float = true })\n\
                 -- o.window(\"dev.tensaku.Tensaku\", { tag = \"-default-opacity\" })\n";
        assert!(matches!(
            apply_window_rules(c),
            WindowRuleOutcome::Appended(_)
        ));
    }

    #[test]
    fn window_rules_partial_appends_full_block() {
        // float present but no opacity opt-out → still append the block.
        let c = "o.window(\"dev.tensaku.Tensaku\", { float = true, center = true })\n";
        assert!(matches!(
            apply_window_rules(c),
            WindowRuleOutcome::Appended(_)
        ));
    }

    #[test]
    fn omarchy_detected_via_env() {
        // A non-empty $OMARCHY_PATH is sufficient, regardless of dirs.
        assert!(is_omarchy_with(
            Some(OsStr::new("/home/u/.local/share/omarchy")),
            Some(Path::new("/zzz-no-such-data-home")),
        ));
    }

    #[test]
    fn omarchy_absent_when_no_signal() {
        // Empty env value doesn't count; missing dir doesn't count.
        assert!(!is_omarchy_with(
            Some(OsStr::new("")),
            Some(Path::new("/zzz-no-such-data-home")),
        ));
        assert!(!is_omarchy_with(
            None,
            Some(Path::new("/zzz-no-such-data-home"))
        ));
        assert!(!is_omarchy_with(None, None));
    }

    #[test]
    fn recognizes_modern_omarchy_tensaku_default() {
        assert!(capture_script_defaults_to_tensaku(
            r#"SCREENSHOT_EDITOR="${OMARCHY_SCREENSHOT_EDITOR:-tensaku-edit}""#
        ));
        assert!(capture_script_defaults_to_tensaku(
            r#"  SCREENSHOT_EDITOR = "${OMARCHY_SCREENSHOT_EDITOR:-tensaku-edit}"  "#
        ));
    }

    #[test]
    fn ignores_comments_and_other_editor_defaults() {
        assert!(!capture_script_defaults_to_tensaku(
            r#"# SCREENSHOT_EDITOR="${OMARCHY_SCREENSHOT_EDITOR:-tensaku-edit}""#
        ));
        assert!(!capture_script_defaults_to_tensaku(
            r#"SCREENSHOT_EDITOR="${OMARCHY_SCREENSHOT_EDITOR:-satty}""#
        ));
        assert!(!capture_script_defaults_to_tensaku("echo tensaku-edit"));
    }

    #[test]
    fn wiring_unset() {
        let w = PathBuf::from("/zzz/.local/bin/tensaku-edit");
        assert_eq!(classify_wiring(None, &w), Wiring::Unset);
        assert_eq!(classify_wiring(Some(OsString::new()), &w), Wiring::Unset);
        // All-whitespace value has no token.
        assert_eq!(
            classify_wiring(Some(OsString::from("   ")), &w),
            Wiring::Unset
        );
    }

    #[test]
    fn wiring_correct_exact_match() {
        // Fake path: canonicalize fails for both, so it falls back to
        // plain equality, which matches.
        let w = PathBuf::from("/zzz/.local/bin/tensaku-edit");
        assert_eq!(
            classify_wiring(Some(OsString::from("/zzz/.local/bin/tensaku-edit")), &w),
            Wiring::Correct,
        );
    }

    #[test]
    fn wiring_elsewhere_keeps_the_other_path() {
        let w = PathBuf::from("/zzz/.local/bin/tensaku-edit");
        assert_eq!(
            classify_wiring(Some(OsString::from("/usr/bin/satty")), &w),
            Wiring::Elsewhere(PathBuf::from("/usr/bin/satty")),
        );
    }

    #[test]
    fn wiring_accepts_tensaku_edit_from_another_install_prefix() {
        let root = std::env::temp_dir().join(format!(
            "tensaku-wiring-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let packaged = root.join("usr/bin/tensaku-edit");
        let local = root.join("home/u/.local/bin/tensaku-edit");
        std::fs::create_dir_all(packaged.parent().expect("packaged parent")).unwrap();
        std::fs::create_dir_all(local.parent().expect("local parent")).unwrap();
        std::fs::write(&packaged, "wrapper").unwrap();
        std::fs::write(&local, "wrapper").unwrap();
        assert_eq!(
            classify_wiring(Some(local.into_os_string()), &packaged),
            Wiring::Correct,
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wiring_ignores_trailing_args() {
        let w = PathBuf::from("/zzz/.local/bin/tensaku-edit");
        assert_eq!(
            classify_wiring(
                Some(OsString::from("/zzz/.local/bin/tensaku-edit --foo bar")),
                &w,
            ),
            Wiring::Correct,
        );
    }

    #[test]
    fn wiring_expands_leading_tilde() {
        // Build the wrapper from $HOME so the tilde-expanded candidate
        // resolves to the same path.
        let home = std::env::var_os("HOME").expect("HOME set in test env");
        let w = PathBuf::from(&home).join(".local/bin/tensaku-edit");
        assert_eq!(
            classify_wiring(Some(OsString::from("~/.local/bin/tensaku-edit")), &w),
            Wiring::Correct,
        );
    }
}
