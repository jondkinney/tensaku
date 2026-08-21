//! Pin the finished shot to the desktop.
//!
//! A pinned capture is a small always-on-top window in a corner of the
//! screen, showing what the editor produced. It exists so a reference
//! can stay visible while you work in another window — the thing a
//! screenshot is usually *for* — instead of living in a file you have
//! to keep re-opening.
//!
//! Two decisions worth knowing:
//!
//! - **It is a layer surface**, like the scroll-capture overlay, which
//!   is what keeps it above ordinary windows without asking the
//!   compositor for a rule. A layer surface has no titlebar and the
//!   compositor won't move it, so dragging is implemented here by
//!   walking the anchor margins.
//! - **Edit keeps the annotations live.** The pin and the editor are
//!   the same process, so Edit hides the pin and shows the editor
//!   window again with every drawable still where it was, still
//!   movable. Nothing is serialised and nothing is flattened; the
//!   cost is that the pin lasts as long as the process does.

use crate::ui::toolbars::RobustTooltipExt;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::gtk::gdk_pixbuf::InterpType;
use relm4::gtk::{self, gdk_pixbuf::Pixbuf, prelude::*};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::rc::Rc;
use std::time::Duration;

/// Side of a pinned capture, in CSS pixels.
///
/// Square whatever the shot's shape: pins stack, and a column of
/// mismatched heights is one that has to be laid out rather than
/// counted. The image fills the square and centre-crops, so a wide
/// capture shows its middle instead of becoming a letterboxed sliver
/// too small to recognise.
const PIN_SIDE: i32 = 168;

/// Frame around the preview: the pin needs an edge of its own or it
/// reads as a picture lying loose on the desktop rather than a thing
/// sitting on it.
const PIN_PADDING: i32 = 6;

/// The move handle's tooltip, named because the drag hides it and has
/// to put it back.
const MOVE_TOOLTIP: &str = "Move this pin";

/// The preview's tooltip, hidden while a drag-out is in flight for the
/// same reason.
const PICTURE_TOOLTIP: &str = "Click to open in the editor · Drag into another app to paste it";

/// How often the follower thread reads the pointer.
///
/// Far faster than a frame, because it costs nothing on the UI thread:
/// whatever the compositor answers, the freshest reading is already
/// waiting when the next frame asks for it.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// How far ahead of the pointer to place a dragged pin. Measured: the
/// pin lands about 12ms behind the pointer, and that is a frame of
/// compositor work rather than anything this side can skip.
const LEAD: Duration = Duration::from_millis(12);

/// Most a lead may add, in pixels. Past a flick this fast the guess is
/// worth less than the overshoot it causes.
const LEAD_CAP: i32 = 48;

/// How much of each reading folds into the smoothed speed. Low enough
/// that a jittery hand doesn't throw the lead around, high enough that
/// a change of direction is followed within a frame or two.
const VELOCITY_BLEND: f32 = 0.35;

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
        .build();
    window.add_css_class("pin-window");

    window.init_layer_shell();
    // Overlay, not Top: a pinned reference is meant to stay visible
    // over whatever you switch to, which is the entire point of
    // pinning it rather than leaving the editor open.
    window.set_layer(Layer::Overlay);
    window.set_anchor(Edge::Right, true);
    window.set_anchor(Edge::Bottom, true);
    window.set_namespace(Some(PIN_NAMESPACE));
    // Stack above whatever is already pinned instead of landing on
    // top of it. Each capture is its own process, so the count comes
    // from the compositor rather than from a shared list.
    // Every pin is the same square, so a slot is just an index —
    // and the lowest free one, so a pin dragged out of the column
    // leaves its place for the next.
    let bottom = EDGE_MARGIN + next_slot() * pin_step();
    window.set_margin(Edge::Right, EDGE_MARGIN);
    window.set_margin(Edge::Bottom, bottom);
    // OnDemand rather than Exclusive: the pin should never swallow the
    // keystrokes of whatever the user is actually working in, but Esc
    // has to reach it once it is clicked.
    window.set_keyboard_mode(KeyboardMode::OnDemand);

    window.set_default_size(PIN_SIDE + PIN_PADDING * 2, PIN_SIDE + PIN_PADDING * 2);

    // Scale the pixels here rather than asking the widget to shrink
    // them. A `Picture`'s natural size is its image's, so a 6K capture
    // asked for a 6K surface and the compositor clamped it — which is
    // why a pin showed a corner at full size instead of the shot.
    let preview = cover_thumbnail(image, PIN_SIDE);
    let picture = gtk::Picture::for_pixbuf(&preview);
    picture.set_can_shrink(true);
    picture.set_size_request(PIN_SIDE, PIN_SIDE);

    let frame = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    frame.add_css_class("pin-frame");
    frame.append(&picture);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&frame));

    // The picture and the Edit button do the same thing, so they share
    // one callback rather than each getting a copy of the intent.
    let saved_path_for_drag = actions.saved_path.clone();
    let edit_for_picture = std::rc::Rc::new(actions.on_edit);
    let edit_for_controls = edit_for_picture.clone();
    let actions = PinActions {
        on_edit: Box::new(move || edit_for_controls()),
        ..actions
    };
    let controls = build_controls(&window, bottom, actions);
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

