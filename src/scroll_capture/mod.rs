use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::gdk_pixbuf::Pixbuf;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::capture;

pub mod auto_scroll;
mod stitch;

const BACKDROP_ALPHA: f64 = 0.55;
const BRACKET_LEN: f64 = 22.0;
const BRACKET_WIDTH: f64 = 3.0;
const PILL_GAP: f64 = 18.0;
const MIN_SELECTION: f64 = 8.0;
const CAPTURE_INTERVAL_MS: u64 = 100;
/// Automatic capture spends most timer turns waiting for the worker's ready
/// cycle, which is just an atomic read. Poll more promptly so the fixed render
/// settle is not followed by up to another 100ms of idle time.
const AUTO_CAPTURE_POLL_MS: u64 = 20;
/// Let one fully-rendered GTK frame reach the Wayland compositor before the
/// first screencopy request. Without this barrier, capture can race the frame
/// that hides the in-selection controls and punches the selection transparent.
const OVERLAY_COMMIT_SETTLE_MS: u64 = 34;
/// After the manual pointer park has completed, leave the underlying client
/// enough time to repaint hover-driven UI before preserving the first frame.
const MANUAL_POINTER_HOVER_SETTLE_MS: u64 = 150;
/// Poll the one-shot pointer worker promptly without busy-spinning GTK's main
/// loop. The worker's stop flag is also its completion signal.
const POINTER_FOCUS_POLL_MS: u64 = 20;
const MAX_CONSECUTIVE_CAPTURE_ERRORS: u32 = 3;
/// A first frame that is one solid color is, in practice, screencopy racing
/// the overlay: the transparent-hole frame hasn't reached the compositor yet
/// (slow first paint, compositor fade-in animation), so the copy returns the
/// overlay's own backdrop instead of the app below it. Those frames must
/// never seed the stitcher — retry the same cycle instead. Genuinely solid
/// selections halt with a clear message after this many rejects.
const MAX_CONSECUTIVE_BLANK_FRAMES: u32 = 8;
const MANUAL_STALL_DELAY: Duration = Duration::from_secs(2);
/// A small, low-error manual match is safe to hold as a pending frame even
/// when repeated content makes its runner-up slightly too competitive for the
/// general matcher. Requiring some separation still rejects exact ties.
const MANUAL_AMBIGUOUS_MAX_ERROR: f64 = 2.0;
const MANUAL_AMBIGUOUS_MIN_CONFIDENCE: f64 = 1.02;
const DRAG_THRESHOLD: f64 = 4.0;
/// Match ordinary end detection: one stationary probe may be a delayed paint
/// or swallowed wheel event, while two in a row reliably indicate the page
/// cannot advance any farther.
const AUTO_END_CONFIRMATION_PROBES: u8 = 2;
/// Keep a short history of fully verified normal (three-notch) auto-scroll
/// steps. At a confirmed endpoint the final partial movement cannot exceed a
/// stable full step, which lets us reject distant visual aliases in genuinely
/// repeated content without weakening the image matcher globally.
const AUTO_STEP_CALIBRATION_WINDOW: usize = 5;
const AUTO_STEP_CALIBRATION_MIN_SAMPLES: usize = 3;
const AUTO_STEP_CALIBRATION_MIN_JITTER: usize = 4;
const AUTO_STEP_CALIBRATION_JITTER_PERCENT: usize = 5;
const AUTO_ALIGNMENT_PAUSE_MESSAGE: &str = "Auto-scroll paused — capture lost alignment";
const CONTINUE_MANUALLY_TITLE: &str = "Continue manually";
const CONTINUE_ANYWAY_TITLE: &str = "Continue anyway";
const FINISH_HERE_TITLE: &str = "Finish here";

/// Length of the L-shaped corner brackets (logical pixels). Matches
/// satty crop tool's BRACKET_LENGTH.
const CROP_BRACKET_LENGTH: f64 = 28.0;

/// Length of the parallel "fat bar" edge handle (logical pixels). Matches
/// satty crop tool's EDGE_HANDLE_LENGTH.
const EDGE_HANDLE_LENGTH: f64 = 36.0;

/// Stroke width for corner brackets and edge bars (logical pixels).
/// Matches satty crop tool's HANDLE_STROKE_WIDTH.
const CROP_STROKE_WIDTH: f64 = 5.0;

/// Radius of the central Move handle.
const MOVE_HANDLE_RADIUS: f64 = 18.0;

/// Minimum size the selection can be resized to. Prevents the rect from
/// flipping inside-out during a drag.
const MIN_SELECTION_SIZE: f64 = 32.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResizeHandle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    /// Drag from the center of the selection to move the whole rect
    /// without resizing.
    Move,
}

impl ResizeHandle {
    /// Center of this handle for a given selection. Resize handles sit ON
    /// the selection edges (matching the crop tool); Move sits at the
    /// selection's center.
    fn center(self, sel: Selection) -> (f64, f64) {
        match self {
            ResizeHandle::TopLeft => (sel.x, sel.y),
            ResizeHandle::Top => (sel.x + sel.w / 2.0, sel.y),
            ResizeHandle::TopRight => (sel.x + sel.w, sel.y),
            ResizeHandle::Right => (sel.x + sel.w, sel.y + sel.h / 2.0),
            ResizeHandle::BottomRight => (sel.x + sel.w, sel.y + sel.h),
            ResizeHandle::Bottom => (sel.x + sel.w / 2.0, sel.y + sel.h),
            ResizeHandle::BottomLeft => (sel.x, sel.y + sel.h),
            ResizeHandle::Left => (sel.x, sel.y + sel.h / 2.0),
            ResizeHandle::Move => (sel.x + sel.w / 2.0, sel.y + sel.h / 2.0),
        }
    }

    /// Compute the new selection when this handle has been dragged. For
    /// resize handles, `mouse_x/y` is the new position of the dragged edge
    /// or corner. For Move, the entire rect is translated by the delta
    /// between `drag_origin` and `(mouse_x, mouse_y)`.
    fn apply(
        self,
        anchor: Selection,
        drag_origin: (f64, f64),
        mouse_x: f64,
        mouse_y: f64,
    ) -> Selection {
        if matches!(self, ResizeHandle::Move) {
            let dx = mouse_x - drag_origin.0;
            let dy = mouse_y - drag_origin.1;
            return Selection {
                x: anchor.x + dx,
                y: anchor.y + dy,
                w: anchor.w,
                h: anchor.h,
            };
        }
        let right = anchor.x + anchor.w;
        let bottom = anchor.y + anchor.h;
        let (x1, y1, x2, y2) = match self {
            ResizeHandle::TopLeft => (mouse_x, mouse_y, right, bottom),
            ResizeHandle::Top => (anchor.x, mouse_y, right, bottom),
            ResizeHandle::TopRight => (anchor.x, mouse_y, mouse_x, bottom),
            ResizeHandle::Right => (anchor.x, anchor.y, mouse_x, bottom),
            ResizeHandle::BottomRight => (anchor.x, anchor.y, mouse_x, mouse_y),
            ResizeHandle::Bottom => (anchor.x, anchor.y, right, mouse_y),
            ResizeHandle::BottomLeft => (mouse_x, anchor.y, right, mouse_y),
            ResizeHandle::Left => (mouse_x, anchor.y, right, bottom),
            ResizeHandle::Move => unreachable!(),
        };
        let lx = x1.min(x2);
        let ly = y1.min(y2);
        let w = (x2 - x1).abs().max(MIN_SELECTION_SIZE);
        let h = (y2 - y1).abs().max(MIN_SELECTION_SIZE);
        Selection { x: lx, y: ly, w, h }
    }
}

/// Half-thickness of the resize hit band around each edge of the
/// selection. Anywhere within this distance of an edge counts as
/// grabbing that edge.
const EDGE_HIT_SLACK: f64 = 12.0;

/// Distance from a corner anchor within which the hit prefers the
/// corner (diagonal resize) over an adjacent edge. Larger than
/// EDGE_HIT_SLACK so corners are easy to grab.
const CORNER_HIT_RADIUS: f64 = 20.0;

/// Selected phase: is the pointer over the page exposed inside the region —
/// strictly inside the rectangle and not on a resize/move handle? While it is,
/// the overlay releases its keyboard grab so wheel and PageUp/Home reach the
/// underlying app (lining the content up before capture); everywhere else the
/// overlay keeps an exclusive grab so Esc, the restore key, the handles and
/// the mode buttons work.
fn pointer_over_selected_page(sel: Selection, x: f64, y: f64) -> bool {
    let inside = x > sel.x + EDGE_HIT_SLACK
        && y > sel.y + EDGE_HIT_SLACK
        && x < sel.x + sel.w - EDGE_HIT_SLACK
        && y < sel.y + sel.h - EDGE_HIT_SLACK;
    inside && hit_test_handle(sel, x, y).is_none()
}

fn hit_test_handle(sel: Selection, x: f64, y: f64) -> Option<ResizeHandle> {
    // 1) Corners win if you're near one (so you get diagonal resize even
    // though the edge bands overlap there).
    for h in [
        ResizeHandle::TopLeft,
        ResizeHandle::TopRight,
        ResizeHandle::BottomRight,
        ResizeHandle::BottomLeft,
    ] {
        let (cx, cy) = h.center(sel);
        let r = CORNER_HIT_RADIUS;
        if (x - cx).powi(2) + (y - cy).powi(2) <= r * r {
            return Some(h);
        }
    }

    // 2) Move handle in the center (only inside the selection rect to
    // avoid overlapping the edge hit zones when the selection is small).
    let (mcx, mcy) = ResizeHandle::Move.center(sel);
    let mr = MOVE_HANDLE_RADIUS + 3.0;
    if (x - mcx).powi(2) + (y - mcy).powi(2) <= mr * mr {
        return Some(ResizeHandle::Move);
    }

    // 3) Edges: anywhere along an edge (between corners) within
    // EDGE_HIT_SLACK perpendicular distance grabs that edge.
    let within_x = x >= sel.x - EDGE_HIT_SLACK && x <= sel.x + sel.w + EDGE_HIT_SLACK;
    let within_y = y >= sel.y - EDGE_HIT_SLACK && y <= sel.y + sel.h + EDGE_HIT_SLACK;
    if within_x && (y - sel.y).abs() <= EDGE_HIT_SLACK {
        return Some(ResizeHandle::Top);
    }
    if within_x && (y - (sel.y + sel.h)).abs() <= EDGE_HIT_SLACK {
        return Some(ResizeHandle::Bottom);
    }
    if within_y && (x - sel.x).abs() <= EDGE_HIT_SLACK {
        return Some(ResizeHandle::Left);
    }
    if within_y && (x - (sel.x + sel.w)).abs() <= EDGE_HIT_SLACK {
        return Some(ResizeHandle::Right);
    }
    None
}

