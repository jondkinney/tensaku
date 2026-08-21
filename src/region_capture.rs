//! Choose what to capture.
//!
//! An overlay covering the focused output: drag a region, press Space
//! to snap to a window instead, F for the whole screen, or S to hand
//! the whole thing to scrolling capture before any pixels are taken.
//!
//! This replaces the slurp+grim pair the launcher scripts used to run,
//! and the reason to own it is the modes: a separate selector can't
//! offer "actually, capture this window" or "actually, make this a
//! scrolling capture", because by the time it has answered, the
//! decision is already made.
//!
//! The picture is taken BEFORE the overlay appears, and the overlay
//! selects on that frozen image. Capturing afterwards means racing the
//! compositor to unmap a layer surface, and losing that race bakes
//! this overlay's own hint line into the screenshot. Capturing first
//! makes it impossible: the shot predates the overlay.
//!
//! It also gives the frozen-screen feel every capture tool has —
//! what you selected is what you saw, even if a video kept playing
//! underneath.

use anyhow::{Result, anyhow};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::gtk::gdk_pixbuf::Pixbuf;
use relm4::gtk::{self, gdk, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;

use crate::capture::Rect;
use crate::windows::{WindowTarget, visible_windows, window_at};

/// Ignore drags this small: a click that wobbles by a pixel is a click,
/// and capturing a 3×2 region helps nobody.
const MIN_DRAG: f64 = 8.0;


/// What the user chose.
///
/// Rectangles are in the captured image's own pixels, not the
/// overlay's logical ones — the overlay knows both and the caller
/// would have to go looking for the monitor's scale to convert.
#[derive(Debug, Clone, PartialEq)]
pub enum RegionOutcome {
    /// Crop the capture to this rectangle.
    Region(Rect),
    /// Keep the whole capture.
    Fullscreen,
    /// Start over in scrolling capture.
    Scroll,
    /// Escape, or the overlay closed without a choice.
    Cancelled,
}

/// Which thing the pointer is choosing right now.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Drag out a rectangle.
    Area,
    /// Point at a window and take its frame.
    Window,
}

impl Mode {
    /// The line along the bottom of the overlay. It names the keys for
    /// the mode you're in, and the one key that leaves it — a capture
    /// overlay is modal and full-screen, so an unexplained mode is a
    /// dead end.
    fn hint(self) -> &'static str {
        match self {
            Mode::Area => {
                "Drag to select  ·  Space: window  ·  F: full screen  ·  S: scrolling  ·  Esc"
            }
            Mode::Window => {
                "Click a window  ·  Space: back to area  ·  F: full screen  ·  S: scrolling  ·  Esc"
            }
        }
    }
}

struct State {
    /// The screen as it was when the overlay went up.
    frozen: Pixbuf,
    /// The dim sheet, as four strips around the selection, plus the
    /// outline drawn over it, and the surface holding them.
    ///
    /// Widgets rather than painting: a `DrawingArea` renders through
    /// cairo on the CPU, so every motion event composited a 6K-wide
    /// surface by hand and the rectangle trailed the pointer. Moving
    /// five widgets hands that work to GTK, which composites the
    /// capture as an already-uploaded texture.
    fixed: gtk::Fixed,
    shade: [gtk::Box; 4],
    outline: gtk::Box,
    /// Full-width and full-height guides through the pointer, shown
    /// before a drag starts. They line the pointer up with what is on
    /// screen, which is how you find the edge of a window or a text
    /// column without dragging first to see where you landed.
    crosshair: [gtk::Box; 2],
    /// Where the pointer is, in logical pixels.
    pointer: (f64, f64),
    /// The overlay's size in logical pixels, learned once it maps.
    size: (i32, i32),
    /// Image pixels per logical pixel, learned at draw time — the
    /// overlay is laid out in logical units and the capture is in
    /// device ones, and on a 2x display those differ by exactly the
    /// factor that would otherwise halve every selection.
    image_scale: f64,
    mode: Mode,
    /// Where the current drag started, in overlay coordinates.
    origin: (f64, f64),
    /// The rectangle being dragged, or the last one dragged.
    selection: Option<Rect>,
    dragging: bool,
    windows: Vec<WindowTarget>,
    /// The window under the pointer while in window mode.
    hovered: Option<WindowTarget>,
}