/// Layer-surface namespace, so pins can count each other.
const PIN_NAMESPACE: &str = "tensaku-pin";

/// How far apart stacked pins sit, centre to centre.
fn pin_step() -> i32 {
    PIN_SIDE + PIN_PADDING * 2 + PIN_GAP
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
    let side = PIN_SIDE + PIN_PADDING * 2;
    let (x, y, _, _) = rect;
    if (x - (screen.0 - EDGE_MARGIN - side)).abs() > TOLERANCE {
        return None;
    }
    let base = screen.1 - EDGE_MARGIN - side;
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
/// Asks the compositor where the existing pins actually are, rather
/// than keeping a count: every capture is a separate process, a file
/// of slots would go stale the first time one crashed, and a count
/// can't tell a pin that has been dragged away from one that hasn't.
/// A compositor that can't be asked answers zero, and the new pin
/// lands in the corner like the first one.
fn next_slot() -> i32 {
    let Ok(output) = std::process::Command::new("hyprctl")
        .args(["-j", "layers"])
        .output()
    else {
        return 0;
    };
    let Ok(text) = String::from_utf8(output.stdout) else {
        return 0;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return 0;
    };
    let Some(monitor) = crate::display::hyprland_focused_monitor() else {
        return 0;
    };
    // The monitor reports device pixels; layers are placed in logical
    // ones, which is the space the margins are in too.
    let scale = (monitor.scale as f64).max(0.0001);
    let screen = (
        (monitor.width as f64 / scale).round() as i32,
        (monitor.height as f64 / scale).round() as i32,
    );

    let occupied: Vec<i32> = json
        .as_object()
        .into_iter()
        .flat_map(|monitors| monitors.values())
        .filter_map(|monitor| monitor.get("levels")?.as_object())
        .flat_map(|levels| levels.values())
        .filter_map(|layers| layers.as_array())
        .flatten()
        .filter(|layer| layer.get("namespace").and_then(|n| n.as_str()) == Some(PIN_NAMESPACE))
        .filter_map(|layer| {
            let field = |key: &str| layer.get(key)?.as_i64().map(|v| v as i32);
            Some((field("x")?, field("y")?, field("w")?, field("h")?))
        })
        .filter_map(|rect| slot_of(rect, screen))
        .collect();
    first_free_slot(&occupied)
}

/// A square thumbnail of `image` that fills `side` and centre-crops,
/// the way CSS `object-fit: cover` does.
///
/// Fitting inside the square would letterbox a wide capture down to a
/// sliver, and a pin you can't recognise is one you have to open to
/// identify.
fn cover_thumbnail(image: &Pixbuf, side: i32) -> Pixbuf {
    let (w, h) = (image.width().max(1), image.height().max(1));
    let scale = (side as f64 / w as f64).max(side as f64 / h as f64);
    let scaled_w = ((w as f64 * scale).round() as i32).max(side);
    let scaled_h = ((h as f64 * scale).round() as i32).max(side);
    let Some(scaled) = image.scale_simple(scaled_w, scaled_h, InterpType::Bilinear) else {
        return image.clone();
    };
    // Take the middle: a capture's edges are where its chrome is, and
    // its middle is what it was taken of.
    let x = ((scaled_w - side) / 2).max(0);
    let y = ((scaled_h - side) / 2).max(0);
    scaled.new_subpixbuf(x, y, side.min(scaled_w), side.min(scaled_h))
}

/// The hover toolbar: edit, copy, copy path, close.
fn build_controls(window: &gtk::Window, bottom_margin: i32, actions: PinActions) -> gtk::Box {
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
    install_move(window, &move_handle, bottom_margin);
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
fn install_move(window: &gtk::Window, target: &gtk::Button, bottom_margin: i32) {
    let right = Rc::new(Cell::new(EDGE_MARGIN));
    let bottom = Rc::new(Cell::new(bottom_margin));

    let press = gtk::GestureClick::new();
    {
        let window = window.clone();
        let right = right.clone();
        let bottom = bottom.clone();
        press.connect_pressed(move |_, _, _, _| {
            let Some(origin) = crate::hypr::cursor_position() else {
                return;
            };
            let start = (right.get(), bottom.get());

            // The pointer is read on a thread of its own. Asking for
            // it inline stalled the UI thread for five milliseconds at
            // the ninetieth percentile — the compositor answers slowly
            // exactly when it is busy, which is precisely when we have
            // just asked it to move a window. Off-thread, the answer
            // is always waiting and never in the way.
            let cursor = Arc::new(CursorFollow::new(origin));
            {
                let cursor = Arc::clone(&cursor);
                std::thread::spawn(move || {
                    let mut last = std::time::Instant::now();
                    while cursor.following.load(Ordering::Relaxed) {
                        if let Some((x, y)) = crate::hypr::cursor_position() {
                            let now = std::time::Instant::now();
                            cursor.sample(x, y, now.duration_since(last).as_secs_f32());
                            last = now;
                        }
                        std::thread::sleep(POLL_INTERVAL);
                    }
                });
            }

            let right = right.clone();
            let bottom = bottom.clone();
            let cursor_for_tick = Arc::clone(&cursor);
            // On the frame clock rather than a timer: a margin can
            // only take effect on a frame, so setting it more often
            // than that is work the compositor throws away, and
            // setting it on any other cadence means landing a step
            // late.
            window.add_tick_callback(move |window, _| {
                if !cursor_for_tick.following.load(Ordering::Relaxed) {
                    crate::ui::toolbars::dismiss_active_tooltip();
                    return gtk::glib::ControlFlow::Break;
                }
                // Every frame: the pointer never leaves the handle
                // during a drag, so nothing else takes the tooltip
                // down.
                crate::ui::toolbars::dismiss_active_tooltip();

                let (x, y) = cursor_for_tick.predicted();
                // Anchored bottom-right, so moving right shrinks the
                // right margin. Clamped so a pin can't be pushed off
                // the edge it hangs from and out of reach.
                let new_right = (start.0 - (x - origin.0)).max(0);
                let new_bottom = (start.1 - (y - origin.1)).max(0);
                if (new_right, new_bottom) != (right.get(), bottom.get()) {
                    right.set(new_right);
                    bottom.set(new_bottom);
                    window.set_margin(Edge::Right, new_right);
                    window.set_margin(Edge::Bottom, new_bottom);
                }
                gtk::glib::ControlFlow::Continue
            });
            // The gesture's own end, and a pointer that leaves without
            // one — a compositor grab ending, the window outrunning the
            // pointer — both have to stop the follow, or the pin
            // chases the cursor forever.
            let stop = Arc::clone(&cursor);
            RELEASE.with(|slot| *slot.borrow_mut() = Some(stop));
        });
    }
    {
        press.connect_released(|_, _, _, _| stop_following());
    }
    {
        let cancel = gtk::EventControllerMotion::new();
        cancel.connect_leave(|_| stop_following());
        target.add_controller(cancel);
    }
    target.add_controller(press);
}

/// The pointer position a drag is following, shared with the thread
/// that reads it.
struct CursorFollow {
    x: AtomicI32,
    y: AtomicI32,
    /// Pointer speed in pixels per second, smoothed. Used to place the
    /// pin where the pointer will be when the frame lands rather than
    /// where it was when the frame was built.
    vx: AtomicI32,
    vy: AtomicI32,
    following: AtomicBool,
}

impl CursorFollow {
    fn new(origin: (i32, i32)) -> Self {
        Self {
            x: AtomicI32::new(origin.0),
            y: AtomicI32::new(origin.1),
            vx: AtomicI32::new(0),
            vy: AtomicI32::new(0),
            following: AtomicBool::new(true),
        }
    }

    /// Where the pointer will be `LEAD` from now, at its current
    /// speed.
    ///
    /// Measured end to end, a pin lands about 12ms after the pointer
    /// it is following: the compositor has to accept a new margin,
    /// lay the surface out and present it, and that is a frame's work
    /// however the margin got there. Leading by that much cancels it,
    /// which is what turns "close behind" into "attached".
    ///
    /// Capped, because a lead is a guess: at a direction change the
    /// guess is wrong, and a small wrong guess reads as softness while
    /// a large one reads as the pin overshooting and snapping back.
    fn predicted(&self) -> (i32, i32) {
        let lead = LEAD.as_secs_f32();
        let step = |v: &AtomicI32| {
            ((v.load(Ordering::Relaxed) as f32 * lead).round() as i32).clamp(-LEAD_CAP, LEAD_CAP)
        };
        (
            self.x.load(Ordering::Relaxed) + step(&self.vx),
            self.y.load(Ordering::Relaxed) + step(&self.vy),
        )
    }

    /// Record a reading and fold it into the smoothed speed.
    fn sample(&self, x: i32, y: i32, dt: f32) {
        if dt > 0.0 {
            let blend = |old: &AtomicI32, delta: i32| {
                let instant = delta as f32 / dt;
                let previous = old.load(Ordering::Relaxed) as f32;
                // A plain difference of two readings is mostly noise
                // at this rate; the average of the recent past is what
                // a hand is actually doing.
                old.store(
                    (previous * (1.0 - VELOCITY_BLEND) + instant * VELOCITY_BLEND).round() as i32,
                    Ordering::Relaxed,
                );
            };
            blend(&self.vx, x - self.x.load(Ordering::Relaxed));
            blend(&self.vy, y - self.y.load(Ordering::Relaxed));
        }
        self.x.store(x, Ordering::Relaxed);
        self.y.store(y, Ordering::Relaxed);
    }
}

thread_local! {
    /// The drag in progress, so a release or a stray leave can end it.
    /// One pin per process, and one drag at a time within it.
    static RELEASE: RefCell<Option<Arc<CursorFollow>>> = const { RefCell::new(None) };
}

fn stop_following() {
    RELEASE.with(|slot| {
        if let Some(cursor) = slot.borrow_mut().take() {
            cursor.following.store(false, Ordering::Relaxed);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        DRAG_ICON_MAX, EDGE_MARGIN, PIN_PADDING, PIN_SIDE, cover_thumbnail, first_free_slot,
        fit_within, pin_step, slot_of,
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
    fn a_wide_capture_fills_the_square() {
        let wide = Pixbuf::new(Colorspace::Rgb, true, 8, 1600, 400).unwrap();
        let thumb = cover_thumbnail(&wide, PIN_SIDE);
        assert_eq!((thumb.width(), thumb.height()), (PIN_SIDE, PIN_SIDE));
    }

    /// A tall stitch is cropped to its middle for the same reason.
    #[test]
    fn a_tall_capture_fills_the_square() {
        let tall = Pixbuf::new(Colorspace::Rgb, true, 8, 600, 9000).unwrap();
        let thumb = cover_thumbnail(&tall, PIN_SIDE);
        assert_eq!((thumb.width(), thumb.height()), (PIN_SIDE, PIN_SIDE));
    }

    /// Pins stack clear of each other, so a second one doesn't bury
    /// the first — seeing both is the point of stacking them.
    #[test]
    fn stacked_pins_do_not_overlap() {
        assert!(pin_step() > PIN_SIDE + PIN_PADDING * 2);
    }

    /// A pin sitting in the column is recognised as being in its slot,
    /// so the next one goes above it rather than on it.
    #[test]
    fn a_pin_in_the_column_holds_its_slot() {
        let screen = (3072, 1728);
        let side = PIN_SIDE + PIN_PADDING * 2;
        let x = screen.0 - EDGE_MARGIN - side;
        let base = screen.1 - EDGE_MARGIN - side;
        assert_eq!(slot_of((x, base, side, side), screen), Some(0));
        assert_eq!(slot_of((x, base - pin_step(), side, side), screen), Some(1));
    }

    /// A pin dragged out of the column gives its slot back: stacking
    /// above where it used to be would leave a hole at the bottom and
    /// walk new pins off the top of the screen.
    #[test]
    fn a_moved_pin_frees_its_slot() {
        let screen = (3072, 1728);
        let side = PIN_SIDE + PIN_PADDING * 2;
        let x = screen.0 - EDGE_MARGIN - side;
        let base = screen.1 - EDGE_MARGIN - side;
        // Dragged left, and dragged up between two slots.
        assert_eq!(slot_of((x - 300, base, side, side), screen), None);
        assert_eq!(slot_of((x, base - pin_step() / 2, side, side), screen), None);
    }

    /// The next pin takes the lowest gap, not the top of the pile.
    #[test]
    fn the_lowest_free_slot_wins() {
        assert_eq!(first_free_slot(&[]), 0);
        assert_eq!(first_free_slot(&[0, 1]), 2);
        assert_eq!(first_free_slot(&[0, 2]), 1);
        assert_eq!(first_free_slot(&[1, 2]), 0);
    }
}
