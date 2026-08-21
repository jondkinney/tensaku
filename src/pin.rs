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

use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::gtk::{self, gdk_pixbuf::Pixbuf, prelude::*};
use std::cell::Cell;
use std::rc::Rc;

/// Width of a pinned capture, in CSS pixels. Height follows the
/// image's aspect. Big enough to read a line of text in a terminal
/// shot, small enough to leave the desktop usable.
const PIN_WIDTH: i32 = 280;

/// Gap between the pin and the screen edge it starts anchored to.
const EDGE_MARGIN: i32 = 24;

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
}

/// Build and show the pin window for `image`.
pub fn open(image: &Pixbuf, actions: PinActions) -> gtk::Window {
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
    window.set_margin(Edge::Right, EDGE_MARGIN);
    window.set_margin(Edge::Bottom, EDGE_MARGIN);
    // OnDemand rather than Exclusive: the pin should never swallow the
    // keystrokes of whatever the user is actually working in, but Esc
    // has to reach it once it is clicked.
    window.set_keyboard_mode(KeyboardMode::OnDemand);

    let (width, height) = scaled_size(image.width(), image.height());
    window.set_default_size(width, height);

    let picture = gtk::Picture::for_pixbuf(image);
    picture.set_can_shrink(true);
    // `keep_aspect_ratio` in this GTK build; a long stitch is capped
    // in `scaled_size`, so the picture letterboxes rather than
    // stretching when the cap bites.
    picture.set_keep_aspect_ratio(true);
    picture.set_size_request(width, height);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));

    let controls = build_controls(&window, actions);
    overlay.add_overlay(&controls);

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

    install_drag(&window, &overlay);

    window.set_child(Some(&overlay));
    window.present();
    window
}

/// Fit `PIN_WIDTH` while keeping the capture's aspect, and never let a
/// very tall shot (a scroll capture) grow past a screenful.
fn scaled_size(image_width: i32, image_height: i32) -> (i32, i32) {
    let width = PIN_WIDTH.min(image_width.max(1));
    let height = (width as f64 * image_height.max(1) as f64 / image_width.max(1) as f64)
        .round()
        .max(1.0) as i32;
    // A long stitch pinned at its full aspect would run off the screen;
    // cap it and let the picture letterbox instead.
    let capped = height.min(PIN_WIDTH * 3);
    (width, capped)
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
        b.set_tooltip_text(Some(tooltip));
        b
    };

    let PinActions {
        on_edit,
        on_copy,
        on_copy_path,
        path_known,
    } = actions;

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
            "Save and copy file path"
        },
    );
    copy_path.connect_clicked(move |btn| {
        on_copy_path();
        btn.set_tooltip_text(Some("Path copied"));
    });
    controls.append(&copy_path);

    let close = button("dismiss-regular", "Close");
    let window_for_close = window.clone();
    close.connect_clicked(move |_| window_for_close.close());
    controls.append(&close);

    controls
}

/// Drag the pin around by its image.
///
/// A layer surface is positioned by its anchor margins, not by the
/// compositor, so a drag has to move those margins itself — and both
/// anchors are on the far edges, so a rightward drag *shrinks* the
/// right margin. The margins are held here rather than read back
/// because `LayerShell` exposes no getter for them.
fn install_drag(window: &gtk::Window, target: &gtk::Overlay) {
    let right = Rc::new(Cell::new(EDGE_MARGIN));
    let bottom = Rc::new(Cell::new(EDGE_MARGIN));
    let start = Rc::new(Cell::new((EDGE_MARGIN, EDGE_MARGIN)));

    let drag = gtk::GestureDrag::new();
    {
        let right = right.clone();
        let bottom = bottom.clone();
        let start = start.clone();
        drag.connect_drag_begin(move |_, _, _| start.set((right.get(), bottom.get())));
    }
    {
        let window = window.clone();
        let right = right.clone();
        let bottom = bottom.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let (start_right, start_bottom) = start.get();
            // Clamped at zero so a drag can't push the pin off the
            // screen edge it is anchored to and out of reach.
            let new_right = (start_right - dx.round() as i32).max(0);
            let new_bottom = (start_bottom - dy.round() as i32).max(0);
            right.set(new_right);
            bottom.set(new_bottom);
            window.set_margin(Edge::Right, new_right);
            window.set_margin(Edge::Bottom, new_bottom);
        });
    }
    target.add_controller(drag);
}

#[cfg(test)]
mod tests {
    use super::{PIN_WIDTH, scaled_size};

    #[test]
    fn a_pin_keeps_the_capture_aspect() {
        let (w, h) = scaled_size(1600, 900);
        assert_eq!(w, PIN_WIDTH);
        assert_eq!(h, (PIN_WIDTH as f64 * 900.0 / 1600.0).round() as i32);
    }

    /// A shot narrower than the pin shouldn't be blown up to fill it.
    #[test]
    fn a_small_capture_stays_its_own_size() {
        let (w, h) = scaled_size(120, 60);
        assert_eq!((w, h), (120, 60));
    }

    /// A long scroll capture is capped rather than running off screen.
    #[test]
    fn a_long_stitch_is_capped() {
        let (_, h) = scaled_size(1000, 40_000);
        assert_eq!(h, PIN_WIDTH * 3);
    }
}
