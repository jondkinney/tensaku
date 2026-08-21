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
    let state = Rc::new(RefCell::new(State {
        frozen,
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
    window.set_cursor_from_name(Some("crosshair"));

    let overlay = gtk::Overlay::new();
    let drawing = gtk::DrawingArea::new();
    drawing.set_hexpand(true);
    drawing.set_vexpand(true);
    overlay.set_child(Some(&drawing));

    let hint = gtk::Label::new(Some(Mode::Area.hint()));
    hint.add_css_class("region-capture-hint");
    hint.set_halign(gtk::Align::Center);
    hint.set_valign(gtk::Align::End);
    hint.set_margin_bottom(48);
    overlay.add_overlay(&hint);
    window.set_child(Some(&overlay));

    install_css(app);
    install_draw(&drawing, &state);
    install_pointer(&drawing, &state, &window, shared);
    install_keys(&window, &state, &drawing, &hint, shared);

    window.present();
}

/// Paint the dim sheet and cut the pending selection out of it.
fn install_draw(drawing: &gtk::DrawingArea, state: &Rc<RefCell<State>>) {
    let state = Rc::clone(state);
    drawing.set_draw_func(move |_, ctx, width, height| {
        let mut state = state.borrow_mut();
        if width < 1 || height < 1 {
            return;
        }
        // The capture is in device pixels and the overlay is laid out
        // in logical ones. Learn the ratio here, where both are known,
        // and record it for the selection to convert with.
        let scale = state.frozen.width() as f64 / width as f64;
        state.image_scale = scale;

        // The frozen screen, then a sheet over it. Painting the
        // capture rather than leaving the surface transparent is what
        // makes the selection show what was there when you pressed the
        // key, rather than whatever moved since.
        ctx.save().ok();
        ctx.scale(1.0 / scale, 1.0 / scale);
        ctx.set_source_pixbuf(&state.frozen, 0.0, 0.0);
        let _ = ctx.paint();
        ctx.restore().ok();

        ctx.set_source_rgba(0.0, 0.0, 0.0, 0.45);
        let _ = ctx.paint();

        let Some(rect) = pending_rect(&state) else {
            return;
        };
        let (x, y, w, h) = (
            rect.x as f64,
            rect.y as f64,
            rect.width as f64,
            rect.height as f64,
        );
        if w < 1.0 || h < 1.0 {
            return;
        }
        // Repaint the capture inside the selection, undimmed, so the
        // rectangle previews exactly the pixels it will keep.
        ctx.save().ok();
        ctx.rectangle(x, y, w, h);
        ctx.clip();
        ctx.scale(1.0 / scale, 1.0 / scale);
        ctx.set_source_pixbuf(&state.frozen, 0.0, 0.0);
        let _ = ctx.paint();
        ctx.restore().ok();

        ctx.set_source_rgba(1.0, 1.0, 1.0, 0.9);
        ctx.set_line_width(1.0);
        ctx.rectangle(x + 0.5, y + 0.5, w - 1.0, h - 1.0);
        let _ = ctx.stroke();
    });
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
    drawing: &gtk::DrawingArea,
    state: &Rc<RefCell<State>>,
    window: &gtk::ApplicationWindow,
    shared: &Rc<RefCell<RegionOutcome>>,
) {
    let motion = gtk::EventControllerMotion::new();
    {
        let state = Rc::clone(state);
        let drawing = drawing.clone();
        motion.connect_motion(move |_, x, y| {
            let mut state = state.borrow_mut();
            if state.mode != Mode::Window {
                return;
            }
            let hit = window_at(&state.windows, x, y).cloned();
            if hit != state.hovered {
                state.hovered = hit;
                drop(state);
                drawing.queue_draw();
            }
        });
    }
    drawing.add_controller(motion);

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
        let drawing = drawing.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            {
                let mut state = state.borrow_mut();
                if state.mode != Mode::Area {
                    return;
                }
                let origin = state.origin;
                state.selection = Some(rect_between(origin, (origin.0 + dx, origin.1 + dy)));
            }
            drawing.queue_draw();
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
    drawing.add_controller(drag);
}

fn install_keys(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<State>>,
    drawing: &gtk::DrawingArea,
    hint: &gtk::Label,
    shared: &Rc<RefCell<RegionOutcome>>,
) {
    let keys = gtk::EventControllerKey::new();
    let state = Rc::clone(state);
    let drawing = drawing.clone();
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
                window_for_keys.set_cursor_from_name(Some(
                    match state.borrow().mode {
                        Mode::Area => "crosshair",
                        Mode::Window => "pointer",
                    },
                ));
                drawing.queue_draw();
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
         .region-capture-hint {
             background: rgba(0, 0, 0, 0.75);
             border-radius: 8px;
             color: #ffffff;
             font-size: 1.05em;
             padding: 8px 16px;
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
