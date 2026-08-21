//! Preferences dialog — keyboard-shortcut customization and other
//! session-wide settings (annotation size factor, scroll inversion,
//! Esc behavior, palette visibility, sticky in-session defaults).
//!
//! Lays out one row per tool with a recorder button that captures a
//! single keypress and writes it into the working keybind map. Save
//! commits keybinds to `APP_CONFIG`; Cancel discards keybind edits.
//! The behavior toggles apply immediately on change and persist to the
//! active config file on the spot — they're not part of the keybind
//! Cancel/Save transaction. The global scroll-capture chord remains in
//! `state.toml` because it also mirrors an external compositor binding.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use relm4::Sender;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::prelude::*;

use crate::configuration::APP_CONFIG;
use crate::sketch_board::{SketchBoardInput, SketchBoardOutput};
use crate::tools::Tools;

/// Order tools appear in the prefs dialog. Mirrors the top
/// toolbar's left-to-right order so the user can scan visually
/// across both surfaces without re-translating positions.
const ROW_ORDER: &[Tools] = &[
    Tools::Pointer,
    Tools::Crop,
    Tools::Brush,
    Tools::Line,
    Tools::Arrow,
    Tools::Rectangle,
    Tools::Ellipse,
    Tools::Text,
    Tools::Marker,
    Tools::Blur,
    Tools::Highlighter,
    Tools::Spotlight,
];

/// Label shown on the recorder button while waiting for a keypress.
const PROMPT_LABEL: &str = "Press a key…";

