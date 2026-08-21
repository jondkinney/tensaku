//! Per-frame profiler for the on-screen render path, gated behind the
//! `TENSAKU_PERF` environment variable.
//!
//! Motivation: the canvas cost of a frame is almost entirely
//! proportional to the *framebuffer pixel count*, which varies wildly
//! between machines (a 6K panel at 2× scale renders ~20 M canvas
//! pixels — 10× a 1080p window). A report of "it feels laggy" from a
//! display we don't have is only actionable with per-phase numbers
//! from that display, so this prints them.
//!
//! Modes:
//!
//! * `TENSAKU_PERF=1` — CPU time spent issuing each phase's draw
//!   calls, plus the wall-clock period between frames. GL is
//!   asynchronous, so the phase numbers here only cover command
//!   submission; the frame *period* is what reveals a GPU-bound
//!   canvas (period high, CPU low).
//! * `TENSAKU_PERF=gpu` — additionally `glFinish()` at every phase
//!   boundary, which drains the pipeline so each phase's GPU cost is
//!   attributed to that phase. This makes the app slower overall
//!   (the CPU stalls on every boundary) but it's the only way to see
//!   *which* pass is eating the frame.
//!
//! Caveat on reading the phases: femtovg batches draw calls and only
//! submits them at `Canvas::flush`, so passes that don't flush
//! themselves report near-zero and their GPU cost lands in `flush`.
//! Passes that DO flush internally — the spotlight overlay, a Blur
//! drawable's re-sample — are attributed correctly. The `extras`
//! (`bg-upload`, `blur-resample`, `canvas-grow`, `input-drag`, …) are
//! the numbers to read first.
//!
//! Extra probes, all requiring `TENSAKU_PERF`:
//!
//! * `TENSAKU_PERF_SPIN=1` — render continuously instead of on demand.
//! * `TENSAKU_PERF_EVERY=N` — report every N frames (default 30).
//! * `TENSAKU_PERF_READBACK=1` — one whole-framebuffer readback and
//!   one region readback per frame, to price the two shapes of
//!   `glReadPixels` against each other.
//!
//! Output is one averaged line every `REPORT_EVERY` frames on stderr.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::Instant;

use glow::HasContext;

/// Default frames to average before printing a line. High enough
/// that the log doesn't flood during a drag, low enough that a
/// stutter shows up while the user is still doing the thing that
/// caused it.
const REPORT_EVERY_DEFAULT: u32 = 30;

/// Frames per report, overridable with `TENSAKU_PERF_EVERY` (set it
/// to 1 to see every frame while chasing a stutter).
fn report_every() -> u32 {
    static N: OnceLock<u32> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("TENSAKU_PERF_EVERY")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(REPORT_EVERY_DEFAULT)
    })
}

/// Phases we time, in render order. Kept as a fixed list so the
/// accumulator is a plain array and `mark` is an index bump.
const PHASES: [&str; 5] = ["shadow", "background", "spotlight", "drawables", "flush"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Cpu,
    Gpu,
}

fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("TENSAKU_PERF").as_deref() {
        Ok("gpu") => Mode::Gpu,
        Ok("0") | Err(_) => Mode::Off,
        Ok(_) => Mode::Cpu,
    })
}

/// True when any profiling mode is active. Call sites use this to
/// skip work that only exists to feed the report.
pub fn enabled() -> bool {
    mode() != Mode::Off
}

/// `TENSAKU_PERF_SPIN=1` re-queues a render at the end of every
/// frame, turning the canvas into a free-running animation so
/// steady-state frame cost can be measured without a human driving
/// the mouse. Only meaningful together with `TENSAKU_PERF`.
pub fn spin() -> bool {
    static SPIN: OnceLock<bool> = OnceLock::new();
    *SPIN.get_or_init(|| enabled() && std::env::var("TENSAKU_PERF_SPIN").is_ok_and(|v| v != "0"))
}

/// Optional per-frame extras recorded by `extra`, keyed by a static
/// label. Used for costs that aren't a fixed render phase — e.g. the
/// full-framebuffer readback a Blur drawable does when its cache is
/// invalidated.
type Extras = std::collections::BTreeMap<&'static str, (f64, u32)>;

struct State {
    /// Elapsed ms accumulated per phase across the current window.
    totals: [f64; PHASES.len()],
    /// Sum of frame periods (wall clock between `begin_frame` calls).
    period_total: f64,
    /// Sum of whole-frame render times.
    frame_total: f64,
    frames: u32,
    /// Start of the phase currently being timed.
    phase_start: Option<Instant>,
    /// Index of the next phase to record.
    phase_idx: usize,
    /// Start of the current frame's render.
    frame_start: Option<Instant>,
    /// `begin_frame` timestamp of the previous frame.
    last_frame: Option<Instant>,
    extras: Extras,
}

impl State {
    const fn new() -> Self {
        Self {
            totals: [0.0; PHASES.len()],
            period_total: 0.0,
            frame_total: 0.0,
            frames: 0,
            phase_start: None,
            phase_idx: 0,
            frame_start: None,
            last_frame: None,
            extras: Extras::new(),
        }
    }
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State::new()) };
    /// Cached glow context used only for the `gpu` mode's fences.
    /// Built lazily on the GTK main thread while the GLArea's context
    /// is current, so it's safe to reuse for the process lifetime.
    static GL: RefCell<Option<glow::Context>> = const { RefCell::new(None) };
}

