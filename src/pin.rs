//! Pin the finished shot to the desktop.
//!
//! A pinned capture is a small window in the corner of the screen
//! showing what the editor produced. It exists so a reference can stay
//! visible while you work in another window — the thing a screenshot
//! is usually *for* — instead of living in a file you have to keep
//! re-opening.
//!
//! Two decisions worth knowing:
//!
//! - **It is an ordinary floating window**, and its handle asks the
//!   compositor to drag it. A compositor moves a floating window
//!   during its own frame with the client out of the loop, which is
//!   why that feels instant; a client-positioned surface — which is
//!   what this was, as a layer surface walking its own anchor margins
//!   — cannot be anywhere but a round trip behind the pointer.
//! - **Edit keeps the annotations live.** The pin and the editor are
//!   the same process, so Edit hides the pin and shows the editor
//!   window again with every drawable still where it was, still
//!   movable. Nothing is serialised and nothing is flattened; the
//!   cost is that the pin lasts as long as the process does.
//!
//! Off Hyprland it degrades rather than breaking. The drag is the
//! standard `xdg_toplevel.move` request that every compositor
//! implements, so it works anywhere. Floating, pinning across
//! workspaces and landing in a corner slot are Hyprland dispatchers,
//! and a Wayland client cannot place itself without something like
//! them: elsewhere the pin opens wherever the compositor decides, and
//! draws its own border, since nothing promises one around an
//! undecorated window.

use crate::ui::toolbars::RobustTooltipExt;
use relm4::gtk::gdk_pixbuf::InterpType;
use relm4::gtk::{self, gdk_pixbuf::Pixbuf, prelude::*};

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// Width of a pinned capture, in CSS pixels. The height follows the
/// display's aspect.
///
/// Shaped like the screen, so a full-screen capture — the most common
/// kind — fits its pin exactly, with nothing cropped away. Everything
/// else fills the same frame and centre-crops, which keeps pins a
/// uniform size: they stack, and a column of mismatched heights is one
/// that has to be laid out rather than counted.
const PIN_WIDTH: i32 = 224;

/// Fallback aspect when the display can't be asked. 16:9 is the shape
/// of most screens and a reasonable guess for the rest.
const FALLBACK_ASPECT: f64 = 9.0 / 16.0;

/// The frame a pin's picture fills: the display's aspect at
/// [`PIN_WIDTH`].
fn pin_size() -> (i32, i32) {
    let aspect = crate::display::hyprland_focused_monitor()
        .filter(|m| m.width > 0 && m.height > 0)
        .map(|m| m.height as f64 / m.width as f64)
        .unwrap_or(FALLBACK_ASPECT);
    // Clamped so an unusual display — a tall pivot, an ultrawide —
    // still yields a pin rather than a line.
    let height = ((PIN_WIDTH as f64 * aspect).round() as i32).clamp(PIN_WIDTH / 4, PIN_WIDTH * 2);
    (PIN_WIDTH, height)
}

/// The move handle's tooltip, named because the drag hides it and has
/// to put it back.
const MOVE_TOOLTIP: &str = "Move this pin";

/// The preview's tooltip, hidden while a drag-out is in flight for the
/// same reason.
const PICTURE_TOOLTIP: &str = "Click to open in the editor · Drag into another app to paste it";

/// How often, and how many times, to look for the window before
/// giving up on placing it. Half a second in total: long enough for a
/// compositor under load, short enough that a failure doesn't leave a
/// pin drifting in from the middle of the screen much later.
const SETTLE_INTERVAL: Duration = Duration::from_millis(50);
const SETTLE_ATTEMPTS: u32 = 10;

/// Gap between stacked pins.
const PIN_GAP: i32 = 12;

/// Gap between the pin and the screen edge it starts anchored to.
const EDGE_MARGIN: i32 = 24;

/// Longest edge of the picture that follows the pointer during a
/// drag-out. A drag icon is a token for what is being carried, not a
/// copy of it — handing GTK the full-size texture makes the pointer
/// drag a whole 6K capture across the screen.
const DRAG_ICON_MAX: i32 = 256;

/// How long a confirmation stays up. Long enough to read two words,
/// short enough that it doesn't sit on the image.
const TOAST_MS: u64 = 1400;

/// What the pin can do to the session that owns it.
pub struct PinActions {
    /// Restore the editor with its annotations intact.
    pub on_edit: Box<dyn Fn()>,
    /// Put the image on the clipboard.
    pub on_copy: Box<dyn Fn()>,
    /// Put the file's path on the clipboard, saving the shot first if
    /// it isn't on disk yet.
    pub on_copy_path: Box<dyn Fn()>,
    /// Whether the shot has already been written somewhere. Only
    /// changes the wording: the button is offered either way, because
    /// "where is this file" is a question you ask of a pinned shot
    /// whether or not you remembered to save it first.
    pub path_known: bool,
    /// The saved file, when there is one. Only the drag-out needs the
    /// path itself: a drop target that takes files wants a file, and
    /// there isn't one to offer until the shot has been saved.
    pub saved_path: Option<String>,
}

/// Flash a short message over the pinned image.
pub type Toast = Rc<dyn Fn(&str)>;

