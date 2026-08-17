use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};
use rustix::fs::{MemfdFlags, memfd_create};
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use crate::capture::outputs::OutputTracker;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

/// One discrete wheel notch worth of axis distance. The accompanying
/// `discrete` value is what tells clients how many physical wheel detents the
/// event represents.
const NOTCH_VALUE: f64 = 10.0;

/// linux/input-event-codes.h KEY_DOWN. We send several Down-arrow presses
/// per scroll tick instead of one PgDn so the per-tick scroll delta is
/// smaller than the user's selection height — that way each captured frame
/// overlaps the previous one and the stitcher can find the alignment.
const KEY_DOWN: u32 = 108;

/// linux/input-event-codes.h KEY_RIGHT — used for horizontal auto-scroll.
const KEY_RIGHT: u32 = 106;

/// Direction the auto-scroll worker should drive the underlying app.
#[derive(Clone, Copy, Debug)]
pub enum ScrollDirection {
    Down,
    Right,
}

impl ScrollDirection {
    fn keycode(self) -> u32 {
        match self {
            ScrollDirection::Down => KEY_DOWN,
            ScrollDirection::Right => KEY_RIGHT,
        }
    }

    fn axis(self) -> wl_pointer::Axis {
        match self {
            ScrollDirection::Down => wl_pointer::Axis::VerticalScroll,
            ScrollDirection::Right => wl_pointer::Axis::HorizontalScroll,
        }
    }
}

/// How many Down-arrow presses per scroll tick. 5 presses ≈ 200 logical px
/// of scroll in most browsers (~40 logical px per arrow), small enough to
/// fit within typical selections while still progressing visibly.
pub const ARROWS_PER_TICK: u32 = 5;

/// Minimal xkb keymap. Maps xkb keycode 116 (kernel KEY_DOWN=108 + 8 xkb
/// offset) to the Down keysym and 114 (kernel KEY_RIGHT=106 + 8) to Right.
const KEYMAP_TEMPLATE: &str = r#"xkb_keymap {
    xkb_keycodes "minimal" {
        minimum = 8;
        maximum = 255;
        <DOWN>  = 116;
        <RIGHT> = 114;
    };
    xkb_types "complete" {
        type "ONE_LEVEL" {
            modifiers = none;
            level_name[Level1] = "Any";
        };
    };
    xkb_compatibility "complete" {};
    xkb_symbols "minimal" {
        key <DOWN>  { [ Down  ] };
        key <RIGHT> { [ Right ] };
    };
};
"#;

/// Time between injecting a wheel group and announcing that its rendered
/// frame is ready to capture. At 150 ms, a 60 Hz client gets roughly nine
/// frames to paint while the capture handshake still prevents the next wheel
/// event from racing the screenshot in progress.
pub const SCROLL_SETTLE_MS: u64 = 150;

/// How many wheel notches per auto-scroll tick. 3 notches ≈ 300–360px in
/// most browsers (notch * the app's per-tick pixel multiplier).
pub const NOTCHES_PER_TICK: u32 = 3;

const INPUT_REGION_SETTLE_MS: u64 = 50;
const UINPUT_DEVICE_SETTLE_MS: u64 = 150;
const POINTER_NUDGE_SETTLE_MS: u64 = 20;
const ACK_POLL_MS: u64 = 5;

struct State {
    registry_state: RegistryState,
    outputs: OutputTracker,
}

/// Lock-step coordination between the input worker and GTK's capture loop.
///
/// The worker publishes a monotonically increasing ready cycle only after the
/// underlying app has had time to render. The capture loop snapshots that
/// cycle and calls [`CaptureHandshake::acknowledge`] when the capture attempt
/// has completed. Until then, the worker will not inject another scroll.
///
/// A fresh handshake must be created for each auto-scroll run.
#[derive(Debug)]
struct CaptureAcknowledgement {
    cycle: u64,
    scroll_notches: u32,
    consumed: bool,
}