/// Drain the GL pipeline so the time attributed to the phase that
/// just ended actually includes its GPU work. No-op outside `gpu`
/// mode.
fn fence() {
    if mode() != Mode::Gpu {
        return;
    }
    GL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            // SAFETY: the loader is only called while the GLArea's
            // context is current (every call site sits inside
            // `GLAreaImpl::render`).
            let ctx = unsafe {
                glow::Context::from_loader_function(|s| epoxy::get_proc_addr(s) as *const _)
            };
            *slot = Some(ctx);
        }
        if let Some(ctx) = slot.as_ref() {
            unsafe { ctx.finish() };
        }
    });
}

/// Start timing a frame. Safe to call unconditionally — returns
/// immediately when profiling is off.
pub fn begin_frame() {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(prev) = s.last_frame {
            s.period_total += (now - prev).as_secs_f64() * 1000.0;
        }
        s.last_frame = Some(now);
        s.frame_start = Some(now);
        s.phase_start = Some(now);
        s.phase_idx = 0;
    });
}

/// Close out the phase that just finished. Phases are recorded in the
/// fixed `PHASES` order; extra calls past the end are ignored so a
/// render path that skips a pass can't corrupt the report.
pub fn mark() {
    if !enabled() {
        return;
    }
    fence();
    let now = Instant::now();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let idx = s.phase_idx;
        if idx < PHASES.len()
            && let Some(start) = s.phase_start
        {
            s.totals[idx] += (now - start).as_secs_f64() * 1000.0;
            s.phase_idx = idx + 1;
        }
        s.phase_start = Some(now);
    });
}

/// `TENSAKU_PERF_READBACK=1` performs a whole-framebuffer
/// `Canvas::screenshot` and a region read per frame, timed as
/// `readback-full` and `readback-region`. Both are synchronous
/// `glReadPixels` calls; the first is what the Blur tool used to do on
/// every frame of a drag, the second is what it does now. Measurable
/// without driving the mouse.
pub fn readback_probe() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| enabled() && std::env::var("TENSAKU_PERF_READBACK").is_ok_and(|v| v != "0"))
}

/// Record an off-phase cost under `label`. The report prints its
/// mean per occurrence plus how many times it fired in the window,
/// so a cost that only happens on some frames (a blur re-sample, a
/// spotlight rebuild) is still legible.
pub fn extra(label: &'static str, ms: f64) {
    if !enabled() {
        return;
    }
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let e = s.extras.entry(label).or_insert((0.0, 0));
        e.0 += ms;
        e.1 += 1;
    });
}

/// RAII timer: records elapsed time under `label` when dropped.
/// Use where the timed region has several exit points (the sketch
/// board's `update` returns early from a dozen branches).
pub struct Guard {
    label: &'static str,
    start: Instant,
}

impl Drop for Guard {
    fn drop(&mut self) {
        extra(
            self.label,
            (Instant::now() - self.start).as_secs_f64() * 1000.0,
        );
    }
}

/// Start an RAII timer, or `None` when profiling is off.
pub fn guard(label: &'static str) -> Option<Guard> {
    enabled().then(|| Guard {
        label,
        start: Instant::now(),
    })
}

/// Time `f`, recording its duration under `label`. Includes a GL
/// fence in `gpu` mode so GPU-side work inside `f` is attributed to
/// it rather than leaking into a later phase.
pub fn timed<T>(label: &'static str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let start = Instant::now();
    let out = f();
    fence();
    extra(label, (Instant::now() - start).as_secs_f64() * 1000.0);
    out
}

/// Finish the frame and, every `REPORT_EVERY` frames, print the
/// averaged breakdown. `canvas_w`/`canvas_h` are framebuffer (device)
/// pixels — the number the cost actually scales with — and
/// `drawables` is how many annotations were in the stack, so a report
/// can be read against "does it get worse with more shapes?".
pub fn end_frame(canvas_w: u32, canvas_h: u32, dpr: f32, drawables: usize) {
    if !enabled() {
        return;
    }
    fence();
    let now = Instant::now();
    let report = STATE.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(start) = s.frame_start {
            s.frame_total += (now - start).as_secs_f64() * 1000.0;
        }
        s.frames += 1;
        if s.frames < report_every() {
            return None;
        }
        let n = s.frames as f64;
        let phases: Vec<String> = PHASES
            .iter()
            .zip(s.totals.iter())
            .map(|(name, total)| format!("{name} {:.2}", total / n))
            .collect();
        let extras: String = s
            .extras
            .iter()
            .map(|(label, (total, hits))| {
                format!(" | {label} {:.2} ms x{hits}", total / *hits as f64)
            })
            .collect();
        let line = format!(
            "tensaku perf: {}x{} fb ({:.2} MPx) dpr {dpr} | frame {:.2} ms, period {:.2} ms \
             ({:.0} fps) | {} | drawables {drawables}{extras}",
            canvas_w,
            canvas_h,
            (canvas_w as f64 * canvas_h as f64) / 1.0e6,
            s.frame_total / n,
            s.period_total / n,
            if s.period_total > 0.0 {
                1000.0 / (s.period_total / n)
            } else {
                0.0
            },
            phases.join(", "),
        );
        *s = State::new();
        Some(line)
    });
    if let Some(line) = report {
        eprintln!("{line}");
    }
}