#[derive(Default, Clone, Copy, Debug, PartialEq)]
struct Selection {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Selection {
    fn is_valid(&self) -> bool {
        self.w >= MIN_SELECTION && self.h >= MIN_SELECTION
    }
}

/// Physical-screen target shared by manual pointer parking and auto-scroll.
/// Keeping it inside the selection preserves scroll targeting while favoring
/// the lower-right page/scrollbar gutter, where hover UI is least likely.
fn pointer_park_target(selection: Selection, scale: i32) -> (i32, i32) {
    let scale = scale.max(1);
    let x = (selection.x + selection.w - 30.0).max(selection.x + 1.0) as i32;
    let y = (selection.y + selection.h - 60.0).max(selection.y + 1.0) as i32;
    (x * scale, y * scale)
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Phase {
    AwaitingDrag,
    Dragging,
    Selected,
    Capturing,
}

#[derive(Clone, Copy, Debug)]
enum CaptureMode {
    Manual(stitch::StitchAxis),
    Auto(auto_scroll::ScrollDirection),
}

impl CaptureMode {
    fn axis(self) -> stitch::StitchAxis {
        match self {
            CaptureMode::Manual(axis) => axis,
            CaptureMode::Auto(auto_scroll::ScrollDirection::Down) => stitch::StitchAxis::Vertical,
            CaptureMode::Auto(auto_scroll::ScrollDirection::Right) => {
                stitch::StitchAxis::Horizontal
            }
        }
    }

    fn is_manual(self) -> bool {
        matches!(self, Self::Manual(_))
    }
}

fn manual_mode_preserving_axis(mode: CaptureMode) -> CaptureMode {
    CaptureMode::Manual(mode.axis())
}

struct CaptureBuffer {
    /// Incremental output storage. Later source frames are reduced to their
    /// newly exposed strip instead of being retained as full screenshots.
    stitch: stitch::StitchAccumulator,
    /// Downsampled grayscale used to measure motion against the next frame.
    last_gray: stitch::GrayView,
    /// Manual captures may begin anywhere in a scrollable area. The first
    /// accepted movement locks whether the user is revealing content below
    /// or above the initial viewport; changing direction after a retained
    /// band would revisit pixels already represented in the output.
    manual_direction: Option<ManualDirection>,
    /// Latest small manual movement, kept as one replaceable full frame until
    /// enough progress accumulates to justify another retained stitch band.
    pending_manual: Option<PendingManualFrame>,
}

struct PendingManualFrame {
    pixbuf: Pixbuf,
    delta: usize,
    direction: ManualDirection,
}

#[derive(Clone)]
enum AutoAlignmentState {
    /// The first uncertain frame is deliberately not committed. The worker
    /// is asked for smaller confirmation scrolls so later frames can either
    /// disambiguate the seam or reliably establish the end of the page.
    Probing {
        lookahead: stitch::ForwardLookahead,
        first_pixbuf: Pixbuf,
        first_gray: stitch::GrayView,
        stationary_probes: u8,
    },
    /// Automatic lookahead still found more than one plausible path. The
    /// worker remains blocked on `cycle` until the user chooses how to proceed.
    Paused {
        lookahead: stitch::ForwardLookahead,
        first_pixbuf: Pixbuf,
        first_gray: stitch::GrayView,
        second_pixbuf: Pixbuf,
        second_gray: stitch::GrayView,
        best_effort: Option<AutoBestEffort>,
        reason: AutoProbePauseReason,
        cycle: u64,
    },
    /// The user chose to continue manually. Keep the original confirmed
    /// baseline and first uncertain frame until later manual scrolling exposes
    /// enough unique pixels to resolve the entire gap without guessing.
    ManualRecovery {
        lookahead: stitch::ForwardLookahead,
        first_pixbuf: Pixbuf,
        first_gray: stitch::GrayView,
        automatic_mode: CaptureMode,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum AutoBestEffort {
    TwoFrames(stitch::ForwardMatchPath),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoProbePauseReason {
    StillAmbiguous,
    Stationary,
}

fn accepting_auto_pause_reaches_end(reason: AutoProbePauseReason) -> bool {
    reason == AutoProbePauseReason::Stationary
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum AutoProbeDecision {
    Commit {
        path: stitch::ForwardMatchPath,
        periodic: bool,
    },
    ProbeAgain,
    End(stitch::ForwardMatchCandidate),
    Pause {
        reason: AutoProbePauseReason,
        best_effort: Option<AutoBestEffort>,
    },
}

#[derive(Clone, Debug, Default)]
struct AutoStepCalibration {
    recent: Vec<usize>,
}

impl AutoStepCalibration {
    fn record_verified_normal_step(&mut self, delta: usize) {
        if delta == 0 {
            return;
        }
        self.recent.push(delta);
        if self.recent.len() > AUTO_STEP_CALIBRATION_WINDOW {
            self.recent.remove(0);
        }
    }

    /// Return a conservative physical upper bound only after several normal
    /// scroll steps agree closely. Unstable samples deliberately yield no
    /// bound, preserving the explicit pause instead of guessing.
    fn endpoint_upper_bound(&self) -> Option<usize> {
        if self.recent.len() < AUTO_STEP_CALIBRATION_MIN_SAMPLES {
            return None;
        }
        let mut sorted = self.recent.clone();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2];
        let percentage_jitter = median
            .saturating_mul(AUTO_STEP_CALIBRATION_JITTER_PERCENT)
            .saturating_add(99)
            / 100;
        let jitter = percentage_jitter.max(AUTO_STEP_CALIBRATION_MIN_JITTER);
        let min = *sorted.first()?;
        let max = *sorted.last()?;
        if max.saturating_sub(min) > jitter {
            return None;
        }
        max.checked_add(jitter)
    }
}

fn auto_probe_decision(
    resolution: stitch::ForwardLookaheadResolution,
    prior_stationary_probes: u8,
    calibrated_end_candidate: Option<stitch::ForwardMatchCandidate>,
) -> AutoProbeDecision {
    match resolution {
        stitch::ForwardLookaheadResolution::Resolved(path) => AutoProbeDecision::Commit {
            path,
            periodic: false,
        },
        stitch::ForwardLookaheadResolution::LowErrorPeriodic(path) => AutoProbeDecision::Commit {
            path,
            periodic: true,
        },
        stitch::ForwardLookaheadResolution::Unresolved { best_effort } => {
            AutoProbeDecision::Pause {
                reason: AutoProbePauseReason::StillAmbiguous,
                best_effort: best_effort.map(AutoBestEffort::TwoFrames),
            }
        }
        stitch::ForwardLookaheadResolution::StationaryProbe { first_match } => {
            if prior_stationary_probes.saturating_add(1) < AUTO_END_CONFIRMATION_PROBES {
                return AutoProbeDecision::ProbeAgain;
            }
            match first_match {
                stitch::StationaryProbeFirstMatch::Unique(candidate) => {
                    AutoProbeDecision::End(candidate)
                }
                stitch::StationaryProbeFirstMatch::Ambiguous { best_effort } => {
                    if let Some(candidate) = calibrated_end_candidate {
                        AutoProbeDecision::End(candidate)
                    } else if let Some(candidate) = best_effort {
                        // Two stationary probes prove that automatic motion
                        // has reached the endpoint. Repeated pixels may leave
                        // more than one visually valid F0→F1 offset, but that
                        // is not a capture failure: use the matcher-ranked
                        // forward seam and finish without interrupting the
                        // user merely because the content repeats.
                        AutoProbeDecision::End(candidate)
                    } else {
                        AutoProbeDecision::Pause {
                            reason: AutoProbePauseReason::Stationary,
                            best_effort: None,
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManualHandoffPointerPolicy {
    ParkInSelection,
    LeaveUnchanged,
}

fn manual_handoff_pointer_policy(park_manual_pointer: bool) -> ManualHandoffPointerPolicy {
    if park_manual_pointer {
        ManualHandoffPointerPolicy::ParkInSelection
    } else {
        ManualHandoffPointerPolicy::LeaveUnchanged
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoCaptureAcknowledgement {
    Normal,
    Probe,
    Hold,
}

enum CaptureAnalysis {
    Initial,
    Motion(stitch::MotionEstimate),
    BeginAutoProbe(stitch::ForwardLookahead),
    ResolveAutoProbe(stitch::ForwardLookaheadResolution),
    ResolveManualRecovery(stitch::ForwardLookaheadResolution),
}

#[derive(Clone)]
struct CapturingControls {
    pill: gtk::Box,
    status: gtk::Label,
    cancel: gtk::Button,
    done: gtk::Button,
    continue_manual: gtk::Button,
    continue_anyway: gtk::Button,
    finish_here: gtk::Button,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManualDirection {
    Forward,
    Reverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManualAmbiguousAction {
    Accept(ManualDirection, usize),
    KeepScrolling,
    ReturnTo(ManualDirection),
}

impl ManualAmbiguousAction {
    fn status(self) -> Option<&'static str> {
        match self {
            Self::Accept(_, _) => None,
            Self::KeepScrolling => Some("Repeated content — keep scrolling to confirm"),
            Self::ReturnTo(ManualDirection::Forward) => {
                Some("Content moved backward — scroll down to resume")
            }
            Self::ReturnTo(ManualDirection::Reverse) => {
                Some("Content moved forward — scroll up to resume")
            }
        }
    }
}

impl ManualDirection {
    fn from_signed_delta(delta: isize) -> Option<(Self, usize)> {
        match delta.cmp(&0) {
            std::cmp::Ordering::Greater => Some((Self::Forward, delta as usize)),
            std::cmp::Ordering::Less => Some((Self::Reverse, delta.unsigned_abs())),
            std::cmp::Ordering::Equal => None,
        }
    }

    fn signed_label(self, delta: usize) -> String {
        match self {
            Self::Forward => format!("+{delta}"),
            Self::Reverse => format!("-{delta}"),
        }
    }
}

fn manual_ambiguous_action(
    candidate: isize,
    error: f64,
    confidence: f64,
    pending_threshold: usize,
) -> ManualAmbiguousAction {
    let Some((direction, delta)) = ManualDirection::from_signed_delta(candidate) else {
        return ManualAmbiguousAction::KeepScrolling;
    };
    // Ambiguous offsets are retained only as replaceable pending frames. A
    // candidate at or above the coalescing threshold would be committed as a
    // permanent band despite its weak peak separation.
    if delta < pending_threshold
        && error.is_finite()
        && error <= MANUAL_AMBIGUOUS_MAX_ERROR
        && confidence >= MANUAL_AMBIGUOUS_MIN_CONFIDENCE
    {
        ManualAmbiguousAction::Accept(direction, delta)
    } else {
        ManualAmbiguousAction::KeepScrolling
    }
}

fn manual_motion_search_bound(frame: &Pixbuf, axis: stitch::StitchAxis) -> usize {
    let axis_len = match axis {
        stitch::StitchAxis::Vertical => frame.height(),
        stitch::StitchAxis::Horizontal => frame.width(),
    }
    .max(1) as usize;
    (axis_len / 8)
        .clamp(128, 512)
        .min(axis_len.saturating_sub(1).max(1))
}

fn recover_bounded_manual_motion(
    strict: stitch::MotionEstimate,
    bounded: stitch::MotionEstimate,
    max_delta: usize,
) -> stitch::MotionEstimate {
    let signed_delta = match bounded.motion {
        stitch::Motion::Forward(delta) => Some(delta as isize),
        stitch::Motion::Reverse(delta) => Some(-(delta as isize)),
        stitch::Motion::Ambiguous(delta) => Some(delta),
        stitch::Motion::Stationary | stitch::Motion::Unmatchable => None,
    };
    let strong_nearby_match = signed_delta.is_some_and(|delta| {
        delta != 0
            && delta.unsigned_abs() <= max_delta
            && bounded.error.is_finite()
            && bounded.error <= MANUAL_AMBIGUOUS_MAX_ERROR
            && bounded.confidence >= MANUAL_AMBIGUOUS_MIN_CONFIDENCE
    });
    if strong_nearby_match { bounded } else { strict }
}

#[derive(Default)]
struct ManualStallDetector {
    armed: bool,
    still_since: Option<Instant>,
    cue_visible: bool,
}

impl ManualStallDetector {
    /// Record measurable progress. Returns whether a previous stall cue should
    /// be cleared from the UI.
    fn movement(&mut self) -> bool {
        let clear_cue = self.cue_visible;
        self.armed = true;
        self.still_since = None;
        self.cue_visible = false;
        clear_cue
    }

    /// Record an unchanged visual sample. Returns true exactly once after a
    /// sufficiently long stall, and never before real movement has armed it.
    fn still(&mut self, now: Instant) -> bool {
        if !self.armed {
            return false;
        }
        let since = self.still_since.get_or_insert(now);
        if now.saturating_duration_since(*since) < MANUAL_STALL_DELAY {
            return false;
        }
        self.armed = false;
        self.still_since = None;
        self.cue_visible = true;
        true
    }

    fn interrupt(&mut self) {
        self.still_since = None;
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

struct OverlayState {
    phase: Phase,
    drag_origin: (f64, f64),
    drag_active: bool,
    selection: Selection,
    resize_handle: Option<ResizeHandle>,
    resize_anchor: Selection,
    capture: Option<CaptureBuffer>,
    capture_timer: Option<glib::SourceId>,
    /// Monotonic identity for the active capture. Deferred paint/timer
    /// callbacks use this to avoid arming a stale selection after cancellation.
    capture_epoch: u64,
    capture_mode: Option<CaptureMode>,
    /// Whether a manual capture should relocate the pointer before frame one.
    /// Snapshotted when the standalone scroll-capture overlay is launched.
    park_manual_pointer: bool,
    /// Selected phase: the keyboard grab is currently released because the
    /// pointer is over the page inside the region (see
    /// `pointer_over_selected_page`).
    selected_keyboard_released: bool,
    /// Connector name of the monitor the overlay covers (e.g. `DP-3`). Every
    /// screencopy request and virtual-pointer warp targets this output so a
    /// second monitor never changes where frames come from or where the
    /// pointer lands. `None` = compositor default / first output.
    output_name: Option<String>,
    auto_scroll_stop: Option<Arc<AtomicBool>>,
    pointer_focus_stop: Option<Arc<AtomicBool>>,
    /// Lower-right focus target for the active manual capture. When parking is
    /// enabled it is consumed before frame one; otherwise it remains as the
    /// marker that triggers an in-place pointer refocus after frame one.
    manual_pointer_target: Option<(i32, i32)>,
    auto_scroll_monitor: Option<glib::SourceId>,
    auto_scroll_handshake: Option<auto_scroll::CaptureHandshake>,
    auto_alignment: Option<AutoAlignmentState>,
    auto_step_calibration: AutoStepCalibration,
    /// Set only after the user explicitly chooses Continue anyway. The editor
    /// surfaces a warning because that seam was not uniquely verifiable.
    unverified_auto_seams: u32,
    /// Last stable auto-scroll cycle that has been captured and acknowledged.
    last_captured_cycle: u64,
    /// Two stationary auto cycles mean the scroll target reached its end.
    consecutive_no_scroll: u32,
    /// Once motion becomes unsafe to stitch, stop sampling and leave the
    /// partial result available explicitly rather than silently truncating.
    capture_halted: bool,
    /// Distinguishes a genuine stationary end from a failed worker or a
    /// rejected motion sequence when the worker exits.
    auto_reached_end: bool,
    manual_stall: ManualStallDetector,
    manual_alignment_warning: Option<ManualAmbiguousAction>,
    consecutive_capture_errors: u32,
    /// Solid-color first frames rejected in a row — see
    /// `MAX_CONSECUTIVE_BLANK_FRAMES`.
    consecutive_blank_frames: u32,
}

/// Run the scroll-capture overlay. Returns a stitched image and any warning
/// attached to an explicitly accepted uncertain seam, or `Ok(None)` on
/// Cancel/Esc.
pub struct ScrollCaptureOutcome {
    pub image: Pixbuf,
    pub warning: Option<String>,
}

/// How the scroll overlay ended.
pub enum ScrollRun {
    /// A stitched capture came back.
    Captured(ScrollCaptureOutcome),
    /// The user asked for an ordinary capture instead. Nothing has
    /// been captured, so handing over costs nothing.
    SwitchToArea,
    /// Escape, or the overlay closed without capturing.
    Cancelled,
}

/// Selected phase only: hand the pointer and keyboard to the page inside the
/// region while the cursor is over it, and take them back everywhere else.
/// Hyprland pins pointer focus to any layer surface holding an exclusive
/// keyboard grab, so the surface input region alone cannot let wheel events
/// through — the grab itself has to go while the pointer is over the page.
fn update_selected_keyboard_zone(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    x: f64,
    y: f64,
) {
    let release = {
        let mut s = state.borrow_mut();
        if s.phase != Phase::Selected || s.drag_active || s.resize_handle.is_some() {
            return;
        }
        let release = pointer_over_selected_page(s.selection, x, y);
        if release == s.selected_keyboard_released {
            return;
        }
        s.selected_keyboard_released = release;
        release
    };
    window.set_keyboard_mode(if release {
        KeyboardMode::None
    } else {
        KeyboardMode::Exclusive
    });
}

/// Re-take the exclusive grab (pointer moved onto overlay chrome such as the
/// mode buttons, which sit inside the region but belong to the overlay).
fn hold_selected_keyboard(state: &Rc<RefCell<OverlayState>>, window: &gtk::ApplicationWindow) {
    let mut s = state.borrow_mut();
    if s.phase != Phase::Selected || !s.selected_keyboard_released {
        return;
    }
    s.selected_keyboard_released = false;
    drop(s);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
}

/// The GdkMonitor whose connector (e.g. `DP-3`) matches `name`.
fn gdk_monitor_named(name: &str) -> Option<gtk::gdk::Monitor> {
    let display = gtk::gdk::Display::default()?;
    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|index| monitors.item(index))
        .filter_map(|item| item.downcast::<gtk::gdk::Monitor>().ok())
        .find(|monitor| {
            monitor
                .connector()
                .is_some_and(|connector| connector == name)
        })
}

pub fn run(park_manual_pointer: bool) -> Result<ScrollRun> {
    let result: Rc<RefCell<Option<ScrollCaptureOutcome>>> = Rc::new(RefCell::new(None));
    let switch_to_area = Rc::new(std::cell::Cell::new(false));

    let app = gtk::Application::builder()
        .application_id("dev.tensaku.Tensaku.scroll-capture")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    {
        let result = Rc::clone(&result);
        let switch_to_area = Rc::clone(&switch_to_area);
        app.connect_activate(move |app| {
            build_overlay(app, &result, park_manual_pointer, &switch_to_area)
        });
    }

    let exit_code = app.run_with_args::<&str>(&[]);
    if exit_code != gtk::glib::ExitCode::SUCCESS {
        return Err(anyhow!(
            "scroll-capture overlay exited with code {:?}",
            exit_code
        ));
    }
    // A capture wins over the switch: the flag can only be set while
    // nothing has been captured, but reading it in this order means a
    // race could never discard a finished stitch.
    Ok(match result.borrow_mut().take() {
        Some(outcome) => ScrollRun::Captured(outcome),
        None if switch_to_area.get() => ScrollRun::SwitchToArea,
        None => ScrollRun::Cancelled,
    })
}

fn build_overlay(
    app: &gtk::Application,
    result: &Rc<RefCell<Option<ScrollCaptureOutcome>>>,
    park_manual_pointer: bool,
    switch_to_area: &Rc<std::cell::Cell<bool>>,
) {
    let state = Rc::new(RefCell::new(OverlayState {
        phase: Phase::AwaitingDrag,
        drag_origin: (0.0, 0.0),
        drag_active: false,
        selection: Selection::default(),
        resize_handle: None,
        resize_anchor: Selection::default(),
        capture: None,
        capture_timer: None,
        capture_epoch: 0,
        capture_mode: None,
        park_manual_pointer,
        selected_keyboard_released: false,
        output_name: None,
        auto_scroll_stop: None,
        pointer_focus_stop: None,
        manual_pointer_target: None,
        auto_scroll_monitor: None,
        auto_scroll_handshake: None,
        auto_alignment: None,
        auto_step_calibration: AutoStepCalibration::default(),
        unverified_auto_seams: 0,
        last_captured_cycle: 0,
        consecutive_no_scroll: 0,
        capture_halted: false,
        auto_reached_end: false,
        manual_stall: ManualStallDetector::default(),
        manual_alignment_warning: None,
        consecutive_capture_errors: 0,
        consecutive_blank_frames: 0,
    }));

    let window = gtk::ApplicationWindow::new(app);
    window.init_layer_shell();
    // Pin the overlay to the focused Hyprland monitor and remember its
    // connector so capture and pointer warps address the same output. Other
    // compositors fall back to the monitor the surface is mapped on.
    if let Some(monitor) = crate::display::hyprland_focused_monitor() {
        if let Some(gdk_monitor) = gdk_monitor_named(&monitor.name) {
            window.set_monitor(Some(&gdk_monitor));
        }
        state.borrow_mut().output_name = Some(monitor.name);
    }
    {
        let state_w = Rc::clone(&state);
        window.connect_map(move |window| {
            if state_w.borrow().output_name.is_some() {
                return;
            }
            let connector = window
                .surface()
                .and_then(|surface| WidgetExt::display(window).monitor_at_surface(&surface))
                .and_then(|monitor| monitor.connector())
                .map(|connector| connector.to_string());
            if let Some(connector) = connector {
                eprintln!("scroll-capture: overlay mapped on {connector}");
                state_w.borrow_mut().output_name = Some(connector);
            }
        });
    }
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_namespace(Some("tensaku-scroll-capture"));
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    // -1 = ignore other layer-shell exclusive zones (e.g. waybar) so we cover
    // the entire output edge-to-edge.
    window.set_exclusive_zone(-1);
    window.add_css_class("scroll-capture-overlay");

    install_css(app);

    let overlay = gtk::Overlay::new();
    window.set_child(Some(&overlay));

    let drawing = gtk::DrawingArea::new();
    drawing.set_hexpand(true);
    drawing.set_vexpand(true);
    overlay.set_child(Some(&drawing));

    // Pill widgets go directly into the gtk::Overlay (not into a gtk::Fixed):
    // Fixed allocates itself 0x0 since children are transform-positioned,
    // which leaves transformed children outside the pick rect even though
    // they render fine. Overlay children sized via halign+valign+margins are
    // pickable by the same allocation that draws them.
    let prompt = build_prompt_pill();
    let action_pill = build_action_pill();
    let capturing_controls = build_capturing_pill();
    let capturing_pill = capturing_controls.pill.clone();
    let capture_status = capturing_controls.status.clone();

    for pill in [&prompt, &action_pill, &capturing_pill] {
        pill.set_halign(gtk::Align::Start);
        pill.set_valign(gtk::Align::Start);
        overlay.add_overlay(pill);
    }
    // The creation-time Exclusive keyboard mode stays in effect while the
    // user is choosing a region, so Esc (cancel) and the restore-region key
    // work before anything has been dragged. Keyboard pass-through to the
    // underlying app (PageDown etc.) is only needed while a capture is
    // actually running — `start_capture` switches to KeyboardMode::None —
    // and pointer wheel pass-through is governed by the surface input
    // region, not the keyboard mode.

    // Pointer guides, shown until a region exists. Widgets rather than
    // cairo lines in `draw_backdrop`: they move with every motion
    // event, and moving two strips lets GTK composite them instead of
    // repainting the whole backdrop to shift a line by a pixel.
    let crosshair: [gtk::Box; 2] = std::array::from_fn(|_| {
        let guide = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        guide.add_css_class("capture-crosshair");
        guide.set_visible(false);
        overlay.add_overlay(&guide);
        guide
    });
    crosshair[0].set_halign(gtk::Align::Start);
    crosshair[0].set_valign(gtk::Align::Fill);
    crosshair[0].set_size_request(1, -1);
    crosshair[1].set_halign(gtk::Align::Fill);
    crosshair[1].set_valign(gtk::Align::Start);
    crosshair[1].set_size_request(-1, 1);

    action_pill.set_visible(false);
    capturing_pill.set_visible(false);

    // Auto-scroll choices are shown while the selection is still editable.
    // Starting either manual or automatic capture hides them before the first
    // screenshot, so overlay controls can never be baked into the result.
    let vert_auto_scroll = build_inside_vert_auto_scroll();
    let horiz_auto_scroll = build_inside_horiz_auto_scroll();
    for btn in [&vert_auto_scroll, &horiz_auto_scroll] {
        btn.set_halign(gtk::Align::Start);
        btn.set_valign(gtk::Align::Start);
        btn.set_visible(false);
        overlay.add_overlay(btn);
    }

    // Drawing function pulls from state on each invalidation.
    {
        let state = Rc::clone(&state);
        drawing.set_draw_func(move |_, cr, w, h| {
            let s = state.borrow();
            draw_backdrop(cr, w as f64, h as f64, &s);
        });
    }

    // Drag-select gesture.
    let drag = gtk::GestureDrag::new();
    {
        let state = Rc::clone(&state);
        let drawing_w = drawing.clone();
        let prompt_w = prompt.clone();
        let action_pill_w = action_pill.clone();
        drag.connect_drag_begin(move |_, x, y| {
            // Record origin only. Phase/selection changes happen lazily in
            // drag_update once the user crosses DRAG_THRESHOLD of motion. A
            // tap (zero/tiny motion) is a no-op, so missing a pill button by
            // a few px doesn't reset existing state.
            let mut s = state.borrow_mut();
            s.drag_origin = (x, y);
            s.drag_active = false;
            // If we're past the initial drag and the cursor landed on a
            // resize handle, remember which handle so drag_update resizes
            // instead of starting a new selection.
            // Handles are interactive only in Selected. Once Capturing
            // starts, the selection is locked in (handles are hidden too
            // — see draw_backdrop).
            s.resize_handle = match s.phase {
                Phase::Selected => hit_test_handle(s.selection, x, y),
                _ => None,
            };
            if s.resize_handle.is_some() {
                s.resize_anchor = s.selection;
            }
            let _ = (&prompt_w, &action_pill_w, &drawing_w);
        });
    }
    {
        let state = Rc::clone(&state);
        let drawing_w = drawing.clone();
        let prompt_w = prompt.clone();
        let action_pill_w = action_pill.clone();
        let capturing_pill_w = capturing_pill.clone();
        let crosshair_drag = crosshair.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let mut s = state.borrow_mut();
            if !s.drag_active {
                if dx.abs() < DRAG_THRESHOLD && dy.abs() < DRAG_THRESHOLD {
                    return;
                }
                // Threshold crossed — commit to a real drag.
                s.drag_active = true;
                if s.resize_handle.is_none() {
                    s.phase = Phase::Dragging;
                    drop(s);
                    for guide in &crosshair_drag {
                        guide.set_visible(false);
                    }
                    prompt_w.set_visible(false);
                    action_pill_w.set_visible(false);
                    capturing_pill_w.set_visible(false);
                    let mut s = state.borrow_mut();
                    let (ox, oy) = s.drag_origin;
                    let x = ox.min(ox + dx);
                    let y = oy.min(oy + dy);
                    s.selection = Selection {
                        x,
                        y,
                        w: dx.abs(),
                        h: dy.abs(),
                    };
                    drop(s);
                    drawing_w.queue_draw();
                    return;
                }
            }
            // Resizing or moving an existing selection via a handle.
            if let Some(handle) = s.resize_handle {
                let drag_origin = s.drag_origin;
                let new_sel = handle.apply(
                    s.resize_anchor,
                    drag_origin,
                    drag_origin.0 + dx,
                    drag_origin.1 + dy,
                );
                s.selection = new_sel;
                drop(s);
                drawing_w.queue_draw();
                return;
            }
            // Otherwise, growing a fresh selection.
            let (ox, oy) = s.drag_origin;
            let x = ox.min(ox + dx);
            let y = oy.min(oy + dy);
            s.selection = Selection {
                x,
                y,
                w: dx.abs(),
                h: dy.abs(),
            };
            drop(s);
            drawing_w.queue_draw();
        });
    }
    {
        let state = Rc::clone(&state);
        let drawing_w = drawing.clone();
        let window_w = window.clone();
        let action_pill_w = action_pill.clone();
        let vert_btn_w = vert_auto_scroll.clone();
        let horiz_btn_w = horiz_auto_scroll.clone();
        let prompt_w = prompt.clone();
        drag.connect_drag_end(move |_, _dx, _dy| {
            let mut s = state.borrow_mut();
            if !s.drag_active {
                // Tap that missed a button or handle — leave state alone.
                s.resize_handle = None;
                return;
            }
            s.drag_active = false;
            // Finishing a resize: keep phase, just refresh pill + input
            // region against the new selection rect.
            if s.resize_handle.is_some() {
                s.resize_handle = None;
                let sel = s.selection;
                let phase = s.phase;
                drop(s);
                drawing_w.queue_draw();
                if phase == Phase::Selected {
                    if sel.is_valid() {
                        // Every committed shape change updates the
                        // restorable region — even if this overlay is
                        // cancelled afterwards.
                        crate::state::save_scroll_capture_last_region([sel.x, sel.y, sel.w, sel.h]);
                    }
                    position_selected_controls_and_input(
                        &state,
                        &window_w,
                        &action_pill_w,
                        &vert_btn_w,
                        &horiz_btn_w,
                        sel,
                    );
                }
                return;
            }
            if s.selection.is_valid() {
                s.phase = Phase::Selected;
                let sel = s.selection;
                drop(s);
                // The freshly dragged shape becomes the restorable region
                // immediately — Esc after this still remembers it.
                crate::state::save_scroll_capture_last_region([sel.x, sel.y, sel.w, sel.h]);
                action_pill_w.set_visible(true);
                state.borrow_mut().selected_keyboard_released = false;
                window_w.set_keyboard_mode(KeyboardMode::Exclusive);
                position_selected_controls_and_input(
                    &state,
                    &window_w,
                    &action_pill_w,
                    &vert_btn_w,
                    &horiz_btn_w,
                    sel,
                );
                prompt_w.set_visible(false);
            } else {
                s.phase = Phase::AwaitingDrag;
                s.selection = Selection::default();
                drop(s);
                prompt_w.set_visible(true);
                action_pill_w.set_visible(false);
            }
            drawing_w.queue_draw();
        });
    }
    drawing.add_controller(drag);

    // Cursor shape on handle hover — only in Selected (handles are hidden
    // during Capturing).
    let motion = gtk::EventControllerMotion::new();
    {
        let state = Rc::clone(&state);
        let drawing_w = drawing.clone();
        let window_w = window.clone();
        let crosshair_w = crosshair.clone();
        motion.connect_motion(move |_, x, y| {
            let phase = state.borrow().phase;
            // Guides while a region is still being chosen; once one
            // exists it is the thing being aimed, and they would be two
            // more lines over it.
            let aiming = matches!(phase, Phase::AwaitingDrag);
            crosshair_w[0].set_margin_start(x.round().max(0.0) as i32);
            crosshair_w[1].set_margin_top(y.round().max(0.0) as i32);
            for guide in &crosshair_w {
                guide.set_visible(aiming);
            }
            if !matches!(phase, Phase::Selected) {
                drawing_w.set_cursor_from_name(Some(if aiming { "crosshair" } else { "default" }));
                return;
            }
            update_selected_keyboard_zone(&state, &window_w, x, y);
            let sel = state.borrow().selection;
            let name = match hit_test_handle(sel, x, y) {
                Some(ResizeHandle::TopLeft) | Some(ResizeHandle::BottomRight) => "nwse-resize",
                Some(ResizeHandle::TopRight) | Some(ResizeHandle::BottomLeft) => "nesw-resize",
                Some(ResizeHandle::Top) | Some(ResizeHandle::Bottom) => "ns-resize",
                Some(ResizeHandle::Left) | Some(ResizeHandle::Right) => "ew-resize",
                Some(ResizeHandle::Move) => "move",
                None => "default",
            };
            drawing_w.set_cursor_from_name(Some(name));
        });
    }
    drawing.add_controller(motion);
    // The mode buttons and pill sit inside the region but are overlay chrome:
    // hovering them takes the keyboard back so Esc works there too.
    for widget in [
        action_pill.clone().upcast::<gtk::Widget>(),
        vert_auto_scroll.clone().upcast::<gtk::Widget>(),
        horiz_auto_scroll.clone().upcast::<gtk::Widget>(),
    ] {
        let hover = gtk::EventControllerMotion::new();
        let state = Rc::clone(&state);
        let window_w = window.clone();
        hover.connect_enter(move |_, _, _| hold_selected_keyboard(&state, &window_w));
        widget.add_controller(hover);
    }

    // Center the prompt once we know the surface size.
    {
        let prompt_w = prompt.clone();
        drawing.connect_resize(move |_, w, h| {
            let (pw, ph) = pill_natural_size(&prompt_w);
            let x = ((w as f64 - pw) / 2.0).max(0.0);
            let y = ((h as f64 - ph) / 2.0).max(0.0);
            prompt_w.set_margin_start(x as i32);
            prompt_w.set_margin_top(y as i32);
        });
    }

    // Esc cancels; the configured restore-region key reselects the
    // previous capture's rectangle while no capture is running.
    let keys = gtk::EventControllerKey::new();
    {
        let window_w = window.clone();
        let state_w = Rc::clone(&state);
        let state_keys = Rc::clone(&state);
        let switch_to_area = Rc::clone(switch_to_area);
        let drawing_w = drawing.clone();
        let prompt_w = prompt.clone();
        let action_pill_w = action_pill.clone();
        let vert_btn_w = vert_auto_scroll.clone();
        let horiz_btn_w = horiz_auto_scroll.clone();
        let restore_shortcut = crate::sketch_board::parse_shortcut(
            crate::APP_CONFIG
                .read()
                .scroll_capture_restore_region_shortcut(),
        );
        keys.connect_key_pressed(move |_, key, _, modifier| {
            if key == gtk::gdk::Key::Escape {
                window_w.close();
                return gtk::glib::Propagation::Stop;
            }
            // Before a region exists there is nothing to scroll, so A
            // means the other kind of capture entirely — the mirror of
            // the area overlay's S. Handing back beats making someone
            // close this and press a different keybind.
            if matches!(key, gtk::gdk::Key::a | gtk::gdk::Key::A)
                && modifier.is_empty()
                && matches!(state_keys.borrow().phase, Phase::AwaitingDrag)
            {
                switch_to_area.set(true);
                window_w.close();
                return gtk::glib::Propagation::Stop;
            }
            if let Some((restore_key, restore_mods)) = restore_shortcut {
                let relevant = modifier.intersection(
                    gtk::gdk::ModifierType::CONTROL_MASK
                        | gtk::gdk::ModifierType::SHIFT_MASK
                        | gtk::gdk::ModifierType::ALT_MASK
                        | gtk::gdk::ModifierType::SUPER_MASK,
                );
                if key.to_lower() == restore_key && relevant == restore_mods {
                    restore_previous_region(
                        &state_w,
                        &window_w,
                        &drawing_w,
                        &prompt_w,
                        &action_pill_w,
                        &vert_btn_w,
                        &horiz_btn_w,
                    );
                    return gtk::glib::Propagation::Stop;
                }
            }
            gtk::glib::Propagation::Proceed
        });
    }
    window.add_controller(keys);

    // Every close path, including compositor/window-manager close requests,
    // must stop an injection worker. Button handlers also call this cleanup;
    // it is intentionally idempotent.
    {
        let state = Rc::clone(&state);
        window.connect_close_request(move |_| {
            stop_capture(&state);
            glib::Propagation::Proceed
        });
    }

    // Wire pre-capture pill buttons (Cancel / Start Capture).
    {
        let window_w = window.clone();
        let cancel: gtk::Button = action_pill
            .first_child()
            .and_then(|c| c.downcast().ok())
            .expect("action pill missing cancel button");
        cancel.connect_clicked(move |_| window_w.close());
    }
    {
        let state = Rc::clone(&state);
        let window_w = window.clone();
        let action_pill_w = action_pill.clone();
        let capturing_pill_w = capturing_pill.clone();
        let vert_btn_w = vert_auto_scroll.clone();
        let horiz_btn_w = horiz_auto_scroll.clone();
        let overlay_w = overlay.clone();
        let drawing_w = drawing.clone();
        let capture_status_w = capture_status.clone();
        let start: gtk::Button = action_pill
            .last_child()
            .and_then(|c| c.downcast().ok())
            .expect("action pill missing start-capture button");
        start.connect_clicked(move |_| {
            let _ = start_capture(
                &state,
                &window_w,
                &overlay_w,
                &action_pill_w,
                &capturing_pill_w,
                &vert_btn_w,
                &horiz_btn_w,
                &drawing_w,
                &capture_status_w,
                CaptureMode::Manual(stitch::StitchAxis::Vertical),
            );
        });
    }

    // Wire capturing-pill buttons (Cancel / Done).
    wire_capturing_pill(&state, &window, &capturing_controls, result);

    // Inside-selection Auto-Scroll buttons wire to start_auto_scroll_at.
    {
        let state_w = Rc::clone(&state);
        let window_w = window.clone();
        let overlay_w = overlay.clone();
        let capturing_pill_w = capturing_pill.clone();
        let action_pill_w = action_pill.clone();
        let drawing_w = drawing.clone();
        let capture_status_w = capture_status.clone();
        let vert_btn_w = vert_auto_scroll.clone();
        let horiz_btn_w = horiz_auto_scroll.clone();
        let btn = vert_auto_scroll.clone();
        btn.connect_clicked(move |b| {
            eprintln!("scroll-capture: vertical Auto-Scroll clicked");
            if start_capture(
                &state_w,
                &window_w,
                &overlay_w,
                &action_pill_w,
                &capturing_pill_w,
                &vert_btn_w,
                &horiz_btn_w,
                &drawing_w,
                &capture_status_w,
                CaptureMode::Auto(auto_scroll::ScrollDirection::Down),
            ) {
                start_auto_scroll_at(
                    &state_w,
                    &window_w,
                    &capturing_pill_w,
                    &capture_status_w,
                    b,
                    auto_scroll::ScrollDirection::Down,
                );
            }
        });
    }
    {
        let state_w = Rc::clone(&state);
        let window_w = window.clone();
        let overlay_w = overlay.clone();
        let capturing_pill_w = capturing_pill.clone();
        let action_pill_w = action_pill.clone();
        let drawing_w = drawing.clone();
        let capture_status_w = capture_status.clone();
        let vert_btn_w = vert_auto_scroll.clone();
        let horiz_btn_w = horiz_auto_scroll.clone();
        let btn = horiz_auto_scroll.clone();
        btn.connect_clicked(move |b| {
            eprintln!("scroll-capture: horizontal Auto-Scroll clicked");
            if start_capture(
                &state_w,
                &window_w,
                &overlay_w,
                &action_pill_w,
                &capturing_pill_w,
                &vert_btn_w,
                &horiz_btn_w,
                &drawing_w,
                &capture_status_w,
                CaptureMode::Auto(auto_scroll::ScrollDirection::Right),
            ) {
                start_auto_scroll_at(
                    &state_w,
                    &window_w,
                    &capturing_pill_w,
                    &capture_status_w,
                    b,
                    auto_scroll::ScrollDirection::Right,
                );
            }
        });
    }

    window.present();
}

/// Reselect the previous capture's region (persisted in state.toml) in
/// response to the configured restore-region key. Only meaningful before
/// a capture is running; the saved rect is clamped to the current overlay
/// so a monitor change can't produce an off-screen selection.
fn restore_previous_region(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    drawing: &gtk::DrawingArea,
    prompt: &gtk::Box,
    action_pill: &gtk::Box,
    vert_btn: &gtk::Button,
    horiz_btn: &gtk::Button,
) {
    let Some([x, y, w, h]) = crate::state::load_scroll_capture_last_region() else {
        return;
    };
    let overlay_w = drawing.width() as f64;
    let overlay_h = drawing.height() as f64;
    if overlay_w <= 0.0 || overlay_h <= 0.0 {
        return;
    }
    let w = w.min(overlay_w);
    let h = h.min(overlay_h);
    let sel = Selection {
        x: x.clamp(0.0, (overlay_w - w).max(0.0)),
        y: y.clamp(0.0, (overlay_h - h).max(0.0)),
        w,
        h,
    };
    if !sel.is_valid() {
        return;
    }
    {
        let mut s = state.borrow_mut();
        if !matches!(s.phase, Phase::AwaitingDrag | Phase::Selected) {
            return;
        }
        s.phase = Phase::Selected;
        s.selection = sel;
    }
    prompt.set_visible(false);
    action_pill.set_visible(true);
    state.borrow_mut().selected_keyboard_released = false;
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    position_selected_controls_and_input(state, window, action_pill, vert_btn, horiz_btn, sel);
    drawing.queue_draw();
}

#[allow(clippy::too_many_arguments)]
fn start_capture(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    overlay: &gtk::Overlay,
    action_pill: &gtk::Box,
    capturing_pill: &gtk::Box,
    vert_btn: &gtk::Button,
    horiz_btn: &gtk::Button,
    drawing: &gtk::DrawingArea,
    status: &gtk::Label,
    mode: CaptureMode,
) -> bool {
    let sel = state.borrow().selection;
    status.set_text(match mode {
        CaptureMode::Manual(_) => "Scroll inside selection",
        CaptureMode::Auto(_) => "Auto-scrolling…",
    });
    status.remove_css_class("scroll-capture-status-error");
    if let Some(done) = capturing_done_button(status) {
        done.set_sensitive(false);
    }

    let (pill_w, pill_h) = pill_natural_size(capturing_pill);
    if capture_pill_position(
        overlay.allocated_width() as f64,
        overlay.allocated_height() as f64,
        pill_w,
        pill_h,
        sel,
    )
    .is_none()
    {
        show_selection_space_warning(action_pill);
        return false;
    }

    {
        let mut s = state.borrow_mut();
        s.phase = Phase::Capturing;
        s.capture_epoch = s.capture_epoch.wrapping_add(1);
        s.capture_mode = Some(mode);
        s.capture = None;
        s.pointer_focus_stop = None;
        s.manual_pointer_target = None;
        s.auto_scroll_handshake = None;
        s.auto_alignment = None;
        s.auto_step_calibration = AutoStepCalibration::default();
        s.unverified_auto_seams = 0;
        s.last_captured_cycle = 0;
        s.consecutive_no_scroll = 0;
        s.capture_halted = false;
        s.auto_reached_end = false;
        s.manual_stall.reset();
        s.manual_alignment_warning = None;
        s.consecutive_capture_errors = 0;
        s.consecutive_blank_frames = 0;
    }

    action_pill.set_visible(false);
    vert_btn.set_visible(false);
    horiz_btn.set_visible(false);
    capturing_pill.set_visible(true);
    // Selected mode requests keyboard focus for its controls. Release it
    // before the transparent capture surface is committed so both real-wheel
    // input and the virtual-keyboard fallback reach the underlying app.
    window.set_keyboard_mode(KeyboardMode::None);
    position_capturing_pill_and_input(window, overlay, capturing_pill, sel);

    if mode.is_manual() {
        let scale = window.scale_factor().max(1);
        state.borrow_mut().manual_pointer_target = Some(pointer_park_target(sel, scale));
    }

    schedule_capture_after_overlay_commit(state, window, drawing, sel, status);
    true
}

/// Wait through two GTK frame ticks before arming screencopy. Tick callbacks
/// run before paint: the first tick causes the transparent-hole frame to be
/// rendered and submitted, while the second proves that frame completed. A
/// short compositor settle then prevents capturing the previous surface state.
fn schedule_capture_after_overlay_commit(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    drawing: &gtk::DrawingArea,
    sel: Selection,
    status: &gtk::Label,
) {
    let epoch = state.borrow().capture_epoch;
    let saw_first_tick = Rc::new(std::cell::Cell::new(false));
    let state_w = Rc::clone(state);
    let window_w = window.clone();
    let status_w = status.clone();
    drawing.add_tick_callback(move |_, _| {
        if !saw_first_tick.replace(true) {
            return glib::ControlFlow::Continue;
        }

        let state_w = Rc::clone(&state_w);
        let window_w = window_w.clone();
        let status_w = status_w.clone();
        glib::timeout_add_local_once(Duration::from_millis(OVERLAY_COMMIT_SETTLE_MS), move || {
            let still_current = capture_epoch_is_current(&state_w, epoch);
            if still_current {
                // Round-trip GTK's Wayland connection so the layer-surface
                // buffer and input-region commit are processed before the
                // independent screencopy connection asks for a frame.
                gtk::prelude::WidgetExt::display(&window_w).sync();
                let still_current = capture_epoch_is_current(&state_w, epoch);
                if still_current {
                    start_capture_after_optional_pointer_park(
                        &state_w, &window_w, sel, &status_w, epoch,
                    );
                }
            }
        });
        glib::ControlFlow::Break
    });
    drawing.queue_draw();
}

/// A paused choice panel may temporarily overlap the selected rectangle when
/// there is no outside gutter large enough for it. Wait until the restored
/// compact pill has been painted and committed before manual sampling resumes.
fn schedule_manual_resume_after_pill_commit(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    status: &gtk::Label,
    selection: Selection,
) {
    let Some(pill) = capturing_pill(status) else {
        run_capture_tick_and_reschedule(state, window, selection, status);
        return;
    };
    let epoch = state.borrow().capture_epoch;
    let saw_first_tick = Rc::new(std::cell::Cell::new(false));
    let state_w = Rc::clone(state);
    let window_w = window.clone();
    let status_w = status.clone();
    pill.add_tick_callback(move |_, _| {
        if !saw_first_tick.replace(true) {
            return glib::ControlFlow::Continue;
        }
        // A RefCell borrow used directly as a `match` scrutinee lives through
        // the selected arm. Snapshot the policy in a short scope so either
        // continuation may safely mutate capture state. Use `try_borrow` here
        // because a panic must never cross GTK's C callback boundary.
        let policy = {
            let Ok(state) = state_w.try_borrow() else {
                return glib::ControlFlow::Continue;
            };
            if state.phase != Phase::Capturing
                || state.capture_epoch != epoch
                || !state.capture_mode.is_some_and(CaptureMode::is_manual)
            {
                return glib::ControlFlow::Break;
            }
            manual_handoff_pointer_policy(state.park_manual_pointer)
        };

        gtk::prelude::WidgetExt::display(&window_w).sync();
        match policy {
            ManualHandoffPointerPolicy::ParkInSelection => {
                park_pointer_then_resume_manual(&state_w, &window_w, selection, &status_w, epoch);
            }
            ManualHandoffPointerPolicy::LeaveUnchanged => {
                status_w.set_text("Move the pointer inside the selection, then scroll");
                run_capture_tick_and_reschedule(&state_w, &window_w, selection, &status_w);
            }
        }
        glib::ControlFlow::Break
    });
    pill.queue_draw();
}

fn schedule_auto_resume_after_pill_commit(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    status: &gtk::Label,
    selection: Selection,
    handshake: auto_scroll::CaptureHandshake,
    cycle: u64,
) {
    // Apply this before the frame barrier so the round-trip below fences the
    // keyboard-focus release as well as the compact-pill repaint.
    window.set_keyboard_mode(KeyboardMode::None);
    let Some(pill) = capturing_pill(status) else {
        gtk::prelude::WidgetExt::display(window).sync();
        let epoch = state.borrow().capture_epoch;
        park_pointer_then_resume_auto(state, window, selection, status, epoch, handshake, cycle);
        return;
    };
    let epoch = state.borrow().capture_epoch;
    let saw_first_tick = Rc::new(std::cell::Cell::new(false));
    let state_w = Rc::clone(state);
    let window_w = window.clone();
    let status_w = status.clone();
    pill.add_tick_callback(move |_, _| {
        if !saw_first_tick.replace(true) {
            return glib::ControlFlow::Continue;
        }
        let should_resume = {
            let Ok(state) = state_w.try_borrow() else {
                return glib::ControlFlow::Continue;
            };
            if state.phase != Phase::Capturing || state.capture_epoch != epoch {
                return glib::ControlFlow::Break;
            }
            matches!(state.capture_mode, Some(CaptureMode::Auto(_)))
                && state.auto_alignment.is_none()
        };
        if !should_resume {
            return glib::ControlFlow::Break;
        }

        gtk::prelude::WidgetExt::display(&window_w).sync();
        park_pointer_then_resume_auto(
            &state_w,
            &window_w,
            selection,
            &status_w,
            epoch,
            handshake.clone(),
            cycle,
        );
        glib::ControlFlow::Break
    });
    pill.queue_draw();
}

fn park_pointer_then_resume_auto(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    selection: Selection,
    status: &gtk::Label,
    epoch: u64,
    handshake: auto_scroll::CaptureHandshake,
    cycle: u64,
) {
    let (cursor_x, cursor_y) = pointer_park_target(selection, window.scale_factor().max(1));
    let stop = Arc::new(AtomicBool::new(false));
    let output_name = state.borrow().output_name.clone();
    if let Err(error) = auto_scroll::focus_underlying_once(
        Arc::clone(&stop),
        cursor_x,
        cursor_y,
        output_name.as_deref(),
    ) {
        // A real-wheel worker will repeat this focus attempt immediately
        // before injecting; the keyboard fallback only needs the committed
        // KeyboardMode::None above. Let the worker decide whether its own
        // backend remains usable.
        eprintln!("scroll-capture: could not pre-focus automatic continuation: {error:#}");
        handshake.acknowledge(cycle);
        run_capture_tick_and_reschedule(state, window, selection, status);
        return;
    }
    if !capture_epoch_is_current(state, epoch) {
        stop.store(true, Ordering::Release);
        return;
    }
    state.borrow_mut().pointer_focus_stop = Some(Arc::clone(&stop));

    let state_w = Rc::clone(state);
    let window_w = window.clone();
    let status_w = status.clone();
    glib::timeout_add_local(Duration::from_millis(POINTER_FOCUS_POLL_MS), move || {
        let still_current = capture_epoch_is_current(&state_w, epoch)
            && matches!(state_w.borrow().capture_mode, Some(CaptureMode::Auto(_)))
            && state_w.borrow().auto_alignment.is_none();
        if !still_current {
            stop.store(true, Ordering::Release);
            return glib::ControlFlow::Break;
        }
        if !stop.load(Ordering::Acquire) {
            return glib::ControlFlow::Continue;
        }

        {
            let mut state = state_w.borrow_mut();
            if state
                .pointer_focus_stop
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &stop))
            {
                state.pointer_focus_stop = None;
            }
        }
        handshake.acknowledge(cycle);
        run_capture_tick_and_reschedule(&state_w, &window_w, selection, &status_w);
        glib::ControlFlow::Break
    });
}

fn park_pointer_then_resume_manual(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    selection: Selection,
    status: &gtk::Label,
    epoch: u64,
) {
    let (cursor_x, cursor_y) = pointer_park_target(selection, window.scale_factor().max(1));
    let stop = Arc::new(AtomicBool::new(false));
    let output_name = state.borrow().output_name.clone();
    if let Err(error) = auto_scroll::focus_underlying_once(
        Arc::clone(&stop),
        cursor_x,
        cursor_y,
        output_name.as_deref(),
    ) {
        eprintln!("scroll-capture: could not repark pointer for manual continuation: {error:#}");
        status.set_text("Move the pointer inside the selection, then scroll");
        run_capture_tick_and_reschedule(state, window, selection, status);
        return;
    }
    if !capture_epoch_is_current(state, epoch) {
        stop.store(true, Ordering::Release);
        return;
    }
    state.borrow_mut().pointer_focus_stop = Some(Arc::clone(&stop));
    wait_for_manual_pointer_park(state, window, selection, status, epoch, stop);
}

fn capture_epoch_is_current(state: &Rc<RefCell<OverlayState>>, epoch: u64) -> bool {
    let state = state.borrow();
    state.phase == Phase::Capturing && state.capture_epoch == epoch
}

/// Start frame one immediately unless manual pointer parking is enabled. The
/// park worker's stop flag becomes true when its absolute warp and focus
/// nudges have completed; only then do we start the hover repaint delay.
fn start_capture_after_optional_pointer_park(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    sel: Selection,
    status: &gtk::Label,
    epoch: u64,
) {
    let target = {
        let state = state.borrow();
        (state.park_manual_pointer && state.capture_mode.is_some_and(CaptureMode::is_manual))
            .then_some(state.manual_pointer_target)
            .flatten()
    };
    let Some((cursor_x, cursor_y)) = target else {
        eprintln!("scroll-capture: overlay frame committed; starting capture");
        run_capture_tick_and_reschedule(state, window, sel, status);
        return;
    };

    let stop = Arc::new(AtomicBool::new(false));
    let output_name = state.borrow().output_name.clone();
    if let Err(error) = auto_scroll::focus_underlying_once(
        Arc::clone(&stop),
        cursor_x,
        cursor_y,
        output_name.as_deref(),
    ) {
        // Preserve the target so frame one's existing in-place refocus path
        // still runs. A failed park must not make the capture unusable.
        eprintln!("scroll-capture: could not park manual capture pointer: {error:#}");
        run_capture_tick_and_reschedule(state, window, sel, status);
        return;
    }

    if !capture_epoch_is_current(state, epoch) {
        stop.store(true, Ordering::Release);
        return;
    }
    {
        let mut state = state.borrow_mut();
        state.manual_pointer_target = None;
        state.pointer_focus_stop = Some(Arc::clone(&stop));
    }
    eprintln!("scroll-capture: overlay frame committed; parking manual pointer");
    wait_for_manual_pointer_park(state, window, sel, status, epoch, stop);
}

fn wait_for_manual_pointer_park(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    sel: Selection,
    status: &gtk::Label,
    epoch: u64,
    stop: Arc<AtomicBool>,
) {
    let state_w = Rc::clone(state);
    let window_w = window.clone();
    let status_w = status.clone();
    glib::timeout_add_local(Duration::from_millis(POINTER_FOCUS_POLL_MS), move || {
        if !capture_epoch_is_current(&state_w, epoch) {
            return glib::ControlFlow::Break;
        }
        if !stop.load(Ordering::Acquire) {
            return glib::ControlFlow::Continue;
        }

        {
            let mut state = state_w.borrow_mut();
            if state
                .pointer_focus_stop
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &stop))
            {
                state.pointer_focus_stop = None;
            }
        }

        let state_w = Rc::clone(&state_w);
        let window_w = window_w.clone();
        let status_w = status_w.clone();
        glib::timeout_add_local_once(
            Duration::from_millis(MANUAL_POINTER_HOVER_SETTLE_MS),
            move || {
                if capture_epoch_is_current(&state_w, epoch) {
                    eprintln!("scroll-capture: manual pointer parked; starting capture");
                    run_capture_tick_and_reschedule(&state_w, &window_w, sel, &status_w);
                }
            },
        );
        glib::ControlFlow::Break
    });
}

/// Schedule the next sample relative to completion of the current one. A
/// repeating GLib timeout stays perpetually overdue when screencopy + matching
/// takes longer than the interval, starving GTK paints and button events.
fn schedule_capture_tick(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    sel: Selection,
    status: &gtk::Label,
) {
    let interval_ms = capture_poll_interval_ms(state.borrow().capture_mode);
    let state_w = Rc::clone(state);
    let window_w = window.clone();
    let status_w = status.clone();
    let timer = glib::timeout_add_local_once(Duration::from_millis(interval_ms), move || {
        state_w.borrow_mut().capture_timer = None;
        run_capture_tick_and_reschedule(&state_w, &window_w, sel, &status_w);
    });
    state.borrow_mut().capture_timer = Some(timer);
}

fn run_capture_tick_and_reschedule(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    sel: Selection,
    status: &gtk::Label,
) {
    let control = capture_tick(state, window, sel, status);
    if control == glib::ControlFlow::Continue && state.borrow().phase == Phase::Capturing {
        schedule_capture_tick(state, window, sel, status);
    }
}

fn capture_poll_interval_ms(mode: Option<CaptureMode>) -> u64 {
    match mode {
        Some(CaptureMode::Auto(_)) => AUTO_CAPTURE_POLL_MS,
        Some(CaptureMode::Manual(_)) | None => CAPTURE_INTERVAL_MS,
    }
}

fn start_manual_pointer_refocus(state: &Rc<RefCell<OverlayState>>) {
    if state.borrow().pointer_focus_stop.is_some() {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    match auto_scroll::refocus_under_pointer_once(Arc::clone(&stop)) {
        Ok(()) => state.borrow_mut().pointer_focus_stop = Some(stop),
        Err(error) => {
            eprintln!("scroll-capture: could not refocus under manual pointer: {error:#}")
        }
    }
}

/// Whether every sampled pixel of `pixbuf` is one color. Samples a ~50×50
/// grid: any differing pixel proves real content immediately, so normal
/// frames exit within a few comparisons; only genuinely solid frames walk
/// the whole grid. Alpha is ignored (the capture backend forces it opaque).
fn pixbuf_is_uniform(pixbuf: &Pixbuf) -> bool {
    let width = pixbuf.width() as usize;
    let height = pixbuf.height() as usize;
    if width == 0 || height == 0 {
        return true;
    }
    let channels = pixbuf.n_channels() as usize;
    let stride = pixbuf.rowstride() as usize;
    let bytes = pixbuf.read_pixel_bytes();
    let data = bytes.as_ref();
    let first = &data[0..3];
    let step_x = (width / 50).max(1);
    let step_y = (height / 50).max(1);
    for y in (0..height).step_by(step_y) {
        let row = y * stride;
        for x in (0..width).step_by(step_x) {
            let px = row + x * channels;
            if data[px..px + 3] != *first {
                return false;
            }
        }
    }
    true
}

/// When `TENSAKU_CAPTURE_DEBUG_DIR` is set, save every frame screencopy
/// hands the capture loop into that directory (`frame-NNNN[-cCYCLE].png`)
/// so blank/garbage capture reports can be diagnosed from a real run.
fn dump_debug_frame(pixbuf: &Pixbuf, cycle: Option<u64>) {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let Ok(dir) = std::env::var("TENSAKU_CAPTURE_DEBUG_DIR") else {
        return;
    };
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let suffix = cycle.map(|c| format!("-c{c}")).unwrap_or_default();
    let path = format!("{dir}/frame-{seq:04}{suffix}.png");
    if let Err(error) = pixbuf.savev(&path, "png", &[]) {
        eprintln!("scroll-capture: debug frame dump failed: {error}");
    }
}

fn capture_tick(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    sel: Selection,
    status: &gtk::Label,
) -> glib::ControlFlow {
    if state.borrow().phase != Phase::Capturing {
        return glib::ControlFlow::Break;
    }
    if state.borrow().capture_halted {
        return glib::ControlFlow::Break;
    }
    if matches!(
        state.borrow().auto_alignment.as_ref(),
        Some(AutoAlignmentState::Paused { .. })
    ) {
        return glib::ControlFlow::Break;
    }
    let (mode, auto_cycle) = {
        let s = state.borrow();
        let Some(mode) = s.capture_mode else {
            return glib::ControlFlow::Continue;
        };
        match mode {
            CaptureMode::Manual(_) => (mode, None),
            CaptureMode::Auto(_) => {
                let Some(handshake) = s.auto_scroll_handshake.clone() else {
                    // Worker setup is still running in the deferred idle.
                    return glib::ControlFlow::Continue;
                };
                let cycle = handshake.ready_cycle();
                if cycle == 0 || cycle <= s.last_captured_cycle {
                    return glib::ControlFlow::Continue;
                }
                (mode, Some((handshake, cycle)))
            }
        }
    };

    let rect = capture::Rect {
        x: sel.x.round() as i32,
        y: sel.y.round() as i32,
        width: sel.w.round() as i32,
        height: sel.h.round() as i32,
    };
    let output_name = state.borrow().output_name.clone();
    match capture::capture_region(rect, output_name.as_deref()) {
        Ok(pixbuf) => {
            dump_debug_frame(&pixbuf, auto_cycle.as_ref().map(|(_, cycle)| *cycle));
            if state.borrow().capture.is_none() && pixbuf_is_uniform(&pixbuf) {
                let mut s = state.borrow_mut();
                s.consecutive_blank_frames += 1;
                eprintln!(
                    "scroll-capture: rejected solid-color first frame ({}/{})",
                    s.consecutive_blank_frames, MAX_CONSECUTIVE_BLANK_FRAMES
                );
                if s.consecutive_blank_frames >= MAX_CONSECUTIVE_BLANK_FRAMES {
                    halt_capture(&mut s, status, "Capture shows only a solid color — retry");
                    window.set_keyboard_mode(KeyboardMode::Exclusive);
                }
                // Leave any automatic cycle unacknowledged so the worker
                // holds the same screen instead of scrolling past content
                // that was never recorded.
                return glib::ControlFlow::Continue;
            }
            let gray = stitch::downsample_to_gray_for_axis(&pixbuf, mode.axis());
            let analysis = {
                let state = state.borrow();
                match state.capture.as_ref() {
                    None => CaptureAnalysis::Initial,
                    Some(capture) => match mode {
                        CaptureMode::Manual(_) => match state.auto_alignment.as_ref() {
                            Some(AutoAlignmentState::ManualRecovery { lookahead, .. }) => {
                                CaptureAnalysis::ResolveManualRecovery(lookahead.resolve(&gray))
                            }
                            Some(AutoAlignmentState::Probing { .. })
                            | Some(AutoAlignmentState::Paused { .. }) => unreachable!(
                                "automatic alignment state does not match manual capture mode"
                            ),
                            None => {
                                let strict =
                                    stitch::classify_motion(&capture.last_gray, &gray, mode.axis());
                                let estimate = if matches!(
                                    strict.motion,
                                    stitch::Motion::Ambiguous(_)
                                ) {
                                    let bound = manual_motion_search_bound(&pixbuf, mode.axis());
                                    let bounded = stitch::classify_motion_bounded(
                                        &capture.last_gray,
                                        &gray,
                                        mode.axis(),
                                        bound,
                                    );
                                    let recovered =
                                        recover_bounded_manual_motion(strict, bounded, bound);
                                    if recovered != strict {
                                        eprintln!(
                                            "scroll-capture: recovered nearby manual motion {:?} (error={:.3}, confidence={:.3}, bound={bound}px)",
                                            recovered.motion, recovered.error, recovered.confidence
                                        );
                                    }
                                    recovered
                                } else {
                                    strict
                                };
                                CaptureAnalysis::Motion(estimate)
                            }
                        },
                        CaptureMode::Auto(_) => match state.auto_alignment.as_ref() {
                            Some(AutoAlignmentState::Probing { lookahead, .. }) => {
                                CaptureAnalysis::ResolveAutoProbe(lookahead.resolve_auto(&gray))
                            }
                            Some(AutoAlignmentState::Paused { .. })
                            | Some(AutoAlignmentState::ManualRecovery { .. }) => unreachable!(
                                "paused automatic alignment is filtered before screencopy"
                            ),
                            None => match stitch::classify_forward_with_lookahead(
                                &capture.last_gray,
                                &gray,
                                mode.axis(),
                            ) {
                                stitch::ForwardMatch::Classified(estimate) => {
                                    CaptureAnalysis::Motion(estimate)
                                }
                                stitch::ForwardMatch::Ambiguous(lookahead) => {
                                    CaptureAnalysis::BeginAutoProbe(lookahead)
                                }
                            },
                        },
                    },
                }
            };
            let mut s = state.borrow_mut();
            s.consecutive_capture_errors = 0;
            s.consecutive_blank_frames = 0;
            let mut acknowledgement = AutoCaptureAcknowledgement::Normal;
            match analysis {
                CaptureAnalysis::Initial => {
                    match stitch::StitchAccumulator::new(&pixbuf, mode.axis()) {
                        Ok(accumulator) => {
                            s.capture = Some(CaptureBuffer {
                                stitch: accumulator,
                                last_gray: gray,
                                manual_direction: None,
                                pending_manual: None,
                            });
                            s.consecutive_no_scroll = 0;
                            if let Some(done) = capturing_done_button(status) {
                                done.set_sensitive(true);
                            }
                            if s.manual_pointer_target.take().is_some() {
                                let state = Rc::clone(state);
                                glib::idle_add_local_once(move || {
                                    let should_focus = {
                                        let state = state.borrow();
                                        state.phase == Phase::Capturing
                                            && state
                                                .capture_mode
                                                .is_some_and(CaptureMode::is_manual)
                                    };
                                    if should_focus {
                                        start_manual_pointer_refocus(&state);
                                    }
                                });
                            }
                            eprintln!("scroll-capture: kept initial frame");
                        }
                        Err(error) => {
                            eprintln!("scroll-capture: could not initialize stitch: {error}");
                            halt_capture(&mut s, status, "Capture failed — retry");
                        }
                    }
                }
                CaptureAnalysis::Motion(estimate) => {
                    if !matches!(estimate.motion, stitch::Motion::Stationary) {
                        eprintln!(
                            "scroll-capture: motion={:?} error={:.3} confidence={:.3}",
                            estimate.motion, estimate.error, estimate.confidence
                        );
                    }
                    match estimate.motion {
                        stitch::Motion::Forward(delta) => {
                            record_motion_frame(
                                &mut s,
                                status,
                                pixbuf,
                                gray,
                                ManualDirection::Forward,
                                delta,
                                mode,
                            );
                            if matches!(mode, CaptureMode::Auto(_)) && !s.capture_halted {
                                s.auto_step_calibration.record_verified_normal_step(delta);
                            }
                        }
                        stitch::Motion::Stationary => {
                            if matches!(mode, CaptureMode::Auto(_)) {
                                s.consecutive_no_scroll += 1;
                                eprintln!(
                                    "scroll-capture: stationary auto cycle ({} consecutive)",
                                    s.consecutive_no_scroll
                                );
                                if s.consecutive_no_scroll >= 2 {
                                    s.auto_reached_end = true;
                                    status.set_text("End reached");
                                    if let Some(stop) = s.auto_scroll_stop.clone() {
                                        stop.store(true, Ordering::Release);
                                    }
                                    eprintln!(
                                        "scroll-capture: reached end of content; stopping auto-scroll"
                                    );
                                }
                            } else {
                                clear_manual_alignment_warning(&mut s, status);
                                let had_pending = s
                                    .capture
                                    .as_ref()
                                    .is_some_and(|capture| capture.pending_manual.is_some());
                                if had_pending {
                                    // Stationary relative to the committed
                                    // baseline means a small pending movement
                                    // was reversed; discard it and re-arm the
                                    // detector from this newly observed change.
                                    if let Some(capture) = s.capture.as_mut() {
                                        capture.pending_manual = None;
                                        if capture.stitch.frame_count() == 1 {
                                            capture.manual_direction = None;
                                        }
                                    }
                                    manual_progress(&mut s, status);
                                } else {
                                    manual_still(&mut s, status);
                                }
                            }
                        }
                        stitch::Motion::Ambiguous(candidate) => {
                            if mode.is_manual() {
                                let threshold = manual_coalesce_threshold(&pixbuf, mode.axis());
                                let action = manual_ambiguous_action(
                                    candidate,
                                    estimate.error,
                                    estimate.confidence,
                                    threshold,
                                );
                                match action {
                                    ManualAmbiguousAction::Accept(direction, delta) => {
                                        let signed = direction.signed_label(delta);
                                        eprintln!(
                                            "scroll-capture: accepting nearby repeated-content candidate ({signed} px)"
                                        );
                                        record_motion_frame(
                                            &mut s, status, pixbuf, gray, direction, delta, mode,
                                        );
                                    }
                                    warning => {
                                        show_manual_alignment_warning(&mut s, status, warning);
                                    }
                                }
                            } else {
                                eprintln!(
                                    "scroll-capture: rejected ambiguous auto-scroll candidate {candidate:+} px"
                                );
                                halt_capture(&mut s, status, "Stopped — capture lost alignment");
                            }
                        }
                        stitch::Motion::Reverse(delta) => {
                            if mode.is_manual() {
                                record_motion_frame(
                                    &mut s,
                                    status,
                                    pixbuf,
                                    gray,
                                    ManualDirection::Reverse,
                                    delta,
                                    mode,
                                );
                            } else {
                                eprintln!(
                                    "scroll-capture: rejected backward auto movement of {delta} px"
                                );
                                halt_capture(&mut s, status, "Stopped — content moved backward");
                            }
                        }
                        stitch::Motion::Unmatchable => {
                            eprintln!(
                                "scroll-capture: rejected frame without enough reliable overlap"
                            );
                            halt_capture(&mut s, status, "Stopped — retry with slower scrolling");
                        }
                    }
                }
                CaptureAnalysis::BeginAutoProbe(lookahead) => {
                    let estimate = lookahead.estimate();
                    eprintln!(
                        "scroll-capture: similar-looking auto frame {:?} (error={:.3}, confidence={:.3}); requesting one-notch confirmation",
                        estimate.motion, estimate.error, estimate.confidence
                    );
                    s.auto_alignment = Some(AutoAlignmentState::Probing {
                        lookahead,
                        first_pixbuf: pixbuf,
                        first_gray: gray,
                        stationary_probes: 0,
                    });
                    status.set_text("Verifying scroll alignment…");
                    status.remove_css_class("scroll-capture-status-error");
                    acknowledgement = AutoCaptureAcknowledgement::Probe;
                }
                CaptureAnalysis::ResolveAutoProbe(resolution) => {
                    let Some(AutoAlignmentState::Probing {
                        lookahead,
                        first_pixbuf,
                        first_gray,
                        stationary_probes,
                    }) = s.auto_alignment.take()
                    else {
                        halt_capture(&mut s, status, "Capture failed — retry");
                        return glib::ControlFlow::Break;
                    };
                    let calibrated_end_candidate = s
                        .auto_step_calibration
                        .endpoint_upper_bound()
                        .and_then(|bound| lookahead.unique_candidate_at_most(bound));
                    match auto_probe_decision(
                        resolution,
                        stationary_probes,
                        calibrated_end_candidate,
                    ) {
                        AutoProbeDecision::Commit { path, periodic } => {
                            if periodic {
                                eprintln!(
                                    "scroll-capture: accepted matcher-grade forward frames at +{} px then +{} px after automatic confirmation",
                                    path.first_delta, path.second_delta
                                );
                            } else {
                                eprintln!(
                                    "scroll-capture: confirmed forward frames at +{} px then +{} px",
                                    path.first_delta, path.second_delta
                                );
                            }
                            record_auto_alignment_path(
                                &mut s,
                                status,
                                first_pixbuf,
                                first_gray,
                                pixbuf,
                                gray,
                                path,
                                mode,
                            );
                            if !s.capture_halted {
                                s.auto_step_calibration
                                    .record_verified_normal_step(path.first_delta);
                                status.set_text("Auto-scrolling…");
                                status.remove_css_class("scroll-capture-status-error");
                            }
                        }
                        AutoProbeDecision::ProbeAgain => {
                            let confirmed = stationary_probes.saturating_add(1);
                            eprintln!(
                                "scroll-capture: confirmation probe was stationary ({confirmed}/{AUTO_END_CONFIRMATION_PROBES}); checking once more"
                            );
                            s.auto_alignment = Some(AutoAlignmentState::Probing {
                                lookahead,
                                first_pixbuf,
                                first_gray,
                                stationary_probes: confirmed,
                            });
                            status.set_text("Confirming end of content…");
                            status.remove_css_class("scroll-capture-status-error");
                            acknowledgement = AutoCaptureAcknowledgement::Probe;
                        }
                        AutoProbeDecision::End(candidate) => {
                            if calibrated_end_candidate == Some(candidate) {
                                eprintln!(
                                    "scroll-capture: stationary probes confirmed end; stable physical-step history isolated +{} px from repeated visual aliases",
                                    candidate.delta
                                );
                            } else {
                                eprintln!(
                                    "scroll-capture: stationary probes confirmed end with matcher-ranked +{} px forward seam",
                                    candidate.delta
                                );
                            }
                            record_motion_frame(
                                &mut s,
                                status,
                                first_pixbuf,
                                first_gray,
                                ManualDirection::Forward,
                                candidate.delta,
                                mode,
                            );
                            if !s.capture_halted {
                                s.auto_reached_end = true;
                                s.consecutive_no_scroll = AUTO_END_CONFIRMATION_PROBES as u32;
                                if let Some(stop) = s.auto_scroll_stop.clone() {
                                    stop.store(true, Ordering::Release);
                                }
                                if let Some(pill) = capturing_pill(status) {
                                    end_of_content_ui(window, &pill, status, sel);
                                } else {
                                    status.set_text("End reached");
                                    status.remove_css_class("scroll-capture-status-error");
                                }
                            }
                            // The worker observes its stop flag while blocked
                            // on this cycle; neither stationary probe should
                            // authorize another scroll.
                            acknowledgement = AutoCaptureAcknowledgement::Hold;
                        }
                        AutoProbeDecision::Pause {
                            reason,
                            best_effort,
                        } => {
                            let cycle = auto_cycle
                                .as_ref()
                                .map(|(_, cycle)| *cycle)
                                .expect("automatic probe must have a ready cycle");
                            match reason {
                                AutoProbePauseReason::StillAmbiguous => eprintln!(
                                    "scroll-capture: automatic lookahead remained uncertain{}",
                                    if best_effort.is_some() {
                                        "; a best-effort path is available"
                                    } else {
                                        ""
                                    }
                                ),
                                AutoProbePauseReason::Stationary => eprintln!(
                                    "scroll-capture: confirmation scroll did not move; pausing without appending a duplicate frame"
                                ),
                            }
                            let can_continue_anyway = best_effort.is_some();
                            s.auto_alignment = Some(AutoAlignmentState::Paused {
                                lookahead,
                                first_pixbuf,
                                first_gray,
                                second_pixbuf: pixbuf,
                                second_gray: gray,
                                best_effort,
                                reason,
                                cycle,
                            });
                            show_auto_alignment_pause_ui(window, status, sel, can_continue_anyway);
                            window.set_keyboard_mode(KeyboardMode::Exclusive);
                            acknowledgement = AutoCaptureAcknowledgement::Hold;
                        }
                    }
                }
                CaptureAnalysis::ResolveManualRecovery(resolution) => {
                    let Some(AutoAlignmentState::ManualRecovery {
                        lookahead,
                        first_pixbuf,
                        first_gray,
                        automatic_mode,
                    }) = s.auto_alignment.take()
                    else {
                        halt_capture(&mut s, status, "Capture failed — retry");
                        return glib::ControlFlow::Break;
                    };
                    match resolution {
                        stitch::ForwardLookaheadResolution::Resolved(path)
                        | stitch::ForwardLookaheadResolution::LowErrorPeriodic(path) => {
                            eprintln!(
                                "scroll-capture: manual continuation confirmed the pending gap at +{} px then +{} px",
                                path.first_delta, path.second_delta
                            );
                            record_auto_alignment_path(
                                &mut s,
                                status,
                                first_pixbuf,
                                first_gray,
                                pixbuf,
                                gray,
                                path,
                                automatic_mode,
                            );
                            if !s.capture_halted {
                                if let Some(capture) = s.capture.as_mut() {
                                    capture.manual_direction = Some(ManualDirection::Forward);
                                }
                                manual_progress(&mut s, status);
                                status.set_text("Scroll inside selection");
                                status.remove_css_class("scroll-capture-status-error");
                            }
                        }
                        stitch::ForwardLookaheadResolution::Unresolved { .. } => {
                            s.auto_alignment = Some(AutoAlignmentState::ManualRecovery {
                                lookahead,
                                first_pixbuf,
                                first_gray,
                                automatic_mode,
                            });
                            s.manual_stall.interrupt();
                            status.set_text("Keep scrolling a little farther to continue");
                            status.remove_css_class("scroll-capture-status-error");
                        }
                        stitch::ForwardLookaheadResolution::StationaryProbe { .. } => {
                            // The viewport has not advanced beyond the frame
                            // retained by the paused automatic capture. Keep
                            // all evidence provisional and wait for the user
                            // to reveal genuinely new pixels.
                            s.auto_alignment = Some(AutoAlignmentState::ManualRecovery {
                                lookahead,
                                first_pixbuf,
                                first_gray,
                                automatic_mode,
                            });
                            s.manual_stall.interrupt();
                            status.set_text("Keep scrolling a little farther to continue");
                            status.remove_css_class("scroll-capture-status-error");
                        }
                    }
                }
            }

            if s.capture_halted {
                window.set_keyboard_mode(KeyboardMode::Exclusive);
            }
            if let Some((handshake, cycle)) = auto_cycle {
                s.last_captured_cycle = cycle;
                drop(s);
                // The worker may inject its next wheel event only after this
                // screenshot and motion classification are both complete.
                match acknowledgement {
                    AutoCaptureAcknowledgement::Normal => handshake.acknowledge(cycle),
                    AutoCaptureAcknowledgement::Probe => {
                        handshake.acknowledge_with_scroll_notches(cycle, 1)
                    }
                    AutoCaptureAcknowledgement::Hold => {}
                }
            }
        }
        Err(e) => {
            // Leave an automatic cycle unacknowledged so the next timer tick
            // retries the same stable screen instead of scrolling past it.
            eprintln!("scroll-capture: capture_region failed: {e}");
            let mut s = state.borrow_mut();
            if s.capture_mode.is_some_and(CaptureMode::is_manual) {
                s.manual_stall.interrupt();
            }
            s.consecutive_capture_errors += 1;
            if s.consecutive_capture_errors >= MAX_CONSECUTIVE_CAPTURE_ERRORS {
                halt_capture(&mut s, status, "Capture failed repeatedly — retry");
                window.set_keyboard_mode(KeyboardMode::Exclusive);
            }
        }
    }
    if state.borrow().auto_reached_end
        || matches!(
            state.borrow().auto_alignment.as_ref(),
            Some(AutoAlignmentState::Paused { .. })
        )
    {
        glib::ControlFlow::Break
    } else {
        glib::ControlFlow::Continue
    }
}

#[allow(clippy::too_many_arguments)]
fn record_auto_alignment_path(
    state: &mut OverlayState,
    status: &gtk::Label,
    first_pixbuf: Pixbuf,
    first_gray: stitch::GrayView,
    second_pixbuf: Pixbuf,
    second_gray: stitch::GrayView,
    path: stitch::ForwardMatchPath,
    mode: CaptureMode,
) {
    record_motion_frame(
        state,
        status,
        first_pixbuf,
        first_gray,
        ManualDirection::Forward,
        path.first_delta,
        mode,
    );
    if state.capture_halted {
        return;
    }
    record_motion_frame(
        state,
        status,
        second_pixbuf,
        second_gray,
        ManualDirection::Forward,
        path.second_delta,
        mode,
    );
}

fn record_motion_frame(
    state: &mut OverlayState,
    status: &gtk::Label,
    pixbuf: Pixbuf,
    gray: stitch::GrayView,
    direction: ManualDirection,
    delta: usize,
    mode: CaptureMode,
) {
    let manual = mode.is_manual();
    if manual {
        let (locked, has_committed_band) = state
            .capture
            .as_ref()
            .map(|capture| (capture.manual_direction, capture.stitch.frame_count() > 1))
            .expect("motion estimate requires an initialized capture");
        if locked.is_some_and(|locked| locked != direction) {
            if has_committed_band {
                show_manual_alignment_warning(
                    state,
                    status,
                    ManualAmbiguousAction::ReturnTo(locked.expect("checked above")),
                );
                return;
            }

            // Before any band is committed, the user may cross the initial
            // viewport and choose the other direction without corrupting the
            // document-order invariant.
            let capture = state
                .capture
                .as_mut()
                .expect("motion estimate requires an initialized capture");
            capture.pending_manual = None;
            capture.manual_direction = Some(direction);
        } else if locked.is_none() {
            state
                .capture
                .as_mut()
                .expect("motion estimate requires an initialized capture")
                .manual_direction = Some(direction);
        }
    }

    let coalesce = manual && delta < manual_coalesce_threshold(&pixbuf, mode.axis());
    let previous_pending_delta = state
        .capture
        .as_ref()
        .and_then(|capture| capture.pending_manual.as_ref())
        .filter(|pending| pending.direction == direction)
        .map(|pending| pending.delta);
    let repeated_pending = coalesce && pending_delta_is_still(previous_pending_delta, delta);
    if repeated_pending {
        let capture = state
            .capture
            .as_mut()
            .expect("motion estimate requires an initialized capture");
        capture.pending_manual = Some(PendingManualFrame {
            pixbuf,
            delta,
            direction,
        });
        manual_still(state, status);
        eprintln!(
            "scroll-capture: pending manual movement unchanged at {} px",
            direction.signed_label(delta)
        );
        return;
    }

    if manual {
        manual_progress(state, status);
    }
    let capture = state
        .capture
        .as_mut()
        .expect("motion estimate requires an initialized capture");
    if coalesce {
        capture.pending_manual = Some(PendingManualFrame {
            pixbuf,
            delta,
            direction,
        });
        state.consecutive_no_scroll = 0;
        eprintln!(
            "scroll-capture: coalescing small manual movement ({} px)",
            direction.signed_label(delta)
        );
        return;
    }

    let append = match direction {
        ManualDirection::Forward => capture.stitch.push_forward(&pixbuf, delta),
        ManualDirection::Reverse => capture.stitch.push_reverse(&pixbuf, delta),
    };
    match append {
        Ok(()) => {
            capture.last_gray = gray;
            capture.pending_manual = None;
            let count = capture.stitch.frame_count();
            state.consecutive_no_scroll = 0;
            eprintln!(
                "scroll-capture: kept frame {count} ({} px)",
                direction.signed_label(delta)
            );
        }
        Err(error) => {
            eprintln!("scroll-capture: could not append incremental frame: {error}");
            halt_capture(state, status, "Capture failed — retry");
        }
    }
}

fn halt_capture(state: &mut OverlayState, status: &gtk::Label, message: &str) {
    state.capture_halted = true;
    state.manual_stall.reset();
    state.manual_alignment_warning = None;
    clear_manual_stall_ui(status);
    status.set_text(message);
    status.add_css_class("scroll-capture-status-error");
    if let Some(stop) = state.auto_scroll_stop.clone() {
        stop.store(true, Ordering::Release);
    }
}

fn capturing_done_button(status: &gtk::Label) -> Option<gtk::Button> {
    status
        .next_sibling()
        .and_then(|widget| widget.downcast::<gtk::Button>().ok())
}

fn capturing_pill(status: &gtk::Label) -> Option<gtk::Box> {
    status
        .parent()
        .and_then(|widget| widget.parent())
        .and_then(|widget| widget.downcast::<gtk::Box>().ok())
}

fn auto_alignment_pause_widgets(
    status: &gtk::Label,
) -> Option<(gtk::Box, gtk::Button, gtk::Button, gtk::Button)> {
    let pill = capturing_pill(status)?;
    let actions = pill.last_child()?.downcast::<gtk::Box>().ok()?;
    let continue_manual = actions.first_child()?.downcast::<gtk::Button>().ok()?;
    let continue_anyway = continue_manual
        .next_sibling()?
        .downcast::<gtk::Button>()
        .ok()?;
    let finish_here = continue_anyway
        .next_sibling()?
        .downcast::<gtk::Button>()
        .ok()?;
    Some((actions, continue_manual, continue_anyway, finish_here))
}

fn show_auto_alignment_pause_ui(
    window: &gtk::ApplicationWindow,
    status: &gtk::Label,
    selection: Selection,
    can_continue_anyway: bool,
) {
    status.set_text(AUTO_ALIGNMENT_PAUSE_MESSAGE);
    status.set_width_chars(-1);
    status.set_max_width_chars(36);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    status.set_single_line_mode(true);
    status.set_wrap(false);
    status.add_css_class("scroll-capture-status-error");
    if let Some(done) = capturing_done_button(status) {
        done.set_visible(false);
    }
    let Some((actions, continue_manual, continue_anyway, _)) = auto_alignment_pause_widgets(status)
    else {
        return;
    };
    continue_anyway.set_sensitive(can_continue_anyway);
    continue_anyway.set_visible(can_continue_anyway);
    actions.set_visible(true);
    continue_manual.grab_focus();

    // The choices add a second row and change the pill's natural size. Refresh
    // both placement and the layer-surface input region after GTK measures it.
    let Some(pill) = capturing_pill(status) else {
        return;
    };
    pill.add_css_class("scroll-capture-paused");
    let Some(overlay) = pill
        .parent()
        .and_then(|widget| widget.downcast::<gtk::Overlay>().ok())
    else {
        return;
    };
    position_capturing_pill_and_input(window, &overlay, &pill, selection);
    let window = window.clone();
    glib::idle_add_local_once(move || {
        position_capturing_pill_and_input(&window, &overlay, &pill, selection);
    });
}

fn restore_capturing_pill_ui(
    window: &gtk::ApplicationWindow,
    status: &gtk::Label,
    selection: Selection,
    message: &str,
) {
    status.set_text(message);
    status.set_width_chars(32);
    status.set_max_width_chars(32);
    status.set_single_line_mode(false);
    status.set_wrap(false);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    status.remove_css_class("scroll-capture-status-error");
    if let Some(done) = capturing_done_button(status) {
        done.set_visible(true);
    }
    if let Some((actions, _, _, _)) = auto_alignment_pause_widgets(status) {
        actions.set_visible(false);
    }
    let Some(pill) = capturing_pill(status) else {
        return;
    };
    pill.remove_css_class("scroll-capture-paused");
    let Some(overlay) = pill
        .parent()
        .and_then(|widget| widget.downcast::<gtk::Overlay>().ok())
    else {
        return;
    };
    position_capturing_pill_and_input(window, &overlay, &pill, selection);
}

fn show_manual_stall_ui(status: &gtk::Label) {
    let status = status.clone();
    // Apply the visual change in its own main-loop turn. The screencopy and
    // matcher run synchronously in the sampling callback; deferring here gives
    // GTK's frame clock a chance to paint the label and keyframe animation.
    glib::timeout_add_local_once(Duration::from_millis(1), move || {
        status.set_text("No movement — click Done or keep scrolling");
        status.remove_css_class("scroll-capture-status-error");
        if let Some(done) = capturing_done_button(&status) {
            done.add_css_class("scroll-capture-done-highlight");
        }
        let Some(pill) = capturing_pill(&status) else {
            return;
        };
        pill.add_css_class("scroll-capture-stall-cue");
        pill.add_css_class("scroll-capture-shake");
        pill.queue_draw();
        glib::timeout_add_local_once(Duration::from_millis(600), move || {
            pill.remove_css_class("scroll-capture-shake");
        });
    });
}

fn clear_manual_stall_ui(status: &gtk::Label) {
    status.set_text("Scroll inside selection");
    status.remove_css_class("scroll-capture-status-error");
    if let Some(done) = capturing_done_button(status) {
        done.remove_css_class("scroll-capture-done-highlight");
    }
    if let Some(pill) = capturing_pill(status) {
        pill.remove_css_class("scroll-capture-stall-cue");
        pill.remove_css_class("scroll-capture-shake");
    }
}

fn show_manual_alignment_warning(
    state: &mut OverlayState,
    status: &gtk::Label,
    action: ManualAmbiguousAction,
) {
    let Some(message) = action.status() else {
        return;
    };
    state.manual_stall.interrupt();
    if state.manual_alignment_warning == Some(action) {
        return;
    }
    state.manual_alignment_warning = Some(action);
    clear_manual_stall_ui(status);
    status.set_text(message);
    status.add_css_class("scroll-capture-status-error");
    eprintln!("scroll-capture: manual alignment warning: {message}");
}

fn clear_manual_alignment_warning(state: &mut OverlayState, status: &gtk::Label) -> bool {
    if state.manual_alignment_warning.take().is_none() {
        return false;
    }
    clear_manual_stall_ui(status);
    eprintln!("scroll-capture: manual alignment recovered");
    true
}

fn manual_progress(state: &mut OverlayState, status: &gtk::Label) {
    let clear_stall = state.manual_stall.movement();
    let clear_alignment = clear_manual_alignment_warning(state, status);
    if clear_stall && !clear_alignment {
        clear_manual_stall_ui(status);
    }
}

fn manual_still(state: &mut OverlayState, status: &gtk::Label) {
    let starting_stall = state.manual_stall.armed && state.manual_stall.still_since.is_none();
    if state.manual_stall.still(Instant::now()) {
        eprintln!("scroll-capture: manual content stopped changing; cueing Done");
        show_manual_stall_ui(status);
    } else if starting_stall {
        eprintln!("scroll-capture: manual content is stationary; waiting before cue");
    }
}

fn manual_coalesce_threshold(frame: &Pixbuf, axis: stitch::StitchAxis) -> usize {
    let axis_len = match axis {
        stitch::StitchAxis::Vertical => frame.height(),
        stitch::StitchAxis::Horizontal => frame.width(),
    }
    .max(1) as usize;
    (axis_len / 8)
        .clamp(16, 128)
        .min(axis_len.saturating_sub(1).max(1))
}

fn pending_delta_is_still(previous: Option<usize>, current: usize) -> bool {
    previous == Some(current)
}

fn wire_capturing_pill(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    controls: &CapturingControls,
    result: &Rc<RefCell<Option<ScrollCaptureOutcome>>>,
) {
    {
        let window = window.clone();
        let state = Rc::clone(state);
        controls.cancel.connect_clicked(move |_| {
            stop_capture_with_window(&state, &window);
            window.close();
        });
    }

    connect_finish_capture_button(&controls.done, state, window, &controls.status, result);
    connect_finish_capture_button(
        &controls.finish_here,
        state,
        window,
        &controls.status,
        result,
    );

    {
        let state = Rc::clone(state);
        let window = window.clone();
        let status = controls.status.clone();
        controls.continue_manual.connect_clicked(move |_| {
            continue_auto_alignment_manually(&state, &window, &status);
        });
    }

    {
        let state = Rc::clone(state);
        let window = window.clone();
        let status = controls.status.clone();
        controls.continue_anyway.connect_clicked(move |_| {
            continue_auto_alignment_anyway(&state, &window, &status);
        });
    }
}

fn continue_auto_alignment_manually(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    status: &gtk::Label,
) {
    let (timer, monitor, stop, selection, axis) = {
        let mut state = state.borrow_mut();
        let Some(paused_mode) = state.capture_mode else {
            status.set_text("Capture state changed — try again");
            status.add_css_class("scroll-capture-status-error");
            return;
        };
        let Some(AutoAlignmentState::Paused {
            lookahead,
            first_pixbuf,
            first_gray,
            ..
        }) = state.auto_alignment.take()
        else {
            return;
        };
        let manual_mode = manual_mode_preserving_axis(paused_mode);
        let axis = manual_mode.axis();
        state.auto_alignment = Some(AutoAlignmentState::ManualRecovery {
            lookahead,
            first_pixbuf,
            first_gray,
            automatic_mode: paused_mode,
        });
        let timer = state.capture_timer.take();
        let monitor = state.auto_scroll_monitor.take();
        let stop = state.auto_scroll_stop.take();
        state.auto_scroll_handshake = None;
        state.capture_mode = Some(manual_mode);
        state.last_captured_cycle = 0;
        state.consecutive_no_scroll = 0;
        state.capture_halted = false;
        state.auto_reached_end = false;
        state.manual_stall.reset();
        state.manual_alignment_warning = None;
        if let Some(capture) = state.capture.as_mut() {
            capture.pending_manual = None;
            capture.manual_direction =
                (capture.stitch.frame_count() > 1).then_some(ManualDirection::Forward);
        }
        (timer, monitor, stop, state.selection, axis)
    };
    if let Some(timer) = timer {
        timer.remove();
    }
    if let Some(monitor) = monitor {
        monitor.remove();
    }
    if let Some(stop) = stop {
        stop.store(true, Ordering::Release);
    }
    eprintln!("scroll-capture: user continued uncertain {axis:?} capture manually");
    window.set_keyboard_mode(KeyboardMode::None);
    restore_capturing_pill_ui(window, status, selection, "Scroll manually to continue");
    schedule_manual_resume_after_pill_commit(state, window, status, selection);
}

fn continue_auto_alignment_anyway(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    status: &gtk::Label,
) {
    if state.borrow().auto_scroll_handshake.is_none() {
        status.set_text("Auto-scroll stopped — click Finish here");
        status.add_css_class("scroll-capture-status-error");
        return;
    }
    let (handshake, cycle, selection, success, reached_end) = {
        let mut state = state.borrow_mut();
        let Some(mode) = state.capture_mode else {
            status.set_text("Capture state changed — click Finish here");
            status.add_css_class("scroll-capture-status-error");
            return;
        };
        let Some(handshake) = state.auto_scroll_handshake.clone() else {
            status.set_text("Auto-scroll stopped — click Finish here");
            status.add_css_class("scroll-capture-status-error");
            return;
        };
        let Some(AutoAlignmentState::Paused {
            lookahead,
            first_pixbuf,
            first_gray,
            second_pixbuf,
            second_gray,
            best_effort,
            reason,
            cycle,
        }) = state.auto_alignment.take()
        else {
            return;
        };
        let Some(best_effort) = best_effort else {
            state.auto_alignment = Some(AutoAlignmentState::Paused {
                lookahead,
                first_pixbuf,
                first_gray,
                second_pixbuf,
                second_gray,
                best_effort,
                reason,
                cycle,
            });
            return;
        };
        let AutoBestEffort::TwoFrames(path) = best_effort;
        record_auto_alignment_path(
            &mut state,
            status,
            first_pixbuf,
            first_gray,
            second_pixbuf,
            second_gray,
            path,
            mode,
        );
        let success = !state.capture_halted;
        let reached_end = success && accepting_auto_pause_reaches_end(reason);
        if success {
            state.unverified_auto_seams += 1;
            if reached_end {
                state.auto_reached_end = true;
                state.consecutive_no_scroll = AUTO_END_CONFIRMATION_PROBES as u32;
                if let Some(stop) = state.auto_scroll_stop.clone() {
                    stop.store(true, Ordering::Release);
                }
            } else {
                state.consecutive_no_scroll = 0;
            }
        }
        (handshake, cycle, state.selection, success, reached_end)
    };

    restore_capturing_pill_ui(
        window,
        status,
        selection,
        if reached_end {
            "End reached"
        } else if success {
            "Auto-scrolling…"
        } else {
            "Capture failed — click Done to save the confirmed part"
        },
    );
    if !success {
        status.add_css_class("scroll-capture-status-error");
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        return;
    }

    eprintln!("scroll-capture: user accepted one unverified automatic seam");
    if reached_end {
        if let Some(pill) = capturing_pill(status) {
            end_of_content_ui(window, &pill, status, selection);
        }
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        eprintln!("scroll-capture: accepted the final uncertain seam; endpoint already confirmed");
        return;
    }
    schedule_auto_resume_after_pill_commit(state, window, status, selection, handshake, cycle);
}

fn unverified_seam_warning(count: u32) -> Option<String> {
    match count {
        0 => None,
        1 => Some("Capture may contain a repeated or missing section.".to_string()),
        count => Some(format!(
            "Capture may contain {count} repeated or missing sections."
        )),
    }
}

fn connect_finish_capture_button(
    button: &gtk::Button,
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    status: &gtk::Label,
    result: &Rc<RefCell<Option<ScrollCaptureOutcome>>>,
) {
    let window = window.clone();
    let state = Rc::clone(state);
    let status = status.clone();
    let result = Rc::clone(result);
    button.connect_clicked(move |_| {
        let final_manual_selection = {
            let state = state.borrow();
            (state.capture_mode.is_some_and(CaptureMode::is_manual) && !state.capture_halted)
                .then_some(state.selection)
        };
        if let Some(selection) = final_manual_selection {
            // Capture once at the click boundary so movement in the final
            // timer interval is not omitted.
            let _ = capture_tick(&state, &window, selection, &status);
        }
        stop_capture_with_window(&state, &window);
        let (capture, axis, unverified_seams) = {
            let mut state = state.borrow_mut();
            let capture = state.capture.take();
            let axis = state
                .capture_mode
                .unwrap_or(CaptureMode::Manual(stitch::StitchAxis::Vertical))
                .axis();
            (capture, axis, state.unverified_auto_seams)
        };
        let Some(mut capture) = capture else {
            eprintln!("scroll-capture: Done — no frames were captured");
            window.close();
            return;
        };
        if let Some(pending) = capture.pending_manual.take() {
            let append = match pending.direction {
                ManualDirection::Forward => {
                    capture.stitch.push_forward(&pending.pixbuf, pending.delta)
                }
                ManualDirection::Reverse => {
                    capture.stitch.push_reverse(&pending.pixbuf, pending.delta)
                }
            };
            if let Err(error) = append {
                eprintln!("scroll-capture: could not finish pending manual frame: {error}");
                window.close();
                return;
            }
        }
        let count = capture.stitch.frame_count();
        eprintln!("scroll-capture: Done — finishing {count} {axis:?} frame(s)...");
        let t0 = std::time::Instant::now();
        match capture.stitch.finish() {
            Ok(pixbuf) => {
                eprintln!(
                    "scroll-capture: stitched output {}x{} in {:?}",
                    pixbuf.width(),
                    pixbuf.height(),
                    t0.elapsed()
                );
                let warning = unverified_seam_warning(unverified_seams);
                *result.borrow_mut() = Some(ScrollCaptureOutcome {
                    image: pixbuf,
                    warning,
                });
            }
            Err(error) => {
                eprintln!("scroll-capture: stitch failed: {error}");
            }
        }
        window.close();
    });
}

fn stop_capture_with_window(state: &Rc<RefCell<OverlayState>>, window: &gtk::ApplicationWindow) {
    // Re-grab the keyboard in case Cancel/Done is pressed during capture
    // (capture runs with KeyboardMode::None so keys pass through to the
    // app). Exclusive — not OnDemand — because Hyprland hands OnDemand
    // focus back to the previously focused toplevel, which sent Esc to
    // whatever window launched the capture instead of cancelling it.
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    stop_capture(state);
}

fn stop_capture(state: &Rc<RefCell<OverlayState>>) {
    let timer = state.borrow_mut().capture_timer.take();
    if let Some(t) = timer {
        t.remove();
    }
    let monitor = state.borrow_mut().auto_scroll_monitor.take();
    if let Some(m) = monitor {
        m.remove();
    }
    if let Some(stop) = state.borrow_mut().auto_scroll_stop.take() {
        stop.store(true, Ordering::Release);
    }
    if let Some(stop) = state.borrow_mut().pointer_focus_stop.take() {
        stop.store(true, Ordering::Release);
    }
    let mut s = state.borrow_mut();
    s.phase = Phase::Selected;
    s.manual_pointer_target = None;
    s.auto_scroll_handshake = None;
    s.auto_alignment = None;
    s.manual_stall.reset();
    s.manual_alignment_warning = None;
}

/// Start a lock-step real-wheel worker after `start_capture` has hidden the
/// selected auto-scroll button and made the selection input-transparent.
fn start_auto_scroll_at(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    capturing_pill: &gtk::Box,
    status: &gtk::Label,
    clicked_btn: &gtk::Button,
    direction: auto_scroll::ScrollDirection,
) {
    if state.borrow().auto_scroll_stop.is_some() {
        return;
    }

    // Park near the lower-right of the selection. This is usually empty page
    // or scrollbar gutter, which avoids hover animations while ensuring the
    // wheel targets a scrollable surface inside the selected application.
    let scale = clicked_btn.scale_factor().max(1);
    let sel = state.borrow().selection;
    let (cursor_x, cursor_y) = pointer_park_target(sel, scale);
    let output_name = state.borrow().output_name.clone();

    let state_w = Rc::clone(state);
    let window_w = window.clone();
    let pill_w = capturing_pill.clone();
    let status_w = status.clone();
    glib::idle_add_local_once(move || {
        if state_w.borrow().phase != Phase::Capturing {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let handshake = auto_scroll::CaptureHandshake::new();
        if let Err(e) = auto_scroll::spawn_worker(
            Arc::clone(&stop),
            handshake.clone(),
            cursor_x,
            cursor_y,
            direction,
            output_name.as_deref(),
        ) {
            eprintln!("scroll-capture: auto-scroll failed to start: {e}");
            // Capture is still useful without injection: transparently fall
            // back to the same manual sampler the Start Capture button uses.
            state_w.borrow_mut().capture_mode =
                Some(CaptureMode::Manual(CaptureMode::Auto(direction).axis()));
            status_w.set_text("Auto-scroll unavailable — scroll manually");
            status_w.add_css_class("scroll-capture-status-error");
            return;
        }
        {
            let mut s = state_w.borrow_mut();
            s.auto_scroll_stop = Some(stop);
            s.auto_scroll_handshake = Some(handshake);
            s.last_captured_cycle = 0;
            s.consecutive_no_scroll = 0;
        }

        let monitor = {
            let state = Rc::clone(&state_w);
            let pill = pill_w.clone();
            let status = status_w.clone();
            let window = window_w.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || {
                let mut s = state.borrow_mut();
                let Some(stop) = s.auto_scroll_stop.clone() else {
                    return glib::ControlFlow::Break;
                };
                if !stop.load(Ordering::Acquire) {
                    return glib::ControlFlow::Continue;
                }
                s.auto_scroll_stop = None;
                s.auto_scroll_handshake = None;
                s.auto_scroll_monitor = None;
                let capture_timer = s.capture_timer.take();
                let reached_end = s.auto_reached_end;
                let halted = s.capture_halted;
                let selection = s.selection;
                if !reached_end && !halted {
                    s.capture_halted = true;
                }
                drop(s);
                if let Some(timer) = capture_timer {
                    timer.remove();
                }
                window.set_keyboard_mode(KeyboardMode::Exclusive);
                if reached_end {
                    end_of_content_ui(&window, &pill, &status, selection);
                } else {
                    if !halted {
                        status.set_text("Auto-scroll stopped unexpectedly");
                        status.add_css_class("scroll-capture-status-error");
                    }
                }
                glib::ControlFlow::Break
            })
        };
        state_w.borrow_mut().auto_scroll_monitor = Some(monitor);
    });
}

fn end_of_content_ui(
    window: &gtk::ApplicationWindow,
    capturing_pill: &gtk::Box,
    status: &gtk::Label,
    selection: Selection,
) {
    // Highlight Done, but do not restore the direction buttons: a capture's
    // axis and frame sequence stay locked until the user finishes or cancels.
    // Automatic sampling has already stopped before either caller reaches
    // this function, so the compact terminal controls may safely overlap the
    // captured rectangle when the only outside gutter is visually far away.
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    if let Some(done) = capturing_done_button(status) {
        done.add_css_class("scroll-capture-done-highlight");
    }
    status.set_text("End reached");
    status.remove_css_class("scroll-capture-status-error");

    let Some(overlay) = capturing_pill
        .parent()
        .and_then(|widget| widget.downcast::<gtk::Overlay>().ok())
    else {
        return;
    };
    position_settled_pill_and_input(window, &overlay, capturing_pill, selection);

    // The shorter status text changes the natural width. Re-run placement
    // after GTK has measured that terminal state and update the input region
    // to the final bounds in the same main-loop turn.
    let window = window.clone();
    let pill = capturing_pill.clone();
    glib::idle_add_local_once(move || {
        position_settled_pill_and_input(&window, &overlay, &pill, selection);
    });
}

fn build_prompt_pill() -> gtk::Box {
    let pill = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    pill.add_css_class("scroll-capture-pill");
    pill.add_css_class("scroll-capture-prompt");
    // Names the way out as well as the way in: this overlay is modal
    // and full-screen, and A is how you get to an ordinary capture
    // without closing it and pressing a different keybind.
    let label = gtk::Label::new(Some(
        "Drag to capture the scrolling part of the screen  ·  A: normal capture",
    ));
    label.add_css_class("scroll-capture-prompt-label");
    pill.append(&label);
    pill
}

fn build_action_pill() -> gtk::Box {
    let pill = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    pill.add_css_class("scroll-capture-pill");
    pill.add_css_class("scroll-capture-actions");

    let cancel = gtk::Button::with_label("\u{2715}  Cancel");
    cancel.add_css_class("scroll-capture-button");
    cancel.add_css_class("scroll-capture-cancel");
    pill.append(&cancel);

    let start = gtk::Button::with_label("\u{2195}  Manual Scroll");
    start.add_css_class("scroll-capture-button");
    start.add_css_class("scroll-capture-primary");
    // GTK tooltips are separate compositor surfaces. A visible tooltip can
    // outlive this button for a frame after capture starts and get baked into
    // the initial screenshot, so keep this guidance accessibility-only.
    start.update_property(&[gtk::accessible::Property::Description(
        "Start capturing, then scroll the selected content yourself",
    )]);
    pill.append(&start);

    pill
}

fn build_capturing_pill() -> CapturingControls {
    let pill = gtk::Box::new(gtk::Orientation::Vertical, 4);
    pill.add_css_class("scroll-capture-pill");
    pill.add_css_class("scroll-capture-actions");
    pill.add_css_class("scroll-capture-capturing");

    let capture_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    pill.append(&capture_row);

    let cancel = gtk::Button::with_label("\u{2715}  Cancel");
    cancel.add_css_class("scroll-capture-button");
    cancel.add_css_class("scroll-capture-cancel");
    capture_row.append(&cancel);

    let status = gtk::Label::new(None);
    status.add_css_class("scroll-capture-status");
    status.set_width_chars(32);
    status.set_max_width_chars(32);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    capture_row.append(&status);

    let done = gtk::Button::with_label("\u{2713}  Done");
    done.add_css_class("scroll-capture-button");
    done.add_css_class("scroll-capture-primary");
    capture_row.append(&done);

    let pause_actions = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    pause_actions.add_css_class("scroll-capture-pause-actions");
    pause_actions.set_halign(gtk::Align::Center);
    pause_actions.set_visible(false);
    pill.append(&pause_actions);

    let continue_manual = build_pause_action_button(CONTINUE_MANUALLY_TITLE);
    continue_manual.add_css_class("scroll-capture-primary");
    continue_manual.update_property(&[
        gtk::accessible::Property::Label(CONTINUE_MANUALLY_TITLE),
        gtk::accessible::Property::Description(
            "Continue capturing while you scroll the selected content yourself",
        ),
    ]);
    pause_actions.append(&continue_manual);

    let continue_anyway = build_pause_action_button(CONTINUE_ANYWAY_TITLE);
    continue_anyway.add_css_class("scroll-capture-warning-action");
    continue_anyway.update_property(&[
        gtk::accessible::Property::Label(CONTINUE_ANYWAY_TITLE),
        gtk::accessible::Property::Description(
            "Continue automatic capture despite the uncertainty; the result may repeat or skip content",
        ),
    ]);
    pause_actions.append(&continue_anyway);

    let finish_here = build_pause_action_button(FINISH_HERE_TITLE);
    finish_here.update_property(&[
        gtk::accessible::Property::Label(FINISH_HERE_TITLE),
        gtk::accessible::Property::Description(
            "Finish with only the part of the capture whose alignment was confirmed",
        ),
    ]);
    pause_actions.append(&finish_here);

    CapturingControls {
        pill,
        status,
        cancel,
        done,
        continue_manual,
        continue_anyway,
        finish_here,
    }
}

fn build_pause_action_button(title: &str) -> gtk::Button {
    let button = gtk::Button::with_label(title);
    button.add_css_class("scroll-capture-button");
    button.add_css_class("scroll-capture-pause-button");
    button
}

/// Vertical Auto-Scroll button — small dark pill with ▼ icon, anchored
/// bottom-center inside the selection. Click sends real wheel events.
///
/// Forces a fixed size_request so positioning + the surface input-region
/// rect agree on the button's bounds even before its first allocation.
/// Without this, `measure()` on a never-shown button can return values
/// smaller than the eventual allocation (CSS not applied yet), which both
/// off-centers the pill and leaves part of it outside the input region —
/// causing clicks to fall through to the underlying app.
fn build_inside_vert_auto_scroll() -> gtk::Button {
    let btn = gtk::Button::with_label("\u{25BC}  Auto-Scroll");
    btn.add_css_class("scroll-capture-button");
    btn.add_css_class("scroll-capture-auto");
    btn.add_css_class("scroll-capture-inside-auto");
    btn.update_property(&[gtk::accessible::Property::Description(
        "Capture while automatically scrolling down",
    )]);
    btn.set_size_request(VERT_AUTO_SCROLL_W as i32, VERT_AUTO_SCROLL_H as i32);
    btn
}

/// Horizontal Auto-Scroll button — labeled pill anchored right-center
/// inside the selection, mirroring the vertical button's treatment so the
/// direction is legible without hovering. Click sends real horizontal
/// wheel events. Both direction buttons are always offered: Wayland has
/// no protocol for asking the underlying window which axes it can
/// scroll, so hiding one would just guess wrong.
fn build_inside_horiz_auto_scroll() -> gtk::Button {
    let btn = gtk::Button::with_label("\u{25B6}  Auto-Scroll");
    btn.add_css_class("scroll-capture-button");
    btn.add_css_class("scroll-capture-auto");
    btn.add_css_class("scroll-capture-inside-auto");
    btn.update_property(&[gtk::accessible::Property::Description(
        "Capture while automatically scrolling right",
    )]);
    btn.set_size_request(HORIZ_AUTO_SCROLL_W as i32, HORIZ_AUTO_SCROLL_H as i32);
    btn
}

const VERT_AUTO_SCROLL_W: f64 = 150.0;
const VERT_AUTO_SCROLL_H: f64 = 40.0;
const HORIZ_AUTO_SCROLL_W: f64 = 150.0;
const HORIZ_AUTO_SCROLL_H: f64 = 40.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PillMargins {
    start: i32,
    end: i32,
    top: i32,
    bottom: i32,
}

impl PillMargins {
    fn from_widget(pill: &gtk::Box) -> Self {
        Self {
            start: pill.margin_start(),
            end: pill.margin_end(),
            top: pill.margin_top(),
            bottom: pill.margin_bottom(),
        }
    }

    fn apply(self, pill: &gtk::Box) {
        pill.set_margin_start(self.start);
        pill.set_margin_end(self.end);
        pill.set_margin_top(self.top);
        pill.set_margin_bottom(self.bottom);
    }
}

fn with_temporary_margin_state<T>(
    saved: PillMargins,
    mut apply: impl FnMut(PillMargins),
    operation: impl FnOnce() -> T,
) -> T {
    apply(PillMargins::default());
    let result = operation();
    apply(saved);
    result
}

fn pill_natural_size(pill: &gtk::Box) -> (f64, f64) {
    let saved = PillMargins::from_widget(pill);
    // Positional margins are part of GTK's measurement contract, not merely
    // values added to the returned extent. Subtracting them afterward leaves
    // dependent-axis measurement and cached layout contaminated by the pill's
    // previous screen position. Measure from a genuinely margin-neutral state
    // and then put the widget back exactly as it was.
    with_temporary_margin_state(
        saved,
        |margins| margins.apply(pill),
        || {
            let (_, w_nat, _, _) = pill.measure(gtk::Orientation::Horizontal, -1);
            let (_, h_nat, _, _) = pill.measure(gtk::Orientation::Vertical, w_nat);
            (w_nat.max(1) as f64, h_nat.max(1) as f64)
        },
    )
}

fn position_selected_controls_and_input(
    state: &Rc<RefCell<OverlayState>>,
    window: &gtk::ApplicationWindow,
    pill: &gtk::Box,
    vert_btn: &gtk::Button,
    horiz_btn: &gtk::Button,
    sel: Selection,
) {
    let state = Rc::clone(state);
    let window = window.clone();
    let pill = pill.clone();
    let vert_btn = vert_btn.clone();
    let horiz_btn = horiz_btn.clone();
    glib::idle_add_local_once(move || {
        {
            let current = state.borrow();
            if current.phase != Phase::Selected || current.selection != sel {
                return;
            }
        }
        let (pw, ph) = measured_pill_size(&pill);
        // Keep Manual Scroll inside the selected content. Once the pill is
        // hidden, the pointer is already over the intended scroll target; a
        // tiny generated nudge refreshes compositor focus without moving it.
        let x = (sel.x + (sel.w - pw) / 2.0)
            .max(sel.x + 4.0)
            .min((sel.x + sel.w - pw - 4.0).max(sel.x + 4.0));
        let y = (sel.y + 16.0)
            .max(sel.y + 4.0)
            .min((sel.y + sel.h - ph - 4.0).max(sel.y + 4.0));
        pill.set_margin_start(x as i32);
        pill.set_margin_top(y as i32);

        // Vertical Auto-Scroll: bottom-center inside the selection.
        let (vw, vh) = (VERT_AUTO_SCROLL_W, VERT_AUTO_SCROLL_H);
        let vx = (sel.x + (sel.w - vw) / 2.0)
            .max(sel.x + 4.0)
            .min((sel.x + sel.w - vw - 4.0).max(sel.x + 4.0));
        let vy = (sel.y + sel.h - INSIDE_AUTO_SCROLL_INSET - vh).max(sel.y + 4.0);
        vert_btn.set_margin_start(vx as i32);
        vert_btn.set_margin_top(vy as i32);
        vert_btn.set_visible(true);

        // Horizontal Auto-Scroll: right-center inside the selection.
        let (hw, hh) = (HORIZ_AUTO_SCROLL_W, HORIZ_AUTO_SCROLL_H);
        let hx = (sel.x + sel.w - INSIDE_AUTO_SCROLL_INSET - hw).max(sel.x + 4.0);
        let hy = (sel.y + (sel.h - hh) / 2.0)
            .max(sel.y + 4.0)
            .min((sel.y + sel.h - hh - 4.0).max(sel.y + 4.0));
        horiz_btn.set_margin_start(hx as i32);
        horiz_btn.set_margin_top(hy as i32);
        horiz_btn.set_visible(true);

        let Some(surface) = window.surface() else {
            return;
        };
        // The overlay owns everything except the page exposed inside the
        // region: the dimmed surround (drag there to redraw), the edge bands
        // and move handle, the pill and the mode buttons. The exposed page
        // receives the pointer, and — while the pointer is over it — the
        // keyboard (see `update_selected_keyboard_zone`).
        let band = EDGE_HIT_SLACK as i32;
        let sx = sel.x as i32;
        let sy = sel.y as i32;
        let sw = sel.w as i32;
        let sh = sel.h as i32;
        let region = cairo::Region::create_rectangle(&cairo::RectangleInt::new(
            0,
            0,
            surface.width().max(1),
            surface.height().max(1),
        ));
        region
            .subtract_rectangle(&cairo::RectangleInt::new(sx, sy, sw.max(0), sh.max(0)))
            .ok();
        let pill_pad: i32 = 6;
        region
            .union_rectangle(&cairo::RectangleInt::new(
                x as i32 - pill_pad,
                y as i32 - pill_pad,
                pw as i32 + 2 * pill_pad,
                ph as i32 + 2 * pill_pad,
            ))
            .ok();

        // Edge bands and the move handle keep selection editing available.
        let bands = [
            cairo::RectangleInt::new(sx - band, sy - band, sw + 2 * band, 2 * band),
            cairo::RectangleInt::new(sx - band, sy + sh - band, sw + 2 * band, 2 * band),
            cairo::RectangleInt::new(sx - band, sy - band, 2 * band, sh + 2 * band),
            cairo::RectangleInt::new(sx + sw - band, sy - band, 2 * band, sh + 2 * band),
        ];
        for b in &bands {
            region.union_rectangle(b).ok();
        }

        let (mcx, mcy) = ResizeHandle::Move.center(sel);
        let mr = MOVE_HANDLE_RADIUS as i32 + 6;
        region
            .union_rectangle(&cairo::RectangleInt::new(
                mcx as i32 - mr,
                mcy as i32 - mr,
                2 * mr,
                2 * mr,
            ))
            .ok();

        // Include both pre-capture mode controls in the selected-phase input
        // region. They are removed from it before any screenshot is taken.
        let control_pad: i32 = 4;
        for rect in [
            cairo::RectangleInt::new(
                vx as i32 - control_pad,
                vy as i32 - control_pad,
                vw as i32 + 2 * control_pad,
                vh as i32 + 2 * control_pad,
            ),
            cairo::RectangleInt::new(
                hx as i32 - control_pad,
                hy as i32 - control_pad,
                hw as i32 + 2 * control_pad,
                hh as i32 + 2 * control_pad,
            ),
        ] {
            region.union_rectangle(&rect).ok();
        }
        surface.set_input_region(&region);
    });
}

/// Distance the auto-scroll choices sit from the selection's bottom/right.
const INSIDE_AUTO_SCROLL_INSET: f64 = 40.0;

fn position_capturing_pill_and_input(
    window: &gtk::ApplicationWindow,
    overlay: &gtk::Overlay,
    pill: &gtk::Box,
    sel: Selection,
) {
    // Cancel/Done stays outside the capture rectangle. It is the overlay's
    // entire input region while capturing, leaving the selected application
    // fully available for physical or generated scrolling.
    let (pw, ph) = pill_natural_size(pill);
    let preferred = capture_pill_position(
        overlay.allocated_width() as f64,
        overlay.allocated_height() as f64,
        pw,
        ph,
        sel,
    );
    let pause_actions_visible = pill.last_child().is_some_and(|widget| widget.is_visible());
    if preferred.is_none() && !pause_actions_visible {
        eprintln!(
            "scroll-capture: capture controls no longer fit outside the selected region (overlay {}x{}, pill {pw:.0}x{ph:.0})",
            overlay.allocated_width(),
            overlay.allocated_height()
        );
        return;
    }
    let position = if pause_actions_visible {
        // The paused choice panel can be larger than the normal Cancel/Done
        // row that was validated before capture. Sampling is stopped while it
        // is visible, so anchor the expanded controls inside the selection.
        // Resuming first hides this row and restores the normal outside-only
        // capture position.
        settled_pill_position(
            overlay.allocated_width() as f64,
            overlay.allocated_height() as f64,
            pw,
            ph,
            sel,
        )
    } else {
        preferred
    };
    let Some((x, y)) = position else {
        eprintln!(
            "scroll-capture: paused controls do not fit on overlay {}x{} (pill {pw:.0}x{ph:.0})",
            overlay.allocated_width(),
            overlay.allocated_height()
        );
        return;
    };
    if pause_actions_visible {
        eprintln!(
            "scroll-capture: positioning stopped controls with the selection (overlay {}x{}, pill {pw:.0}x{ph:.0})",
            overlay.allocated_width(),
            overlay.allocated_height()
        );
    }
    eprintln!(
        "scroll-capture: positioning capture controls at ({x:.0}, {y:.0}), size {pw:.0}x{ph:.0}"
    );
    apply_capturing_pill_position_and_input(window, pill, x, y, pw, ph);
}

/// Sampling has stopped, so a terminal compact pill uses the same inside-edge
/// placement policy as the expanded pause panel. This keeps Finish controls
/// visually attached to the captured area without weakening the strict
/// outside-only rule used while frames are still being collected.
fn position_settled_pill_and_input(
    window: &gtk::ApplicationWindow,
    overlay: &gtk::Overlay,
    pill: &gtk::Box,
    sel: Selection,
) {
    let (pw, ph) = pill_natural_size(pill);
    let Some((x, y)) = settled_pill_position(
        overlay.allocated_width() as f64,
        overlay.allocated_height() as f64,
        pw,
        ph,
        sel,
    ) else {
        eprintln!(
            "scroll-capture: finished controls do not fit on overlay {}x{} (pill {pw:.0}x{ph:.0})",
            overlay.allocated_width(),
            overlay.allocated_height()
        );
        return;
    };
    eprintln!(
        "scroll-capture: positioning finished controls at ({x:.0}, {y:.0}), size {pw:.0}x{ph:.0}"
    );
    apply_capturing_pill_position_and_input(window, pill, x, y, pw, ph);
}

fn apply_capturing_pill_position_and_input(
    window: &gtk::ApplicationWindow,
    pill: &gtk::Box,
    x: f64,
    y: f64,
    pill_w: f64,
    pill_h: f64,
) {
    pill.set_margin_start(x as i32);
    pill.set_margin_top(y as i32);

    let Some(surface) = window.surface() else {
        return;
    };
    let pad: i32 = 6;
    let region = cairo::Region::create_rectangle(&cairo::RectangleInt::new(
        x as i32 - pad,
        y as i32 - pad,
        pill_w as i32 + 2 * pad,
        pill_h as i32 + 2 * pad,
    ));
    surface.set_input_region(&region);
}

fn capture_pill_position(
    overlay_w: f64,
    overlay_h: f64,
    pill_w: f64,
    pill_h: f64,
    sel: Selection,
) -> Option<(f64, f64)> {
    let (max_x, max_y) = pill_position_limits(overlay_w, overlay_h, pill_w, pill_h)?;

    let centered_x = (sel.x + (sel.w - pill_w) / 2.0).clamp(PILL_EDGE_PAD, max_x);
    let centered_y = (sel.y + (sel.h - pill_h) / 2.0).clamp(PILL_EDGE_PAD, max_y);
    let mut candidates = Vec::with_capacity(4);

    let below_y = sel.y + sel.h + PILL_GAP;
    if below_y <= max_y {
        candidates.push((centered_x, below_y));
    }
    let above_y = sel.y - pill_h - PILL_GAP;
    if above_y >= PILL_EDGE_PAD {
        candidates.push((centered_x, above_y));
    }

    let right_x = sel.x + sel.w + PILL_GAP;
    if right_x <= max_x {
        candidates.push((right_x, centered_y));
    }
    let left_x = sel.x - pill_w - PILL_GAP;
    if left_x >= PILL_EDGE_PAD {
        candidates.push((left_x, centered_y));
    }

    minimum_score_position(&candidates, |(x, y)| {
        pill_center_distance_squared(x, y, pill_w, pill_h, sel)
    })
}

const PILL_EDGE_PAD: f64 = 8.0;

fn pill_position_limits(
    overlay_w: f64,
    overlay_h: f64,
    pill_w: f64,
    pill_h: f64,
) -> Option<(f64, f64)> {
    let max_x = overlay_w - pill_w - PILL_EDGE_PAD;
    let max_y = overlay_h - pill_h - PILL_EDGE_PAD;
    (max_x >= PILL_EDGE_PAD && max_y >= PILL_EDGE_PAD).then_some((max_x, max_y))
}

/// Once sampling has stopped, keep pause/finish controls attached to the
/// content they describe by placing them inside the selection. Active capture
/// uses `capture_pill_position` directly and therefore retains its strict
/// outside-only rule.
fn settled_pill_position(
    overlay_w: f64,
    overlay_h: f64,
    pill_w: f64,
    pill_h: f64,
    sel: Selection,
) -> Option<(f64, f64)> {
    settled_pill_inside_position(overlay_w, overlay_h, pill_w, pill_h, sel)
        .or_else(|| settled_pill_overlap_position(overlay_w, overlay_h, pill_w, pill_h, sel))
}

/// Place stopped-sampling controls as close as possible to the selected edge
/// when the selection is too small to contain them. Each candidate starts
/// adjacent to a different selection edge and is clamped onto the output. The
/// candidate that obscures the least selected content wins.
fn settled_pill_overlap_position(
    overlay_w: f64,
    overlay_h: f64,
    pill_w: f64,
    pill_h: f64,
    sel: Selection,
) -> Option<(f64, f64)> {
    let (max_x, max_y) = pill_position_limits(overlay_w, overlay_h, pill_w, pill_h)?;
    let centered_x = (sel.x + (sel.w - pill_w) / 2.0).clamp(PILL_EDGE_PAD, max_x);
    let centered_y = (sel.y + (sel.h - pill_h) / 2.0).clamp(PILL_EDGE_PAD, max_y);
    let candidates = [
        (
            centered_x,
            (sel.y + sel.h + PILL_GAP).clamp(PILL_EDGE_PAD, max_y),
        ),
        (
            centered_x,
            (sel.y - pill_h - PILL_GAP).clamp(PILL_EDGE_PAD, max_y),
        ),
        (
            (sel.x + sel.w + PILL_GAP).clamp(PILL_EDGE_PAD, max_x),
            centered_y,
        ),
        (
            (sel.x - pill_w - PILL_GAP).clamp(PILL_EDGE_PAD, max_x),
            centered_y,
        ),
    ];
    minimum_score_position(&candidates, |(x, y)| {
        pill_selection_overlap_area(x, y, pill_w, pill_h, sel)
    })
}

/// Place stopped-sampling controls fully inside the visible part of the
/// selection. Bottom-center is the stable preference for vertical scrolling;
/// the inset shrinks only when the selection has no room to spare. Returning
/// `None` means full containment is impossible and lets the caller use its
/// close-to-selection fallback.
fn settled_pill_inside_position(
    overlay_w: f64,
    overlay_h: f64,
    pill_w: f64,
    pill_h: f64,
    sel: Selection,
) -> Option<(f64, f64)> {
    pill_position_limits(overlay_w, overlay_h, pill_w, pill_h)?;

    // Restrict anchors to the visible, padded portion of the selection so a
    // selection touching an output edge never clips its controls off-screen.
    let left = sel.x.max(PILL_EDGE_PAD);
    let top = sel.y.max(PILL_EDGE_PAD);
    let right = (sel.x + sel.w).min(overlay_w - PILL_EDGE_PAD);
    let bottom = (sel.y + sel.h).min(overlay_h - PILL_EDGE_PAD);
    let available_w = right - left;
    let available_h = bottom - top;
    if available_w < pill_w || available_h < pill_h {
        return None;
    }

    let max_x = right - pill_w;
    let max_y = bottom - pill_h;
    let centered_x = (sel.x + (sel.w - pill_w) / 2.0).clamp(left, max_x);
    let vertical_inset = PILL_GAP.min((available_h - pill_h).max(0.0));
    Some((centered_x, max_y - vertical_inset))
}

fn minimum_score_position(
    candidates: &[(f64, f64)],
    score: impl Fn((f64, f64)) -> f64,
) -> Option<(f64, f64)> {
    let mut best: Option<((f64, f64), f64)> = None;
    for &candidate in candidates {
        let candidate_score = score(candidate);
        if best
            .as_ref()
            .is_none_or(|(_, best_score)| candidate_score.total_cmp(best_score).is_lt())
        {
            best = Some((candidate, candidate_score));
        }
    }
    best.map(|(candidate, _)| candidate)
}

fn pill_center_distance_squared(x: f64, y: f64, pill_w: f64, pill_h: f64, sel: Selection) -> f64 {
    let dx = x + pill_w / 2.0 - (sel.x + sel.w / 2.0);
    let dy = y + pill_h / 2.0 - (sel.y + sel.h / 2.0);
    dx * dx + dy * dy
}

fn pill_selection_overlap_area(x: f64, y: f64, pill_w: f64, pill_h: f64, sel: Selection) -> f64 {
    let overlap_w = (x + pill_w).min(sel.x + sel.w) - x.max(sel.x);
    let overlap_h = (y + pill_h).min(sel.y + sel.h) - y.max(sel.y);
    overlap_w.max(0.0) * overlap_h.max(0.0)
}

fn show_selection_space_warning(action_pill: &gtk::Box) {
    let Some(button) = action_pill
        .last_child()
        .and_then(|child| child.downcast::<gtk::Button>().ok())
    else {
        return;
    };
    button.set_label("Leave room outside selection");
    button.add_css_class("scroll-capture-warning");
    glib::timeout_add_local_once(Duration::from_secs(3), move || {
        button.set_label("↕  Manual Scroll");
        button.remove_css_class("scroll-capture-warning");
    });
}

fn measured_pill_size(pill: &gtk::Box) -> (f64, f64) {
    // Allocations retain the widget's previous overlay placement and may be
    // stale immediately after pause/end content changes. Use the same
    // margin-neutral natural size for every placement and input-region update.
    pill_natural_size(pill)
}

fn draw_backdrop(cr: &cairo::Context, w: f64, h: f64, s: &OverlayState) {
    let _ = cr.save();
    cr.set_operator(cairo::Operator::Source);

    cr.set_source_rgba(0.0, 0.0, 0.0, BACKDROP_ALPHA);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    let active_rect = match s.phase {
        Phase::Dragging | Phase::Selected | Phase::Capturing => Some(s.selection),
        Phase::AwaitingDrag => None,
    };

    if let Some(sel) = active_rect {
        // Punch the selection clear so the underlying screen shows through.
        // Snap to integer logical coords so Cairo's anti-aliasing doesn't
        // partially-clear the boundary rows: a partially-cleared row keeps
        // some of our dark backdrop, which becomes a faint dark line at
        // every frame seam in the stitched output.
        cr.set_operator(cairo::Operator::Clear);
        cr.rectangle(sel.x.round(), sel.y.round(), sel.w.round(), sel.h.round());
        let _ = cr.fill();

        cr.set_operator(cairo::Operator::Over);
        // Subtle outline at the selection edge for visual definition.
        // SKIPPED while a capture session is sampling OR assembling: even
        // though the stroke is mathematically half a pixel outside the
        // selection's pixel boundary, Cairo's anti-aliasing bleeds a tiny
        // fraction of the outline's alpha into the boundary row of the
        // selection, and that row lands in every captured frame as a seam
        // line. Gating on `Phase::Capturing` alone is NOT enough: the
        // end-of-capture transition (End reached → pill reshown) can
        // repaint this outline while the final stationary-confirmation
        // frames are still being sampled — which stitched a single white
        // line into long captures near their bottom edge. `capture` stays
        // `Some` from the first kept frame until the stitch is finalized
        // or cancelled, so gate on it too.
        let capture_session_active = matches!(s.phase, Phase::Capturing) || s.capture.is_some();
        if !capture_session_active {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.55);
            cr.set_line_width(1.0);
            cr.rectangle(sel.x - 0.5, sel.y - 0.5, sel.w + 1.0, sel.h + 1.0);
            let _ = cr.stroke();
        }

        match s.phase {
            // Selected: full handle set (brackets + edge bars + Move) so
            // the user can edit the selection — unless capture frames may
            // still be sampled (see the outline gate above); the opaque
            // white handle bars are even more visible in a leaked frame
            // than the thin outline.
            Phase::Selected if !capture_session_active => draw_handles(cr, sel),
            Phase::Selected => {}
            // Capturing: handles intentionally HIDDEN. They'd otherwise
            // end up baked into every captured frame.
            Phase::Capturing => {}
            // Mid-drag: minimal corner-bracket affordance.
            _ => {
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
                cr.set_line_width(BRACKET_WIDTH);
                draw_corner_brackets(cr, sel);
            }
        }
    }
    let _ = cr.restore();
}

fn draw_handles(cr: &cairo::Context, sel: Selection) {
    cr.set_operator(cairo::Operator::Over);
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.set_line_width(CROP_STROKE_WIDTH);
    cr.set_line_cap(cairo::LineCap::Square);
    cr.set_line_join(cairo::LineJoin::Miter);

    // Corner L-brackets — arms extend INWARD from each corner, like the
    // crop tool's brackets.
    let l = CROP_BRACKET_LENGTH;
    let x0 = sel.x;
    let y0 = sel.y;
    let x1 = sel.x + sel.w;
    let y1 = sel.y + sel.h;
    // Top-left
    cr.move_to(x0 + l, y0);
    cr.line_to(x0, y0);
    cr.line_to(x0, y0 + l);
    let _ = cr.stroke();
    // Top-right
    cr.move_to(x1 - l, y0);
    cr.line_to(x1, y0);
    cr.line_to(x1, y0 + l);
    let _ = cr.stroke();
    // Bottom-right
    cr.move_to(x1 - l, y1);
    cr.line_to(x1, y1);
    cr.line_to(x1, y1 - l);
    let _ = cr.stroke();
    // Bottom-left
    cr.move_to(x0 + l, y1);
    cr.line_to(x0, y1);
    cr.line_to(x0, y1 - l);
    let _ = cr.stroke();

    // Edge "fat bar" handles — parallel segments centered on each edge
    // midpoint, lying along the edge direction.
    let half = EDGE_HANDLE_LENGTH / 2.0;
    let mx = sel.x + sel.w / 2.0;
    let my = sel.y + sel.h / 2.0;
    // Top edge — horizontal bar at y0
    cr.move_to(mx - half, y0);
    cr.line_to(mx + half, y0);
    let _ = cr.stroke();
    // Bottom edge
    cr.move_to(mx - half, y1);
    cr.line_to(mx + half, y1);
    let _ = cr.stroke();
    // Left edge — vertical bar at x0
    cr.move_to(x0, my - half);
    cr.line_to(x0, my + half);
    let _ = cr.stroke();
    // Right edge
    cr.move_to(x1, my - half);
    cr.line_to(x1, my + half);
    let _ = cr.stroke();

    // Move handle: filled circle with a 4-way arrow glyph at the center.
    let (cx, cy) = ResizeHandle::Move.center(sel);
    let r = MOVE_HANDLE_RADIUS;
    cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.6);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.set_line_width(2.5);
    let _ = cr.stroke();

    // 4-way arrow glyph inside.
    let arm = r * 0.55;
    let head = r * 0.18;
    cr.set_line_width(2.0);
    cr.move_to(cx, cy - arm);
    cr.line_to(cx, cy + arm);
    cr.move_to(cx - arm, cy);
    cr.line_to(cx + arm, cy);
    let _ = cr.stroke();
    for (ax, ay, hx1, hy1, hx2, hy2) in [
        (
            cx,
            cy - arm,
            cx - head,
            cy - arm + head,
            cx + head,
            cy - arm + head,
        ),
        (
            cx,
            cy + arm,
            cx - head,
            cy + arm - head,
            cx + head,
            cy + arm - head,
        ),
        (
            cx - arm,
            cy,
            cx - arm + head,
            cy - head,
            cx - arm + head,
            cy + head,
        ),
        (
            cx + arm,
            cy,
            cx + arm - head,
            cy - head,
            cx + arm - head,
            cy + head,
        ),
    ] {
        cr.move_to(hx1, hy1);
        cr.line_to(ax, ay);
        cr.line_to(hx2, hy2);
    }
    let _ = cr.stroke();
}

fn draw_corner_brackets(cr: &cairo::Context, sel: Selection) {
    let l = BRACKET_LEN;
    let half = BRACKET_WIDTH / 2.0;
    let x0 = sel.x;
    let y0 = sel.y;
    let x1 = sel.x + sel.w;
    let y1 = sel.y + sel.h;

    // top-left
    cr.move_to(x0 - half, y0 + l);
    cr.line_to(x0 - half, y0 - half);
    cr.line_to(x0 + l, y0 - half);
    // top-right
    cr.move_to(x1 - l, y0 - half);
    cr.line_to(x1 + half, y0 - half);
    cr.line_to(x1 + half, y0 + l);
    // bottom-right
    cr.move_to(x1 + half, y1 - l);
    cr.line_to(x1 + half, y1 + half);
    cr.line_to(x1 - l, y1 + half);
    // bottom-left
    cr.move_to(x0 + l, y1 + half);
    cr.line_to(x0 - half, y1 + half);
    cr.line_to(x0 - half, y1 - l);

    let _ = cr.stroke();
}

fn install_css(_app: &gtk::Application) {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_page_zone_excludes_handles_bands_and_outside() {
        let sel = Selection {
            x: 100.0,
            y: 100.0,
            w: 400.0,
            h: 300.0,
        };
        // Deep inside the region: the page gets pointer + keyboard.
        assert!(pointer_over_selected_page(sel, 300.0, 200.0));
        // Outside, on the dimmed surround.
        assert!(!pointer_over_selected_page(sel, 50.0, 50.0));
        assert!(!pointer_over_selected_page(sel, 600.0, 200.0));
        // On the edge band and a corner handle.
        assert!(!pointer_over_selected_page(
            sel,
            300.0,
            100.0 + EDGE_HIT_SLACK / 2.0
        ));
        assert!(!pointer_over_selected_page(sel, 100.0, 100.0));
        // On the centre move handle.
        let (mx, my) = ResizeHandle::Move.center(sel);
        assert!(!pointer_over_selected_page(sel, mx, my));
        assert!(pointer_over_selected_page(
            sel,
            mx + MOVE_HANDLE_RADIUS + 10.0,
            my
        ));
    }

    #[test]
    fn pointer_park_target_uses_lower_right_inset_and_output_scale() {
        assert_eq!(
            pointer_park_target(
                Selection {
                    x: 10.0,
                    y: 20.0,
                    w: 200.0,
                    h: 300.0,
                },
                2,
            ),
            (360, 520)
        );
    }

    #[test]
    fn pointer_park_target_stays_inside_a_small_selection() {
        assert_eq!(
            pointer_park_target(
                Selection {
                    x: 10.0,
                    y: 20.0,
                    w: 20.0,
                    h: 30.0,
                },
                0,
            ),
            (11, 21)
        );
    }

    #[test]
    fn neutral_measurement_clears_then_restores_every_margin() {
        use std::cell::Cell;

        // The reported failure measured a ~367x84 pause panel as ~1435x937:
        // exactly one stale ~1068x853 screen-position term remained after the
        // old subtract-after-measure approach. Observe zero here instead of
        // trying to reverse GTK's margin-influenced measurement afterward.
        let saved = PillMargins {
            start: 1_068,
            end: 13,
            top: 853,
            bottom: 7,
        };
        let current = Cell::new(saved);
        let observed_during_measurement =
            with_temporary_margin_state(saved, |margins| current.set(margins), || current.get());

        assert_eq!(observed_during_measurement, PillMargins::default());
        assert_eq!(current.get(), saved);
    }

    #[test]
    fn capture_pill_uses_each_available_outside_gutter() {
        let below = capture_pill_position(
            1000.0,
            800.0,
            300.0,
            50.0,
            Selection {
                x: 100.0,
                y: 100.0,
                w: 600.0,
                h: 400.0,
            },
        );
        assert_eq!(below, Some((250.0, 518.0)));

        let above = capture_pill_position(
            1000.0,
            800.0,
            300.0,
            50.0,
            Selection {
                x: 100.0,
                y: 600.0,
                w: 600.0,
                h: 180.0,
            },
        );
        assert_eq!(above, Some((250.0, 532.0)));

        let right = capture_pill_position(
            1000.0,
            800.0,
            300.0,
            50.0,
            Selection {
                x: 50.0,
                y: 0.0,
                w: 200.0,
                h: 800.0,
            },
        );
        assert_eq!(right, Some((268.0, 375.0)));
    }

    #[test]
    fn capture_pill_chooses_the_geometrically_closest_outside_gutter() {
        // Both below and right fit. The old fixed side order chose below even
        // though the narrow selection's right edge is substantially closer.
        assert_eq!(
            capture_pill_position(
                1000.0,
                800.0,
                200.0,
                80.0,
                Selection {
                    x: 100.0,
                    y: 100.0,
                    w: 150.0,
                    h: 500.0,
                },
            ),
            Some((268.0, 310.0))
        );
    }

    #[test]
    fn capture_pill_refuses_to_overlap_a_full_overlay_selection() {
        assert_eq!(
            capture_pill_position(
                1000.0,
                800.0,
                300.0,
                50.0,
                Selection {
                    x: 0.0,
                    y: 0.0,
                    w: 1000.0,
                    h: 800.0,
                },
            ),
            None
        );
    }

    #[test]
    fn stopped_pill_moves_inside_even_when_an_outside_gutter_is_nearby() {
        let selection = Selection {
            x: 100.0,
            y: 100.0,
            w: 600.0,
            h: 400.0,
        };

        // While frames are being sampled, the pill remains below and does
        // not cover even one pixel of the selected region.
        let active = capture_pill_position(1000.0, 800.0, 300.0, 50.0, selection);
        assert_eq!(active, Some((250.0, 518.0)));
        let (active_x, active_y) = active.unwrap();
        assert_eq!(
            pill_selection_overlap_area(active_x, active_y, 300.0, 50.0, selection),
            0.0
        );

        // Pausing or reaching the end stops sampling, so the same controls
        // move to the selection's inside bottom edge unconditionally.
        let stopped = settled_pill_position(1000.0, 800.0, 300.0, 50.0, selection);
        assert_eq!(stopped, Some((250.0, 432.0)));
        let (stopped_x, stopped_y) = stopped.unwrap();
        assert_eq!(
            pill_selection_overlap_area(stopped_x, stopped_y, 300.0, 50.0, selection),
            300.0 * 50.0
        );
    }

    #[test]
    fn paused_pill_stays_with_a_tall_right_edge_selection() {
        // This mirrors a rightmost, full-height terminal on a 3072x1728
        // logical-pixel display. An expanded panel technically fits to its
        // left, but its center would land over the unrelated middle window.
        let selection = Selection {
            x: 2306.0,
            y: 0.0,
            w: 754.0,
            h: 1728.0,
        };
        let position = settled_pill_position(3072.0, 1728.0, 432.0, 83.0, selection);
        assert_eq!(position, Some((2467.0, 1619.0)));
        let (x, y) = position.expect("pause panel should fit inside the selection");
        assert_eq!(
            pill_selection_overlap_area(x, y, 432.0, 83.0, selection),
            432.0 * 83.0
        );
    }

    #[test]
    fn finished_compact_pill_stays_with_a_tall_right_edge_selection() {
        let selection = Selection {
            x: 2306.0,
            y: 0.0,
            w: 754.0,
            h: 1728.0,
        };
        assert_eq!(
            settled_pill_position(3072.0, 1728.0, 300.0, 50.0, selection),
            Some((2533.0, 1652.0))
        );
        // Active sampling deliberately keeps the same compact pill outside.
        assert_eq!(
            capture_pill_position(3072.0, 1728.0, 300.0, 50.0, selection),
            Some((1988.0, 839.0))
        );
    }

    #[test]
    fn paused_pill_stays_inside_a_tall_left_selection() {
        let selection = Selection {
            x: 20.0,
            y: 0.0,
            w: 600.0,
            h: 800.0,
        };
        assert_eq!(
            settled_pill_position(1000.0, 800.0, 500.0, 80.0, selection),
            Some((70.0, 694.0))
        );
    }

    #[test]
    fn paused_pill_uses_the_inside_lower_edge_for_a_wide_top_selection() {
        let selection = Selection {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 600.0,
        };
        assert_eq!(
            settled_pill_position(1000.0, 800.0, 300.0, 200.0, selection),
            Some((350.0, 382.0))
        );
    }

    #[test]
    fn paused_pill_full_screen_anchor_is_inside_bottom_center() {
        assert_eq!(
            settled_pill_position(
                1000.0,
                800.0,
                300.0,
                50.0,
                Selection {
                    x: 0.0,
                    y: 0.0,
                    w: 1000.0,
                    h: 800.0,
                },
            ),
            Some((350.0, 724.0))
        );
    }

    #[test]
    fn manual_stall_detector_cues_once_only_after_progress_and_delay() {
        let start = Instant::now();
        let mut detector = ManualStallDetector::default();

        assert!(!detector.still(start + Duration::from_secs(30)));
        assert!(!detector.movement());
        assert!(!detector.still(start));
        assert!(!detector.still(start + MANUAL_STALL_DELAY - Duration::from_millis(1)));
        assert!(detector.still(start + MANUAL_STALL_DELAY));
        assert!(!detector.still(start + MANUAL_STALL_DELAY + Duration::from_secs(5)));

        assert!(detector.movement());
        assert!(!detector.still(start + Duration::from_secs(10)));
        assert!(detector.still(start + Duration::from_secs(10) + MANUAL_STALL_DELAY));
    }

    #[test]
    fn manual_stall_detector_interruption_restarts_the_delay() {
        let start = Instant::now();
        let mut detector = ManualStallDetector::default();
        detector.movement();
        assert!(!detector.still(start));
        detector.interrupt();
        assert!(!detector.still(start + MANUAL_STALL_DELAY));
        assert!(detector.still(start + MANUAL_STALL_DELAY * 2));
    }

    #[test]
    fn repeated_pending_delta_counts_as_still() {
        assert!(!pending_delta_is_still(None, 80));
        assert!(pending_delta_is_still(Some(80), 80));
        assert!(!pending_delta_is_still(Some(80), 84));
    }

    #[test]
    fn automatic_capture_polls_ready_frames_without_manual_sampling_delay() {
        assert_eq!(
            capture_poll_interval_ms(Some(CaptureMode::Auto(auto_scroll::ScrollDirection::Down))),
            AUTO_CAPTURE_POLL_MS
        );
        assert_eq!(
            capture_poll_interval_ms(Some(CaptureMode::Manual(stitch::StitchAxis::Vertical))),
            CAPTURE_INTERVAL_MS
        );
        assert_eq!(capture_poll_interval_ms(None), CAPTURE_INTERVAL_MS);
    }

    #[test]
    fn accepts_only_strong_nearby_signed_ambiguous_candidates() {
        assert_eq!(
            manual_ambiguous_action(104, 1.05, 1.052, 128),
            ManualAmbiguousAction::Accept(ManualDirection::Forward, 104)
        );
        assert_eq!(
            manual_ambiguous_action(128, 1.05, 1.052, 128),
            ManualAmbiguousAction::KeepScrolling
        );
        assert_eq!(
            manual_ambiguous_action(104, 2.01, 1.052, 128),
            ManualAmbiguousAction::KeepScrolling
        );
        assert_eq!(
            manual_ambiguous_action(104, 1.05, 1.019, 128),
            ManualAmbiguousAction::KeepScrolling
        );
        assert_eq!(
            manual_ambiguous_action(-104, 1.05, 1.052, 128),
            ManualAmbiguousAction::Accept(ManualDirection::Reverse, 104)
        );
        assert_eq!(
            manual_ambiguous_action(0, 1.05, f64::INFINITY, 128),
            ManualAmbiguousAction::KeepScrolling
        );
        assert_eq!(
            manual_ambiguous_action(104, 0.0, f64::INFINITY, 128),
            ManualAmbiguousAction::Accept(ManualDirection::Forward, 104)
        );
    }

    #[test]
    fn ambiguous_manual_guidance_is_actionable() {
        assert_eq!(
            ManualAmbiguousAction::KeepScrolling.status(),
            Some("Repeated content — keep scrolling to confirm")
        );
        assert_eq!(
            ManualAmbiguousAction::ReturnTo(ManualDirection::Forward).status(),
            Some("Content moved backward — scroll down to resume")
        );
        assert_eq!(
            ManualAmbiguousAction::ReturnTo(ManualDirection::Reverse).status(),
            Some("Content moved forward — scroll up to resume")
        );
        assert_eq!(
            ManualAmbiguousAction::Accept(ManualDirection::Forward, 42).status(),
            None
        );
    }

    #[test]
    fn automatic_pause_copy_is_plain_language_and_actionable() {
        assert_eq!(
            AUTO_ALIGNMENT_PAUSE_MESSAGE,
            "Auto-scroll paused — capture lost alignment"
        );
        assert_eq!(
            [
                CONTINUE_MANUALLY_TITLE,
                CONTINUE_ANYWAY_TITLE,
                FINISH_HERE_TITLE,
            ],
            ["Continue manually", "Continue anyway", "Finish here"]
        );
        assert_ne!(CONTINUE_ANYWAY_TITLE, "Use best match");
    }

    #[test]
    fn continuing_manually_preserves_the_automatic_scroll_axis() {
        for (automatic, expected_axis) in [
            (
                auto_scroll::ScrollDirection::Down,
                stitch::StitchAxis::Vertical,
            ),
            (
                auto_scroll::ScrollDirection::Right,
                stitch::StitchAxis::Horizontal,
            ),
        ] {
            let mode = manual_mode_preserving_axis(CaptureMode::Auto(automatic));
            assert!(mode.is_manual());
            assert_eq!(mode.axis(), expected_axis);
        }
    }

    #[test]
    fn stationary_probe_confirms_end_before_committing_a_unique_first_frame() {
        let candidate = stitch::ForwardMatchCandidate {
            delta: 37,
            error: 0.5,
        };
        assert_eq!(
            auto_probe_decision(
                stitch::ForwardLookaheadResolution::StationaryProbe {
                    first_match: stitch::StationaryProbeFirstMatch::Unique(candidate),
                },
                0,
                None,
            ),
            AutoProbeDecision::ProbeAgain
        );
        assert_eq!(
            auto_probe_decision(
                stitch::ForwardLookaheadResolution::StationaryProbe {
                    first_match: stitch::StationaryProbeFirstMatch::Ambiguous {
                        best_effort: Some(candidate),
                    },
                },
                0,
                None,
            ),
            AutoProbeDecision::ProbeAgain
        );
        assert_eq!(
            auto_probe_decision(
                stitch::ForwardLookaheadResolution::StationaryProbe {
                    first_match: stitch::StationaryProbeFirstMatch::Unique(candidate),
                },
                1,
                None,
            ),
            AutoProbeDecision::End(candidate)
        );
        assert_eq!(
            auto_probe_decision(
                stitch::ForwardLookaheadResolution::StationaryProbe {
                    first_match: stitch::StationaryProbeFirstMatch::Ambiguous { best_effort: None },
                },
                1,
                None,
            ),
            AutoProbeDecision::Pause {
                reason: AutoProbePauseReason::Stationary,
                best_effort: None,
            }
        );
        assert_eq!(
            auto_probe_decision(
                stitch::ForwardLookaheadResolution::StationaryProbe {
                    first_match: stitch::StationaryProbeFirstMatch::Ambiguous {
                        best_effort: Some(candidate),
                    },
                },
                1,
                None,
            ),
            AutoProbeDecision::End(candidate)
        );

        let two_frame_path = stitch::ForwardMatchPath {
            first_delta: 37,
            second_delta: 11,
            total_error: 1.0,
        };
        assert_eq!(
            auto_probe_decision(
                stitch::ForwardLookaheadResolution::Unresolved {
                    best_effort: Some(two_frame_path),
                },
                0,
                None,
            ),
            AutoProbeDecision::Pause {
                reason: AutoProbePauseReason::StillAmbiguous,
                best_effort: Some(AutoBestEffort::TwoFrames(two_frame_path)),
            }
        );
        assert_eq!(
            auto_probe_decision(
                stitch::ForwardLookaheadResolution::Resolved(two_frame_path),
                1,
                None,
            ),
            AutoProbeDecision::Commit {
                path: two_frame_path,
                periodic: false,
            }
        );
        assert_eq!(
            auto_probe_decision(
                stitch::ForwardLookaheadResolution::LowErrorPeriodic(two_frame_path),
                1,
                None,
            ),
            AutoProbeDecision::Commit {
                path: two_frame_path,
                periodic: true,
            }
        );
    }

    #[test]
    fn stable_verified_auto_steps_produce_a_conservative_endpoint_bound() {
        let mut calibration = AutoStepCalibration::default();
        for delta in [230, 231] {
            calibration.record_verified_normal_step(delta);
        }
        assert_eq!(calibration.endpoint_upper_bound(), None);

        calibration.record_verified_normal_step(232);
        assert_eq!(calibration.endpoint_upper_bound(), Some(244));

        calibration.record_verified_normal_step(231);
        calibration.record_verified_normal_step(230);
        calibration.record_verified_normal_step(232);
        assert_eq!(calibration.recent, vec![231, 232, 231, 230, 232]);
        assert_eq!(calibration.endpoint_upper_bound(), Some(244));
    }

    #[test]
    fn unstable_auto_steps_do_not_authorize_an_endpoint_guess() {
        let mut calibration = AutoStepCalibration::default();
        for delta in [180, 231, 300] {
            calibration.record_verified_normal_step(delta);
        }
        assert_eq!(calibration.endpoint_upper_bound(), None);
    }

    #[test]
    fn calibrated_endpoint_candidate_avoids_the_repeated_content_pause() {
        let best_effort = stitch::ForwardMatchCandidate {
            delta: 1_683,
            error: 0.278,
        };
        let physical_candidate = stitch::ForwardMatchCandidate {
            delta: 231,
            error: 0.0,
        };
        let resolution = stitch::ForwardLookaheadResolution::StationaryProbe {
            first_match: stitch::StationaryProbeFirstMatch::Ambiguous {
                best_effort: Some(best_effort),
            },
        };

        assert_eq!(
            auto_probe_decision(resolution, 1, Some(physical_candidate)),
            AutoProbeDecision::End(physical_candidate)
        );
    }

    #[test]
    fn accepting_an_uncertain_stationary_endpoint_finishes_without_more_scrolling() {
        assert!(accepting_auto_pause_reaches_end(
            AutoProbePauseReason::Stationary
        ));
        assert!(!accepting_auto_pause_reaches_end(
            AutoProbePauseReason::StillAmbiguous
        ));
    }

    #[test]
    fn manual_handoff_respects_the_pointer_parking_preference() {
        assert_eq!(
            manual_handoff_pointer_policy(true),
            ManualHandoffPointerPolicy::ParkInSelection
        );
        assert_eq!(
            manual_handoff_pointer_policy(false),
            ManualHandoffPointerPolicy::LeaveUnchanged
        );
    }

    #[test]
    fn unverified_seam_warning_is_absent_until_user_continues_anyway() {
        assert_eq!(unverified_seam_warning(0), None);
        assert_eq!(
            unverified_seam_warning(1).as_deref(),
            Some("Capture may contain a repeated or missing section.")
        );
        assert_eq!(
            unverified_seam_warning(2).as_deref(),
            Some("Capture may contain 2 repeated or missing sections.")
        );
    }
}