/// Pretty chip text for the restore-region shortcut ("Ctrl+Shift+R",
/// "R", or an em-dash when unset/unparseable).
fn restore_shortcut_display(shortcut: &str) -> String {
    if shortcut.trim().is_empty() {
        return "—".into();
    }
    shortcut
        .split('+')
        .map(|token| {
            let token = token.trim();
            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => "Ctrl".to_string(),
                "shift" => "Shift".to_string(),
                "alt" => "Alt".to_string(),
                "super" | "meta" | "mod4" => "Super".to_string(),
                other => other.to_uppercase(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Canonical config string ("ctrl+shift+r" style) for a recorded key
/// press, restricted to what `sketch_board::parse_shortcut` can read
/// back: ASCII alphanumerics and F-keys, with Ctrl/Shift/Alt/Super
/// modifiers. `None` = unusable key, keep listening.
fn restore_shortcut_string(key: gdk::Key, modifier: gdk::ModifierType) -> Option<String> {
    let key_token = if let Some(c) = key.to_unicode().filter(|c| c.is_ascii_alphanumeric()) {
        c.to_ascii_lowercase().to_string()
    } else {
        let name = key.name()?;
        let is_fkey = name.starts_with('F')
            && name.len() >= 2
            && name[1..].chars().all(|c| c.is_ascii_digit());
        if !is_fkey {
            return None;
        }
        name.to_ascii_lowercase()
    };

    let mut tokens = Vec::new();
    if modifier.contains(gdk::ModifierType::CONTROL_MASK) {
        tokens.push("ctrl".to_string());
    }
    if modifier.contains(gdk::ModifierType::SHIFT_MASK) {
        tokens.push("shift".to_string());
    }
    if modifier.contains(gdk::ModifierType::ALT_MASK) {
        tokens.push("alt".to_string());
    }
    if modifier.contains(gdk::ModifierType::SUPER_MASK) {
        tokens.push("super".to_string());
    }
    tokens.push(key_token);
    Some(tokens.join("+"))
}

/// Display fragment for an unset shortcut. Most tools won't be in
/// this state, but the configuration's default doesn't bind every
/// tool (e.g. there's no default for Spotlight in user config until
/// they set it), so this covers the gap.
const EMPTY_LABEL: &str = "—";

/// Per-row state — kept alive in a `Vec` on the dialog so each row's
/// closures can find and refresh sibling rows when a key reassignment
/// orphans them.
struct Row {
    tool: Tools,
    button: gtk::Button,
}

impl Row {
    /// Refresh the button label to reflect the working-map value for
    /// this row's tool (i.e. find the char that currently points to
    /// `self.tool`, or fall back to the empty marker).
    fn refresh(&self, working: &HashMap<char, Tools>) {
        let ch = current_char_for(working, self.tool);
        self.button.set_label(&label_for(ch));
    }
}

/// Locate the character currently mapped to `tool` in the working map,
/// if any. The map is char→Tool so a reverse lookup is necessary.
fn current_char_for(working: &HashMap<char, Tools>, tool: Tools) -> Option<char> {
    working.iter().find_map(|(c, t)| (*t == tool).then_some(*c))
}

/// Format a character (or its absence) for display on the recorder
/// button. Uppercased so single-letter shortcuts read consistently
/// regardless of how the user persisted them.
fn label_for(ch: Option<char>) -> String {
    match ch {
        Some(c) => c.to_ascii_uppercase().to_string(),
        None => EMPTY_LABEL.to_string(),
    }
}

/// Open the Preferences dialog, parented (transient) to `root` so the
/// window manager treats it as a modal child of the main satty window.
///
/// `sketch_board_sender` is the channel by which the annotation-size
/// SpinButton pushes its live value into sketch_board's `self.style`
/// so a change takes effect immediately for the next stroke (otherwise
/// APP_CONFIG would update but sketch_board's already-captured value
/// wouldn't refresh until the next launch).
///
/// `prefs_factor_spin_slot` is App's shared handle to this dialog's
/// annotation-size SpinButton + its `value-changed` signal id. The
/// dialog populates it on open and clears it on close so the welcome
/// modal's live updates can push values straight into this spin (and
/// be told whether to bother — `None` means "Preferences isn't open,
/// no UI to sync").
pub fn open<W: IsA<gtk::Widget>>(
    root: &W,
    sketch_board_sender: Sender<SketchBoardInput>,
    prefs_factor_spin_slot: std::rc::Rc<
        std::cell::RefCell<Option<(gtk::SpinButton, gtk::glib::SignalHandlerId)>>,
    >,
) {
    let toplevel = root.root().and_then(|r| r.downcast::<gtk::Window>().ok());

    let dialog = gtk::Window::builder()
        .title("Preferences")
        .modal(true)
        .destroy_with_parent(true)
        // Sized to fit the longest tool label + recorder chip
        // comfortably; anything wider just adds dead space on each
        // side of the row.
        .default_width(320)
        .resizable(false)
        .build();
    if let Some(w) = &toplevel {
        dialog.set_transient_for(Some(w));
    }

    // True while the scroll-capture chord recorder is armed. Shared with
    // the dialog-level Esc handler below so Esc cancels an in-flight
    // recording (handled by the evdev recorder's own ESC capture) instead
    // of closing the whole dialog out from under it.
    let scroll_recording_active: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // Window-level shortcuts. Esc closes the dialog; Super+W also
    // closes it (so the user's "close window" muscle memory targets
    // the dialog instead of falling through to satty's main window,
    // which would otherwise be Hyprland's natural target). Bubble
    // phase so the per-row recorder controller — which uses Esc to
    // cancel a recording — gets first crack at the keystroke while
    // recording is in progress.
    {
        let dialog_for_keys = dialog.clone();
        let recording_for_keys = scroll_recording_active.clone();
        let key_controller = gtk::EventControllerKey::new();
        key_controller.connect_key_pressed(move |_c, key, _code, mods| {
            if key == gdk::Key::Escape && mods.is_empty() {
                // While recording, Esc cancels the capture (the evdev
                // recorder sees the Esc press and reports it as a cancel)
                // rather than closing the dialog.
                if recording_for_keys.get() {
                    return gtk::glib::Propagation::Stop;
                }
                dialog_for_keys.close();
                return gtk::glib::Propagation::Stop;
            }
            if mods.contains(gdk::ModifierType::SUPER_MASK)
                && matches!(key, gdk::Key::w | gdk::Key::W)
            {
                dialog_for_keys.close();
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
        dialog.add_controller(key_controller);
    }

    // Cap the dialog at 95% of the parent canvas height. The entire
    // dialog content (shortcuts + behavior + buttons) sits inside
    // ONE outer scroller — if it fits naturally the dialog shrinks
    // to its content with no scrollbar; if it overflows the parent
    // height, the whole panel scrolls together instead of just the
    // shortcuts list scrolling separately from the behavior section
    // beneath it.
    let parent_h = toplevel
        .as_ref()
        .map(|w| w.height())
        .filter(|h| *h > 0)
        .unwrap_or(900);
    let max_dialog_h = (((parent_h as f64) * 0.95) as i32).max(320);

    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    let top_version_label = gtk::Label::builder()
        .label(format!("Tensaku {}", env!("CARGO_PKG_VERSION")))
        .halign(gtk::Align::End)
        .build();
    top_version_label.add_css_class("dim-label");
    outer.append(&top_version_label);

    let heading = gtk::Label::builder()
        .label("Keyboard Shortcuts")
        .halign(gtk::Align::Start)
        .build();
    heading.add_css_class("title-3");
    outer.append(&heading);

    let hint = gtk::Label::builder()
        .label(
            "Click a shortcut and press a key to record. \
             Press Esc to cancel a recording.",
        )
        .wrap(true)
        .xalign(0.0)
        .build();
    hint.add_css_class("dim-label");
    outer.append(&hint);

    // Working keybind map — clones the current APP_CONFIG state so the
    // user's edits are scratch until they press Save.
    let initial_shortcuts: HashMap<char, Tools> = APP_CONFIG.read().keybinds().shortcuts().clone();
    let working: Rc<RefCell<HashMap<char, Tools>>> = Rc::new(RefCell::new(initial_shortcuts));

    // Shared "is some row currently recording" flag. We only allow one
    // row in recording state at a time — clicking a second row while
    // the first is active cancels the first.
    let rows: Rc<RefCell<Vec<Row>>> = Rc::new(RefCell::new(Vec::new()));
    let recording_row: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));

    // List of recorder rows in a scrolled container so longer tool
    // lists scroll rather than blowing past the dialog's chrome.
    let list_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    for tool in ROW_ORDER {
        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        // Prefs-only mnemonic hints that connect each tool's default
        // single-key shortcut to something memorable. Kept here rather
        // than in `display_name` so tool tooltips / toasts stay terse.
        // (Counter / Text / Arrow / etc. are self-evidently mnemonic, so
        // they get no hint.)
        let hint = match *tool {
            Tools::Highlighter => Some("wide marker"),  // w → wide
            Tools::Spotlight => Some("glow"),           // g → glow
            Tools::Line => Some("left-hand twin of L"), // s sits where L does, left hand
            Tools::Brush => Some("zigzag stroke"),      // z → a zigzag freehand stroke
            Tools::Crop => Some("scissors"),            // x looks like scissors ✂
            // Pointer (v) gets no hint — it's the default selection tool
            // and reads clearly on its own (cf. Photoshop).
            _ => None,
        };
        let label_text = match hint {
            Some(h) => format!("{} ({h})", tool.display_name()),
            None => tool.display_name().to_string(),
        };
        let name = gtk::Label::builder()
            .label(label_text)
            .halign(gtk::Align::Start)
            .hexpand(true)
            .build();
        row_box.append(&name);

        let ch = current_char_for(&working.borrow(), *tool);
        let button = gtk::Button::builder()
            .label(label_for(ch))
            .width_request(96)
            .halign(gtk::Align::End)
            .build();
        button.add_css_class("monospace");
        row_box.append(&button);

        list_box.append(&row_box);

        let row_index = rows.borrow().len();
        rows.borrow_mut().push(Row {
            tool: *tool,
            button: button.clone(),
        });

        // Click → enter recording mode for this row.
        let working_for_click = working.clone();
        let rows_for_click = rows.clone();
        let recording_for_click = recording_row.clone();
        let tool_for_click = *tool;
        let button_for_click = button.clone();
        button.connect_clicked(move |btn| {
            // Cancel any other row that was mid-recording — refresh its
            // label from the working map (its prior committed value).
            if let Some(prev) = recording_for_click.get()
                && prev != row_index
                && let Some(row) = rows_for_click.borrow().get(prev)
            {
                row.refresh(&working_for_click.borrow());
            }
            recording_for_click.set(Some(row_index));
            btn.set_label(PROMPT_LABEL);
            btn.grab_focus();

            // Attach a one-shot key controller. Esc reverts; any other
            // single character commits as the new shortcut for this
            // row's tool. We capture from the inner button (not the
            // window) so the controller's lifetime is tied to the
            // button — disconnecting from the button on the same tick
            // we capture would have to wait for the event handler to
            // return first.
            let controller = gtk::EventControllerKey::new();
            let working_inner = working_for_click.clone();
            let rows_inner = rows_for_click.clone();
            let recording_inner = recording_for_click.clone();
            let btn_inner = button_for_click.clone();
            let tool_inner = tool_for_click;
            controller.connect_key_pressed(move |ctrl, key, _code, modifier| {
                // Ignore plain modifier presses (Shift / Ctrl / etc.)
                // so the user can hold modifiers and then press a key
                // without the bare modifier being captured first.
                if matches!(
                    key,
                    gdk::Key::Shift_L
                        | gdk::Key::Shift_R
                        | gdk::Key::Control_L
                        | gdk::Key::Control_R
                        | gdk::Key::Alt_L
                        | gdk::Key::Alt_R
                        | gdk::Key::Super_L
                        | gdk::Key::Super_R
                ) {
                    return gtk::glib::Propagation::Proceed;
                }

                // Esc → cancel recording, revert label.
                if key == gdk::Key::Escape {
                    if let Some(row) = rows_inner.borrow().get(row_index) {
                        row.refresh(&working_inner.borrow());
                    }
                    recording_inner.set(None);
                    // One-shot: drop the controller so we don't keep
                    // intercepting subsequent presses.
                    btn_inner.remove_controller(ctrl);
                    return gtk::glib::Propagation::Stop;
                }

                // Disallow modifier-combined keys — shortcuts are
                // single chars throughout the codebase.
                if !modifier.is_empty()
                    && modifier.intersection(
                        gdk::ModifierType::CONTROL_MASK
                            | gdk::ModifierType::ALT_MASK
                            | gdk::ModifierType::SUPER_MASK,
                    ) != gdk::ModifierType::empty()
                {
                    return gtk::glib::Propagation::Proceed;
                }

                // Try to turn the key into a single printable char.
                let Some(c_raw) = key.to_unicode() else {
                    return gtk::glib::Propagation::Proceed;
                };
                let ch = c_raw.to_ascii_lowercase();
                if !ch.is_ascii_alphanumeric() {
                    // Reject punctuation / control / etc. for now —
                    // matches the existing configuration's validation.
                    return gtk::glib::Propagation::Proceed;
                }

                // Commit: drop any other tool that owned `ch`, then
                // assign `ch` → this row's tool. Refresh BOTH rows so
                // the displaced tool's label updates to "—".
                let mut map = working_inner.borrow_mut();
                let displaced: Option<Tools> = map.get(&ch).copied();
                // First, drop the assignment this tool currently holds
                // (if any) so the map stays in (char → unique tool)
                // shape after the insert.
                map.retain(|_, t| *t != tool_inner);
                map.insert(ch, tool_inner);
                drop(map);

                let working_snapshot = working_inner.borrow();
                for (i, row) in rows_inner.borrow().iter().enumerate() {
                    if i == row_index || displaced == Some(row.tool) {
                        row.refresh(&working_snapshot);
                    }
                }
                drop(working_snapshot);

                recording_inner.set(None);
                btn_inner.remove_controller(ctrl);
                gtk::glib::Propagation::Stop
            });
            btn.add_controller(controller);
        });
    }

    outer.append(&list_box);

    // ── Scroll capture ──────────────────────────────────────────────
    // A recordable system-wide chord that launches scroll-capture mode.
    // Unlike the per-tool shortcuts above (scratch until Save), this one
    // applies immediately: recording or clearing it registers /
    // unregisters the Hyprland keybind and persists on the spot — so it
    // lives outside the dialog's Save/Cancel transaction, like the
    // behavior toggles below.
    let scroll_heading = gtk::Label::builder()
        .label("Scroll Capture")
        .halign(gtk::Align::Start)
        .margin_top(8)
        .build();
    scroll_heading.add_css_class("title-3");
    outer.append(&scroll_heading);

    let scroll_hint = gtk::Label::builder()
        .label(
            "A global shortcut that launches a scrolling screenshot capture. \
             Click the chip and press a key combination (Super included). \
             On supported Hyprland/Omarchy sessions, Tensaku activates and \
             saves it automatically.",
        )
        .wrap(true)
        .xalign(0.0)
        .build();
    scroll_hint.add_css_class("dim-label");
    outer.append(&scroll_hint);

    let scroll_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let scroll_label = gtk::Label::builder()
        .label("Trigger scroll capture")
        .halign(gtk::Align::Start)
        .hexpand(true)
        .build();
    scroll_row.append(&scroll_label);

    // The chip renders the chord as glyphs (⌃ ⇧ ⌥ + the Omarchy Super
    // logo) via Pango markup, so a Label child carries the markup rather
    // than the Button's plain-text label.
    let scroll_chip_label = gtk::Label::new(None);
    scroll_chip_label.set_use_markup(true);
    let scroll_chip = gtk::Button::builder()
        .child(&scroll_chip_label)
        .width_request(140)
        .halign(gtk::Align::End)
        .tooltip_text("Click, then press the key combination to record")
        .build();
    scroll_row.append(&scroll_chip);

    let scroll_clear = gtk::Button::builder()
        .label("✕")
        .tooltip_text("Clear the scroll-capture shortcut")
        .valign(gtk::Align::Center)
        .build();
    scroll_clear.add_css_class("flat");
    scroll_clear.add_css_class("circular");
    scroll_row.append(&scroll_clear);
    outer.append(&scroll_row);

    // One-line feedback after recording/clearing (e.g. "active now and
    // saved", or a permission error). Hidden until there's something to
    // say.
    let scroll_status = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .build();
    scroll_status.add_css_class("dim-label");
    outer.append(&scroll_status);

    let park_pointer_help = "Moves the pointer near the lower-right of the selected area before capture to reduce \
         hover effects. Turn this off to leave the pointer where you placed it.";
    let park_pointer_check = gtk::CheckButton::builder()
        .label("Park pointer when manual scroll capture starts")
        .tooltip_text(park_pointer_help)
        .active(
            APP_CONFIG
                .read()
                .park_pointer_during_manual_scroll_capture(),
        )
        .build();
    park_pointer_check
        .update_property(&[gtk::accessible::Property::Description(park_pointer_help)]);
    park_pointer_check.connect_toggled(|btn| {
        let value = btn.is_active();
        let current = APP_CONFIG
            .read()
            .park_pointer_during_manual_scroll_capture();
        if value == current {
            return;
        }
        let result = APP_CONFIG
            .write()
            .save_park_pointer_during_manual_scroll_capture(value);
        if let Err(error) = result {
            eprintln!("Warning: could not save park-pointer-during-manual-scroll-capture: {error}");
            btn.set_active(current);
        }
    });
    outer.append(&park_pointer_check);

    // Recordable in-overlay key that reselects the previous capture's
    // region while choosing a selection. Commits immediately, like the
    // other Scroll Capture controls.
    let restore_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let restore_help = "While selecting a scroll-capture region, press this key to reselect \
         the region used by the previous capture.";
    let restore_label = gtk::Label::builder()
        .label("Restore previous region")
        .tooltip_text(restore_help)
        .halign(gtk::Align::Start)
        .hexpand(true)
        .build();
    restore_row.append(&restore_label);
    let restore_chip = gtk::Button::builder()
        .label(restore_shortcut_display(
            APP_CONFIG.read().scroll_capture_restore_region_shortcut(),
        ))
        .width_request(96)
        .halign(gtk::Align::End)
        .tooltip_text("Click, then press a key (modifiers allowed) to record")
        .build();
    restore_chip.add_css_class("monospace");
    restore_chip.update_property(&[gtk::accessible::Property::Description(restore_help)]);
    restore_row.append(&restore_chip);
    outer.append(&restore_row);

    restore_chip.connect_clicked(move |btn| {
        btn.set_label(PROMPT_LABEL);
        btn.grab_focus();
        let controller = gtk::EventControllerKey::new();
        let btn_inner = btn.clone();
        controller.connect_key_pressed(move |ctrl, key, _code, modifier| {
            if matches!(
                key,
                gdk::Key::Shift_L
                    | gdk::Key::Shift_R
                    | gdk::Key::Control_L
                    | gdk::Key::Control_R
                    | gdk::Key::Alt_L
                    | gdk::Key::Alt_R
                    | gdk::Key::Super_L
                    | gdk::Key::Super_R
            ) {
                return gtk::glib::Propagation::Proceed;
            }

            let current = APP_CONFIG
                .read()
                .scroll_capture_restore_region_shortcut()
                .to_string();
            if key == gdk::Key::Escape {
                btn_inner.set_label(&restore_shortcut_display(&current));
                btn_inner.remove_controller(ctrl);
                return gtk::glib::Propagation::Stop;
            }

            let Some(shortcut) = restore_shortcut_string(key, modifier) else {
                return gtk::glib::Propagation::Proceed;
            };
            match APP_CONFIG
                .write()
                .save_scroll_capture_restore_region_shortcut(shortcut.clone())
            {
                Ok(()) => btn_inner.set_label(&restore_shortcut_display(&shortcut)),
                Err(error) => {
                    eprintln!(
                        "Warning: could not save scroll-capture-restore-region-shortcut: {error}"
                    );
                    btn_inner.set_label(&restore_shortcut_display(&current));
                }
            }
            btn_inner.remove_controller(ctrl);
            gtk::glib::Propagation::Stop
        });
        btn.add_controller(controller);
    });

    // Committed chord (mirrors state.toml) and the in-flight recorder
    // handle (Some while listening for a keypress).
    let scroll_chord: Rc<RefCell<Option<String>>> =
        Rc::new(RefCell::new(crate::state::load_scroll_capture_shortcut()));
    let scroll_recorder: Rc<RefCell<Option<crate::chord_capture::Recording>>> =
        Rc::new(RefCell::new(None));

    render_scroll_chip(
        &scroll_chip_label,
        &scroll_clear,
        scroll_chord.borrow().as_deref(),
    );

    scroll_chip.connect_clicked({
        let scroll_chord = scroll_chord.clone();
        let scroll_recorder = scroll_recorder.clone();
        let recording_active = scroll_recording_active.clone();
        let chip_label = scroll_chip_label.clone();
        let clear_btn = scroll_clear.clone();
        let status = scroll_status.clone();
        move |_| {
            // Cancel any in-flight recording before re-arming.
            if let Some(rec) = scroll_recorder.borrow_mut().take() {
                rec.cancel();
            }
            recording_active.set(true);
            status.set_visible(false);
            chip_label.set_markup("Press a shortcut…");
            clear_btn.set_visible(false);

            let rec =
                crate::chord_capture::record_async(crate::chord_capture::DEFAULT_RECORD_TIMEOUT, {
                    let scroll_chord = scroll_chord.clone();
                    let scroll_recorder = scroll_recorder.clone();
                    let recording_active = recording_active.clone();
                    let chip_label = chip_label.clone();
                    let clear_btn = clear_btn.clone();
                    let status = status.clone();
                    move |result| {
                        scroll_recorder.borrow_mut().take();
                        recording_active.set(false);
                        match result {
                            // Bare Esc cancels — revert to the prior chord.
                            Ok(chord) if chord == "ESC" => {
                                render_scroll_chip(
                                    &chip_label,
                                    &clear_btn,
                                    scroll_chord.borrow().as_deref(),
                                );
                            }
                            Ok(chord) => {
                                // Drop any prior bind, then register the new one.
                                if let Some(old) = scroll_chord.borrow().clone()
                                    && old != chord
                                {
                                    crate::hypr_bind::unregister(&old);
                                }
                                crate::state::save_scroll_capture_shortcut(Some(chord.clone()));
                                let outcome = crate::hypr_bind::register(&chord);
                                *scroll_chord.borrow_mut() = Some(chord.clone());
                                render_scroll_chip(&chip_label, &clear_btn, Some(&chord));
                                show_scroll_status(&status, scroll_register_message(outcome));
                            }
                            Err(e) => {
                                render_scroll_chip(
                                    &chip_label,
                                    &clear_btn,
                                    scroll_chord.borrow().as_deref(),
                                );
                                show_scroll_status(&status, e.to_string());
                            }
                        }
                    }
                });
            *scroll_recorder.borrow_mut() = Some(rec);
        }
    });

    scroll_clear.connect_clicked({
        let scroll_chord = scroll_chord.clone();
        let scroll_recorder = scroll_recorder.clone();
        let recording_active = scroll_recording_active.clone();
        let chip_label = scroll_chip_label.clone();
        let clear_btn = scroll_clear.clone();
        let status = scroll_status.clone();
        move |_| {
            if let Some(rec) = scroll_recorder.borrow_mut().take() {
                rec.cancel();
            }
            recording_active.set(false);
            if let Some(old) = scroll_chord.borrow_mut().take() {
                crate::hypr_bind::unregister(&old);
            }
            crate::state::save_scroll_capture_shortcut(None);
            render_scroll_chip(&chip_label, &clear_btn, None);
            status.set_visible(false);
        }
    });

    // Behavior section sits BELOW the shortcuts list — the keyboard
    // recorder is the dialog's primary content, the behavior
    // toggles are secondary preferences. Each toggle applies
    // immediately and persists to the active config file on click;
    // the dialog's Save button only commits the keyboard shortcuts.
    let behavior_heading = gtk::Label::builder()
        .label("Behavior")
        .halign(gtk::Align::Start)
        .margin_top(8)
        .build();
    behavior_heading.add_css_class("title-3");
    outer.append(&behavior_heading);

    // Annotation size factor — the multiplier that scales every
    // Size-based metric (text height, line width, arrow heads, blur
    // radius). Mostly set once during onboarding to match the user's
    // display scale; this row lets them tune it later without hunting
    // through config files. Changes write through the active config file
    // immediately and push directly into sketch_board so the very
    // next stroke uses the new factor.
    let factor_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let factor_label = gtk::Label::builder()
        .label("Annotation size factor")
        .halign(gtk::Align::Start)
        .build();
    factor_row.append(&factor_label);
    // "?" help button — re-launches the first-run welcome dialog so the
    // user can revisit the explanation of what this factor controls and
    // re-pick a value through the onboarding UI (including the
    // "Use detected" / "1.00×" reset shortcuts that this spin button
    // alone doesn't expose). Sits right next to the label so the
    // affordance reads as "what is this setting?" rather than as a
    // sibling control of the spin.
    let factor_help = gtk::Button::builder()
        .label("?")
        .tooltip_text("What does this do? Re-open the welcome guide.")
        .valign(gtk::Align::Center)
        .hexpand(false)
        .build();
    factor_help.add_css_class("circular");
    factor_help.add_css_class("flat");
    let factor_help_sender = sketch_board_sender.clone();
    factor_help.connect_clicked(move |_| {
        let _ = factor_help_sender.send(SketchBoardInput::Output(
            SketchBoardOutput::OpenWelcomeDialog,
        ));
    });
    factor_row.append(&factor_help);
    // Spacer so the spin lands flush against the right edge instead of
    // hugging the help button. `hexpand` on a blank Label is the most
    // compact way to flex-fill the gap without dragging in an Adw box.
    let factor_row_spacer = gtk::Label::builder().hexpand(true).build();
    factor_row.append(&factor_row_spacer);
    let factor_spin = gtk::SpinButton::builder()
        .adjustment(&gtk::Adjustment::new(
            APP_CONFIG.read().annotation_size_factor().into(),
            // 0.10..=10.0 with 0.1 detents matches the canvas-side
            // Alt+scroll constants in `scroll_annotation_multiplier`
            // so both paths land on the same grid.
            0.10,
            10.0,
            0.10,
            0.50,
            0.0,
        ))
        .climb_rate(0.1)
        .digits(1)
        .numeric(true)
        .build();
    let factor_sender = sketch_board_sender.clone();
    let factor_handler = factor_spin.connect_value_changed(move |btn| {
        // Persist + broadcast happens centrally in App's
        // AnnotationFactorChanged handler so the welcome modal (if
        // open) gets the value pushed in too.
        let value = btn.value() as f32;
        let _ = factor_sender.send(SketchBoardInput::Output(
            SketchBoardOutput::AnnotationFactorChanged(value),
        ));
    });
    factor_spin.set_tooltip_text(Some(
        "Scales the size of every annotation (text, line width, arrow heads, …). \
         Set this to roughly match your display scale; values above 1 enlarge.",
    ));
    factor_row.append(&factor_spin);
    outer.append(&factor_row);

    // Hand App a clone of the spin + its signal id so the welcome
    // dialog's live updates can push values in here. Clear on close so
    // App stops trying to update a destroyed widget.
    *prefs_factor_spin_slot.borrow_mut() = Some((factor_spin.clone(), factor_handler));
    let slot_for_close = prefs_factor_spin_slot.clone();
    dialog.connect_close_request(move |_| {
        slot_for_close.borrow_mut().take();
        relm4::gtk::glib::Propagation::Proceed
    });

    let invert_scroll_check = gtk::CheckButton::builder()
        .label("Invert scrolling direction")
        .active(APP_CONFIG.read().invert_scrolling())
        .build();
    invert_scroll_check.connect_toggled(|btn| {
        let value = btn.is_active();
        let current = APP_CONFIG.read().invert_scrolling();
        if value == current {
            return;
        }
        let result = APP_CONFIG.write().save_invert_scrolling(value);
        if let Err(error) = result {
            eprintln!("Warning: could not save invert-scrolling: {error}");
            btn.set_active(current);
        }
    });
    outer.append(&invert_scroll_check);

    
    let close_on_esc_check = gtk::CheckButton::builder()
        .label("Close window on Esc")
        .active(APP_CONFIG.read().close_on_esc())
        .build();
    close_on_esc_check.connect_toggled(|btn| {
        let value = btn.is_active();
        let current = APP_CONFIG.read().close_on_esc();
        if value == current {
            return;
        }
        let result = APP_CONFIG.write().save_close_on_esc(value);
        if let Err(error) = result {
            eprintln!("Warning: could not save close-on-esc: {error}");
            btn.set_active(current);
        }
    });
    outer.append(&close_on_esc_check);

    let close_on_copy_check = gtk::CheckButton::builder()
        .label("Close window on copy")
        .tooltip_text("Close Tensaku after copying the annotated image to the clipboard (Ctrl+C).")
        .active(APP_CONFIG.read().close_on_copy())
        .build();
    close_on_copy_check.connect_toggled(|btn| {
        let value = btn.is_active();
        let current = APP_CONFIG.read().close_on_copy();
        if value == current {
            return;
        }
        let result = APP_CONFIG.write().save_close_on_copy(value);
        if let Err(error) = result {
            eprintln!("Warning: could not save close-on-copy: {error}");
            btn.set_active(current);
        }
    });
    outer.append(&close_on_copy_check);

    let close_on_save_check = gtk::CheckButton::builder()
        .label("Close window on save")
        .tooltip_text("Close Tensaku after saving the annotated image to a file (Ctrl+S).")
        .active(APP_CONFIG.read().close_on_save())
        .build();
    close_on_save_check.connect_toggled(|btn| {
        let value = btn.is_active();
        let current = APP_CONFIG.read().close_on_save();
        if value == current {
            return;
        }
        let result = APP_CONFIG.write().save_close_on_save(value);
        if let Err(error) = result {
            eprintln!("Warning: could not save close-on-save: {error}");
            btn.set_active(current);
        }
    });
    outer.append(&close_on_save_check);

    let resize_window_to_content_on_crop_check = gtk::CheckButton::builder()
        .label("Resize window to content on crop")
        .tooltip_text(
            "When on, applying, editing, or reverting a crop resizes the editor window to \
             follow the cropped content. When off, the editor window keeps its current size \
             and fits the cropped image within it.",
        )
        .active(APP_CONFIG.read().resize_window_to_content_on_crop())
        .build();
    resize_window_to_content_on_crop_check.connect_toggled(|btn| {
        let value = btn.is_active();
        let current = APP_CONFIG.read().resize_window_to_content_on_crop();
        if value == current {
            return;
        }

        let result = APP_CONFIG
            .write()
            .save_resize_window_to_content_on_crop(value);
        if let Err(error) = result {
            eprintln!("Warning: could not save resize-window-to-content-on-crop: {error}");
            // Keep the checkbox aligned with the canonical on-disk value.
            // The resulting toggled signal exits through the equality
            // guard above, so this cannot recurse indefinitely.
            btn.set_active(current);
        }
    });
    outer.append(&resize_window_to_content_on_crop_check);

    let hide_palette_check = gtk::CheckButton::builder()
        .label("Hide default palette colors")
        .tooltip_text(
            "When on, the color picker hides its built-in 10-color palette column \
             and shows only the colors you've saved. The 1–9, 0 number-key shortcuts \
             then pick from your saved custom colors instead of the defaults.",
        )
        .active(APP_CONFIG.read().hide_default_palette())
        .build();
    hide_palette_check.connect_toggled(|btn| {
        let value = btn.is_active();
        let current = APP_CONFIG.read().hide_default_palette();
        if value == current {
            return;
        }
        let result = APP_CONFIG.write().save_hide_default_palette(value);
        if let Err(error) = result {
            eprintln!("Warning: could not save hide-default-palette: {error}");
            btn.set_active(current);
        }
    });
    outer.append(&hide_palette_check);

    // When on, per-tool adjustments (size, fill, highlighter opacity,
    // brush smoothness) stick across tool switches and only re-seed
    // from saved defaults on a fresh app launch. Off (default) keeps
    // the original snap-back-on-tool-switch behavior.
    let sticky_defaults_check = gtk::CheckButton::builder()
        .label("Keep in-session tool adjustments across tool switches")
        .tooltip_text(
            "When off, switching tools snaps each tool back to its saved default. \
             When on, your in-session size / fill / opacity tweaks persist until \
             you close the app.",
        )
        .active(APP_CONFIG.read().sticky_session_defaults())
        .build();
    sticky_defaults_check.connect_toggled(|btn| {
        let value = btn.is_active();
        let current = APP_CONFIG.read().sticky_session_defaults();
        if value == current {
            return;
        }
        let result = APP_CONFIG.write().save_sticky_session_defaults(value);
        if let Err(error) = result {
            eprintln!("Warning: could not save sticky-session-defaults: {error}");
            btn.set_active(current);
        }
    });
    outer.append(&sticky_defaults_check);

    let button_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .margin_top(8)
        .build();
    let cancel_btn = gtk::Button::builder().label("Cancel").build();
    let dialog_for_cancel = dialog.clone();
    cancel_btn.connect_clicked(move |_| dialog_for_cancel.close());
    button_row.append(&cancel_btn);

    let save_btn = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .build();
    let dialog_for_save = dialog.clone();
    let working_for_save = working.clone();
    save_btn.connect_clicked(move |_| {
        let map = working_for_save.borrow().clone();
        let result = APP_CONFIG.write().save_keybinds(map);
        match result {
            Ok(()) => dialog_for_save.close(),
            Err(error) => eprintln!("Warning: could not save keybinds: {error}"),
        }
    });
    button_row.append(&save_btn);
    outer.append(&button_row);

    let outer_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .max_content_height(max_dialog_h)
        .child(&outer)
        .build();
    dialog.set_child(Some(&outer_scroller));
    dialog.present();
}

/// Paint the scroll-capture chip and toggle the clear button to match
/// the committed chord (glyph markup) or the unset prompt.
fn render_scroll_chip(label: &gtk::Label, clear: &gtk::Button, chord: Option<&str>) {
    match chord {
        Some(c) => {
            label.set_markup(&crate::glyph_font::chord_markup(c));
            clear.set_visible(true);
        }
        None => {
            label.set_markup("Click to set");
            clear.set_visible(false);
        }
    }
}

/// Show a one-line status under the scroll-capture row.
fn show_scroll_status(label: &gtk::Label, msg: String) {
    label.set_text(&msg);
    label.set_visible(true);
}

/// Human-readable summary of what registering the bind achieved.
fn scroll_register_message(outcome: crate::hypr_bind::RegisterOutcome) -> String {
    match (outcome.live, outcome.persisted) {
        (true, true) => "Shortcut active.".to_string(),
        (true, false) => "Shortcut active for this session only.".to_string(),
        (false, true) => "Shortcut saved, but not active yet.".to_string(),
        (false, false) => "Recorded, but couldn't register it with Hyprland.".to_string(),
    }
}
