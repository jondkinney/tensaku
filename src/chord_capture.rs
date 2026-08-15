//! Keyboard-chord recorder for the scroll-capture shortcut in
//! Preferences.
//!
//! The Wayland compositor (Hyprland) consumes Super-bound combos before
//! a client window ever sees them, and GTK's key controller can't
//! observe a chord the WM has already grabbed — so the Preferences
//! dialog can't honestly record SUPER-containing chords through GTK.
//! This module sidesteps GTK entirely: it opens evdev keyboards, derives
//! the keysym + modifier mask via xkbcommon, and returns the first
//! non-modifier press as a chord string in the canonical
//! `CTRL+SHIFT+ALT+SUPER+KEY` form. This is the same mechanism (and the
//! same token vocabulary) the cohort apps vernier / hyprcorrect use, so
//! a chord recorded here registers with Hyprland identically.
//!
//! Requires the user to be in the `input` group so `/dev/input/event*`
//! is readable. [`RecordError::Permission`] explains the fix otherwise.
//!
//! [`record_async`] is the GTK entry point: it runs the blocking
//! recorder on a worker thread and delivers the result back on the main
//! loop, returning a [`Recording`] handle the caller cancels on Esc.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use evdev::{Device, EventSummary, KeyCode};
use relm4::gtk::glib;
use xkbcommon::xkb;

/// Default time the recorder waits for a keypress before giving up — the
/// user may take a moment between clicking the chip and pressing the
/// chord.
pub const DEFAULT_RECORD_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors recording a chord directly via evdev.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// No keyboard devices were found under `/dev/input`.
    #[error("no keyboard devices found under /dev/input")]
    NoKeyboards,
    /// `/dev/input` devices exist but could not be opened.
    #[error(
        "permission denied reading /dev/input — add your user to the 'input' group (`sudo usermod -aG input $USER`) and log back in"
    )]
    Permission,
    /// xkbcommon failed to compile the system keymap.
    #[error("could not compile the keyboard layout (xkbcommon)")]
    Keymap,
    /// No key was pressed within the timeout.
    #[error("chord-capture timed out")]
    Timeout,
}

/// Open every keyboard under `/dev/input`, wait for the first
/// non-modifier key press, and return the chord string.
///
/// Blocking; runs the evdev reader threads only for the duration of the
/// call. Prefer [`record_async`] from the GTK main loop.
pub fn record_chord(timeout: Duration) -> Result<String, RecordError> {
    let keymap_text = compile_keymap()?;
    let keyboards = keyboard_devices()?;
    let (tx, rx) = mpsc::channel::<String>();
    for device in keyboards {
        let tx = tx.clone();
        let keymap_text = keymap_text.clone();
        // Detached on purpose — the reader is blocked inside
        // `evdev::Device::fetch_events`, and there's no portable way to
        // interrupt that from another thread. Once the winning reader's
        // `tx.send()` succeeds, this `rx` is dropped; the other readers
        // exit on their *next* event (`tx.send` returns `Err`), which
        // happens the next time the user touches a key on that device.
        thread::spawn(move || read_until_chord(device, &keymap_text, &tx));
    }
    drop(tx);
    match rx.recv_timeout(timeout) {
        Ok(chord) => Ok(chord),
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
            Err(RecordError::Timeout)
        }
    }
}

/// Handle to an in-flight [`record_async`] recording. Drop or
/// [`cancel`](Self::cancel) it to ignore the eventual result (the worker
/// thread can't be interrupted mid-`fetch_events`, but its result is
/// discarded and the callback never fires).
pub struct Recording {
    cancelled: Rc<Cell<bool>>,
}

impl Recording {
    /// Stop delivering the result — the in-flight capture is abandoned.
    pub fn cancel(&self) {
        self.cancelled.set(true);
    }
}

/// Record a chord without blocking the UI: spawn the blocking recorder
/// on a worker thread and poll for its result on the GTK main loop,
/// invoking `on_result` once when it arrives (unless cancelled). Returns
/// a [`Recording`] handle the caller cancels on Esc / re-record.
///
/// Must be called from the GTK main thread (it installs a `glib`
/// timeout source). The `on_result` closure also runs on the main
/// thread, so it may touch widgets freely.
pub fn record_async<F>(timeout: Duration, on_result: F) -> Recording
where
    F: Fn(Result<String, RecordError>) + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<String, RecordError>>();
    thread::spawn(move || {
        let _ = tx.send(record_chord(timeout));
    });

    let cancelled = Rc::new(Cell::new(false));
    let cancelled_poll = cancelled.clone();
    glib::timeout_add_local(Duration::from_millis(40), move || {
        if cancelled_poll.get() {
            return glib::ControlFlow::Break;
        }
        match rx.try_recv() {
            Ok(result) => {
                on_result(result);
                glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });

    Recording { cancelled }
}

fn compile_keymap() -> Result<String, RecordError> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap =
        xkb::Keymap::new_from_names(&context, "", "", "", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
            .ok_or(RecordError::Keymap)?;
    Ok(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
}

fn keyboard_devices() -> Result<Vec<Device>, RecordError> {
    let entries = match std::fs::read_dir("/dev/input") {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(RecordError::Permission);
        }
        Err(_) => return Err(RecordError::NoKeyboards),
    };

    let mut keyboards = Vec::new();
    let mut permission_denied = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_event_node = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event_node {
            continue;
        }
        match Device::open(&path) {
            Ok(device) if is_keyboard(&device) => keyboards.push(device),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                permission_denied = true;
            }
            Err(e) => eprintln!("chord_capture: error opening {}: {e}", path.display()),
        }
    }
    if !keyboards.is_empty() {
        Ok(keyboards)
    } else if permission_denied {
        Err(RecordError::Permission)
    } else {
        Err(RecordError::NoKeyboards)
    }
}

