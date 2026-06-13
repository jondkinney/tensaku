//! Register the bundled Adwaita Sans face with fontconfig at startup so
//! the toolbar tooltips can render the standard ⌃ ⇧ ⌥ modifier glyphs in
//! the same face the cohort apps (hyprcorrect / vernier / mousehop) use
//! for their chord chips — so the shortcuts read identically across the
//! three apps regardless of the host's default UI font.
//!
//! On Linux, Pango resolves font families through fontconfig, so the
//! bytes embedded via `include_bytes!` are dropped into the user cache
//! dir on first launch and registered with the process's fontconfig
//! configuration via `FcConfigAppFontAddFile`. This must run before the
//! first text layout so Pango's font map picks the face up when it is
//! first built.
//!
//! `FcConfigAppFontAddFile` works on every fontconfig/Pango version we
//! ship against — unlike Pango 1.56's `add_font_file`. libfontconfig is
//! already linked transitively through GTK/Pango, so faces added here
//! are visible to the tooltip markup.
//!
//! Tensaku only needs the standard modifier glyphs (no Super key appears
//! in its tooltips), so unlike mousehop this registers Adwaita Sans
//! alone — no Omarchy-logo face.

use std::fs;
use std::path::{Path, PathBuf};

// The registered Pango family name is "Adwaita Sans" — referenced
// literally in the tooltip markup (`<span face="Adwaita Sans">…`) since
// those strings are `const`/view-property literals that can't
// interpolate. Matches the cohort apps' chord chip.

const ADWAITA_SANS_TTF: &[u8] = include_bytes!("assets/AdwaitaSans-Regular.ttf");

/// Materialize the bundled face in the per-user cache dir and register
/// it with fontconfig so Pango can resolve the "Adwaita Sans" family.
/// Call once, early in startup, before any text is laid out. Safe to
/// call multiple times: the cache write is idempotent and re-adding a
/// known file is a no-op for fontconfig.
pub fn install() {
    let Some(dir) = cache_dir() else {
        eprintln!("glyph_font: no cache dir, skipping bundled-font registration");
        return;
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("glyph_font: create_dir_all({}): {e}", dir.display());
        return;
    }

    let adwaita_path = dir.join("tensaku-AdwaitaSans-Regular.ttf");
    if let Err(e) = ensure_file(&adwaita_path, ADWAITA_SANS_TTF) {
        eprintln!(
            "glyph_font: writing Adwaita Sans to {}: {e}",
            adwaita_path.display()
        );
        return;
    }
    if !register_app_font(&adwaita_path) {
        eprintln!(
            "glyph_font: fontconfig did not accept Adwaita Sans at {}",
            adwaita_path.display()
        );
    }
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
