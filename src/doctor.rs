//! `--doctor`: a quick environment check — report whether the optional
//! external tools the Tensaku screenshot workflow leans on are present.
//! Tensaku degrades gracefully without them; this just makes a missing
//! piece easy to spot.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// Is `bin` an executable file somewhere on `$PATH`?
pub(crate) fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// A single environment check shown in the `--doctor` report.
struct Check {
    label: &'static str,
    ok: bool,
    /// Shown indented below the label when the check fails.
    hint: &'static str,
}

/// Print the environment report.
pub fn run() -> Result<()> {
    let checks = [
        Check {
            label: "Wayland session (WAYLAND_DISPLAY)",
            ok: std::env::var_os("WAYLAND_DISPLAY").is_some(),
            hint: "Tensaku is a Wayland app — launch it from a Wayland session.",
        },
        Check {
            label: "grim — screenshot capture",
            ok: on_path("grim"),
            hint: "Install grim to pipe screenshots in: grim -g \"$(slurp)\" - | tensaku -f -",
        },
        Check {
            label: "slurp — region selector",
            ok: on_path("slurp"),
            hint: "Install slurp to drag-select a capture region.",
        },
        Check {
            label: "wl-copy — clipboard (default copy-command)",
            ok: on_path("wl-copy"),
            hint: "Install wl-clipboard, or set copy-command to your clipboard tool.",
        },
    ];

    println!("Tensaku environment check\n");
    let mut missing = 0;
    for c in &checks {
        if c.ok {
            println!("  [ ok ]  {}", c.label);
        } else {
            missing += 1;
            println!("  [miss]  {}", c.label);
            println!("          {}", c.hint);
        }
    }

    println!();
    if missing == 0 {
        println!("All good — every external tool Tensaku's workflow uses is present.");
    } else {
        println!("{missing} missing. Tensaku still runs, but the noted features won't work.");
    }

    if crate::omarchy_wrapper::is_omarchy() {
        report_omarchy_wrapper();
    }
    Ok(())
}

/// On Omarchy, report whether the screenshot wrapper is available. Current
/// Omarchy defaults to `tensaku-edit`; only legacy versions require an
/// `$OMARCHY_SCREENSHOT_EDITOR` override.
fn report_omarchy_wrapper() {
    println!();
    println!("Omarchy detected — screenshot integration:");
    report_editor_wiring();
    report_capture_key();
}

/// Which capture the screenshot key runs.
///
/// Reports rather than judges: binding the key to Tensaku is opt-in, so
/// leaving Omarchy's own capture in place is a choice, not a fault, and
/// never counts toward the missing tally.
fn report_capture_key() {
    use crate::omarchy_wrapper::bound_capture_key;
    use tensaku_cli::command_line::DEFAULT_CAPTURE_KEY;

    match bound_capture_key() {
        Some(key) => println!("  [ ok ]  {key} opens Tensaku's capture overlay"),
        None => {
            println!("  [ -- ]  no key opens Tensaku's capture overlay yet");
            println!("          → run: tensaku --wire-capture-key   (binds {DEFAULT_CAPTURE_KEY})");
        }
    }
}

