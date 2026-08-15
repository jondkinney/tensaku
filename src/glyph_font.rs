//! Register the bundled glyph faces with fontconfig at startup so the
//! toolbar tooltips and the scroll-capture shortcut recorder can render
//! modifier-key glyphs in the same faces the cohort apps (hyprcorrect /
//! vernier / mousehop) use for their chord chips — so shortcuts read
//! identically across the apps regardless of the host's default UI font.
//!
//! Two faces are registered:
//!   * **Adwaita Sans** — the standard ⌃ ⇧ ⌥ ⌘ modifier glyphs and the
//!     trigger letters/symbols. Always registered.
//!   * **Omarchy** — the Hyprland/Omarchy logo at U+E900, used as the
//!     Super-key glyph in the scroll-capture shortcut chip (matching
//!     mousehop). Best-effort: if the face doesn't register, the chord
//!     markup falls back to the Mac-style ⌘ glyph so the chip still
//!     reads as a Super shortcut.
//!
//! On Linux, Pango resolves font families through fontconfig, so the
//! bytes embedded via `include_bytes!` are dropped into the user cache
//! dir on first launch and registered with the process's fontconfig
//! configuration via `FcConfigAppFontAddFile`. This must run before the
//! first text layout so Pango's font map picks the faces up when it is
//! first built.
//!
//! `FcConfigAppFontAddFile` works on every fontconfig/Pango version we
//! ship against — unlike Pango 1.56's `add_font_file`. libfontconfig is
//! already linked transitively through GTK/Pango, so faces added here
//! are visible to the tooltip markup.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// The registered Pango family names — referenced literally in the
// tooltip markup (`<span face="Adwaita Sans">…`) and the chord-chip
// markup below, since those strings can't interpolate a constant. Match
// the cohort apps' chord chip.
const ADWAITA_SANS_TTF: &[u8] = include_bytes!("assets/AdwaitaSans-Regular.ttf");
const OMARCHY_TTF: &[u8] = include_bytes!("assets/omarchy.ttf");

/// The Omarchy logo's codepoint in the Private Use Area — the Super-key
/// glyph in the chord chip. Matches mousehop's `OMARCHY_LOGO`.
const OMARCHY_LOGO: char = '\u{e900}';

// Chord-chip span sizing, mirroring mousehop so the chip renders
// identically across the cohort. Pango `size` is in 1024ths of a point;
// the Omarchy logo fills its full em square, so it's rendered ~0.72× the
// modifier-glyph size and nudged onto the letters' baseline via `rise`.
const CHIP_FAMILY: &str = "Adwaita Sans";
const OMARCHY_FAMILY: &str = "omarchy";
const CHIP_PT_K: u32 = 14_000;
const OMARCHY_PT_K: u32 = 10_080;
const OMARCHY_RISE: i32 = -250;
/// THREE-PER-EM SPACE — the gap between chord segments. Matches the
/// cohort chip's separator.
const CHIP_SEPARATOR: &str = "\u{2004}";

/// Whether the Omarchy face registered successfully. Set once by
/// [`install`]; read by [`omarchy_available`] / [`chord_markup`]. `false`
/// until `install` runs, so callers before startup get the safe fallback.
static OMARCHY_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Materialize the bundled faces in the per-user cache dir and register
/// them with fontconfig so Pango can resolve the "Adwaita Sans" and
/// "omarchy" families. Call once, early in startup, before any text is
/// laid out. Safe to call multiple times: the cache write is idempotent
/// and re-adding a known file is a no-op for fontconfig.
pub fn install() {
    let Some(dir) = cache_dir() else {
        eprintln!("glyph_font: no cache dir, skipping bundled-font registration");
        let _ = OMARCHY_AVAILABLE.set(false);
        return;
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("glyph_font: create_dir_all({}): {e}", dir.display());
        let _ = OMARCHY_AVAILABLE.set(false);
        return;
    }

    let adwaita_path = dir.join("tensaku-AdwaitaSans-Regular.ttf");
    if let Err(e) = ensure_file(&adwaita_path, ADWAITA_SANS_TTF) {
        eprintln!(
            "glyph_font: writing Adwaita Sans to {}: {e}",
            adwaita_path.display()
        );
    } else if !register_app_font(&adwaita_path) {
        eprintln!(
            "glyph_font: fontconfig did not accept Adwaita Sans at {}",
            adwaita_path.display()
        );
    }

    // Omarchy logo face — best-effort; the chord chip falls back to ⌘
    // when it's unavailable.
    let omarchy_path = dir.join("tensaku-omarchy.ttf");
    let omarchy_ok = match ensure_file(&omarchy_path, OMARCHY_TTF) {
        Ok(()) => register_app_font(&omarchy_path),
        Err(e) => {
            eprintln!(
                "glyph_font: writing Omarchy face to {}: {e}",
                omarchy_path.display()
            );
            false
        }
    };
    if !omarchy_ok {
        eprintln!("glyph_font: Omarchy face unavailable — chord chips will fall back to ⌘");
    }
    let _ = OMARCHY_AVAILABLE.set(omarchy_ok);
}

/// Whether the Omarchy logo face is usable for the Super-key glyph.
pub fn omarchy_available() -> bool {
    *OMARCHY_AVAILABLE.get().unwrap_or(&false)
}