/// A live pin: the window, and a handle for confirming what its
/// buttons did.
pub struct Pin {
    pub window: gtk::Window,
    /// Flash a short message over the image. Called when a copy
    /// actually lands, not when its button is pressed — copying a path
    /// can wait on a Save As dialog, and a confirmation shown before
    /// the thing happens is just a lie with good timing.
    pub toast: Toast,
}

/// Build and show the pin window for `image`.
pub fn open(image: &Pixbuf, actions: PinActions) -> Pin {
    let window = gtk::Window::builder()
        .resizable(false)
        .decorated(false)
        .title(pin_title())
        .build();
    window.add_css_class("pin-window");

    let (frame_w, frame_h) = pin_size();
    window.set_default_size(frame_w, frame_h);

    // Float it, show it on every workspace, and put it in its slot —
    // once it has a surface for the compositor to match on.
    let desktop = Desktop::detect();
    if desktop.places_windows() {
        // Closing a pin leaves a hole in the column, and the survivors
        // drop down to fill it. The pin that is leaving does the
        // re-stacking on its way out: placement is title-addressed
        // compositor IPC, so any process can move any pin, and the
        // leaving one is the only one that knows a gap just opened.
        //
        // On close-request, not destroy: closing this window hides it
        // and keeps the process (and the editor behind it) alive, so
        // destroy does not fire until the process winds down — long
        // after the gap opened.
        let own_title = pin_title();
        window.connect_close_request(move |_| {
            close_column_gaps(desktop, &own_title);
            gtk::glib::Propagation::Proceed
        });

        let slot = next_slot(desktop);
        window.connect_map(move |_| {
            // Not immediately: `map` is this side's word for "shown",
            // and the compositor has not necessarily registered the
            // window under its title yet — a request naming a window
            // that isn't there reports success and does nothing, which
            // is how a pin ended up centred and unpinned. Retry until
            // it is there.
            let attempts = Cell::new(0);
            gtk::glib::timeout_add_local(SETTLE_INTERVAL, move || {
                attempts.set(attempts.get() + 1);
                if !desktop.sees_pin() {
                    return if attempts.get() < SETTLE_ATTEMPTS {
                        gtk::glib::ControlFlow::Continue
                    } else {
                        gtk::glib::ControlFlow::Break
                    };
                }
                desktop.arrange(&pin_title(), slot);
                gtk::glib::ControlFlow::Break
            });
        });
    }

    // Scale the pixels here rather than asking the widget to shrink
    // them. A `Picture`'s natural size is its image's, so a 6K capture
    // asked for a 6K surface and the compositor clamped it — which is
    // why a pin showed a corner at full size instead of the shot.
    let preview = cover_thumbnail(image, (frame_w, frame_h));
    let picture = gtk::Picture::for_pixbuf(&preview);
    picture.set_can_shrink(true);
    picture.set_size_request(frame_w, frame_h);

    // On Hyprland the picture is the whole window: the compositor
    // draws the border and the shadow around a floating window, and a
    // mat of our own would be a second frame inside the first.
    //
    // Elsewhere there is no such promise for an undecorated window, so
    // the pin brings its own edge rather than reading as a picture
    // lying loose on the desktop.
    let overlay = gtk::Overlay::new();
    if desktop.places_windows() {
        overlay.set_child(Some(&picture));
    } else {
        let frame = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        frame.add_css_class("pin-frame");
        frame.append(&picture);
        overlay.set_child(Some(&frame));
    }

    // The picture and the Edit button do the same thing, so they share
    // one callback rather than each getting a copy of the intent.
    let saved_path_for_drag = actions.saved_path.clone();
    let edit_for_picture = std::rc::Rc::new(actions.on_edit);
    let edit_for_controls = edit_for_picture.clone();
    let actions = PinActions {
        on_edit: Box::new(move || edit_for_controls()),
        ..actions
    };
    let controls = build_controls(&window, actions);
    overlay.add_overlay(&controls);

    let (toast_label, toast) = build_toast();
    overlay.add_overlay(&toast_label);

    // The toolbar is furniture: it would cover the top of the image it
    // describes, so it appears on hover and gets out of the way again.
    controls.set_visible(false);
    let motion = gtk::EventControllerMotion::new();
    {
        let controls = controls.clone();
        motion.connect_enter(move |_, _, _| controls.set_visible(true));
    }
    {
        let controls = controls.clone();
        motion.connect_leave(move |_| controls.set_visible(false));
    }
    overlay.add_controller(motion);

    // The picture is the shot: clicking it opens the shot, dragging it
    // carries the shot somewhere.
    picture.install_tooltip(PICTURE_TOOLTIP);
    picture.set_cursor_from_name(Some("pointer"));
    install_drag_out(&picture, image, saved_path_for_drag);
    {
        let edit = edit_for_picture;
        let click = gtk::GestureClick::new();
        click.connect_released(move |gesture, count, _, _| {
            if count == 1 {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                edit();
            }
        });
        picture.add_controller(click);
    }

    window.set_child(Some(&overlay));
    window.present();
    Pin { window, toast }
}