fn report_editor_wiring() {
    use crate::omarchy_wrapper::{
        Wiring, classify_wiring, configured_editor, installed_wrapper,
        omarchy_capture_defaults_to_tensaku,
    };

    let Some(target) = installed_wrapper() else {
        println!("  [miss]  tensaku-edit wrapper not installed");
        println!("          → run: tensaku --install-omarchy-wrapper");
        return;
    };
    println!("  [ ok ]  wrapper installed: {}", target.display());

    if omarchy_capture_defaults_to_tensaku() {
        match classify_default_omarchy_integration(
            configured_editor(),
            std::env::var_os("OMARCHY_SCREENSHOT_EDITOR"),
            &target,
        ) {
            DefaultOmarchyIntegration::Default => {
                println!("  [ ok ]  Omarchy defaults to tensaku-edit (no override needed)");
            }
            DefaultOmarchyIntegration::Explicit => {
                println!("  [ ok ]  explicit OMARCHY_SCREENSHOT_EDITOR override uses Tensaku");
            }
            DefaultOmarchyIntegration::PersistentOverride(other) => {
                println!(
                    "  [miss]  envs.conf overrides Omarchy's Tensaku default with {}",
                    other.display()
                );
                println!(
                    "          → remove that OMARCHY_SCREENSHOT_EDITOR override to use the default"
                );
            }
            DefaultOmarchyIntegration::ActiveOverride(other) => {
                println!(
                    "  [miss]  this session overrides Omarchy's Tensaku default with {}",
                    other.display()
                );
                println!("          → unset OMARCHY_SCREENSHOT_EDITOR to use the default");
            }
        }
        return;
    }

    // Legacy Omarchy needs persistent envs.conf wiring. Keep the old report
    // and recovery command only when its capture script has no Tensaku default.
    let active_correct = matches!(
        classify_wiring(std::env::var_os("OMARCHY_SCREENSHOT_EDITOR"), &target),
        Wiring::Correct
    );
    let mut needs_setup = false;
    match classify_wiring(configured_editor(), &target) {
        Wiring::Correct => {
            println!("  [ ok ]  OMARCHY_SCREENSHOT_EDITOR wired in envs.conf");
            if !active_correct {
                println!("          (active after next login)");
            }
        }
        Wiring::Elsewhere(other) => {
            println!(
                "  [miss]  envs.conf wires OMARCHY_SCREENSHOT_EDITOR at {}",
                other.display()
            );
            needs_setup = true;
        }
        Wiring::Unset => {
            if active_correct {
                println!(
                    "  [miss]  OMARCHY_SCREENSHOT_EDITOR is set this session but \
                     not in envs.conf — it won't persist"
                );
            } else {
                println!("  [miss]  OMARCHY_SCREENSHOT_EDITOR is not wired in envs.conf");
            }
            needs_setup = true;
        }
    }
    if needs_setup {
        println!("          → run: tensaku --wire-omarchy");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DefaultOmarchyIntegration {
    Default,
    Explicit,
    PersistentOverride(PathBuf),
    ActiveOverride(PathBuf),
}

fn classify_default_omarchy_integration(
    configured: Option<OsString>,
    active: Option<OsString>,
    wrapper: &Path,
) -> DefaultOmarchyIntegration {
    use crate::omarchy_wrapper::{Wiring, classify_wiring};

    match classify_wiring(configured, wrapper) {
        Wiring::Elsewhere(other) => DefaultOmarchyIntegration::PersistentOverride(other),
        Wiring::Correct => match classify_wiring(active, wrapper) {
            Wiring::Elsewhere(other) => DefaultOmarchyIntegration::ActiveOverride(other),
            Wiring::Correct | Wiring::Unset => DefaultOmarchyIntegration::Explicit,
        },
        Wiring::Unset => match classify_wiring(active, wrapper) {
            Wiring::Elsewhere(other) => DefaultOmarchyIntegration::ActiveOverride(other),
            Wiring::Correct => DefaultOmarchyIntegration::Explicit,
            Wiring::Unset => DefaultOmarchyIntegration::Default,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrapper() -> PathBuf {
        PathBuf::from("/zzz/tensaku-edit")
    }

    #[test]
    fn modern_omarchy_needs_no_override() {
        assert_eq!(
            classify_default_omarchy_integration(None, None, &wrapper()),
            DefaultOmarchyIntegration::Default
        );
    }

    #[test]
    fn modern_omarchy_accepts_an_explicit_tensaku_override() {
        let wrapper = wrapper();
        assert_eq!(
            classify_default_omarchy_integration(
                Some(wrapper.clone().into_os_string()),
                None,
                &wrapper,
            ),
            DefaultOmarchyIntegration::Explicit
        );
    }

    #[test]
    fn modern_omarchy_reports_other_editor_overrides() {
        let wrapper = wrapper();
        assert_eq!(
            classify_default_omarchy_integration(
                Some(OsString::from("/usr/bin/other")),
                None,
                &wrapper,
            ),
            DefaultOmarchyIntegration::PersistentOverride(PathBuf::from("/usr/bin/other"))
        );
        assert_eq!(
            classify_default_omarchy_integration(
                None,
                Some(OsString::from("/usr/bin/other")),
                &wrapper,
            ),
            DefaultOmarchyIntegration::ActiveOverride(PathBuf::from("/usr/bin/other"))
        );
    }
}