/// Show the overlay over `frozen` and wait for a choice.
pub fn run(frozen: Pixbuf) -> Result<RegionOutcome> {
    let shared: Rc<RefCell<RegionOutcome>> = Rc::new(RefCell::new(RegionOutcome::Cancelled));

    let app = gtk::Application::builder()
        .application_id("dev.tensaku.Tensaku.region-capture")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    {
        let shared = Rc::clone(&shared);
        app.connect_activate(move |app| build_overlay(app, &shared, frozen.clone()));
    }

    let exit_code = app.run_with_args::<&str>(&[]);
    if exit_code != gtk::glib::ExitCode::SUCCESS {
        return Err(anyhow!("region-capture overlay exited with {exit_code:?}"));
    }
    Ok(shared.borrow().clone())
}

fn build_overlay(
    app: &gtk::Application,
    shared: &Rc<RefCell<RegionOutcome>>,
    frozen: Pixbuf,
) {
    // Windows are read once, at the moment the overlay goes up. The
    // screen is frozen behind it from the user's point of view, so a
    // list that shifted underneath would snap to something that is no
    // longer where it was drawn.
    let shade = std::array::from_fn(|_| {
        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        strip.add_css_class("region-capture-shade");
        strip
    });
    let outline = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    outline.add_css_class("region-capture-outline");
    outline.set_visible(false);

    let crosshair: [gtk::Box; 2] = std::array::from_fn(|_| {
        let guide = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        guide.add_css_class("capture-crosshair");
        guide.set_visible(false);
        guide
    });

    let fixed = gtk::Fixed::new();
    let state = Rc::new(RefCell::new(State {
        frozen: frozen.clone(),
        fixed: fixed.clone(),
        shade: shade.clone(),
        outline: outline.clone(),
        crosshair: crosshair.clone(),
        pointer: (0.0, 0.0),
        size: (0, 0),
        image_scale: 1.0,
        mode: Mode::Area,
        origin: (0.0, 0.0),
        selection: None,
        dragging: false,
        windows: visible_windows(),
        hovered: None,
    }));

    let window = gtk::ApplicationWindow::new(app);
    window.init_layer_shell();
    if let Some(monitor) = crate::display::hyprland_focused_monitor()
        && let Some(gdk_monitor) = gdk_monitor_named(&monitor.name)
    {
        window.set_monitor(Some(&gdk_monitor));
    }
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_namespace(Some("tensaku-region-capture"));
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    // -1 ignores other layer-shell exclusive zones, so the overlay
    // covers the bar as well: a region that can't include the bar is a
    // region that can't screenshot the bar.
    window.set_exclusive_zone(-1);
    window.add_css_class("region-capture-overlay");

    let overlay = gtk::Overlay::new();
    // The capture as a widget: GTK uploads it once and composites it,
    // instead of cairo rescaling it on every frame.
    let picture = gtk::Picture::for_pixbuf(&frozen);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    overlay.set_child(Some(&picture));

    for strip in &shade {
        fixed.put(strip, 0.0, 0.0);
    }
    fixed.put(&outline, 0.0, 0.0);
    for guide in &crosshair {
        fixed.put(guide, 0.0, 0.0);
    }
    overlay.add_overlay(&fixed);
    // The cursor goes on the widget the pointer is actually over, not
    // the window: GTK resolves it from the widget under the pointer
    // outward, and every event here lands on this surface.
    apply_cursor(&fixed, Mode::Area);

    // The same pill the scrolling capture uses, down to the class
    // names — the two overlays are one tool wearing two hats, and a
    // second style for the same sentence would say otherwise.
    let hint = gtk::Label::new(Some(Mode::Area.hint()));
    hint.add_css_class("scroll-capture-prompt-label");
    let hint_pill = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    hint_pill.add_css_class("scroll-capture-pill");
    hint_pill.add_css_class("scroll-capture-prompt");
    hint_pill.append(&hint);
    // Centred on screen, like the scrolling capture's prompt: the
    // instructions are the only thing to read before a drag starts, so
    // they belong where the eye already is rather than in a corner it
    // has to find.
    hint_pill.set_halign(gtk::Align::Center);
    hint_pill.set_valign(gtk::Align::Center);
    overlay.add_overlay(&hint_pill);
    window.set_child(Some(&overlay));

    install_css(app);
    install_pointer(&fixed, &state, &window, shared);
    install_keys(&window, &state, &hint, shared);

    // The overlay's size is only known once it maps, and every strip
    // is positioned in it — so learn it there and lay out the idle
    // dimmed state immediately.
    {
        let state_for_map = Rc::clone(&state);
        let picture_for_map = picture.clone();
        window.connect_map(move |_| {
            let state_for_idle = Rc::clone(&state_for_map);
            let picture = picture_for_map.clone();
            gtk::glib::idle_add_local_once(move || {
                let (w, h) = (picture.width(), picture.height());
                if w < 1 || h < 1 {
                    return;
                }
                let mut state = state_for_idle.borrow_mut();
                state.size = (w, h);
                // Image pixels per logical pixel: the capture is in
                // device pixels and the strips are placed in logical
                // ones, and on a 2x display ignoring that would halve
                // every selection.
                state.image_scale = state.frozen.width() as f64 / w as f64;
                layout_shade(&state);
            });
        });
    }

    window.present();
}