/// The confirmation strip and the closure that flashes it.
fn build_toast() -> (gtk::Label, Toast) {
    let label = gtk::Label::new(None);
    label.add_css_class("pin-toast");
    label.set_halign(gtk::Align::Center);
    label.set_valign(gtk::Align::End);
    label.set_margin_bottom(10);
    label.set_visible(false);

    // Each showing takes a ticket; a timer only hides the toast if its
    // ticket is still the current one. Without that, two copies in
    // quick succession would have the first timer hide the second
    // message early.
    let generation = Rc::new(Cell::new(0u64));
    let label_for_show = label.clone();
    let show = move |message: &str| {
        label_for_show.set_text(message);
        label_for_show.set_visible(true);
        let ticket = generation.get().wrapping_add(1);
        generation.set(ticket);
        let label = label_for_show.clone();
        let generation = generation.clone();
        relm4::gtk::glib::timeout_add_local_once(Duration::from_millis(TOAST_MS), move || {
            if generation.get() == ticket {
                label.set_visible(false);
            }
        });
    };
    (label, Rc::new(show))
}

/// Title prefix, shared by every pin so they can recognise each other,
/// and followed by something unique so a dispatcher can name exactly
/// one of them.
///
/// Without the unique half, `title:^(Tensaku Pin)$` matches every pin
/// on screen and the compositor acts on whichever it finds first — so
/// opening a second pin moved the first one into the second's slot and
/// left the second where it spawned.
const PIN_TITLE: &str = "Tensaku Pin";

/// This pin's own title.
fn pin_title() -> String {
    // The process id is enough: a capture is a process, and a process
    // has one pin.
    format!("{PIN_TITLE} {}", std::process::id())
}

/// The compositors this pin knows how to ask for placement.
///
/// Wayland has no protocol for a window to position itself — that is
/// deliberate, and every corner-placed window on Wayland goes through
/// compositor-specific IPC. So this is a short list rather than a
/// capability check, and everything gated on it is a nicety: an
/// unrecognised compositor still gets a pin that opens, drags, edits,
/// copies and drags out. It just lands where the compositor decides.
#[derive(Clone, Copy, PartialEq)]
enum Desktop {
    Hyprland,
    Sway,
    Unknown,
}

impl Desktop {
    fn detect() -> Self {
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            Desktop::Hyprland
        } else if std::env::var_os("SWAYSOCK").is_some() {
            Desktop::Sway
        } else {
            Desktop::Unknown
        }
    }

    fn places_windows(self) -> bool {
        self != Desktop::Unknown
    }

    /// Float the pin titled `title`, show it on every workspace, and
    /// put it in `slot`.
    ///
    /// Floating first in both: a tiled window has no position of its
    /// own to set.
    fn arrange(self, title: &str, slot: i32) {
        let Some(screen) = self.screen_size() else {
            return;
        };
        let (x, y) = slot_origin(screen, slot);
        match self {
            Desktop::Hyprland => {
                let selector = format!("window = \"title:^({title})$\"");
                hypr_dispatch(&format!("hl.dsp.window.float({{ {selector} }})"));
                hypr_dispatch(&format!("hl.dsp.window.pin({{ {selector} }})"));
                hypr_dispatch(&format!(
                    "hl.dsp.window.move({{ x = {x}, y = {y}, relative = false, {selector} }})"
                ));
            }
            Desktop::Sway => {
                sway_command(&format!(
                    "[title=\"^{title}$\"] floating enable, sticky enable, \
                     move absolute position {x} {y}"
                ));
            }
            Desktop::Unknown => {}
        }
    }

    /// Move the pin titled `title` — not necessarily this process's
    /// own — to `(x, y)`. It is already floating and pinned from when
    /// it opened, so only the position needs saying.
    fn move_pin(self, title: &str, x: i32, y: i32) {
        match self {
            Desktop::Hyprland => {
                let selector = format!("window = \"title:^({title})$\"");
                hypr_dispatch(&format!(
                    "hl.dsp.window.move({{ x = {x}, y = {y}, relative = false, {selector} }})"
                ));
            }
            Desktop::Sway => {
                sway_command(&format!(
                    "[title=\"^{title}$\"] move absolute position {x} {y}"
                ));
            }
            Desktop::Unknown => {}
        }
    }

    /// The focused output's size in logical pixels.
    fn screen_size(self) -> Option<(i32, i32)> {
        match self {
            Desktop::Hyprland => {
                let monitor = crate::display::hyprland_focused_monitor()?;
                // Hyprland reports device pixels; windows are placed
                // in logical ones.
                let scale = (monitor.scale as f64).max(0.0001);
                Some((
                    (monitor.width as f64 / scale).round() as i32,
                    (monitor.height as f64 / scale).round() as i32,
                ))
            }
            Desktop::Sway => {
                let outputs: serde_json::Value =
                    serde_json::from_str(&sway_query("get_outputs")?).ok()?;
                let focused = outputs
                    .as_array()?
                    .iter()
                    .find(|o| o.get("focused").and_then(|f| f.as_bool()) == Some(true))?;
                // Sway's rect is already logical.
                let rect = focused.get("rect")?;
                Some((
                    rect.get("width")?.as_i64()? as i32,
                    rect.get("height")?.as_i64()? as i32,
                ))
            }
            Desktop::Unknown => None,
        }
    }

    /// Where every pin currently sits, in logical pixels, by title —
    /// the title is how a move addresses one pin and not the others.
    fn pin_rects(self) -> Vec<(String, (i32, i32, i32, i32))> {
        match self {
            Desktop::Hyprland => hyprland_pin_rects(),
            Desktop::Sway => sway_pin_rects(),
            Desktop::Unknown => Vec::new(),
        }
    }

    /// Whether this pin's window has reached the compositor yet.
    fn sees_pin(self) -> bool {
        let listing = match self {
            Desktop::Hyprland => {
                let output = std::process::Command::new("hyprctl")
                    .args(["-j", "clients"])
                    .output()
                    .ok();
                output.map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            }
            Desktop::Sway => sway_query("get_tree"),
            Desktop::Unknown => None,
        };
        listing.is_some_and(|text| text.contains(&pin_title()))
    }
}

