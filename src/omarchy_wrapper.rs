//! Omarchy screenshot-wrapper integration.
//!
//! Omarchy's screenshot keybinds run `omarchy-capture-screenshot`, which
//! hands the captured image path to whatever `$OMARCHY_SCREENSHOT_EDITOR`
//! names. Tensaku takes its input as a flag, not a positional argument,
//! so a small wrapper at `~/.local/bin/tensaku-edit` adapts the call.
//! This module places that wrapper so a fresh Omarchy install needs no
//! manual setup. It runs two ways, mirroring [`crate::desktop_install`]:
//!
//! - [`run`] — the explicit `--install-omarchy-wrapper` flag; installs,
//!   then reports and verifies the `$OMARCHY_SCREENSHOT_EDITOR` wiring.
//! - [`ensure_first_launch`] — silent and one-shot, on the first normal
//!   launch, when Omarchy is detected.
//!
//! It never edits Omarchy or Hyprland config: pointing
//! `$OMARCHY_SCREENSHOT_EDITOR` at the wrapper stays the user's (or
//! Omarchy's own default) job. The verbose paths only *warn* when it
//! isn't pointed here.

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

    let same = match (
        std::fs::canonicalize(&candidate).ok(),
        std::fs::canonicalize(wrapper).ok(),
    ) {
        (Some(a), Some(b)) => a == b,
        // One or both paths don't exist yet — compare as written.
        _ => candidate == wrapper,
    };

    if same {
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
        Wiring::Unset => {
            println!("OMARCHY_SCREENSHOT_EDITOR is not set.");
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

/// `$XDG_CONFIG_HOME/hypr/envs.conf`, falling back to `~/.config/...`.
fn hypr_envs_conf() -> Result<PathBuf> {
    let base = if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        PathBuf::from(dir)
    } else {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        PathBuf::from(home).join(".config")
    };
    Ok(base.join("hypr").join("envs.conf"))
}

/// Sibling `bindings.conf`, used only to detect conflicting inline binds.
fn hypr_bindings_conf() -> Result<PathBuf> {
    Ok(hypr_envs_conf()?.with_file_name("bindings.conf"))
}

/// The canonical Hyprland env directive wiring the screenshot editor.
fn desired_env_line(wrapper: &str) -> String {
    format!("env = OMARCHY_SCREENSHOT_EDITOR,{wrapper}")
}

/// If `line` is an `env = OMARCHY_SCREENSHOT_EDITOR,<value>` directive,
/// return its `<value>` (trimmed). Comments (`#…`) don't match because
/// they don't start with `env`.
fn env_line_value(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("env")?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix("OMARCHY_SCREENSHOT_EDITOR")?.trim_start();
    let value = rest.strip_prefix(',')?.trim();
    Some(value.to_string())
}

/// The `OMARCHY_SCREENSHOT_EDITOR` value configured in `envs.conf`, if any.
///
/// Unlike the live `$OMARCHY_SCREENSHOT_EDITOR` (which reflects the running
/// session and goes stale after `--wire-omarchy` until the next login), this
/// is the *persistent* wiring — what screenshots will use going forward, and
/// what `--doctor` should report. First matching directive wins, mirroring
/// [`apply_env_line`]. Read-only and best-effort: a missing or unreadable
/// file reads as "not configured".
pub(crate) fn configured_editor() -> Option<OsString> {
    let path = hypr_envs_conf().ok()?;
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
        out.push_str("# Tensaku screenshot editor (set by `tensaku --wire-omarchy`).\n");
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
    // `.output()` (not `.status()`) so hyprctl's own "ok" doesn't leak
    // into our report.
    let ok = std::process::Command::new("hyprctl")
        .arg("keyword")
        .arg("env")
        .arg(format!("OMARCHY_SCREENSHOT_EDITOR,{wrapper}"))
        .output()
        .map(|o| o.status.success())
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
    let Ok(path) = hypr_bindings_conf() else {
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
fn hypr_main_conf() -> Result<PathBuf> {
    Ok(hypr_envs_conf()?.with_file_name("hyprland.conf"))
}

/// Is there an uncommented `windowrule = <action>, match:class <our class>`?
fn has_class_rule(contents: &str, action: &str) -> bool {
    let needle = format!("match:class {WINDOW_CLASS}");
    contents.lines().any(|l| {
        let t = l.trim();
        !t.starts_with('#')
            && t.starts_with("windowrule")
            && t.contains(action)
            && t.contains(&needle)
    })
}

/// The float/center rules that let Tensaku size its own window.
fn window_rules_block() -> String {
    format!(
        "\n# Tensaku: float + center its window. Tensaku sizes its own window\n\
         # around the capture, so it must float with no fixed-size rule. The\n\
         # `tag -floating-window` undoes a distro default (e.g. Omarchy's) that\n\
         # would otherwise pin a fixed size. Added by `tensaku --wire-omarchy`.\n\
         windowrule = tag -floating-window, match:class {WINDOW_CLASS}\n\
         windowrule = float on, match:class {WINDOW_CLASS}\n\
         windowrule = center on, match:class {WINDOW_CLASS}\n"
    )
}

/// How hyprland.conf relates to Tensaku's window rules.
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
    if has_class_rule(contents, "float on") && has_class_rule(contents, "center on") {
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
    let wrapper = find_or_install_wrapper()?;
    let wrapper_str = wrapper.to_string_lossy().into_owned();

    let envs = hypr_envs_conf()?;
    let existing = std::fs::read_to_string(&envs).unwrap_or_default();
    match apply_env_line(&existing, &wrapper_str) {
        EnvLineOutcome::AlreadySet => {
            println!("envs.conf already wires OMARCHY_SCREENSHOT_EDITOR → {wrapper_str}");
        }
        EnvLineOutcome::Updated(new) | EnvLineOutcome::Inserted(new) => {
            if envs.exists() {
                let backup = backup_path(&envs);
                std::fs::copy(&envs, &backup)
                    .with_context(|| format!("back up {}", envs.display()))?;
                println!("Backed up {} → {}", envs.display(), backup.display());
            } else if let Some(dir) = envs.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create {}", dir.display()))?;
            }
            std::fs::write(&envs, new).with_context(|| format!("write {}", envs.display()))?;
            println!(
                "Set OMARCHY_SCREENSHOT_EDITOR → {wrapper_str}\n  in {}",
                envs.display()
            );
        }
    }

    // Float + center the Tensaku window so it can size itself around the
    // capture (otherwise a tiling layout, or a distro's fixed-size rule,
    // fights it). Written to hyprland.conf; applied live via reload below.
    let conf = hypr_main_conf()?;
    let mut rules_changed = false;
    if conf.exists() {
        let existing_rules = std::fs::read_to_string(&conf).unwrap_or_default();
        match apply_window_rules(&existing_rules) {
            WindowRuleOutcome::AlreadyPresent => {
                println!("hyprland.conf already floats + centers the Tensaku window.");
            }
            WindowRuleOutcome::Appended(new) => {
                let backup = backup_path(&conf);
                std::fs::copy(&conf, &backup)
                    .with_context(|| format!("back up {}", conf.display()))?;
                println!("Backed up {} → {}", conf.display(), backup.display());
                std::fs::write(&conf, new).with_context(|| format!("write {}", conf.display()))?;
                println!(
                    "Added float + center window rules for {WINDOW_CLASS}\n  in {}",
                    conf.display()
                );
                rules_changed = true;
            }
        }
    } else {
        println!("(no {} — skipping window rules)", conf.display());
    }

    // Apply live. Window rules are re-applied on reload (env directives are
    // not), so reload first, then re-assert the env so reload doesn't drop it.
    if rules_changed {
        hypr_reload();
    }
    apply_live(&wrapper_str);
    if rules_changed {
        report_config_errors();
    }

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
            env_line_value("env = OMARCHY_SCREENSHOT_EDITOR,/usr/bin/tensaku-edit").as_deref(),
            Some("/usr/bin/tensaku-edit")
        );
        // No spaces around '='.
        assert_eq!(
            env_line_value("env=OMARCHY_SCREENSHOT_EDITOR,/x").as_deref(),
            Some("/x")
        );
        // Leading indentation and trailing space tolerated.
        assert_eq!(
            env_line_value("   env = OMARCHY_SCREENSHOT_EDITOR,/x  ").as_deref(),
            Some("/x")
        );
        // Comments and other vars don't match.
        assert!(env_line_value("# env = OMARCHY_SCREENSHOT_EDITOR,/x").is_none());
        assert!(env_line_value("env = SOMETHING_ELSE,/x").is_none());
    }

    #[test]
    fn apply_env_line_inserts_when_absent() {
        match apply_env_line("# Extra env variables\n", "/usr/bin/tensaku-edit") {
            EnvLineOutcome::Inserted(s) => {
                assert!(s.contains("env = OMARCHY_SCREENSHOT_EDITOR,/usr/bin/tensaku-edit"));
                assert!(s.starts_with("# Extra env variables\n"));
            }
            other => panic!("expected Inserted, got {other:?}"),
        }
    }

    #[test]
    fn apply_env_line_already_set_is_noop() {
        let contents = "env = OMARCHY_SCREENSHOT_EDITOR,/usr/bin/tensaku-edit\n";
        assert_eq!(
            apply_env_line(contents, "/usr/bin/tensaku-edit"),
            EnvLineOutcome::AlreadySet
        );
    }

    #[test]
    fn apply_env_line_updates_when_different() {
        let contents = "a\nenv = OMARCHY_SCREENSHOT_EDITOR,/old/path\nb\n";
        match apply_env_line(contents, "/usr/bin/tensaku-edit") {
            EnvLineOutcome::Updated(s) => {
                assert!(s.contains("env = OMARCHY_SCREENSHOT_EDITOR,/usr/bin/tensaku-edit"));
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
        // The envs.conf comma form is not an inline bind value.
        assert!(inline_bind_editor_value("env = OMARCHY_SCREENSHOT_EDITOR,/x").is_none());
    }

    #[test]
    fn window_rules_appended_when_absent() {
        match apply_window_rules("# my hypr config\n") {
            WindowRuleOutcome::Appended(s) => {
                assert!(s.contains("float on, match:class dev.tensaku.Tensaku"));
                assert!(s.contains("center on, match:class dev.tensaku.Tensaku"));
                assert!(s.contains("tag -floating-window, match:class dev.tensaku.Tensaku"));
                assert!(s.starts_with("# my hypr config\n"));
            }
            other => panic!("expected Appended, got {other:?}"),
        }
    }

    #[test]
    fn window_rules_already_present_is_noop() {
        let c = "windowrule = float on, match:class dev.tensaku.Tensaku\n\
                 windowrule = center on, match:class dev.tensaku.Tensaku\n";
        assert_eq!(apply_window_rules(c), WindowRuleOutcome::AlreadyPresent);
    }

    #[test]
    fn window_rules_commented_out_dont_count() {
        let c = "# windowrule = float on, match:class dev.tensaku.Tensaku\n";
        assert!(matches!(
            apply_window_rules(c),
            WindowRuleOutcome::Appended(_)
        ));
    }

    #[test]
    fn window_rules_partial_appends_full_block() {
        // float present but no center → still append the full block.
        let c = "windowrule = float on, match:class dev.tensaku.Tensaku\n";
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