/// Move the four dim strips so they surround the pending selection,
/// and put the outline on its edge.
///
/// The strips are a frame with a hole in it: top and bottom span the
/// full width, left and right fill the gap between them. With no
/// selection the top strip covers everything and the rest collapse,
/// which is the dimmed-idle state.
fn layout_shade(state: &State) {
    let (width, height) = state.size;
    if width < 1 || height < 1 {
        return;
    }
    let place = |index: usize, x: f64, y: f64, w: f64, h: f64| {
        let strip = &state.shade[index];
        strip.set_size_request(w.max(0.0) as i32, h.max(0.0) as i32);
        state.fixed.move_(strip, x, y);
        strip.set_visible(w >= 1.0 && h >= 1.0);
    };

    let Some(rect) = pending_rect(state) else {
        place(0, 0.0, 0.0, width as f64, height as f64);
        for index in 1..4 {
            place(index, 0.0, 0.0, 0.0, 0.0);
        }
        state.outline.set_visible(false);
        layout_crosshair(state, true);
        return;
    };

    let (x, y, w, h) = (
        rect.x as f64,
        rect.y as f64,
        rect.width as f64,
        rect.height as f64,
    );
    place(0, 0.0, 0.0, width as f64, y);
    place(1, 0.0, y + h, width as f64, height as f64 - (y + h));
    place(2, 0.0, y, x, h);
    place(3, x + w, y, width as f64 - (x + w), h);

    state.outline.set_size_request(w as i32, h as i32);
    state.fixed.move_(&state.outline, x, y);
    state.outline.set_visible(w >= 1.0 && h >= 1.0);
    // Once there is a rectangle, it is the thing being aimed — the
    // guides would just be two more lines over it.
    layout_crosshair(state, false);
}

/// The pointer shape for `mode`: crosshairs while a region is being
/// aimed, because the cursor is the thing doing the aiming, and a hand
/// where the click picks a whole window.
fn apply_cursor(widget: &gtk::Fixed, mode: Mode) {
    widget.set_cursor_from_name(Some(match mode {
        Mode::Area => "crosshair",
        Mode::Window => "pointer",
    }));
}

/// Put the guides through the pointer, or hide them.
fn layout_crosshair(state: &State, visible: bool) {
    let (width, height) = state.size;
    // Window mode picks whole windows, so there is no edge to line up
    // against and the guides are noise.
    let visible = visible && state.mode == Mode::Area;
    let (x, y) = state.pointer;

    let vertical = &state.crosshair[0];
    vertical.set_size_request(1, height);
    state.fixed.move_(vertical, x.round(), 0.0);
    vertical.set_visible(visible);

    let horizontal = &state.crosshair[1];
    horizontal.set_size_request(width, 1);
    state.fixed.move_(horizontal, 0.0, y.round());
    horizontal.set_visible(visible);
}

/// The rectangle the overlay would capture if the user committed now.
fn pending_rect(state: &State) -> Option<Rect> {
    match state.mode {
        Mode::Area => state.selection,
        Mode::Window => state.hovered.as_ref().map(|w| Rect {
            x: w.x as i32,
            y: w.y as i32,
            width: w.width as i32,
            height: w.height as i32,
        }),
    }
}