/// Build Pango markup rendering a chord string (e.g. `"SUPER+SHIFT+S"`,
/// in the canonical CTRL+SHIFT+ALT+SUPER+KEY order produced by the
/// recorder) as glyph chips: ⌃ ⇧ ⌥ in Adwaita Sans, the Super key as the
/// Omarchy logo (or ⌘ fallback), and the trigger as a letter/symbol.
/// Segments are separated by a three-per-em space, matching the cohort.
pub fn chord_markup(chord: &str) -> String {
    chord
        .split('+')
        .filter(|t| !t.is_empty())
        .map(token_markup)
        .collect::<Vec<_>>()
        .join(CHIP_SEPARATOR)
}

/// Markup for a single chord token.
fn token_markup(token: &str) -> String {
    // Modifier glyphs ride in the Adwaita Sans face at the chip size.
    let mod_glyph = |glyph: &str| {
        format!(
            "<span face=\"{CHIP_FAMILY}\" size=\"{CHIP_PT_K}\" weight=\"normal\">{glyph}</span>"
        )
    };
    match token {
        "CTRL" => mod_glyph("\u{2303}"),  // ⌃
        "SHIFT" => mod_glyph("\u{21E7}"), // ⇧
        "ALT" => mod_glyph("\u{2325}"),   // ⌥
        "SUPER" => super_markup(),
        // Named non-letter keys → their conventional glyph, so the chip
        // reads the way the cohort's chips do.
        "ENTER" => mod_glyph("\u{21B5}"),     // ↵
        "ESC" => mod_glyph("\u{238B}"),       // ⎋
        "TAB" => mod_glyph("\u{21E5}"),       // ⇥
        "BACKSPACE" => mod_glyph("\u{232B}"), // ⌫
        "DELETE" => mod_glyph("\u{2326}"),    // ⌦
        "SPACE" => mod_glyph("\u{2423}"),     // ␣
        "UP" => mod_glyph("\u{2191}"),        // ↑
        "DOWN" => mod_glyph("\u{2193}"),      // ↓
        "LEFT" => mod_glyph("\u{2190}"),      // ←
        "RIGHT" => mod_glyph("\u{2192}"),     // →
        "PLUS" => mod_glyph("+"),
        "MINUS" => mod_glyph("-"),
        "EQUAL" => mod_glyph("="),
        // Letters, function keys, and anything else: render verbatim
        // (escaped) in the chip face.
        other => format!(
            "<span face=\"{CHIP_FAMILY}\" size=\"{CHIP_PT_K}\" weight=\"normal\">{}</span>",
            escape_markup(other)
        ),
    }
}

/// The Super-key segment: the Omarchy logo when the face is available,
/// otherwise the Mac-style Command glyph so the chip still reads as a
/// Super shortcut. Mirrors mousehop's `super_markup`.
fn super_markup() -> String {
    if omarchy_available() {
        format!(
            "<span face=\"{OMARCHY_FAMILY}\" size=\"{OMARCHY_PT_K}\" rise=\"{OMARCHY_RISE}\">{OMARCHY_LOGO}</span>"
        )
    } else {
        format!("<span face=\"{CHIP_FAMILY}\" size=\"{CHIP_PT_K}\">\u{2318}</span>")
    }
}

/// Escape the five XML predefined entities so a trigger token like `&`
/// or `<` can't break the Pango markup.
fn escape_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("tensaku").join("fonts"))
}

fn ensure_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Ok(existing) = fs::metadata(path)
        && existing.len() == bytes.len() as u64
    {
        return Ok(());
    }
    fs::write(path, bytes)
}

// fontconfig's `FcConfigAppFontAddFile`, declared directly rather than
// via a `-sys` crate (a new dep would break the Nix sandbox build).
// libfontconfig is already linked transitively through GTK/Pango and is
// the same instance Pango resolves families against, so faces added
// here are visible to the tooltip markup.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    #[link_name = "FcConfigAppFontAddFile"]
    fn fc_config_app_font_add_file(
        config: *mut std::os::raw::c_void,
        file: *const std::os::raw::c_uchar,
    ) -> std::os::raw::c_int;
}

#[cfg(target_os = "linux")]
fn register_app_font(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a null config selects fontconfig's current configuration,
    // initializing it on first use. fontconfig copies the path string,
    // so `c_path` only needs to outlive the call.
    unsafe { fc_config_app_font_add_file(std::ptr::null_mut(), c_path.as_ptr().cast()) != 0 }
}

#[cfg(not(target_os = "linux"))]
fn register_app_font(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_markup_splits_and_maps_modifiers() {
        let m = chord_markup("CTRL+SHIFT+S");
        assert!(m.contains("\u{2303}")); // ⌃
        assert!(m.contains("\u{21E7}")); // ⇧
        assert!(m.contains(">S<")); // the trigger letter
        assert!(m.contains(CHIP_SEPARATOR));
    }

    #[test]
    fn chord_markup_super_falls_back_without_face() {
        // In unit tests `install` never runs, so the Omarchy face is
        // unavailable and SUPER must render the ⌘ fallback.
        let m = chord_markup("SUPER+SPACE");
        assert!(m.contains("\u{2318}")); // ⌘ fallback
        assert!(m.contains("\u{2423}")); // ␣
    }

    #[test]
    fn chord_markup_escapes_trigger() {
        let m = chord_markup("CTRL+&");
        assert!(m.contains("&amp;"));
        assert!(!m.contains("+&<")); // no raw ampersand in a tag
    }

    #[test]
    fn empty_chord_is_empty_markup() {
        assert_eq!(chord_markup(""), "");
    }
}