fn is_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::KEY_A))
}

/// Read one device until the first non-modifier press, then send the
/// chord string back. Exits if the receiver is dropped (chord captured
/// on another device, or timeout / cancel).
fn read_until_chord(mut device: Device, keymap_text: &str, tx: &mpsc::Sender<String>) {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let Some(keymap) = xkb::Keymap::new_from_string(
        &context,
        keymap_text.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    ) else {
        return;
    };
    let mut state = xkb::State::new(&keymap);
    loop {
        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(_) => return,
        };
        for input in events {
            let EventSummary::Key(_, code, value) = input.destructure() else {
                continue;
            };
            // evdev keycodes are offset by 8 from xkb keycodes.
            let keycode = xkb::Keycode::new(u32::from(code.0) + 8);
            if value == 1
                && let Some(chord) = chord_from_state(&state, keycode)
            {
                let _ = tx.send(chord);
                return;
            }
            // Update modifier state on press (1) and release (0), but
            // not on auto-repeat (2).
            if value != 2 {
                let direction = if value == 0 {
                    xkb::KeyDirection::Up
                } else {
                    xkb::KeyDirection::Down
                };
                state.update_key(keycode, direction);
            }
        }
    }
}

/// Build the chord string for a non-modifier key pressed in the current
/// xkb modifier state. Returns `None` for a bare modifier press (so the
/// caller keeps reading). Modifier order is the canonical
/// CTRL, SHIFT, ALT, SUPER — what Hyprland and the cohort chip expect.
fn chord_from_state(state: &xkb::State, keycode: xkb::Keycode) -> Option<String> {
    let sym = state.key_get_one_sym(keycode).raw();
    if is_modifier_keysym(sym) {
        return None;
    }
    let key_token = chord_key_token(sym)?;
    let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
    let mut parts: Vec<&str> = Vec::new();
    if active(xkb::MOD_NAME_CTRL) {
        parts.push("CTRL");
    }
    if active(xkb::MOD_NAME_SHIFT) {
        parts.push("SHIFT");
    }
    if active(xkb::MOD_NAME_ALT) {
        parts.push("ALT");
    }
    if active(xkb::MOD_NAME_LOGO) {
        parts.push("SUPER");
    }
    Some(if parts.is_empty() {
        key_token
    } else {
        format!("{}+{key_token}", parts.join("+"))
    })
}

/// Canonical token for a key's keysym. Matches the cohort's vocabulary
/// so a recorded chord round-trips into glyph markup and Hyprland bind
/// syntax. Keeps Escape from showing up as "ESCAPE" and the punctuation
/// keys from colliding with the `+` modifier separator.
fn chord_key_token(sym: u32) -> Option<String> {
    let named = match sym {
        0xff1b => Some("ESC"),            // Escape
        0xff0d | 0xff8d => Some("ENTER"), // Return / KP_Enter
        0xff09 => Some("TAB"),            // Tab
        0xff08 => Some("BACKSPACE"),      // BackSpace
        0xffff => Some("DELETE"),         // Delete
        0xff52 => Some("UP"),             // Up
        0xff54 => Some("DOWN"),           // Down
        0xff51 => Some("LEFT"),           // Left
        0xff53 => Some("RIGHT"),          // Right
        0x20 => Some("SPACE"),            // space
        0x2b => Some("PLUS"),             // +
        0x2d => Some("MINUS"),            // -
        0x3d => Some("EQUAL"),            // =
        _ => None,
    };
    if let Some(token) = named {
        return Some(token.to_string());
    }
    if (0x21..=0x7E).contains(&sym) {
        let ch = char::from_u32(sym)?.to_ascii_uppercase();
        return Some(ch.to_string());
    }
    let name = xkb::keysym_get_name(xkb::Keysym::from(sym));
    if name.is_empty() {
        return None;
    }
    Some(name.to_ascii_uppercase())
}

fn is_modifier_keysym(sym: u32) -> bool {
    (0xffe1..=0xffee).contains(&sym)
}