/// Convert a logical-pixel rectangle into the captured image's own
/// pixels, clamped to the image so a drag off the edge still crops.
fn to_image_rect(rect: Rect, scale: f64, frozen: &Pixbuf) -> Rect {
    let x = ((rect.x as f64 * scale).round() as i32).clamp(0, frozen.width());
    let y = ((rect.y as f64 * scale).round() as i32).clamp(0, frozen.height());
    let width = ((rect.width as f64 * scale).round() as i32).clamp(0, frozen.width() - x);
    let height = ((rect.height as f64 * scale).round() as i32).clamp(0, frozen.height() - y);
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// The choice a commit would make right now, in image pixels.
fn committed_region(state: &State) -> Option<RegionOutcome> {
    let rect = pending_rect(state)?;
    let rect = to_image_rect(rect, state.image_scale, &state.frozen);
    (rect.width > 0 && rect.height > 0).then_some(RegionOutcome::Region(rect))
}

fn install_pointer(
    surface: &gtk::Fixed,
    state: &Rc<RefCell<State>>,
    window: &gtk::ApplicationWindow,
    shared: &Rc<RefCell<RegionOutcome>>,
) {
    let motion = gtk::EventControllerMotion::new();
    {
        let state = Rc::clone(state);
        motion.connect_motion(move |_, x, y| {
            let mut state = state.borrow_mut();
            state.pointer = (x, y);
            if state.mode == Mode::Window {
                let hit = window_at(&state.windows, x, y).cloned();
                if hit != state.hovered {
                    state.hovered = hit;
                }
            }
            layout_shade(&state);
        });
    }
    surface.add_controller(motion);

    let drag = gtk::GestureDrag::new();
    {
        let state = Rc::clone(state);
        drag.connect_drag_begin(move |_, x, y| {
            let mut state = state.borrow_mut();
            state.origin = (x, y);
            state.dragging = true;
            state.selection = None;
        });
    }
    {
        let state = Rc::clone(state);
        drag.connect_drag_update(move |_, dx, dy| {
            let mut state = state.borrow_mut();
            if state.mode != Mode::Area {
                return;
            }
            let origin = state.origin;
            state.selection = Some(rect_between(origin, (origin.0 + dx, origin.1 + dy)));
            layout_shade(&state);
        });
    }
    {
        let state = Rc::clone(state);
        let window = window.clone();
        let shared = Rc::clone(shared);
        drag.connect_drag_end(move |_, dx, dy| {
            let choice = {
                let mut state = state.borrow_mut();
                state.dragging = false;
                match state.mode {
                    // A click in window mode takes what it points at.
                    Mode::Window => committed_region(&state),
                    // A drag too small to be deliberate isn't a region.
                    Mode::Area if dx.abs() < MIN_DRAG && dy.abs() < MIN_DRAG => {
                        state.selection = None;
                        None
                    }
                    Mode::Area => committed_region(&state),
                }
            };
            if let Some(choice) = choice {
                *shared.borrow_mut() = choice;
                window.close();
            }
        });
    }
    surface.add_controller(drag);
}

fn install_keys(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<State>>,
    hint: &gtk::Label,
    shared: &Rc<RefCell<RegionOutcome>>,
) {
    let cursor_target = state.borrow().fixed.clone();
    let keys = gtk::EventControllerKey::new();
    let state = Rc::clone(state);
    let hint = hint.clone();
    let window_for_keys = window.clone();
    let shared = Rc::clone(shared);
    keys.connect_key_pressed(move |_, key, _, _| {
        let finish = |outcome: RegionOutcome| {
            *shared.borrow_mut() = outcome;
            window_for_keys.close();
            gtk::glib::Propagation::Stop
        };
        match key {
            gdk::Key::Escape => finish(RegionOutcome::Cancelled),
            gdk::Key::f | gdk::Key::F => finish(RegionOutcome::Fullscreen),
            // Handing over before anything is captured is what makes
            // this a mode switch rather than a retake.
            gdk::Key::s | gdk::Key::S => finish(RegionOutcome::Scroll),
            gdk::Key::Return | gdk::Key::KP_Enter => {
                let choice = committed_region(&state.borrow());
                match choice {
                    Some(choice) => finish(choice),
                    None => gtk::glib::Propagation::Stop,
                }
            }
            gdk::Key::space => {
                {
                    let mut state = state.borrow_mut();
                    state.mode = match state.mode {
                        Mode::Area => Mode::Window,
                        Mode::Window => Mode::Area,
                    };
                    // Each mode starts clean: a rectangle left over
                    // from the other one would sit there looking
                    // selected while the keys no longer act on it.
                    state.selection = None;
                    state.hovered = None;
                    hint.set_text(state.mode.hint());
                }
                apply_cursor(&cursor_target, state.borrow().mode);
                layout_shade(&state.borrow());
                gtk::glib::Propagation::Stop
            }
            _ => gtk::glib::Propagation::Proceed,
        }
    });
    window.add_controller(keys);
}

/// Normalise two corners into a rectangle, whichever way it was dragged.
fn rect_between(a: (f64, f64), b: (f64, f64)) -> Rect {
    let x = a.0.min(b.0);
    let y = a.1.min(b.1);
    Rect {
        x: x.round() as i32,
        y: y.round() as i32,
        width: (a.0 - b.0).abs().round() as i32,
        height: (a.1 - b.1).abs().round() as i32,
    }
}

fn gdk_monitor_named(name: &str) -> Option<gdk::Monitor> {
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    for index in 0..monitors.n_items() {
        let monitor = monitors.item(index)?.downcast::<gdk::Monitor>().ok()?;
        if monitor.connector().is_some_and(|c| c == name) {
            return Some(monitor);
        }
    }
    None
}

fn install_css(app: &gtk::Application) {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        ".region-capture-overlay { background: transparent; }
         /* The dim sheet, as strips GTK composites rather than pixels
            cairo blends on every motion event. */
         .region-capture-shade { background: rgba(0, 0, 0, 0.45); }
         .region-capture-outline {
             border: 1px solid rgba(255, 255, 255, 0.9);
         }
         /* Faint enough to read the screen through, which is the point
            of lining up against it. Same class and colour in the
            scrolling capture's stylesheet. */
         .capture-crosshair { background: rgba(255, 255, 255, 0.22); }
         /* Lifted from the scrolling capture's stylesheet: this
            overlay is a separate GTK application with its own CSS, so
            the rules have to exist in both places to look like one
            tool. */
         .scroll-capture-pill {
             background-color: rgba(28, 28, 30, 0.92);
             border-radius: 999px;
             padding: 10px 18px;
             border: 1px solid rgba(255, 255, 255, 0.08);
             box-shadow: 0 6px 18px rgba(0, 0, 0, 0.45);
         }
         .scroll-capture-prompt-label {
             color: rgba(245, 245, 247, 0.92);
             font-size: 14px;
             padding: 0 4px;
         }",
    );
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    let _ = app;
}

