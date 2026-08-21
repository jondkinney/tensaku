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
use crate::scroll_capture::{ResizeHandle, Selection, handle_cursor_name, hit_test_handle};
use crate::windows::{WindowTarget, visible_windows, window_at};

/// Gap between the hint pill and the bottom of the screen. Matches the
/// scrolling capture's prompt, which sits at the same height.
const HINT_MARGIN_BOTTOM: i32 = 48;

/// Edge grip and move puck sizes, in logical pixels. The grips match
/// the tolerance `hit_test_handle` already uses, so what you see is
/// what you can grab.
const GRIP: i32 = 12;
const PUCK: i32 = 30;

/// Space between the move puck and the capture button.
const PUCK_GAP: f64 = 10.0;

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
                "Drag to select  ·  Space: window  ·  R: last region  ·  F: full screen  ·  S: scrolling  ·  Esc"
            }
            Mode::Window => {
                "Click a window  ·  Space: area  ·  R: last region  ·  F: full screen  ·  S: scrolling  ·  Esc"
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
    /// Live pixel size of the selection, in the capture's own pixels —
    /// what the saved file will measure, not what the overlay shows.
    readout: gtk::Button,
    readout_label: gtk::Label,
    /// Eight edge grips and the move puck, shown on a settled
    /// selection so a restored region reads as adjustable rather than
    /// as a picture of where the last one was.
    grips: [gtk::Box; 8],
    puck: gtk::DrawingArea,
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
    /// The handle a drag grabbed, when it started on the edge of an
    /// existing selection — a restored region is meant to be adjusted,
    /// not replaced by whatever you drag next.
    resize_handle: Option<ResizeHandle>,
    resize_anchor: Selection,
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

fn build_overlay(app: &gtk::Application, shared: &Rc<RefCell<RegionOutcome>>, frozen: Pixbuf) {
    // Windows are read once, at the moment the overlay goes up. The
    // screen is frozen behind it from the user's point of view, so a
    // list that shifted underneath would snap to something that is no
    // longer where it was drawn.
    let shade = std::array::from_fn(|_| {
        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        strip.add_css_class("capture-shade");
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
    // The size pill IS the shutter: it already sits under the region
    // saying how big the shot is, and "take it" is the only thing
    // anyone wants to do next. Hovering swaps the number for a camera
    // so the click is advertised before it is made.
    let readout_label = gtk::Label::new(None);
    readout_label.add_css_class("scroll-capture-prompt-label");
    let readout_camera = gtk::Image::from_icon_name("camera-regular");
    let readout_faces = gtk::Stack::new();
    readout_faces.add_named(&readout_label, Some("size"));
    readout_faces.add_named(&readout_camera, Some("camera"));
    readout_faces.set_visible_child_name("size");

    let readout = gtk::Button::new();
    readout.set_child(Some(&readout_faces));
    readout.add_css_class("scroll-capture-pill");
    readout.add_css_class("capture-readout");
    readout.set_focusable(false);
    readout.set_focus_on_click(false);
    readout.set_tooltip_text(Some("Capture this region"));
    readout.set_visible(false);
    // Its own pointer, or it inherits the surface's crosshair and
    // reads as somewhere to start a new drag rather than a button.
    readout.set_cursor_from_name(Some("pointer"));
    {
        let faces = readout_faces.clone();
        let hover = gtk::EventControllerMotion::new();
        let faces_leave = faces.clone();
        hover.connect_enter(move |_, _, _| faces.set_visible_child_name("camera"));
        hover.connect_leave(move |_| faces_leave.set_visible_child_name("size"));
        readout.add_controller(hover);
    }

    let grips: [gtk::Box; 8] = std::array::from_fn(|_| {
        let grip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        grip.add_css_class("capture-grip");
        grip.set_size_request(GRIP, GRIP);
        grip.set_visible(false);
        grip
    });
    // The scrolling capture's puck, drawn by the same code rather than
    // approximated with a glyph: it is one control and should look
    // like one. A DrawingArea this small repaints for nothing.
    let puck = gtk::DrawingArea::new();
    puck.set_size_request(PUCK, PUCK);
    puck.set_visible(false);
    puck.set_cursor_from_name(Some("move"));
    puck.set_draw_func(|_, ctx, w, h| {
        let r = (w.min(h) as f64 / 2.0 - 1.0).max(1.0);
        crate::scroll_capture::draw_move_puck(ctx, w as f64 / 2.0, h as f64 / 2.0, r);
    });

    let state = Rc::new(RefCell::new(State {
        frozen: frozen.clone(),
        grips: grips.clone(),
        puck: puck.clone(),
        readout: readout.clone(),
        readout_label: readout_label.clone(),
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
        resize_handle: None,
        resize_anchor: Selection::default(),
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
    fixed.put(&readout, 0.0, 0.0);
    for grip in &grips {
        fixed.put(grip, 0.0, 0.0);
    }
    fixed.put(&puck, 0.0, 0.0);
    overlay.add_overlay(&fixed);
    {
        // Commits exactly what Enter would, through the same path, so
        // the two can't drift apart about what "this region" means.
        let state_for_shutter = Rc::clone(&state);
        let shared_for_shutter = Rc::clone(shared);
        let window_for_shutter = window.clone();
        // A GestureClick in the capture phase, not `connect_clicked`:
        // the surface underneath carries the selection drag, and a
        // press that reaches it first becomes a drag rather than a
        // click on this button.
        let press = gtk::GestureClick::new();
        press.set_propagation_phase(gtk::PropagationPhase::Capture);
        press.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let choice = committed_region(&state_for_shutter.borrow());
            if let Some(choice) = choice {
                *shared_for_shutter.borrow_mut() = choice;
                window_for_shutter.close();
            }
        });
        readout.add_controller(press);
    }
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
    // Along the bottom: the instructions are read once and then they
    // are in the way, and the middle of the screen is exactly where
    // the aiming happens.
    hint_pill.set_halign(gtk::Align::Center);
    hint_pill.set_valign(gtk::Align::End);
    hint_pill.set_margin_bottom(HINT_MARGIN_BOTTOM);
    overlay.add_overlay(&hint_pill);
    window.set_child(Some(&overlay));

    // The icon bundle is registered by the editor's startup, which
    // this path never runs — without it the capture button renders
    // GTK's missing-image glyph.
    relm4_icons::initialize_icons(
        crate::icons::icon_names::GRESOURCE_BYTES,
        crate::icons::icon_names::RESOURCE_PREFIX,
    );
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
        state.readout.set_visible(false);
        hide_grips(state);
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
    // A fresh drag has no controls to anchor to, so the size follows
    // the corner being pulled. Everything else — a restored region,
    // one being moved or resized — has the control cluster, and the
    // size belongs in it, holding still.
    if fresh_drag(state) {
        layout_readout(state, rect);
    }
    layout_grips(state, rect);
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

/// Whether this drag is drawing a new rectangle rather than adjusting
/// one that already exists.
fn fresh_drag(state: &State) -> bool {
    state.dragging && state.resize_handle.is_none()
}

/// Put the eight grips on the selection's edges and the puck at its
/// middle, or hide them.
///
/// Only on a settled selection: during a drag the rectangle is already
/// following the pointer, and eight squares chasing it would be
/// noise. Their positions are the same ones `hit_test_handle` answers
/// for, so a grip is exactly where the grab is.
fn layout_grips(state: &State, rect: Rect) {
    if fresh_drag(state) || rect.width < 1 || rect.height < 1 {
        hide_grips(state);
        return;
    }
    let (x, y, w, h) = (
        rect.x as f64,
        rect.y as f64,
        rect.width as f64,
        rect.height as f64,
    );
    let half = GRIP as f64 / 2.0;
    let spots = [
        (x, y),
        (x + w / 2.0, y),
        (x + w, y),
        (x + w, y + h / 2.0),
        (x + w, y + h),
        (x + w / 2.0, y + h),
        (x, y + h),
        (x, y + h / 2.0),
    ];
    for (grip, (cx, cy)) in state.grips.iter().zip(spots) {
        state.fixed.move_(grip, cx - half, cy - half);
        grip.set_visible(true);
    }

    // The puck needs room of its own — on a small region it would
    // cover the thing being framed.
    let centre_x = x + w / 2.0;
    let puck_top = y + h / 2.0 - PUCK as f64 / 2.0;
    let fits = w > PUCK as f64 * 2.0 && h > PUCK as f64 * 2.0;
    state
        .fixed
        .move_(&state.puck, centre_x - PUCK as f64 / 2.0, puck_top);
    state.puck.set_visible(fits);

    // Size and shutter ride together under the puck, anchored to the
    // region rather than to the pointer. They move when the region
    // does and not otherwise: a number that jumps around while you are
    // nudging a rectangle into place is harder to read than one that
    // sits still.
    state
        .readout_label
        .set_text(&format!("{} × {}", rect.width, rect.height));
    let (_, pill_w, _, _) = state.readout.measure(gtk::Orientation::Horizontal, -1);
    let (_, pill_h, _, _) = state.readout.measure(gtk::Orientation::Vertical, pill_w);
    let row_y = puck_top + PUCK as f64 + PUCK_GAP;
    let row_fits = fits && h > PUCK as f64 * 2.0 + pill_h as f64 + PUCK_GAP;

    state
        .fixed
        .move_(&state.readout, centre_x - pill_w as f64 / 2.0, row_y);
    state.readout.set_visible(row_fits);
    // Settled: the pill is the shutter again and needs its click back.
    state.readout.set_can_target(true);
}

fn hide_grips(state: &State) {
    for grip in &state.grips {
        grip.set_visible(false);
    }
    state.puck.set_visible(false);
}

/// Show the selection's size beside the corner being dragged.
///
/// In the overlay's own logical pixels, not the capture's device ones.
/// The editor reports the same way — it divides the image by the
/// capture scale so a region framed on a 2x display reads at the size
/// it looked, not double — and a capture tool whose two halves
/// disagree about how big the shot is teaches nobody anything.
fn layout_readout(state: &State, rect: Rect) {
    if rect.width < 1 || rect.height < 1 {
        state.readout.set_visible(false);
        return;
    }
    state
        .readout_label
        .set_text(&format!("{} × {}", rect.width, rect.height));
    state.readout.set_visible(true);
    // Mid-drag it is a number chasing the corner, and a fast drag
    // overruns it — it must not take the pick (and its hand cursor)
    // from the surface. It gets its click back when the drag settles
    // and `layout_grips` makes it the shutter.
    state.readout.set_can_target(false);

    let (_, width, _, _) = state.readout.measure(gtk::Orientation::Horizontal, -1);
    let (_, height, _, _) = state.readout.measure(gtk::Orientation::Vertical, width);
    let corner = state.pointer;
    let origin = state.origin;
    let (x, y) = readout_position(
        origin,
        corner,
        (width as f64, height as f64),
        (state.size.0 as f64, state.size.1 as f64),
    );
    state.fixed.move_(&state.readout, x, y);
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
///
/// Remembers the logical rectangle on the way out, so the next capture
/// can restore it: the saved value has to be in the overlay's own
/// coordinates, since that is what a restored selection is drawn in.
fn committed_region(state: &State) -> Option<RegionOutcome> {
    let logical = pending_rect(state)?;
    let rect = to_image_rect(logical, state.image_scale, &state.frozen);
    if rect.width <= 0 || rect.height <= 0 {
        return None;
    }
    crate::state::save_capture_last_region([
        logical.x as f64,
        logical.y as f64,
        logical.width as f64,
        logical.height as f64,
    ]);
    Some(RegionOutcome::Region(rect))
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
        let surface_w = surface.clone();
        motion.connect_motion(move |_, x, y| {
            let mut state = state.borrow_mut();
            state.pointer = (x, y);
            if state.mode == Mode::Window {
                let hit = window_at(&state.windows, x, y).cloned();
                if hit != state.hovered {
                    state.hovered = hit;
                }
            }
            // Over a settled selection, the cursor says what a drag
            // would do to it — resize from an edge, move from the
            // middle — rather than offering a crosshair that would
            // start a new rectangle.
            if state.mode == Mode::Area
                && !state.dragging
                && let Some(rect) = state.selection
            {
                let handle = hit_test_handle(selection_of(rect), x, y);
                surface_w.set_cursor_from_name(Some(handle_cursor_name(handle, "crosshair")));
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
            // Starting on the edge of the current selection adjusts
            // it; starting anywhere else replaces it.
            state.resize_handle = state
                .selection
                .map(selection_of)
                .and_then(|sel| hit_test_handle(sel, x, y));
            match state.resize_handle {
                Some(_) => {
                    state.resize_anchor = state.selection.map(selection_of).unwrap_or_default()
                }
                None => state.selection = None,
            }
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
            state.pointer = (origin.0 + dx, origin.1 + dy);
            state.selection = Some(match state.resize_handle {
                Some(handle) => {
                    rect_of(handle.apply(state.resize_anchor, origin, origin.0 + dx, origin.1 + dy))
                }
                None => rect_between(origin, (origin.0 + dx, origin.1 + dy)),
            });
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
                let resized = state.resize_handle.take().is_some();
                // A resize adjusts the rectangle; it doesn't commit it.
                // Otherwise nudging a restored region by a pixel would
                // fire the capture before it was right.
                if resized {
                    layout_shade(&state);
                    return;
                }
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
            // The last region, back as a live selection. Same shot
            // twice means the same framing -- documentation sequences,
            // before-and-afters -- without re-dragging it by eye and
            // landing three pixels off.
            gdk::Key::r | gdk::Key::R => {
                let restored = crate::state::load_capture_last_region();
                let Some([x, y, w, h]) = restored else {
                    return gtk::glib::Propagation::Stop;
                };
                let mut state = state.borrow_mut();
                state.mode = Mode::Area;
                state.selection = Some(Rect {
                    x: x.round() as i32,
                    y: y.round() as i32,
                    width: w.round() as i32,
                    height: h.round() as i32,
                });
                // The readout follows the pointer during a drag; with
                // nothing dragged, park it on the restored corner.
                state.origin = (x, y);
                state.pointer = (x + w, y + h);
                hint.set_text(state.mode.hint());
                layout_shade(&state);
                apply_cursor(&cursor_target, Mode::Area);
                gtk::glib::Propagation::Stop
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

/// Where the size readout goes for a selection dragged from `origin`
/// to `corner`, given the readout's own size and the screen's.
///
/// It follows the moving corner and sits on the *outside* of it — the
/// direction of the drag says which side that is, so the label never
/// covers the pixels being measured. Clamped to the screen, because
/// the moving corner is exactly what you drag off the edge.
pub fn readout_position(
    origin: (f64, f64),
    corner: (f64, f64),
    readout: (f64, f64),
    screen: (f64, f64),
) -> (f64, f64) {
    const GAP: f64 = 12.0;
    let x = if corner.0 >= origin.0 {
        corner.0 + GAP
    } else {
        corner.0 - GAP - readout.0
    };
    let y = if corner.1 >= origin.1 {
        corner.1 + GAP
    } else {
        corner.1 - GAP - readout.1
    };
    // Off the edge, flip to the other side of the corner rather than
    // sliding along it: a readout pinned to the screen edge ends up
    // sitting on the selection it is measuring, which is the one place
    // it must not be.
    let x = if x < 0.0 || x + readout.0 > screen.0 {
        let flipped = if corner.0 >= origin.0 {
            corner.0 - GAP - readout.0
        } else {
            corner.0 + GAP
        };
        flipped.clamp(0.0, (screen.0 - readout.0).max(0.0))
    } else {
        x
    };
    let y = if y < 0.0 || y + readout.1 > screen.1 {
        let flipped = if corner.1 >= origin.1 {
            corner.1 - GAP - readout.1
        } else {
            corner.1 + GAP
        };
        flipped.clamp(0.0, (screen.1 - readout.1).max(0.0))
    } else {
        y
    };
    (x, y)
}

/// The scroll overlay's `Selection` for a `Rect`, and back. The two
/// overlays measure the same thing in the same units; only the type
/// differs, and the resize maths lives on `Selection`.
fn selection_of(rect: Rect) -> Selection {
    Selection {
        x: rect.x as f64,
        y: rect.y as f64,
        w: rect.width as f64,
        h: rect.height as f64,
    }
}

fn rect_of(sel: Selection) -> Rect {
    Rect {
        x: sel.x.round() as i32,
        y: sel.y.round() as i32,
        width: sel.w.round() as i32,
        height: sel.h.round() as i32,
    }
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
         .capture-shade { background: rgba(0, 0, 0, 0.45); }
         /* Grips and puck: the scrolling capture's white-on-dark, so a
            selection looks the same in either overlay. */
         .capture-grip {
             background: rgba(255, 255, 255, 0.92);
             border-radius: 3px;
             box-shadow: 0 1px 3px rgba(0, 0, 0, 0.5);
         }
         .capture-readout {
             color: rgba(245, 245, 247, 0.92);
             padding: 4px 12px;
             min-height: 0;
             min-width: 0;
         }
         .capture-readout:hover {
             background-color: rgba(60, 60, 70, 0.94);
         }
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
    use super::{MIN_DRAG, Mode, Rect, readout_position, rect_between, to_image_rect};
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
    /// The readout follows the corner being dragged and sits outside
    /// it, so it never covers the pixels it is measuring.
    #[test]
    fn the_readout_sits_outside_the_moving_corner() {
        let screen = (2000.0, 1200.0);
        let size = (90.0, 30.0);
        // Dragging down-right: below and right of the corner.
        let (x, y) = readout_position((100.0, 100.0), (500.0, 400.0), size, screen);
        assert!(x > 500.0 && y > 400.0);
        // Dragging up-left, with room on that side: above and left,
        // clear of the corner. (Without room it flips — see below.)
        let (x, y) = readout_position((1200.0, 900.0), (600.0, 500.0), size, screen);
        assert!(x + size.0 < 600.0 && y + size.1 < 500.0);
    }

    /// The corner you drag is the one you drag off the screen, so the
    /// readout has to stay on it.
    #[test]
    fn the_readout_stays_on_screen() {
        let screen = (2000.0, 1200.0);
        let size = (90.0, 30.0);
        let (x, y) = readout_position((100.0, 100.0), (1999.0, 1199.0), size, screen);
        assert!(x + size.0 <= screen.0 && y + size.1 <= screen.1);
        let (x, y) = readout_position((500.0, 400.0), (0.0, 0.0), size, screen);
        assert!(x >= 0.0 && y >= 0.0);
    }

    /// At an edge it flips to the far side of the corner rather than
    /// sliding along the edge — which would slide it onto the
    /// selection it is measuring.
    #[test]
    fn the_readout_flips_at_an_edge() {
        let screen = (2000.0, 1200.0);
        let size = (90.0, 30.0);
        // Dragging down-right into the bottom-right corner: the
        // readout would land off-screen, so it moves above and left of
        // the corner instead.
        let corner = (1990.0, 1190.0);
        let (x, y) = readout_position((100.0, 100.0), corner, size, screen);
        assert!(x + size.0 <= corner.0, "expected a flip left of {corner:?}");
        assert!(y + size.1 <= corner.1, "expected a flip above {corner:?}");
    }

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