impl Default for CaptureAcknowledgement {
    fn default() -> Self {
        Self {
            cycle: 0,
            scroll_notches: NOTCHES_PER_TICK,
            consumed: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CaptureHandshake {
    ready_cycle: Arc<AtomicU64>,
    acknowledgement: Arc<Mutex<CaptureAcknowledgement>>,
}

impl CaptureHandshake {
    pub fn new() -> Self {
        Self::default()
    }

    /// Latest stable frame published by the worker, or zero before startup.
    pub fn ready_cycle(&self) -> u64 {
        self.ready_cycle.load(Ordering::Acquire)
    }

    /// Confirm that capture of `cycle` has completed (kept or rejected).
    ///
    /// Values newer than the latest published cycle are clamped, which keeps
    /// an accidental caller-side race from skipping an unseen frame.
    pub fn acknowledge(&self, cycle: u64) {
        self.acknowledge_with_scroll_notches(cycle, NOTCHES_PER_TICK);
    }

    /// Confirm capture of `cycle` and choose the size of the next scroll.
    ///
    /// A one-notch acknowledgement can be used to probe past a frame whose
    /// alignment is uncertain. The choice applies only to the scroll directly
    /// following this cycle; a later call to [`Self::acknowledge`] restores the
    /// normal [`NOTCHES_PER_TICK`] step. Requests are clamped to one normal
    /// tick so callers cannot accidentally inject either no motion or an
    /// unexpectedly large jump.
    pub fn acknowledge_with_scroll_notches(&self, cycle: u64, scroll_notches: u32) {
        let published = self.ready_cycle.load(Ordering::Acquire);
        let acknowledged_cycle = cycle.min(published);
        let mut acknowledgement = self
            .acknowledgement
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // The first acknowledgement owns the scroll request for this cycle.
        // Ignoring duplicates prevents a late caller from changing a request
        // while the worker is about to consume it.
        if acknowledged_cycle > acknowledgement.cycle {
            acknowledgement.cycle = acknowledged_cycle;
            acknowledgement.scroll_notches = scroll_notches.clamp(1, NOTCHES_PER_TICK);
            acknowledgement.consumed = false;
        }
    }

    fn publish_ready(&self) -> u64 {
        self.ready_cycle.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn wait_for_capture(&self, cycle: u64, stop: &AtomicBool) -> Option<u32> {
        loop {
            if stop.load(Ordering::Acquire) {
                return None;
            }
            {
                let mut acknowledgement = self
                    .acknowledgement
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if acknowledgement.cycle >= cycle && !acknowledgement.consumed {
                    acknowledgement.consumed = true;
                    return Some(acknowledgement.scroll_notches);
                }
            }
            thread::sleep(Duration::from_millis(ACK_POLL_MS));
        }
    }
}

/// Smoke test: create a kernel virtual mouse and send three wheel-down
/// notches. Whatever is under the cursor at invocation time should scroll.
pub fn smoke_test() -> Result<()> {
    eprintln!("auto-scroll-test: creating kernel virtual mouse...");
    let mut device = create_uinput_mouse()?;
    let natural_scroll = hyprland_natural_scroll_setting().unwrap_or_else(|| {
        eprintln!(
            "auto-scroll-test: natural-scroll policy unavailable; using physical-wheel convention"
        );
        false
    });
    thread::sleep(Duration::from_millis(UINPUT_DEVICE_SETTLE_MS));
    nudge_uinput_pointer(&mut device)?;

    eprintln!("auto-scroll-test: sending 3 real wheel-down notches...");
    for i in 0..3 {
        emit_uinput_scroll(&mut device, ScrollDirection::Down, 1, natural_scroll)?;
        eprintln!("auto-scroll-test:   notch {} sent", i + 1);
        thread::sleep(Duration::from_millis(600));
    }
    Ok(())
}

fn create_uinput_mouse() -> Result<VirtualDevice> {
    let axes = uinput_mouse_axes();
    let buttons = uinput_mouse_buttons();
    VirtualDevice::builder()
        .context("opening /dev/uinput")?
        .name("Tensaku Auto Scroll")
        .with_relative_axes(&axes)
        .context("enabling virtual mouse axes")?
        .with_keys(&buttons)
        .context("enabling virtual mouse button capability")?
        .build()
        .context("creating virtual mouse")
}

fn uinput_mouse_axes() -> AttributeSet<RelativeAxisCode> {
    AttributeSet::from_iter([
        RelativeAxisCode::REL_X,
        RelativeAxisCode::REL_Y,
        RelativeAxisCode::REL_WHEEL,
        RelativeAxisCode::REL_HWHEEL,
    ])
}

fn uinput_mouse_buttons() -> AttributeSet<KeyCode> {
    // libinput requires at least one mouse button in addition to relative
    // axes before udev classifies a virtual device primarily as a mouse.
    // Without it, compositor device policy (including natural scrolling) can
    // be applied inconsistently. The button is advertised for classification
    // only and is never emitted.
    AttributeSet::from_iter([KeyCode::BTN_LEFT])
}

fn nudge_uinput_pointer(device: &mut VirtualDevice) -> Result<()> {
    for amount in [1, -1] {
        device
            .emit(&[InputEvent::new_now(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_X.0,
                amount,
            )])
            .context("nudging virtual mouse for pointer focus")?;
        thread::sleep(Duration::from_millis(POINTER_NUDGE_SETTLE_MS));
    }
    Ok(())
}

fn emit_uinput_scroll(
    device: &mut VirtualDevice,
    direction: ScrollDirection,
    notches: u32,
    natural_scroll: bool,
) -> Result<()> {
    let (axis, amount) = uinput_scroll_event(direction, notches, natural_scroll);
    device
        .emit(&[InputEvent::new_now(EventType::RELATIVE.0, axis.0, amount)])
        .context("emitting virtual mouse wheel event")
}

fn uinput_scroll_event(
    direction: ScrollDirection,
    notches: u32,
    natural_scroll: bool,
) -> (RelativeAxisCode, i32) {
    let (axis, physical_amount) = match direction {
        // Linux REL_WHEEL follows the physical wheel convention: negative is
        // wheel-down. The compositor applies the user's natural-scroll policy.
        ScrollDirection::Down => (RelativeAxisCode::REL_WHEEL, -(notches as i32)),
        ScrollDirection::Right => (RelativeAxisCode::REL_HWHEEL, notches as i32),
    };
    let amount = if natural_scroll {
        -physical_amount
    } else {
        physical_amount
    };
    (axis, amount)
}

fn hyprland_natural_scroll_setting() -> Option<bool> {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let Ok(output) = Command::new("hyprctl")
        .args(["getoption", "input:natural_scroll", "-j"])
        .output()
    else {
        return None;
    };
    output
        .status
        .success()
        .then(|| parse_hyprland_bool(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

fn parse_hyprland_bool(output: &str) -> Option<bool> {
    let value = output
        .split_once("\"bool\"")
        .and_then(|(_, tail)| tail.trim_start().strip_prefix(':'))
        .map(str::trim_start)?;
    if value.starts_with("true") {
        Some(true)
    } else if value.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Emit one aggregate wheel event followed directly by one frame. A separate
/// `axis` request would replace Hyprland's pending discrete event.
fn scroll(
    pointer: &zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    direction: ScrollDirection,
    notches: u32,
    time: u32,
) {
    if notches == 0 {
        return;
    }
    pointer.axis_discrete(
        time,
        direction.axis(),
        NOTCH_VALUE * f64::from(notches),
        notches as i32,
    );
    // Hyprland associates axis_source with the most recently named axis, so
    // send it after axis_discrete (the protocol permits either ordering).
    pointer.axis_source(wl_pointer::AxisSource::Wheel);
    pointer.frame();
}

/// Wayland event timestamps are milliseconds since some monotonic epoch.
/// We just pass our own monotonic counter; the compositor only uses these
/// for ordering, not absolute time.
fn time_ms(start: Instant) -> u32 {
    start.elapsed().as_millis() as u32
}

fn make_keymap_fd() -> Result<(OwnedFd, u32)> {
    let keymap = format!("{}\0", KEYMAP_TEMPLATE);
    let size = keymap.len() as u32;
    let fd: OwnedFd = memfd_create("tensaku-keymap", MemfdFlags::empty())
        .context("memfd_create for keymap failed")?;
    let mut file = std::fs::File::from(fd);
    file.write_all(keymap.as_bytes())
        .context("writing keymap to memfd")?;
    Ok((file.into(), size))
}

/// Drive the underlying app with real wheel events during capture.
///
/// `mod.rs` must make its layer-shell selection input-transparent and set
/// `KeyboardMode::None` before calling this function. The worker waits briefly
/// for that surface commit, warps to `(cursor_x, cursor_y)`, and nudges by one
/// pixel and back to make the compositor hit-test the underlying client. It
/// prefers a kernel virtual mouse for real wheel input, then tries the wlr
/// virtual-pointer protocol.
///
/// The virtual keyboard is retained only as a compatibility fallback when the
/// compositor cannot provide either real-wheel backend. Caller controls `stop` and
/// must acknowledge each [`CaptureHandshake::ready_cycle`] after capture.
pub fn spawn_worker(
    stop: Arc<AtomicBool>,
    handshake: CaptureHandshake,
    cursor_x: i32,
    cursor_y: i32,
    direction: ScrollDirection,
    output_name: Option<&str>,
) -> Result<()> {
    let uinput_result = match hyprland_natural_scroll_setting() {
        Some(natural_scroll) => spawn_worker_uinput(
            Arc::clone(&stop),
            handshake.clone(),
            cursor_x,
            cursor_y,
            direction,
            natural_scroll,
            output_name,
        ),
        None => Err(anyhow!(
            "compositor natural-scroll policy is unknown; using a logical wheel backend"
        )),
    };
    match uinput_result {
        Ok(()) => Ok(()),
        Err(uinput_error) => match spawn_worker_virtual_pointer(
            Arc::clone(&stop),
            handshake.clone(),
            cursor_x,
            cursor_y,
            direction,
            output_name,
        ) {
            Ok(()) => {
                eprintln!(
                    "auto-scroll: kernel virtual mouse unavailable ({uinput_error:#}); using Wayland virtual pointer"
                );
                Ok(())
            }
            Err(pointer_error) => {
                eprintln!(
                    "auto-scroll: real wheel setup failed (uinput: {uinput_error:#}; Wayland: {pointer_error:#}); falling back to arrow keys"
                );
                spawn_worker_virtual_keyboard(stop, handshake, direction).with_context(|| {
                    format!(
                        "uinput failed ({uinput_error:#}), virtual pointer failed ({pointer_error:#}), and virtual keyboard fallback failed"
                    )
                })
            }
        },
    }
}

fn spawn_worker_uinput(
    stop: Arc<AtomicBool>,
    handshake: CaptureHandshake,
    cursor_x: i32,
    cursor_y: i32,
    direction: ScrollDirection,
    natural_scroll: bool,
    output_name: Option<&str>,
) -> Result<()> {
    let mut device = create_uinput_mouse()?;
    // Parking is useful for keeping hover effects out of captured frames, but
    // it is not a prerequisite for kernel wheel injection. Create at most one
    // Wayland pointer context for the worker and reuse it for every cycle;
    // compositors without the protocol can still use uinput at the cursor's
    // current position.
    let mut focus_context = match create_virtual_pointer(output_name) {
        Ok(context) => Some(context),
        Err(error) => {
            eprintln!(
                "auto-scroll: pointer parking unavailable; continuing with kernel wheel input: {error:#}"
            );
            None
        }
    };
    thread::spawn(move || {
        if sleep_unless_stopped(&stop, Duration::from_millis(UINPUT_DEVICE_SETTLE_MS)) {
            if let Some(context) = focus_context.as_mut() {
                focus_underlying_with_context(context, cursor_x, cursor_y);
            }
            if !stop.load(Ordering::Acquire)
                && let Err(error) = nudge_uinput_pointer(&mut device)
            {
                eprintln!(
                    "auto-scroll: failed to focus underlying content with virtual mouse: {error:#}"
                );
                stop.store(true, Ordering::Release);
            }
        }

        if !stop.load(Ordering::Acquire) {
            run_capture_scroll_loop(&stop, &handshake, |scroll_notches| {
                // The user may have moved onto the outside capture pill while
                // a cycle was paused. Repark before every wheel group so the
                // event cannot scroll Tensaku or another surface.
                if let Some(context) = focus_context.as_mut() {
                    focus_underlying_with_context(context, cursor_x, cursor_y);
                }
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                emit_uinput_scroll(&mut device, direction, scroll_notches, natural_scroll)
            });
        }

        stop.store(true, Ordering::Release);
        if let Some(context) = focus_context {
            destroy_virtual_pointer_context(context);
        }
        eprintln!("auto-scroll: kernel virtual-mouse worker exited");
    });

    eprintln!(
        "auto-scroll: real wheel worker started ({} notches, {}ms settle)",
        NOTCHES_PER_TICK, SCROLL_SETTLE_MS
    );
    Ok(())
}

fn spawn_worker_virtual_pointer(
    stop: Arc<AtomicBool>,
    handshake: CaptureHandshake,
    cursor_x: i32,
    cursor_y: i32,
    direction: ScrollDirection,
    output_name: Option<&str>,
) -> Result<()> {
    let (mut event_queue, pointer, screen_width, screen_height) =
        create_virtual_pointer(output_name)?;

    thread::spawn(move || {
        let start = Instant::now();

        // `spawn_worker` is called from a GTK idle after the buttons are
        // hidden. Give the layer surface's new input region time to commit,
        // warp inside the selected app content, then generate actual relative
        // motion. +1/-1 with separate frames forces hit-testing while
        // returning the pointer to the requested position.
        if sleep_unless_stopped(&stop, Duration::from_millis(INPUT_REGION_SETTLE_MS)) {
            focus_pointer(
                &pointer,
                &mut event_queue,
                start,
                cursor_x,
                cursor_y,
                screen_width,
                screen_height,
            );
            let _ = sleep_unless_stopped(&stop, Duration::from_millis(POINTER_NUDGE_SETTLE_MS));
        }

        if !stop.load(Ordering::Acquire) {
            run_capture_scroll_loop(&stop, &handshake, |scroll_notches| {
                focus_pointer(
                    &pointer,
                    &mut event_queue,
                    start,
                    cursor_x,
                    cursor_y,
                    screen_width,
                    screen_height,
                );
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                scroll(&pointer, direction, scroll_notches, time_ms(start));
                event_queue.flush().map_err(anyhow::Error::from)
            });
        }

        stop.store(true, Ordering::Release);
        pointer.destroy();
        let _ = event_queue.flush();
        eprintln!("auto-scroll: virtual-pointer worker exited");
    });

    eprintln!(
        "auto-scroll: real wheel worker started ({}× notches, {}ms settle)",
        NOTCHES_PER_TICK, SCROLL_SETTLE_MS
    );
    Ok(())
}

type VirtualPointerContext = (
    wayland_client::EventQueue<State>,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    i32,
    i32,
);

/// Create a virtual pointer mapped to the output named `output_name`.
///
/// `motion_absolute` coordinates are physical pixels of that output, so the
/// pointer must be bound to it (`create_virtual_pointer_with_output`, v2);
/// an unbound pointer is mapped across the whole compositor layout, which
/// lands warps on the wrong screen (or far off inside the right one) as soon
/// as a second monitor is connected. Compositors that only offer v1 keep the
/// old whole-layout behavior.
fn create_virtual_pointer(output_name: Option<&str>) -> Result<VirtualPointerContext> {
    let conn = Connection::connect_to_env().context("auto-scroll: failed to connect to Wayland")?;
    let (globals, mut event_queue) =
        registry_queue_init::<State>(&conn).context("auto-scroll: registry init")?;
    let qh = event_queue.handle();
    let manager = globals
        .bind::<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, _, _>(&qh, 1..=2, ())
        .context("compositor does not expose zwlr_virtual_pointer_manager_v1")?;
    let seat = globals
        .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=8, ())
        .context("no wl_seat available")?;
    let mut outputs = OutputTracker::default();
    outputs.bind_all(&globals, &qh);
    let mut state = State {
        registry_state: RegistryState::new(&globals),
        outputs,
    };
    // Output names and modes provide the target and its absolute-pointer
    // coordinate extent. Two roundtrips match the existing path and reliably
    // receive wl_output.mode.
    event_queue
        .roundtrip(&mut state)
        .context("roundtrip 1 for wl_output info")?;
    event_queue
        .roundtrip(&mut state)
        .context("roundtrip 2 for wl_output info")?;
    let (output, info) = state.outputs.select(output_name)?;
    let (screen_width, screen_height) = info.mode.context("wl_output did not report a mode")?;
    let pointer = if manager.version() >= 2 {
        manager.create_virtual_pointer_with_output(Some(&seat), Some(output), &qh, ())
    } else {
        eprintln!(
            "auto-scroll: zwlr_virtual_pointer_manager_v1 v1 only; pointer warps map to the whole layout"
        );
        manager.create_virtual_pointer(Some(&seat), &qh, ())
    };
    event_queue
        .roundtrip(&mut state)
        .context("roundtrip after virtual-pointer creation")?;
    Ok((event_queue, pointer, screen_width, screen_height))
}

/// Move the pointer into the selected app once so physical wheel/touchpad
/// input reaches the now-transparent capture region. No scroll is injected.
pub fn focus_underlying_once(
    stop: Arc<AtomicBool>,
    cursor_x: i32,
    cursor_y: i32,
    output_name: Option<&str>,
) -> Result<()> {
    // Validate/setup synchronously so the caller can surface a useful error;
    // the actual motion remains off GTK's main thread.
    let mut context = create_virtual_pointer(output_name)?;
    thread::spawn(move || {
        if sleep_unless_stopped(&stop, Duration::from_millis(INPUT_REGION_SETTLE_MS)) {
            focus_underlying_with_context(&mut context, cursor_x, cursor_y);
        }
        destroy_virtual_pointer_context(context);
        stop.store(true, Ordering::Release);
    });
    Ok(())
}

/// Re-hit-test whatever point the user currently chose without relocating
/// their cursor. If a relative kernel virtual mouse is unavailable, return an
/// error rather than violating the caller's no-parking preference.
pub fn refocus_under_pointer_once(stop: Arc<AtomicBool>) -> Result<()> {
    let mut device = create_uinput_mouse()
        .context("manual-scroll: relative pointer unavailable; leaving cursor in place")?;
    thread::spawn(move || {
        if sleep_unless_stopped(&stop, Duration::from_millis(UINPUT_DEVICE_SETTLE_MS))
            && let Err(error) = nudge_uinput_pointer(&mut device)
        {
            eprintln!("manual-scroll: could not refresh pointer focus: {error:#}");
        }
        stop.store(true, Ordering::Release);
    });
    Ok(())
}

fn focus_underlying_with_context(
    context: &mut VirtualPointerContext,
    cursor_x: i32,
    cursor_y: i32,
) {
    let (event_queue, pointer, screen_width, screen_height) = context;
    focus_pointer(
        pointer,
        event_queue,
        Instant::now(),
        cursor_x,
        cursor_y,
        *screen_width,
        *screen_height,
    );
}

fn destroy_virtual_pointer_context(mut context: VirtualPointerContext) {
    let (event_queue, pointer, _, _) = &mut context;
    pointer.destroy();
    let _ = event_queue.flush();
}

fn focus_pointer(
    pointer: &zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    event_queue: &mut wayland_client::EventQueue<State>,
    start: Instant,
    cursor_x: i32,
    cursor_y: i32,
    screen_width: i32,
    screen_height: i32,
) {
    let width = screen_width.max(1) as u32;
    let height = screen_height.max(1) as u32;
    let x = cursor_x.clamp(0, screen_width.max(1)) as u32;
    let y = cursor_y.clamp(0, screen_height.max(1)) as u32;

    pointer.motion_absolute(time_ms(start), x, y, width, height);
    pointer.frame();
    if let Err(error) = event_queue.flush() {
        eprintln!("auto-scroll: failed to flush pointer warp: {error}");
        return;
    }
    thread::sleep(Duration::from_millis(POINTER_NUDGE_SETTLE_MS));

    pointer.motion(time_ms(start), 1.0, 0.0);
    pointer.frame();
    if let Err(error) = event_queue.flush() {
        eprintln!("auto-scroll: failed to flush first pointer nudge: {error}");
        return;
    }
    thread::sleep(Duration::from_millis(POINTER_NUDGE_SETTLE_MS));
    pointer.motion(time_ms(start), -1.0, 0.0);
    pointer.frame();
    if let Err(error) = event_queue.flush() {
        eprintln!("auto-scroll: failed to flush return pointer nudge: {error}");
    }
}

/// Publish the initial (unscrolled) frame, then alternate capture
/// acknowledgements and scrolls. There is deliberately no timeout on capture:
/// a slow screenshot blocks scrolling rather than allowing frames to race.
fn run_capture_scroll_loop<F>(stop: &AtomicBool, handshake: &CaptureHandshake, mut inject_scroll: F)
where
    F: FnMut(u32) -> Result<()>,
{
    let mut cycle = handshake.publish_ready();
    while let Some(scroll_notches) = handshake.wait_for_capture(cycle, stop) {
        if stop.load(Ordering::Acquire) {
            break;
        }
        if let Err(error) = inject_scroll(scroll_notches) {
            eprintln!("auto-scroll: input injection failed: {error:#}");
            break;
        }
        if !sleep_unless_stopped(stop, Duration::from_millis(SCROLL_SETTLE_MS)) {
            break;
        }
        cycle = handshake.publish_ready();
    }
}

fn sleep_unless_stopped(stop: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::Acquire) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        thread::sleep(remaining.min(Duration::from_millis(ACK_POLL_MS)));
    }
    false
}

/// Arrow-key compatibility fallback when neither real-wheel backend can be
/// created. Because the overlay has already released keyboard focus, the
/// compositor can restore focus to the underlying application without a
/// synthetic pointer device.
fn spawn_worker_virtual_keyboard(
    stop: Arc<AtomicBool>,
    handshake: CaptureHandshake,
    direction: ScrollDirection,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("auto-scroll: failed to connect to Wayland")?;
    let (globals, mut event_queue) =
        registry_queue_init::<State>(&conn).context("auto-scroll: registry init")?;
    let qh = event_queue.handle();
    let keyboard_manager = globals
        .bind::<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
        .context("compositor does not expose zwp_virtual_keyboard_manager_v1")?;
    let seat = globals
        .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=8, ())
        .context("no wl_seat available")?;
    let mut state = State {
        registry_state: RegistryState::new(&globals),
        outputs: OutputTracker::default(),
    };
    let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
    let (keymap_fd, keymap_size) = make_keymap_fd()?;
    keyboard.keymap(
        wl_keyboard::KeymapFormat::XkbV1.into(),
        keymap_fd.as_fd(),
        keymap_size,
    );
    event_queue
        .roundtrip(&mut state)
        .context("roundtrip after virtual-keyboard keymap")?;

    thread::spawn(move || {
        let start = Instant::now();
        let keycode = direction.keycode();
        if sleep_unless_stopped(&stop, Duration::from_millis(INPUT_REGION_SETTLE_MS)) {
            run_capture_scroll_loop(&stop, &handshake, |scroll_notches| {
                let time = time_ms(start);
                let arrow_presses = arrow_presses_for_notches(scroll_notches);
                for index in 0..arrow_presses {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    keyboard.key(
                        time + index * 2,
                        keycode,
                        wl_keyboard::KeyState::Pressed.into(),
                    );
                    keyboard.key(
                        time + index * 2 + 1,
                        keycode,
                        wl_keyboard::KeyState::Released.into(),
                    );
                }
                event_queue.flush().map_err(anyhow::Error::from)
            });
        }
        stop.store(true, Ordering::Release);
        keyboard.destroy();
        let _ = event_queue.flush();
        eprintln!("auto-scroll: virtual-keyboard fallback exited");
    });
    Ok(())
}

fn arrow_presses_for_notches(scroll_notches: u32) -> u32 {
    // Preserve the existing five-arrow normal tick while making a one-notch
    // probe proportionally smaller. Round up so every accepted request moves.
    (ARROWS_PER_TICK * scroll_notches).div_ceil(NOTCHES_PER_TICK)
}

impl Dispatch<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        _: zwlr_virtual_pointer_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
        _: zwlr_virtual_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        _: zwp_virtual_keyboard_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
        _: zwp_virtual_keyboard_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.outputs.handle(proxy, event);
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers![];
}

delegate_registry!(State);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn capture_handshake_blocks_until_matching_acknowledgement() {
        let handshake = CaptureHandshake::new();
        let cycle = handshake.publish_ready();
        assert_eq!(cycle, 1);
        assert_eq!(handshake.ready_cycle(), 1);

        let stop = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let handshake = handshake.clone();
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                done_tx
                    .send(handshake.wait_for_capture(cycle, &stop))
                    .unwrap();
            })
        };

