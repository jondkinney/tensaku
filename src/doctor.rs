//! `--doctor`: a quick environment check — report whether the optional
//! external tools the Tensaku screenshot workflow leans on are present.
//! Tensaku degrades gracefully without them; this just makes a missing
//! piece easy to spot.

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

/// On Omarchy, report whether the screenshot wrapper is installed and
/// whether `$OMARCHY_SCREENSHOT_EDITOR` is wired to it.
fn report_omarchy_wrapper() {
    use crate::omarchy_wrapper::{
        Wiring, classify_wiring, configured_editor, installed_wrapper, wrapper_path,
    };

    println!();
    println!("Omarchy detected — screenshot integration:");

    // A packaged /usr/bin/tensaku-edit counts as installed, not just the
    // per-user copy; wiring is classified against whichever exists (or the
    // path we'd install to), so this matches what --wire-omarchy does.
    let wrapper = installed_wrapper();
    let mut needs_setup = false;

    match &wrapper {
        Some(p) => println!("  [ ok ]  wrapper installed: {}", p.display()),
        None => {
            println!("  [miss]  wrapper not installed");
            needs_setup = true;
        }
    }

    if let Some(target) = wrapper.or_else(|| wrapper_path().ok()) {
        // envs.conf is the persistent wiring — what screenshots will use
        // going forward, so it (not the live env) drives the verdict. The
        // live $OMARCHY_SCREENSHOT_EDITOR only reflects the running session
        // and goes stale after --wire-omarchy until the next login; we read
        // it only to flag a wiring that isn't active in this session yet, or
        // a session-only value that won't survive a relogin.
        let active_correct = matches!(
            classify_wiring(std::env::var_os("OMARCHY_SCREENSHOT_EDITOR"), &target),
            Wiring::Correct
        );

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
    }

    if needs_setup {
        println!("          → run: tensaku --wire-omarchy");
    }
}