#[cfg(test)]
mod tests {
    use super::{MIN_DRAG, Mode, Rect, rect_between, to_image_rect};
    use relm4::gtk::gdk_pixbuf::{Colorspace, Pixbuf};

    /// Dragging up-left has to produce the same rectangle as dragging
    /// down-right across the same two points.
    #[test]
    fn a_rect_normalises_whichever_way_it_was_dragged() {
        let down_right = rect_between((100.0, 50.0), (300.0, 250.0));
        let up_left = rect_between((300.0, 250.0), (100.0, 50.0));
        assert_eq!(down_right.x, up_left.x);
        assert_eq!(down_right.y, up_left.y);
        assert_eq!((down_right.width, down_right.height), (200, 200));
        assert_eq!((up_left.width, up_left.height), (200, 200));
    }

    /// Both modes name the key that leaves them. A full-screen modal
    /// overlay with an unexplained mode is a dead end.
    #[test]
    fn every_mode_explains_its_way_out() {
        for mode in [Mode::Area, Mode::Window] {
            let hint = mode.hint();
            assert!(hint.contains("Space"), "{hint}");
            assert!(hint.contains("Esc"), "{hint}");
            assert!(hint.contains("scrolling"), "{hint}");
        }
    }

    /// A drag under the threshold isn't a region — the same rounding
    /// the drag-end uses, so a click that wobbles stays a click.
    /// A 2x display hands back a capture twice the overlay's logical
    /// size, and a selection that ignored that would crop a quarter of
    /// what was outlined.
    #[test]
    fn a_selection_converts_into_the_captures_own_pixels() {
        let frozen = Pixbuf::new(Colorspace::Rgb, true, 8, 6144, 3456).unwrap();
        let rect = to_image_rect(
            Rect {
                x: 100,
                y: 50,
                width: 400,
                height: 200,
            },
            2.0,
            &frozen,
        );
        assert_eq!((rect.x, rect.y), (200, 100));
        assert_eq!((rect.width, rect.height), (800, 400));
    }

    /// A drag that runs off the edge crops to the edge rather than
    /// asking for pixels the capture doesn't have.
    #[test]
    fn a_selection_past_the_edge_is_clamped() {
        let frozen = Pixbuf::new(Colorspace::Rgb, true, 8, 1000, 800).unwrap();
        let rect = to_image_rect(
            Rect {
                x: 900,
                y: 700,
                width: 400,
                height: 400,
            },
            1.0,
            &frozen,
        );
        assert_eq!(rect.x + rect.width, 1000);
        assert_eq!(rect.y + rect.height, 800);
    }

    #[test]
    fn a_click_is_not_a_tiny_region() {
        let wobble = rect_between((100.0, 100.0), (100.0 + MIN_DRAG - 1.0, 102.0));
        assert!(wobble.width < MIN_DRAG as i32 && wobble.height < MIN_DRAG as i32);
        let deliberate = rect_between((100.0, 100.0), (100.0 + MIN_DRAG + 1.0, 140.0));
        assert!(deliberate.width > MIN_DRAG as i32);
    }
}