        assert!(
            done_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "worker advanced before capture acknowledged the ready frame"
        );
        handshake.acknowledge(cycle);
        assert_eq!(
            done_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            Some(NOTCHES_PER_TICK)
        );
        waiter.join().unwrap();
    }

    #[test]
    fn acknowledgement_cannot_skip_an_unpublished_cycle() {
        let handshake = CaptureHandshake::new();
        handshake.acknowledge(99);
        assert_eq!(
            handshake
                .acknowledgement
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .cycle,
            0
        );

        let cycle = handshake.publish_ready();
        handshake.acknowledge(99);
        assert_eq!(
            handshake
                .acknowledgement
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .cycle,
            cycle
        );
    }

    #[test]
    fn stop_releases_worker_waiting_for_capture() {
        let handshake = CaptureHandshake::new();
        let cycle = handshake.publish_ready();
        let stop = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let handshake = handshake.clone();
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                done_tx
                    .send(handshake.wait_for_capture(cycle, &stop))
                    .unwrap();
            })
        };

        stop.store(true, Ordering::Release);
        assert_eq!(
            done_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            None
        );
        waiter.join().unwrap();
    }

    #[test]
    fn requested_scroll_notches_are_clamped_and_consumed_once() {
        let handshake = CaptureHandshake::new();
        let stop = AtomicBool::new(false);

        let first_cycle = handshake.publish_ready();
        handshake.acknowledge_with_scroll_notches(first_cycle, 0);
        assert_eq!(handshake.wait_for_capture(first_cycle, &stop), Some(1));
        assert!(
            handshake
                .acknowledgement
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .consumed
        );

        let second_cycle = handshake.publish_ready();
        handshake.acknowledge_with_scroll_notches(second_cycle, u32::MAX);
        assert_eq!(
            handshake.wait_for_capture(second_cycle, &stop),
            Some(NOTCHES_PER_TICK)
        );
    }

    #[test]
    fn capture_loop_uses_one_shot_probe_then_returns_to_default_scroll() {
        let handshake = CaptureHandshake::new();
        let stop = Arc::new(AtomicBool::new(false));
        let (injected_tx, injected_rx) = mpsc::channel();
        let worker = {
            let handshake = handshake.clone();
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                run_capture_scroll_loop(&stop, &handshake, |scroll_notches| {
                    injected_tx.send(scroll_notches).unwrap();
                    Ok(())
                });
            })
        };

        wait_until_ready(&handshake, 1);
        assert!(
            injected_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "the initial frame must be captured before the first scroll"
        );

        handshake.acknowledge(1);
        assert_eq!(
            injected_rx
                .recv_timeout(Duration::from_millis(100))
                .unwrap(),
            NOTCHES_PER_TICK
        );

        wait_until_ready(&handshake, 2);
        handshake.acknowledge_with_scroll_notches(2, 1);
        assert_eq!(
            injected_rx
                .recv_timeout(Duration::from_millis(100))
                .unwrap(),
            1
        );

        wait_until_ready(&handshake, 3);
        handshake.acknowledge(3);
        assert_eq!(
            injected_rx
                .recv_timeout(Duration::from_millis(100))
                .unwrap(),
            NOTCHES_PER_TICK
        );

        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        assert!(
            injected_rx.try_recv().is_err(),
            "one injection is made per ack"
        );
    }

    #[test]
    fn capture_loop_stops_after_two_probe_scrolls_without_terminal_ack() {
        let handshake = CaptureHandshake::new();
        let stop = Arc::new(AtomicBool::new(false));
        let (injected_tx, injected_rx) = mpsc::channel();
        let worker = {
            let handshake = handshake.clone();
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                run_capture_scroll_loop(&stop, &handshake, |scroll_notches| {
                    injected_tx.send(scroll_notches).unwrap();
                    Ok(())
                });
            })
        };

        wait_until_ready(&handshake, 1);
        handshake.acknowledge(1);
        assert_eq!(
            injected_rx
                .recv_timeout(Duration::from_millis(100))
                .unwrap(),
            NOTCHES_PER_TICK
        );

        for cycle in 2..=3 {
            wait_until_ready(&handshake, cycle);
            handshake.acknowledge_with_scroll_notches(cycle, 1);
            assert_eq!(
                injected_rx
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap(),
                1
            );
        }

        // The controller has captured terminal cycle 4 and established the
        // end. It stops the worker without acknowledging that cycle, so no
        // fourth injection can occur.
        wait_until_ready(&handshake, 4);
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        assert!(
            injected_rx.try_recv().is_err(),
            "stopping on the terminal ready cycle must not inject again"
        );
    }

    fn wait_until_ready(handshake: &CaptureHandshake, expected_cycle: u64) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while handshake.ready_cycle() < expected_cycle {
            assert!(Instant::now() < deadline, "worker did not publish cycle");
            thread::sleep(Duration::from_millis(ACK_POLL_MS));
        }
    }

    #[test]
    fn direction_maps_to_the_expected_wheel_axis() {
        assert!(matches!(
            ScrollDirection::Down.axis(),
            wl_pointer::Axis::VerticalScroll
        ));
        assert!(matches!(
            ScrollDirection::Right.axis(),
            wl_pointer::Axis::HorizontalScroll
        ));
    }

    #[test]
    fn uinput_device_advertises_pointer_classification_capabilities() {
        let axes = uinput_mouse_axes();
        assert!(axes.contains(RelativeAxisCode::REL_X));
        assert!(axes.contains(RelativeAxisCode::REL_Y));
        assert!(axes.contains(RelativeAxisCode::REL_WHEEL));
        assert!(axes.contains(RelativeAxisCode::REL_HWHEEL));
        assert!(uinput_mouse_buttons().contains(KeyCode::BTN_LEFT));
    }

    #[test]
    fn uinput_wheel_direction_respects_natural_scroll() {
        assert_eq!(
            uinput_scroll_event(ScrollDirection::Down, 3, false),
            (RelativeAxisCode::REL_WHEEL, -3)
        );
        assert_eq!(
            uinput_scroll_event(ScrollDirection::Down, 3, true),
            (RelativeAxisCode::REL_WHEEL, 3)
        );
        assert_eq!(
            uinput_scroll_event(ScrollDirection::Right, 2, false),
            (RelativeAxisCode::REL_HWHEEL, 2)
        );
        assert_eq!(
            uinput_scroll_event(ScrollDirection::Right, 2, true),
            (RelativeAxisCode::REL_HWHEEL, -2)
        );
    }

    #[test]
    fn parses_hyprland_natural_scroll_setting() {
        assert_eq!(
            parse_hyprland_bool(r#"{"option":"input:natural_scroll","bool":true,"set":true}"#),
            Some(true)
        );
        assert_eq!(
            parse_hyprland_bool(r#"{"option":"input:natural_scroll","bool":false,"set":true}"#),
            Some(false)
        );
        assert_eq!(parse_hyprland_bool("not json"), None);
    }
}