/// Run one sway command, best-effort.
fn sway_command(command: &str) {
    let _ = std::process::Command::new("swaymsg").arg(command).output();
}

/// Ask sway for one of its JSON trees.
fn sway_query(kind: &str) -> Option<String> {
    let output = std::process::Command::new("swaymsg")
        .args(["-t", kind, "-r"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every pin in sway's tree, with its geometry.
///
/// The tree is nested, so this walks it: a floating pin hangs off a
/// workspace's floating list rather than sitting beside the tiled
/// windows.
fn sway_pin_rects() -> Vec<(String, (i32, i32, i32, i32))> {
    let Some(text) = sway_query("get_tree") else {
        return Vec::new();
    };
    let Ok(tree) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut pending = vec![tree];
    while let Some(node) = pending.pop() {
        if let Some(name) = node
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|name| name.starts_with(PIN_TITLE))
            && let Some(rect) = node.get("rect")
        {
            let field = |key: &str| rect.get(key)?.as_i64().map(|v| v as i32);
            if let (Some(x), Some(y), Some(w), Some(h)) =
                (field("x"), field("y"), field("width"), field("height"))
            {
                found.push((name.to_owned(), (x, y, w, h)));
            }
        }
        for key in ["nodes", "floating_nodes"] {
            if let Some(children) = node.get(key).and_then(|c| c.as_array()) {
                pending.extend(children.iter().cloned());
            }
        }
    }
    found
}

/// Run one Hyprland dispatcher, best-effort.
///
/// Dispatchers rather than window rules: rules have to exist before a
/// window maps and live in the user's config, and a pin should need
/// neither.
///
/// Written in the Lua object API, because a Lua-configured Hyprland
/// evaluates the dispatch argument as Lua — classic
/// `movewindowpixel exact X Y,title:...` parses as an expression and
/// fails, which is how a pin ended up in the middle of the screen
/// unpinned while every call reported success. The whole expression is
/// one argument: the window title has a space in it.
fn hypr_dispatch(expression: &str) {
    let _ = std::process::Command::new("hyprctl")
        .arg("dispatch")
        .arg(expression)
        .output();
}

/// How far apart stacked pins sit, centre to centre.
fn pin_step() -> i32 {
    pin_size().1 + PIN_GAP
}

/// The slot a pin at `rect` is sitting in, or `None` if it isn't in
/// one — because it has been dragged somewhere else.
///
/// A moved pin gives its slot back. Counting pins instead would keep
/// stacking above one that is no longer there, leaving a hole at the
/// bottom of the column and pushing new pins off the top of the
/// screen.
fn slot_of(rect: (i32, i32, i32, i32), screen: (i32, i32)) -> Option<i32> {
    /// A pin within a few pixels of a slot is in it: margins round to
    /// integers and compositors report what they rounded to.
    const TOLERANCE: i32 = 4;
    let (frame_w, frame_h) = pin_size();
    let (x, y, _, _) = rect;
    if (x - (screen.0 - EDGE_MARGIN - frame_w)).abs() > TOLERANCE {
        return None;
    }
    let base = screen.1 - EDGE_MARGIN - frame_h;
    let step = pin_step();
    let index = ((base - y) as f64 / step as f64).round() as i32;
    let expected = base - index * step;
    (index >= 0 && (y - expected).abs() <= TOLERANCE).then_some(index)
}

/// The lowest slot nothing is sitting in.
fn first_free_slot(occupied: &[i32]) -> i32 {
    (0..).find(|slot| !occupied.contains(slot)).unwrap_or(0)
}

/// Which slot a new pin should take.
///
/// Asks the compositor where the existing pins are, rather than
/// keeping a count: every capture is a separate process, a file of
/// slots would go stale the first time one crashed, and a count can't
/// tell a pin that has been dragged away from one that hasn't. A
/// compositor that can't be asked answers zero, and the new pin lands
/// in the corner like the first one.
fn next_slot(desktop: Desktop) -> i32 {
    let Some(screen) = desktop.screen_size() else {
        return 0;
    };
    let occupied: Vec<i32> = desktop
        .pin_rects()
        .into_iter()
        .filter_map(|(_, rect)| slot_of(rect, screen))
        .collect();
    first_free_slot(&occupied)
}

/// Top-left corner of `slot` in the pin column, on a screen of this
/// size. `slot_of` is its inverse: a pin moved here is recognised as
/// occupying the slot.
fn slot_origin(screen: (i32, i32), slot: i32) -> (i32, i32) {
    let (frame_w, frame_h) = pin_size();
    (
        screen.0 - EDGE_MARGIN - frame_w,
        screen.1 - EDGE_MARGIN - frame_h - slot * pin_step(),
    )
}

/// The moves that close the gaps in the pin column: every pin still in
/// a slot drops to the lowest slots, keeping its order. Pins already
/// where they belong are skipped — each move is an IPC round trip.
fn compaction_moves(mut column: Vec<(String, i32)>) -> Vec<(String, i32)> {
    column.sort_by_key(|entry| entry.1);
    column
        .into_iter()
        .enumerate()
        .filter(|(index, (_, slot))| *slot != *index as i32)
        .map(|(index, (title, _))| (title, index as i32))
        .collect()
}

/// A pin has closed: re-stack the survivors into the lowest slots so
/// the column has no holes, in the order they already had.
///
/// `own_title` is filtered out rather than trusted to be gone: the
/// compositor may still list the window whose destruction this call is
/// reacting to.
fn close_column_gaps(desktop: Desktop, own_title: &str) {
    compact_column(desktop, Some(own_title));
}

/// Re-stack every pin sitting in a slot into the lowest slots, keeping
/// their order. Pins dragged out of the column have no slot and are
/// left where they were put; `excluded_title` names one to leave out
/// even if it still shows in a slot — the pin that is closing.
fn compact_column(desktop: Desktop, excluded_title: Option<&str>) {
    let Some(screen) = desktop.screen_size() else {
        eprintln!("pin: cannot re-stack, screen size unknown");
        return;
    };
    let column: Vec<(String, i32)> = desktop
        .pin_rects()
        .into_iter()
        .filter(|(title, _)| Some(title.as_str()) != excluded_title)
        .filter_map(|(title, rect)| slot_of(rect, screen).map(|slot| (title, slot)))
        .collect();
    eprintln!("pin: re-stacking, {} pin(s) in the column", column.len());
    for (title, slot) in compaction_moves(column) {
        let (x, y) = slot_origin(screen, slot);
        eprintln!("pin: dropping '{title}' into slot {slot}");
        desktop.move_pin(&title, x, y);
    }
}

/// Every pin Hyprland knows about, with its geometry.
fn hyprland_pin_rects() -> Vec<(String, (i32, i32, i32, i32))> {
    let Ok(output) = std::process::Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
    else {
        return Vec::new();
    };
    let Ok(text) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    let Ok(clients) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    clients
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|client| {
            let title = client
                .get("title")
                .and_then(|t| t.as_str())
                .filter(|title| title.starts_with(PIN_TITLE))?;
            let pair = |key: &str| {
                let v = client.get(key)?.as_array()?;
                Some((v.first()?.as_i64()? as i32, v.get(1)?.as_i64()? as i32))
            };
            let (x, y) = pair("at")?;
            let (w, h) = pair("size")?;
            Some((title.to_owned(), (x, y, w, h)))
        })
        .collect()
}

/// A square thumbnail of `image` that fills `side` and centre-crops,
/// the way CSS `object-fit: cover` does.
///
/// Fitting inside the square would letterbox a wide capture down to a
/// sliver, and a pin you can't recognise is one you have to open to
/// identify.
fn cover_thumbnail(image: &Pixbuf, frame: (i32, i32)) -> Pixbuf {
    let (frame_w, frame_h) = (frame.0.max(1), frame.1.max(1));
    let (w, h) = (image.width().max(1), image.height().max(1));
    let scale = (frame_w as f64 / w as f64).max(frame_h as f64 / h as f64);
    let scaled_w = ((w as f64 * scale).round() as i32).max(frame_w);
    let scaled_h = ((h as f64 * scale).round() as i32).max(frame_h);
    let Some(scaled) = image.scale_simple(scaled_w, scaled_h, InterpType::Bilinear) else {
        return image.clone();
    };
    // Centred horizontally, but anchored to the top rather than the
    // middle: a capture's top is its title bar, its tab strip, its
    // heading — what tells you which shot this is. A tall page cropped
    // to its middle is a slab of body text that could be any of them.
    //
    // A capture shaped like the screen crops nothing either way, which
    // is the point of the frame's shape.
    let x = ((scaled_w - frame_w) / 2).max(0);
    let y = 0;
    scaled.new_subpixbuf(x, y, frame_w.min(scaled_w), frame_h.min(scaled_h))
}

/// The hover toolbar: edit, copy, copy path, close.
fn build_controls(window: &gtk::Window, actions: PinActions) -> gtk::Box {
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    controls.add_css_class("pin-controls");
    controls.set_halign(gtk::Align::End);
    controls.set_valign(gtk::Align::Start);
    controls.set_margin_top(8);
    controls.set_margin_end(8);

    let button = |icon: &str, tooltip: &str| {
        let b = gtk::Button::from_icon_name(icon);
        b.add_css_class("flat");
        b.set_focusable(false);
        b.set_focus_on_click(false);
        // The app's own tooltips, not GTK's: stock ones strand
        // themselves open when a `leave` goes missing, which on a
        // hover-revealed toolbar is every time it hides.
        b.install_tooltip(tooltip);
        b
    };

    let PinActions {
        on_edit,
        on_copy,
        on_copy_path,
        path_known,
        saved_path: _,
    } = actions;

    // Moving the pin lives here rather than on the picture. A layer
    // surface moves by its margins, so a window dragged by its whole
    // face trails the pointer instead of following it — and the
    // picture has better things to answer for: a click opens the shot,
    // a drag carries it into another app.
    let move_handle = button("re-order-dots-horizontal-regular", MOVE_TOOLTIP);
    move_handle.add_css_class("pin-drag-handle");
    install_move(window, &move_handle);
    controls.append(&move_handle);

    let edit = button("pen-regular", "Edit again");
    edit.connect_clicked(move |_| on_edit());
    controls.append(&edit);

    let copy = button("copy-regular", "Copy image to clipboard");
    copy.connect_clicked(move |_| on_copy());
    controls.append(&copy);

    let copy_path = button(
        "link-regular",
        if path_known {
            "Copy file path"
        } else {
            "Save the shot, then copy its path"
        },
    );
    copy_path.connect_clicked(move |_| on_copy_path());
    controls.append(&copy_path);

    let close = button("dismiss-regular", "Close");
    let window_for_close = window.clone();
    close.connect_clicked(move |_| window_for_close.close());
    controls.append(&close);

    controls
}

/// Carry the shot out of the pin and into another application.
///
/// Two payloads where possible: the file, which is what a file manager
/// or an upload field wants, and the image itself, which is what a
/// chat window or an editor takes. An unsaved pin has no file to
/// offer, so it drags the image alone rather than a path to nothing.
fn install_drag_out(handle: &gtk::Picture, image: &Pixbuf, saved_path: Option<String>) {
    let texture = gtk::gdk::Texture::for_pixbuf(image);
    let source = gtk::DragSource::new();
    source.set_actions(gtk::gdk::DragAction::COPY);

    let provider = match saved_path {
        Some(path) => {
            let file = gtk::gio::File::for_path(&path);
            gtk::gdk::ContentProvider::new_union(&[
                gtk::gdk::ContentProvider::for_value(&file.to_value()),
                gtk::gdk::ContentProvider::for_value(&texture.to_value()),
                gtk::gdk::ContentProvider::for_value(&path.to_value()),
            ])
        }
        None => gtk::gdk::ContentProvider::for_value(&texture.to_value()),
    };
    source.set_content(Some(&provider));

    // A thumbnail, held under the pointer's middle so the drop lands
    // where the picture is.
    if let Some((icon, icon_w, icon_h)) = drag_icon(image) {
        source.connect_drag_begin(move |source, _| {
            source.set_icon(Some(&icon), icon_w / 2, icon_h / 2);
        });
    }
    // Same problem on a drag-out: the pointer is over a picture that
    // is now carrying something, and the tooltip follows it.
    source.connect_drag_begin(|_, _| crate::ui::toolbars::dismiss_active_tooltip());
    handle.add_controller(source);
}

/// A thumbnail of `image` for the pointer to carry, with its size.
/// `None` if the scale fails, in which case the drag runs without an
/// icon — better a plain pointer than the full capture.
fn drag_icon(image: &Pixbuf) -> Option<(gtk::gdk::Texture, i32, i32)> {
    let (w, h) = fit_within(image.width(), image.height(), DRAG_ICON_MAX);
    let scaled = image.scale_simple(w, h, relm4::gtk::gdk_pixbuf::InterpType::Bilinear)?;
    Some((gtk::gdk::Texture::for_pixbuf(&scaled), w, h))
}

/// Scale `(width, height)` down so its longest edge is at most `max`,
/// keeping the aspect. Never scales up.
fn fit_within(width: i32, height: i32, max: i32) -> (i32, i32) {
    let width = width.max(1);
    let height = height.max(1);
    let longest = width.max(height);
    if longest <= max {
        return (width, height);
    }
    let ratio = max as f64 / longest as f64;
    (
        ((width as f64 * ratio).round() as i32).max(1),
        ((height as f64 * ratio).round() as i32).max(1),
    )
}

/// Drag the pin around by its handle.
///
/// A layer surface is positioned by its anchor margins, not by the
/// compositor, so a drag has to move those margins itself — and both
/// anchors are on the far edges, so a rightward drag *shrinks* the
/// right margin. The margins are held here rather than read back
/// because `LayerShell` exposes no getter for them.
fn install_move(window: &gtk::Window, target: &gtk::Button) {
    let press = gtk::GestureClick::new();
    let window = window.clone();
    press.connect_pressed(move |gesture, _, x, y| {
        // Hand the drag to the compositor. It moves the window during
        // its own frame with the client out of the loop, which is why
        // this feels instant where every attempt to move the window
        // ourselves trailed: a client-positioned surface cannot be
        // anywhere but one round trip behind the pointer.
        //
        // `xdg_toplevel.move` is the standard request every compositor
        // implements, so this much works off Hyprland too.
        crate::ui::toolbars::dismiss_active_tooltip();
        let Some(surface) = window.surface() else {
            return;
        };
        let Ok(toplevel) = surface.downcast::<gtk::gdk::Toplevel>() else {
            return;
        };
        let Some(device) = gesture.device() else {
            return;
        };
        // The compositor wants the grab in surface coordinates; the
        // gesture reports them relative to the handle it is on.
        let (sx, sy) = target_origin(gesture).unwrap_or((0.0, 0.0));
        toplevel.begin_move(
            &device,
            gesture.current_button() as i32,
            sx + x,
            sy + y,
            gesture.current_event_time(),
        );
    });
    target.add_controller(press);
}

/// Where the gesture's widget sits inside the window, so a press on it
/// can be reported in the window's own coordinates.
fn target_origin(gesture: &gtk::GestureClick) -> Option<(f64, f64)> {
    let widget = gesture.widget()?;
    let root = widget.root()?;
    widget.translate_coordinates(&root, 0.0, 0.0)
}

#[cfg(test)]
mod tests {
    use super::{
        DRAG_ICON_MAX, EDGE_MARGIN, compaction_moves, cover_thumbnail, first_free_slot, fit_within,
        pin_size, pin_step, slot_of, slot_origin,
    };
    use relm4::gtk::gdk_pixbuf::{Colorspace, Pixbuf};

    /// The pointer carries a token, not the capture: a 6K shot has to
    /// come down to something a pointer can drag.
    #[test]
    fn a_drag_icon_is_a_thumbnail() {
        let (w, h) = fit_within(6144, 3456, DRAG_ICON_MAX);
        assert_eq!(w.max(h), DRAG_ICON_MAX);
        assert_eq!(h, (DRAG_ICON_MAX as f64 * 3456.0 / 6144.0).round() as i32);
    }

    /// A tall capture is bounded by its height, not its width.
    #[test]
    fn a_tall_capture_fits_by_its_longest_edge() {
        let (w, h) = fit_within(400, 4000, DRAG_ICON_MAX);
        assert_eq!(h, DRAG_ICON_MAX);
        assert!(w < DRAG_ICON_MAX);
    }

    /// Something already small keeps its size rather than being blown
    /// up to fill the icon.
    #[test]
    fn a_small_capture_is_left_alone() {
        assert_eq!(fit_within(80, 40, DRAG_ICON_MAX), (80, 40));
    }

    /// The preview fills its square and crops rather than fitting
    /// inside it: a wide capture letterboxed into a sliver is
    /// unrecognisable, which defeats having a pin at all.
    #[test]
    fn a_wide_capture_fills_the_frame() {
        let frame = (224, 126);
        let wide = Pixbuf::new(Colorspace::Rgb, true, 8, 4000, 400).unwrap();
        let thumb = cover_thumbnail(&wide, frame);
        assert_eq!((thumb.width(), thumb.height()), frame);
    }

    /// A tall stitch is cropped to its top for the same reason.
    #[test]
    fn a_tall_capture_fills_the_frame() {
        let frame = (224, 126);
        let tall = Pixbuf::new(Colorspace::Rgb, true, 8, 600, 9000).unwrap();
        let thumb = cover_thumbnail(&tall, frame);
        assert_eq!((thumb.width(), thumb.height()), frame);
    }

    /// A tall capture keeps its top, where the heading and the tabs
    /// are — the part that says which shot this is. Its middle is
    /// body text that could belong to any of them.
    #[test]
    fn a_tall_capture_keeps_its_top() {
        let frame = (224, 126);
        // A page with a red band across its first rows and black
        // below. Cover-scaling a 448-wide capture into a 224-wide
        // frame halves it, so the frame shows the capture's top 252
        // rows: the band has to be shallower than that to leave any
        // black in the picture at all.
        let tall = Pixbuf::new(Colorspace::Rgb, true, 8, 448, 4000).unwrap();
        tall.fill(0x000000ff);
        tall.new_subpixbuf(0, 0, 448, 100).fill(0xff0000ff);
        let thumb = cover_thumbnail(&tall, frame);
        let pixels = unsafe { thumb.pixels() };
        let stride = thumb.rowstride() as usize;
        let top_row_is_red = pixels[..3] == [0xff, 0x00, 0x00];
        let bottom_row = &pixels[stride * (frame.1 as usize - 1)..];
        assert!(top_row_is_red, "expected the capture's top to survive");
        assert_eq!(&bottom_row[..3], &[0x00, 0x00, 0x00]);
    }

    /// A capture shaped like the screen loses nothing: the frame is
    /// that shape, which is the reason it is.
    #[test]
    fn a_full_screen_capture_is_not_cropped() {
        let frame = (224, 126);
        // Same 16:9 as the frame, at capture resolution.
        let full = Pixbuf::new(Colorspace::Rgb, true, 8, 6144, 3456).unwrap();
        let thumb = cover_thumbnail(&full, frame);
        assert_eq!((thumb.width(), thumb.height()), frame);
        // Cover-scaling a matching aspect needs no crop in either
        // direction: the scale factors agree.
        let sx = frame.0 as f64 / 6144.0;
        let sy = frame.1 as f64 / 3456.0;
        assert!((sx - sy).abs() < 0.001, "{sx} vs {sy}");
    }

    /// Pins stack clear of each other, so a second one doesn't bury
    /// the first — seeing both is the point of stacking them.
    #[test]
    fn stacked_pins_do_not_overlap() {
        assert!(pin_step() > pin_size().1);
    }

    /// A pin sitting in the column is recognised as being in its slot,
    /// so the next one goes above it rather than on it.
    #[test]
    fn a_pin_in_the_column_holds_its_slot() {
        let screen = (3072, 1728);
        let (frame_w, frame_h) = pin_size();
        let side = frame_w;
        let x = screen.0 - EDGE_MARGIN - side;
        let base = screen.1 - EDGE_MARGIN - frame_h;
        assert_eq!(slot_of((x, base, side, side), screen), Some(0));
        assert_eq!(slot_of((x, base - pin_step(), side, side), screen), Some(1));
    }

    /// A pin dragged out of the column gives its slot back: stacking
    /// above where it used to be would leave a hole at the bottom and
    /// walk new pins off the top of the screen.
    #[test]
    fn a_moved_pin_frees_its_slot() {
        let screen = (3072, 1728);
        let (frame_w, frame_h) = pin_size();
        let side = frame_w;
        let x = screen.0 - EDGE_MARGIN - side;
        let base = screen.1 - EDGE_MARGIN - frame_h;
        // Dragged left, and dragged up between two slots.
        assert_eq!(slot_of((x - 300, base, side, side), screen), None);
        assert_eq!(
            slot_of((x, base - pin_step() / 2, side, side), screen),
            None
        );
    }

    /// The next pin takes the lowest gap, not the top of the pile.
    #[test]
    fn the_lowest_free_slot_wins() {
        assert_eq!(first_free_slot(&[]), 0);
        assert_eq!(first_free_slot(&[0, 1]), 2);
        assert_eq!(first_free_slot(&[0, 2]), 1);
        assert_eq!(first_free_slot(&[1, 2]), 0);
    }

    /// Closing a pin drops the survivors down over the hole it left,
    /// in the order they already had.
    #[test]
    fn survivors_drop_to_close_the_gap() {
        let column = vec![
            ("bottom".to_owned(), 0),
            ("middle".to_owned(), 2),
            ("top".to_owned(), 3),
        ];
        assert_eq!(
            compaction_moves(column),
            vec![("middle".to_owned(), 1), ("top".to_owned(), 2)]
        );
    }

    /// A column with no holes stays put — every move is a compositor
    /// round trip, and one to where a pin already sits buys nothing.
    #[test]
    fn a_compact_column_is_left_alone() {
        let column = vec![("upper".to_owned(), 1), ("lower".to_owned(), 0)];
        assert!(compaction_moves(column).is_empty());
    }

    /// End-to-end against the real compositor: three windows wearing
    /// pin titles go into slots, the middle one closes, and the top
    /// one drops down. Run it by hand inside a Hyprland session:
    /// `cargo test a_closed_pins_survivors_restack -- --ignored --nocapture`
    /// (it opens and closes three small windows on the current screen).
    #[test]
    #[ignore = "drives the live compositor"]
    fn a_closed_pins_survivors_restack() {
        use super::{Desktop, close_column_gaps, slot_origin};
        use relm4::gtk::{self, prelude::*};

        let desktop = Desktop::detect();
        assert!(desktop.places_windows(), "needs a Hyprland or Sway session");
        gtk::init().expect("gtk init");
        let pump = |millis: u64| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);
            while std::time::Instant::now() < deadline {
                while gtk::glib::MainContext::default().iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };

        let titles: Vec<String> = (0..3).map(|n| format!("Tensaku Pin 90000{n}")).collect();
        let (w, h) = pin_size();
        let windows: Vec<gtk::Window> = titles
            .iter()
            .map(|title| {
                let window = gtk::Window::builder()
                    .resizable(false)
                    .decorated(false)
                    .title(title)
                    .default_width(w)
                    .default_height(h)
                    .build();
                window.present();
                window
            })
            .collect();
        pump(400);
        for (slot, title) in titles.iter().enumerate() {
            desktop.arrange(title, slot as i32);
        }
        pump(400);

        let screen = desktop.screen_size().expect("screen size");
        let slots_by_title = |wanted: &[&String]| -> Vec<Option<i32>> {
            let rects = desktop.pin_rects();
            wanted
                .iter()
                .map(|title| {
                    rects
                        .iter()
                        .find(|(t, _)| t == *title)
                        .and_then(|(_, rect)| slot_of(*rect, screen))
                })
                .collect()
        };
        assert_eq!(
            slots_by_title(&titles.iter().collect::<Vec<_>>()),
            vec![Some(0), Some(1), Some(2)],
            "placement into slots failed — rects: {:?}, expected origins: {:?}",
            desktop.pin_rects(),
            (0..3).map(|s| slot_origin(screen, s)).collect::<Vec<_>>()
        );

        // The middle pin closes the way a real one does: close-request
        // runs the re-stack in the closing pin's own process. (Not
        // destroy — closing only hides the window, and destroy waits
        // for the process to wind down.)
        {
            let own_title = titles[1].clone();
            windows[1].connect_close_request(move |_| {
                close_column_gaps(desktop, &own_title);
                gtk::glib::Propagation::Proceed
            });
        }
        windows[1].close();
        pump(600);

        let after = slots_by_title(&[&titles[0], &titles[2]]);
        for window in &windows {
            window.destroy();
        }
        pump(100);
        assert_eq!(
            after,
            vec![Some(0), Some(1)],
            "survivors did not close the gap"
        );
    }

    /// `slot_origin` and `slot_of` agree, so a pin moved down a slot is
    /// recognised as occupying it by the next capture.
    #[test]
    fn a_restacked_pin_lands_in_its_slot() {
        let screen = (3072, 1728);
        let (w, h) = pin_size();
        for slot in 0..3 {
            let (x, y) = slot_origin(screen, slot);
            assert_eq!(slot_of((x, y, w, h), screen), Some(slot));
        }
    }
}
