use anyhow::{Result, bail};
use relm4::gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use relm4::gtk::glib::Bytes;

/// Downsample only across the direction of travel. The motion axis stays at
/// source resolution, so both vertical and horizontal deltas remain exact.
const DOWNSAMPLE_CROSS: usize = 4;

/// At least this fraction of the old and new frame must overlap. A larger
/// jump cannot be stitched safely because there is not enough shared content
/// to establish alignment.
const MIN_OVERLAP_DEN: usize = 3;
const MIN_OVERLAP_PIXELS: usize = 32;

const MIN_MOTION_PIXELS: usize = 3;
const STATIONARY_ERROR: f64 = 1.0;
const MAX_STATIONARY_ERROR: f64 = 8.0;
/// A visually close alignment can still have multiple nearly equal peaks on
/// periodic content (for example, repeated terminal command output). Keep the
/// signed best candidate available to the capture mode so manual capture can
/// apply its small-motion prior without weakening reliable matches globally.
const MAX_AMBIGUOUS_ERROR: f64 = 2.0;
const MAX_MATCH_ERROR: f64 = 24.0;
const MIN_CONFIDENCE: f64 = 1.10;
const MIN_ERROR_MARGIN: f64 = 0.75;

/// A lookahead resolver considers only distinct correlation basins. Keeping
/// this many is ample for normal viewport content while bounding both memory
/// and the cubic path comparison below. If more equally plausible basins are
/// present, the result deliberately remains unresolved.
const MAX_FORWARD_CANDIDATES: usize = 64;
const PATH_DELTA_TOLERANCE: usize = 2;

/// Only a trailing viewport-fixed strip is removed automatically. Keeping the
/// search bounded avoids interpreting a large, flat part of a web page as a
/// fixed footer.
const MAX_STATIONARY_EDGE: usize = 128;
const STATIONARY_EDGE_ERROR: f64 = 1.0;
/// Viewport-fixed chrome (sticky headers, cookie bars, fixed footers) is the
/// same in both frames at shift 0 while the content between them moves. Rows
/// or columns like that are cropped away before the motion search — otherwise
/// they put an identical error floor under every candidate shift and a
/// high-contrast band (a dark header on a light page) leaves no offset able
/// to win by ratio. Each edge gives up at most this fraction of the axis so a
/// genuinely stationary pair still resolves as `Stationary` on the remainder.
const MAX_STATIONARY_SCORING_EDGE_DEN: usize = 4;
/// Rows just above a fixed footer (or a translucent overlay with no opaque
/// part) often carry its drop shadow: the content beneath scrolls, but every
/// frame tints it. Compared document-aligned with the next frame — where the
/// same rows have scrolled up out of the shadow — those rows differ by the
/// tint while plain translated content differs by nothing. Any such rows
/// contiguous with the trailing edge join the trailing extent, so each frame
/// contributes only untinted rows and the shadow appears once, at the end.
const NEAR_STATIONARY_ERROR: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StitchAxis {
    Vertical,
    Horizontal,
}

/// Classification of the second frame relative to the first.
///
/// `Forward` is down for a vertical capture and right for a horizontal one.
/// Deltas are always expressed in source-frame pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Stationary,
    Forward(usize),
    Reverse(usize),
    /// A low-error candidate that was not sufficiently distinct from another
    /// correlation peak. Positive values are forward (down/right) and
    /// negative values are reverse (up/left), in source-frame pixels.
    Ambiguous(isize),
    Unmatchable,
}

/// Motion together with diagnostics useful for logging and tuning. `error`
/// is mean absolute grayscale error per sampled pixel. `confidence` is the
/// ratio between the best distinct competing peak and the chosen peak.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionEstimate {
    pub motion: Motion,
    pub error: f64,
    pub confidence: f64,
}

/// One forward-only alignment candidate retained for lookahead resolution.
/// Deltas use source-frame pixels; error is mean absolute grayscale error.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForwardMatchCandidate {
    pub delta: usize,
    pub error: f64,
}

/// Two adjacent forward deltas supported by a three-frame alignment path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForwardMatchPath {
    pub first_delta: usize,
    pub second_delta: usize,
    pub total_error: f64,
}

/// Result of the first automatic-scroll comparison. An ambiguous result owns
/// the two grayscale frames needed to validate it against one lookahead frame.
#[derive(Clone, Debug)]
pub enum ForwardMatch {
    Classified(MotionEstimate),
    Ambiguous(ForwardLookahead),
}

/// A pending forward alignment. It is intentionally immutable while being
/// resolved so the caller can keep it available for an explicit best-effort
/// choice if automatic resolution remains impossible.
#[derive(Clone, Debug)]
pub struct ForwardLookahead {
    origin: GrayView,
    pending: GrayView,
    axis: StitchAxis,
    first: ForwardCandidateSet,
    estimate: MotionEstimate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ForwardLookaheadResolution {
    Resolved(ForwardMatchPath),
    /// A matcher-grade forward path accepted using the automatic scroller's
    /// known direction. This covers ambiguous periodic paths and two sound
    /// adjacent seams whose cumulative displacement is larger than the
    /// matcher can compare safely. This weaker resolution is never returned
    /// by [`ForwardLookahead::resolve`], which remains strict for callers
    /// without that physical-motion prior.
    LowErrorPeriodic(ForwardMatchPath),
    /// The probe frame did not move relative to the pending frame. It must
    /// not be represented as a two-delta path: doing so would append the same
    /// pixels twice. `first_match` describes only F0→F1 and distinguishes a
    /// unique known-forward basin from an ambiguous best-effort candidate.
    StationaryProbe {
        first_match: StationaryProbeFirstMatch,
    },
    /// `best_effort` is never safe to commit automatically. It exists only so
    /// an explicit “Continue anyway” action can make a deterministic choice.
    Unresolved {
        best_effort: Option<ForwardMatchPath>,
    },
}

/// What the known-forward matcher can establish about F0→F1 when a later
/// confirmation capture is stationary relative to F1.
///
/// A unique, non-truncated forward basin is safe for an automatic caller to
/// commit: any competing peak found by the ordinary signed matcher was in the
/// physically impossible reverse direction. `Ambiguous` preserves the best
/// candidate for an explicit best-effort choice without presenting it as a
/// verified seam.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StationaryProbeFirstMatch {
    Unique(ForwardMatchCandidate),
    Ambiguous {
        best_effort: Option<ForwardMatchCandidate>,
    },
}

#[derive(Clone, Debug)]
struct ForwardCandidateSet {
    candidates: Vec<ForwardMatchCandidate>,
    truncated: bool,
    /// Largest source-pixel delta whose fixed comparison extent was searched.
    /// `None` means the frame geometry itself was not searchable.
    search_max_delta: Option<usize>,
}

/// Downsampled grayscale frame used by the pure motion matcher.
#[derive(Clone, Debug)]
pub struct GrayView {
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    source_width: usize,
}

impl GrayView {
    fn axis_len(&self, axis: StitchAxis) -> usize {
        match axis {
            StitchAxis::Vertical => self.height,
            StitchAxis::Horizontal => self.width,
        }
    }

    fn cross_len(&self, axis: StitchAxis) -> usize {
        match axis {
            StitchAxis::Vertical => self.width,
            StitchAxis::Horizontal => self.height,
        }
    }

    /// The sub-view spanning `[start, end)` along `axis` (rows for vertical
    /// motion, columns for horizontal), with the cross axis untouched.
    fn crop_axis(&self, axis: StitchAxis, start: usize, end: usize) -> GrayView {
        let end = end.min(self.axis_len(axis)).max(start);
        match axis {
            StitchAxis::Vertical => GrayView {
                pixels: self.pixels[start * self.width..end * self.width].to_vec(),
                width: self.width,
                height: end - start,
                source_width: self.source_width,
            },
            StitchAxis::Horizontal => {
                let width = end - start;
                let mut pixels = Vec::with_capacity(width * self.height);
                for row in 0..self.height {
                    let base = row * self.width;
                    pixels.extend_from_slice(&self.pixels[base + start..base + end]);
                }
                // Horizontal views are not downsampled along the motion axis,
                // so the source extent shrinks with the crop.
                let source_scale = self.source_scale(axis);
                GrayView {
                    pixels,
                    width,
                    height: self.height,
                    source_width: width * source_scale,
                }
            }
        }
    }

    fn source_scale(&self, axis: StitchAxis) -> usize {
        match axis {
            StitchAxis::Vertical => 1,
            StitchAxis::Horizontal => {
                // `downsample_to_gray` normally makes this exactly four. Keep
                // the division defensive for very narrow test/capture frames.
                (self.source_width / self.width.max(1)).max(1)
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn downsample_to_gray(p: &Pixbuf) -> GrayView {
    downsample_to_gray_for_axis(p, StitchAxis::Vertical)
}

pub fn downsample_to_gray_for_axis(p: &Pixbuf, axis: StitchAxis) -> GrayView {
    let src_w = p.width().max(0) as usize;
    let src_h = p.height().max(0) as usize;
    let src_stride = p.rowstride().max(0) as usize;
    let src_bytes = p.read_pixel_bytes();
    let src = src_bytes.as_ref();

    let (dst_w, dst_h) = match axis {
        StitchAxis::Vertical => ((src_w / DOWNSAMPLE_CROSS).max(1).min(src_w.max(1)), src_h),
        StitchAxis::Horizontal => (src_w, (src_h / DOWNSAMPLE_CROSS).max(1).min(src_h.max(1))),
    };
    let mut dst = vec![0u8; dst_w * dst_h];

    for gy in 0..dst_h {
        let src_y = match axis {
            StitchAxis::Vertical => gy,
            StitchAxis::Horizontal => (gy * DOWNSAMPLE_CROSS).min(src_h.saturating_sub(1)),
        };
        let src_row = &src[src_y * src_stride..src_y * src_stride + src_w * 4];
        let dst_row = &mut dst[gy * dst_w..gy * dst_w + dst_w];
        for (gx, dst_px) in dst_row.iter_mut().enumerate() {
            let src_x = match axis {
                StitchAxis::Vertical => (gx * DOWNSAMPLE_CROSS).min(src_w.saturating_sub(1)),
                StitchAxis::Horizontal => gx,
            };
            let off = src_x * 4;
            let r = src_row[off] as u32;
            let g = src_row[off + 1] as u32;
            let b = src_row[off + 2] as u32;
            *dst_px = ((r * 77 + g * 150 + b * 29) >> 8) as u8;
        }
    }

    GrayView {
        pixels: dst,
        width: dst_w,
        height: dst_h,
        source_width: src_w,
    }
}

/// Classify signed motion between two frames without relying on a configured
/// scroll amount. The search is coarse-to-fine and all candidates compare the
/// same number of pixels, so large offsets cannot win merely because they
/// have less overlap.
pub fn classify_motion(prev: &GrayView, cur: &GrayView, axis: StitchAxis) -> MotionEstimate {
    classify_motion_with_search_bound(prev, cur, axis, None)
}

/// Classify motion while applying a maximum plausible source-pixel delta.
///
/// This is intended as a manual-capture fallback when [`classify_motion`]
/// reports ambiguous periodic content. Limiting the correlation search lets a
/// caller with a small-motion prior recover the nearby signed peak instead of
/// an equally plausible repetition elsewhere in the viewport. It should not
/// replace the unbounded classifier for automatic capture, where the amount
/// of motion is not constrained by user input.
pub fn classify_motion_bounded(
    prev: &GrayView,
    cur: &GrayView,
    axis: StitchAxis,
    max_source_delta: usize,
) -> MotionEstimate {
    classify_motion_with_search_bound(prev, cur, axis, Some(max_source_delta))
}

/// Classify a frame for known-forward automatic capture and retain the
/// candidate landscape when the ordinary two-frame result is ambiguous.
///
/// The regular classifier is authoritative for stationary and reliable
/// forward results. A reverse or matcher-grade unmatchable result may instead
/// be a periodic alias even though the automatic worker injected only forward
/// input, so its positive candidate landscape receives one lookahead frame.
/// The resolver never admits a negative delta into a path.
pub fn classify_forward_with_lookahead(
    prev: &GrayView,
    cur: &GrayView,
    axis: StitchAxis,
) -> ForwardMatch {
    let estimate = classify_motion(prev, cur, axis);
    let matcher_grade_ambiguity = matches!(estimate.motion, Motion::Unmatchable)
        && estimate.error.is_finite()
        && estimate.error <= MAX_MATCH_ERROR;
    if !matches!(estimate.motion, Motion::Ambiguous(_) | Motion::Reverse(_))
        && !matcher_grade_ambiguity
    {
        return ForwardMatch::Classified(estimate);
    }

    let first = forward_candidate_set(prev, cur, axis);
    if first
        .candidates
        .first()
        .is_none_or(|candidate| candidate.error > MAX_MATCH_ERROR)
    {
        return ForwardMatch::Classified(estimate);
    }

    ForwardMatch::Ambiguous(ForwardLookahead {
        origin: prev.clone(),
        pending: cur.clone(),
        axis,
        first,
        estimate,
    })
}

impl ForwardLookahead {
    pub fn estimate(&self) -> MotionEstimate {
        self.estimate
    }

    /// Return the only retained forward candidate no larger than a trusted
    /// physical-motion bound. This is useful at a confirmed endpoint, where
    /// the final partial scroll cannot exceed a previously observed full
    /// automatic step. A truncated search is never narrowed this way because
    /// a qualifying competing basin may have been omitted.
    pub fn unique_candidate_at_most(
        &self,
        max_source_delta: usize,
    ) -> Option<ForwardMatchCandidate> {
        if self.first.truncated {
            return None;
        }
        let mut candidates = self
            .first
            .candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.delta <= max_source_delta);
        let candidate = candidates.next()?;
        if candidates.next().is_some() {
            return None;
        }
        Some(candidate)
    }

    #[cfg(test)]
    pub fn candidates(&self) -> &[ForwardMatchCandidate] {
        &self.first.candidates
    }

    /// Compare F0→F1, F1→F2, and F0→F2. A path is automatically resolved
    /// only when all three comparisons agree on cumulative forward movement
    /// and that path is distinct from every competing correlation basin.
    pub fn resolve(&self, lookahead: &GrayView) -> ForwardLookaheadResolution {
        self.resolve_with_physical_prior(lookahead, false)
    }

    /// Resolve a known-forward automatic-scroll probe.
    ///
    /// This first applies the same strict three-frame resolution as
    /// [`Self::resolve`]. For matcher-grade repeated content it may additionally
    /// use the automatic worker's known forward direction. Cadence is only a
    /// soft tie-breaker: client acceleration and row rounding make a fixed
    /// full-step/probe ratio too brittle to use as an acceptance gate.
    pub fn resolve_auto(&self, lookahead: &GrayView) -> ForwardLookaheadResolution {
        self.resolve_with_physical_prior(lookahead, true)
    }

    fn resolve_with_physical_prior(
        &self,
        lookahead: &GrayView,
        allow_physical_prior: bool,
    ) -> ForwardLookaheadResolution {
        // A probe can legitimately capture the pending viewport again: the
        // page may have reached its end, the injected wheel event may not
        // have taken effect yet, or painting may not have advanced. The
        // forward-only candidate search deliberately excludes zero, so it
        // would otherwise find a periodic non-zero alias and fabricate a
        // second stitch band. Classify zero motion before that search and
        // preserve only the first-frame match.
        let probe_estimate = classify_motion(&self.pending, lookahead, self.axis);
        if matches!(probe_estimate.motion, Motion::Stationary) {
            let best = self.first.candidates.first().copied();
            let first_match = if !self.first.truncated && self.first.candidates.len() == 1 {
                StationaryProbeFirstMatch::Unique(
                    best.expect("a single forward candidate must have a first candidate"),
                )
            } else {
                StationaryProbeFirstMatch::Ambiguous { best_effort: best }
            };
            return ForwardLookaheadResolution::StationaryProbe { first_match };
        }

        let second = forward_candidate_set(&self.pending, lookahead, self.axis);
        let cumulative = forward_candidate_set(&self.origin, lookahead, self.axis);

        resolve_forward_candidate_sets(&self.first, &second, &cumulative, allow_physical_prior)
    }
}

/// Mean absolute difference between one row/column of `prev` and `cur` at
/// shift 0, over the same cross-axis margin the scorer uses.
fn edge_line_error(prev: &GrayView, cur: &GrayView, axis: StitchAxis, along: usize) -> f64 {
    let cross_len = prev.cross_len(axis);
    let cross_margin = (cross_len / 12).min(cross_len.saturating_sub(1) / 2);
    let cross_step = (cross_len / 128).max(1);
    let mut total = 0u64;
    let mut count = 0usize;
    for cross in (cross_margin..cross_len - cross_margin).step_by(cross_step) {
        let index = match axis {
            StitchAxis::Vertical => along * prev.width + cross,
            StitchAxis::Horizontal => cross * prev.width + along,
        };
        total += prev.pixels[index].abs_diff(cur.pixels[index]) as u64;
        count += 1;
    }
    if count == 0 {
        f64::INFINITY
    } else {
        total as f64 / count as f64
    }
}

/// Rows/columns at the leading and trailing edge that are unchanged between
/// the two frames at shift 0 — viewport-fixed chrome (or flat margins, which
/// carry no alignment signal either way). Each edge is capped at a fraction
/// of the axis so a fully stationary pair keeps enough of the frame to be
/// recognised as such.
fn stationary_scoring_edges(prev: &GrayView, cur: &GrayView, axis: StitchAxis) -> (usize, usize) {
    let axis_len = prev.axis_len(axis);
    let max_edge = axis_len / MAX_STATIONARY_SCORING_EDGE_DEN;
    let mut lead = 0;
    while lead < max_edge && edge_line_error(prev, cur, axis, lead) <= STATIONARY_EDGE_ERROR {
        lead += 1;
    }
    let mut trail = 0;
    while trail < max_edge
        && edge_line_error(prev, cur, axis, axis_len - 1 - trail) <= STATIONARY_EDGE_ERROR
    {
        trail += 1;
    }
    (lead, trail)
}

fn classify_motion_with_search_bound(
    prev: &GrayView,
    cur: &GrayView,
    axis: StitchAxis,
    max_source_delta: Option<usize>,
) -> MotionEstimate {
    if prev.width != cur.width
        || prev.height != cur.height
        || prev.pixels.len() != prev.width.saturating_mul(prev.height)
        || cur.pixels.len() != cur.width.saturating_mul(cur.height)
    {
        return unmatchable();
    }

    let axis_len = prev.axis_len(axis);
    let cross_len = prev.cross_len(axis);
    if axis_len < MIN_OVERLAP_PIXELS + MIN_MOTION_PIXELS || cross_len < 2 {
        return unmatchable();
    }

    // Score only the part of the viewport that actually moved: crop away
    // viewport-fixed bands at both edges. The cropped views feed the same
    // search below, so deltas stay in source pixels of the full frame.
    let (lead, trail) = stationary_scoring_edges(prev, cur, axis);
    if lead + trail > 0 {
        let prev = prev.crop_axis(axis, lead, axis_len - trail);
        let cur = cur.crop_axis(axis, lead, axis_len - trail);
        return classify_motion_cropped(&prev, &cur, axis, max_source_delta);
    }
    classify_motion_cropped(prev, cur, axis, max_source_delta)
}

fn classify_motion_cropped(
    prev: &GrayView,
    cur: &GrayView,
    axis: StitchAxis,
    max_source_delta: Option<usize>,
) -> MotionEstimate {
    let axis_len = prev.axis_len(axis);
    let cross_len = prev.cross_len(axis);
    if axis_len < MIN_OVERLAP_PIXELS + MIN_MOTION_PIXELS || cross_len < 2 {
        return unmatchable();
    }

    let min_overlap = (axis_len / MIN_OVERLAP_DEN)
        .max(MIN_OVERLAP_PIXELS)
        .min(axis_len.saturating_sub(1));
    let safe_max_shift = axis_len.saturating_sub(min_overlap);
    let source_scale = prev.source_scale(axis);
    let max_shift = max_source_delta
        .map(|bound| bound / source_scale)
        .map_or(safe_max_shift, |bound| bound.min(safe_max_shift));
    if max_shift < MIN_MOTION_PIXELS {
        return unmatchable();
    }

    let match_extent = axis_len - max_shift;
    // The coarse pass searches every possible offset, but samples only a
    // small grid of pixels. Skipping offsets would miss a sharp alignment in
    // high-frequency content; sparse pixels give us the same speedup without
    // making that assumption. The fine pass then densely scores the best
    // coarse peaks.
    let coarse_axis_sample = (match_extent / 32).max(1);
    let coarse_cross_sample = (cross_len / 48).max(1);
    let fine_axis_sample = (match_extent / 128).max(1);
    let fine_cross_sample = (cross_len / 128).max(1);

    let zero_error = score_shift(
        prev,
        cur,
        axis,
        0,
        max_shift,
        fine_axis_sample,
        fine_cross_sample,
    );
    if zero_error <= STATIONARY_ERROR {
        return MotionEstimate {
            motion: Motion::Stationary,
            error: zero_error,
            confidence: f64::INFINITY,
        };
    }

    let max_shift_signed = max_shift as isize;
    // Score every possible offset so high-frequency content cannot hide a
    // true non-grid alignment. The coarse score itself uses a sparse pixel
    // grid; the best peaks are then rescored densely below.
    let mut coarse = Vec::with_capacity(max_shift * 2 + 1);
    for shift in -max_shift_signed..=max_shift_signed {
        coarse.push(SearchSample {
            shift,
            error: score_shift(
                prev,
                cur,
                axis,
                shift,
                max_shift,
                coarse_axis_sample,
                coarse_cross_sample,
            ),
        });
    }
    coarse.sort_by(|a, b| a.error.total_cmp(&b.error));

    // Refine the best few coarse peaks. Looking beyond only the absolute
    // coarse winner keeps narrow true matches from being lost to sampling.
    let mut fine = Vec::new();
    for peak in coarse.iter().take(12) {
        let radius = 1isize;
        let start = (peak.shift - radius).max(-max_shift_signed);
        let end = (peak.shift + radius).min(max_shift_signed);
        for candidate in start..=end {
            if fine
                .iter()
                .any(|sample: &SearchSample| sample.shift == candidate)
            {
                continue;
            }
            fine.push(SearchSample {
                shift: candidate,
                error: score_shift(
                    prev,
                    cur,
                    axis,
                    candidate,
                    max_shift,
                    fine_axis_sample,
                    fine_cross_sample,
                ),
            });
        }
    }
    fine.sort_by(|a, b| a.error.total_cmp(&b.error));
    let Some(best) = fine.first().copied() else {
        return unmatchable();
    };

    // A runner-up must be a genuinely distinct peak. Adjacent offsets are
    // part of the same basin and made the former confidence ratio nearly one
    // even when the alignment was clear.
    let runner_neighborhood = (axis_len / 256).clamp(6, 16) as isize;
    let mut runner_error = f64::INFINITY;
    for candidate in coarse
        .iter()
        .filter(|sample| (sample.shift - best.shift).abs() > runner_neighborhood)
        .take(8)
    {
        let error = score_shift(
            prev,
            cur,
            axis,
            candidate.shift,
            max_shift,
            fine_axis_sample,
            fine_cross_sample,
        );
        runner_error = runner_error.min(error);
    }

    let confidence = if best.error <= f64::EPSILON {
        if runner_error <= f64::EPSILON {
            1.0
        } else {
            f64::INFINITY
        }
    } else {
        runner_error / best.error
    };

    let source_delta = best.shift.unsigned_abs().saturating_mul(source_scale);
    if source_delta <= MIN_MOTION_PIXELS && best.error <= MAX_STATIONARY_ERROR {
        return MotionEstimate {
            motion: Motion::Stationary,
            error: best.error,
            confidence,
        };
    }

    let margin = runner_error - best.error;
    if !best.error.is_finite() || best.error > MAX_MATCH_ERROR {
        return MotionEstimate {
            motion: Motion::Unmatchable,
            error: best.error,
            confidence,
        };
    }

    if confidence < MIN_CONFIDENCE || margin < MIN_ERROR_MARGIN {
        let motion = if best.error <= MAX_AMBIGUOUS_ERROR {
            let signed_delta = if best.shift > 0 {
                source_delta as isize
            } else {
                -(source_delta as isize)
            };
            Motion::Ambiguous(signed_delta)
        } else {
            Motion::Unmatchable
        };
        return MotionEstimate {
            motion,
            error: best.error,
            confidence,
        };
    }

    MotionEstimate {
        motion: if best.shift > 0 {
            Motion::Forward(source_delta)
        } else {
            Motion::Reverse(source_delta)
        },
        error: best.error,
        confidence,
    }
}

/// Stitch already-validated forward frames using the measured delta for each
/// adjacent pair. `deltas.len()` must equal `frames.len() - 1`.
///
/// A stationary strip touching the trailing edge (bottom for vertical, right
/// for horizontal) is excluded from every incremental slice and copied once
/// from the final frame. This removes repeated window borders and fixed edge
/// footers without trying to invent pixels hidden behind them.
#[cfg_attr(not(test), allow(dead_code))]
pub fn stitch_with_deltas(frames: &[Pixbuf], deltas: &[usize], axis: StitchAxis) -> Result<Pixbuf> {
    validate_frames(frames)?;
    if deltas.len() != frames.len().saturating_sub(1) {
        bail!(
            "expected {} stitch deltas for {} frames, got {}",
            frames.len().saturating_sub(1),
            frames.len(),
            deltas.len()
        );
    }

    let mut accumulator = StitchAccumulator::new(&frames[0], axis)?;
    for (frame, &delta) in frames.iter().skip(1).zip(deltas) {
        accumulator.push_forward(frame, delta)?;
    }
    debug_assert_eq!(accumulator.frame_count(), frames.len());
    accumulator.finish()
}

/// Incrementally stores a scroll capture without retaining every complete
/// viewport. The first viewport is kept because it is part of the result;
/// later viewports are reduced to the newly exposed strip plus a small,
/// bounded trailing-edge halo.
///
/// The halo is necessary because the width of a fixed bottom/right edge can
/// only be decided after all accepted frames have been seen. Keeping at most
/// [`MAX_STATIONARY_EDGE`] extra rows or columns lets `finish` make that
/// decision without retaining the source `Pixbuf`s.
pub struct StitchAccumulator {
    axis: StitchAxis,
    direction: Option<StitchDirection>,
    width: usize,
    height: usize,
    axis_len: usize,
    cross_len: usize,
    max_edge: usize,
    first_rgba: Vec<u8>,
    bands: Vec<TailBand>,
    total_delta: usize,
    edge_error_sums: Vec<u64>,
    edge_error_counts: Vec<u64>,
    /// Per trailing depth: how much a row differs from the *same document
    /// row* in the following frame (see [`NEAR_STATIONARY_ERROR`]).
    aligned_error_sums: Vec<u64>,
    aligned_error_counts: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StitchDirection {
    Forward,
    Reverse,
}

struct TailBand {
    delta: usize,
    /// Source-frame coordinate of the first retained row/column.
    source_axis_start: usize,
    /// Tight RGBA pixels. Vertical bands are row-major `width` by
    /// `axis_len - source_axis_start`; horizontal bands are row-major
    /// `axis_len - source_axis_start` by `height`.
    rgba: Vec<u8>,
}

impl StitchAccumulator {
    pub fn new(first: &Pixbuf, axis: StitchAxis) -> Result<Self> {
        validate_frame(first, None)?;
        let width = first.width() as usize;
        let height = first.height() as usize;
        let (axis_len, cross_len) = match axis {
            StitchAxis::Vertical => (height, width),
            StitchAxis::Horizontal => (width, height),
        };
        let max_edge = (axis_len / 8).min(MAX_STATIONARY_EDGE);

        Ok(Self {
            axis,
            direction: None,
            width,
            height,
            axis_len,
            cross_len,
            max_edge,
            first_rgba: copy_frame_tight(first)?,
            bands: Vec::new(),
            total_delta: 0,
            edge_error_sums: vec![0; max_edge],
            edge_error_counts: vec![0; max_edge],
            aligned_error_sums: vec![0; max_edge],
            aligned_error_counts: vec![0; max_edge],
        })
    }

    /// Retain one validated forward frame. `delta` is measured in source
    /// pixels along the accumulator's axis.
    pub fn push_forward(&mut self, frame: &Pixbuf, delta: usize) -> Result<()> {
        if self.direction == Some(StitchDirection::Reverse) {
            bail!("cannot mix forward and reverse frames in one scroll capture");
        }
        self.push_oriented(frame, delta)?;
        self.direction = Some(StitchDirection::Forward);
        Ok(())
    }

    /// Retain one reverse frame while keeping the final image in document
    /// order. Reverse input is reflected along the stitch axis, which turns
    /// it into the same forward composition handled by `push_oriented`; the
    /// completed owned pixel buffer is reflected back in-place at finish.
    pub fn push_reverse(&mut self, frame: &Pixbuf, delta: usize) -> Result<()> {
        if self.direction == Some(StitchDirection::Forward) {
            bail!("cannot mix forward and reverse frames in one scroll capture");
        }

        validate_frame(frame, Some((self.width, self.height)))?;
        validate_delta_and_extent(self.axis, self.axis_len, self.total_delta, delta)?;
        let reflected = frame
            .flip(matches!(self.axis, StitchAxis::Horizontal))
            .ok_or_else(|| anyhow::anyhow!("could not reflect reverse scroll frame"))?;

        let first_reverse = self.direction.is_none();
        if first_reverse {
            reverse_rgba_axis_in_place(&mut self.first_rgba, self.width, self.height, self.axis);
        }
        if let Err(error) = self.push_oriented(&reflected, delta) {
            if first_reverse {
                reverse_rgba_axis_in_place(
                    &mut self.first_rgba,
                    self.width,
                    self.height,
                    self.axis,
                );
            }
            return Err(error);
        }
        self.direction = Some(StitchDirection::Reverse);
        Ok(())
    }

    fn push_oriented(&mut self, frame: &Pixbuf, delta: usize) -> Result<()> {
        validate_frame(frame, Some((self.width, self.height)))?;
        let total_delta =
            validate_delta_and_extent(self.axis, self.axis_len, self.total_delta, delta)?;

        // Calculate everything before mutating the accumulator. A malformed
        // frame therefore cannot leave a partially appended capture behind.
        let retained_extent = delta
            .checked_add(self.max_edge)
            .ok_or_else(|| anyhow::anyhow!("retained scroll band overflow"))?;
        let source_axis_start = self.axis_len.saturating_sub(retained_extent);
        let rgba = copy_tail_band(frame, self.axis, self.width, self.height, source_axis_start)?;
        let (edge_sums, edge_counts) = self.edge_observation(frame)?;
        let (aligned_sums, aligned_counts) = self.aligned_observation(frame, delta)?;

        let mut combined_sums = Vec::with_capacity(self.max_edge);
        let mut combined_counts = Vec::with_capacity(self.max_edge);
        let mut combined_aligned_sums = Vec::with_capacity(self.max_edge);
        let mut combined_aligned_counts = Vec::with_capacity(self.max_edge);
        for depth in 0..self.max_edge {
            combined_sums.push(
                self.edge_error_sums[depth]
                    .checked_add(edge_sums[depth])
                    .ok_or_else(|| anyhow::anyhow!("stationary-edge error sum overflow"))?,
            );
            combined_counts.push(
                self.edge_error_counts[depth]
                    .checked_add(edge_counts[depth])
                    .ok_or_else(|| anyhow::anyhow!("stationary-edge sample count overflow"))?,
            );
            combined_aligned_sums.push(
                self.aligned_error_sums[depth]
                    .checked_add(aligned_sums[depth])
                    .ok_or_else(|| anyhow::anyhow!("near-stationary error sum overflow"))?,
            );
            combined_aligned_counts.push(
                self.aligned_error_counts[depth]
                    .checked_add(aligned_counts[depth])
                    .ok_or_else(|| anyhow::anyhow!("near-stationary sample count overflow"))?,
            );
        }
        self.edge_error_sums = combined_sums;
        self.edge_error_counts = combined_counts;
        self.aligned_error_sums = combined_aligned_sums;
        self.aligned_error_counts = combined_aligned_counts;
        self.bands.push(TailBand {
            delta,
            source_axis_start,
            rgba,
        });
        self.total_delta = total_delta;
        Ok(())
    }

    pub fn frame_count(&self) -> usize {
        self.bands.len() + 1
    }

    pub fn finish(self) -> Result<Pixbuf> {
        let strip = self.stationary_trailing_strip();
        let trailing = strip + self.near_stationary_trailing_zone(strip);
        let content_extent = self.axis_len.saturating_sub(trailing);
        if content_extent == 0 {
            bail!("stationary trailing edge covers the entire capture");
        }
        if let Some(bad) = self.bands.iter().find(|band| band.delta >= content_extent) {
            bail!(
                "invalid {:?} stitch delta {} for content extent {}",
                self.axis,
                bad.delta,
                content_extent
            );
        }

        match self.axis {
            StitchAxis::Vertical => self.finish_vertical(trailing),
            StitchAxis::Horizontal => self.finish_horizontal(trailing),
        }
    }

    fn edge_observation(&self, frame: &Pixbuf) -> Result<(Vec<u64>, Vec<u64>)> {
        let mut sums = vec![0u64; self.max_edge];
        let mut counts = vec![0u64; self.max_edge];
        if self.max_edge == 0 {
            return Ok((sums, counts));
        }

        let other_bytes = frame.read_pixel_bytes();
        let other = other_bytes.as_ref();
        let other_stride = frame.rowstride() as usize;
        let cross_step = (self.cross_len / 512).max(1);

        for depth in 0..self.max_edge {
            let position = self.axis_len - depth - 1;
            for cross in (0..self.cross_len).step_by(cross_step) {
                let (x, y) = match self.axis {
                    StitchAxis::Vertical => (cross, position),
                    StitchAxis::Horizontal => (position, cross),
                };
                let first_off = (y * self.width + x) * 4;
                let other_off = y * other_stride + x * 4;
                for channel in 0..3 {
                    sums[depth] = sums[depth]
                        .checked_add(
                            self.first_rgba[first_off + channel]
                                .abs_diff(other[other_off + channel])
                                as u64,
                        )
                        .ok_or_else(|| anyhow::anyhow!("stationary-edge error sum overflow"))?;
                    counts[depth] = counts[depth]
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("stationary-edge sample count overflow"))?;
                }
            }
        }
        Ok((sums, counts))
    }

    /// Compare the trailing rows/columns of the previously retained frame
    /// with the same *document* rows in `frame` (shifted by `delta`). Depth 0
    /// is the trailing edge; depths that scroll past the leading edge are
    /// simply not observed.
    fn aligned_observation(&self, frame: &Pixbuf, delta: usize) -> Result<(Vec<u64>, Vec<u64>)> {
        let mut sums = vec![0u64; self.max_edge];
        let mut counts = vec![0u64; self.max_edge];
        if self.max_edge == 0 {
            return Ok((sums, counts));
        }

        // The previous frame's trailing max_edge rows live either in the
        // full first frame or in the last retained band.
        let (previous, previous_start, previous_row_bytes): (&[u8], usize, usize) =
            match self.bands.last() {
                None => (&self.first_rgba, 0, self.width * 4),
                Some(band) => {
                    let band_row_bytes = match self.axis {
                        StitchAxis::Vertical => self.width * 4,
                        StitchAxis::Horizontal => (self.axis_len - band.source_axis_start) * 4,
                    };
                    (&band.rgba, band.source_axis_start, band_row_bytes)
                }
            };
        let other_bytes = frame.read_pixel_bytes();
        let other = other_bytes.as_ref();
        let other_stride = frame.rowstride() as usize;
        let cross_step = (self.cross_len / 512).max(1);

        for depth in 0..self.max_edge {
            let position = self.axis_len - depth - 1;
            let Some(aligned) = position.checked_sub(delta) else {
                continue;
            };
            if position < previous_start {
                continue;
            }
            for cross in (0..self.cross_len).step_by(cross_step) {
                let (previous_off, other_off) = match self.axis {
                    StitchAxis::Vertical => (
                        (position - previous_start) * previous_row_bytes + cross * 4,
                        aligned * other_stride + cross * 4,
                    ),
                    StitchAxis::Horizontal => (
                        cross * previous_row_bytes + (position - previous_start) * 4,
                        cross * other_stride + aligned * 4,
                    ),
                };
                for channel in 0..3 {
                    sums[depth] = sums[depth]
                        .checked_add(
                            previous[previous_off + channel].abs_diff(other[other_off + channel])
                                as u64,
                        )
                        .ok_or_else(|| anyhow::anyhow!("near-stationary error sum overflow"))?;
                    counts[depth] = counts[depth]
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("near-stationary sample count overflow"))?;
                }
            }
        }
        Ok((sums, counts))
    }

    /// Rows/columns directly above the stationary strip whose document-aligned
    /// difference to the next frame stays above [`NEAR_STATIONARY_ERROR`] —
    /// a fixed overlay's shadow or translucent edge. Bounded so the strip
    /// plus zone still fit inside every retained band.
    fn near_stationary_trailing_zone(&self, strip: usize) -> usize {
        if self.bands.is_empty() {
            return 0;
        }
        let mut zone = 0usize;
        for depth in strip..self.max_edge {
            let count = self.aligned_error_counts[depth];
            let error = if count == 0 {
                0.0
            } else {
                self.aligned_error_sums[depth] as f64 / count as f64
            };
            if error <= NEAR_STATIONARY_ERROR {
                break;
            }
            zone += 1;
        }
        zone
    }

    fn stationary_trailing_strip(&self) -> usize {
        if self.bands.is_empty() {
            return 0;
        }

        let mut strip = 0usize;
        for (&sum, &count) in self.edge_error_sums.iter().zip(&self.edge_error_counts) {
            let error = if count == 0 {
                f64::INFINITY
            } else {
                sum as f64 / count as f64
            };
            if error > STATIONARY_EDGE_ERROR {
                break;
            }
            strip += 1;
        }
        strip
    }

    fn finish_vertical(self, trailing: usize) -> Result<Pixbuf> {
        let Self {
            direction,
            width,
            height,
            first_rgba,
            bands,
            total_delta,
            ..
        } = self;
        let content_bottom = height - trailing;
        let total_height = height + total_delta;
        let row_bytes = width
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("stitched image row size overflow"))?;
        let output_bytes = total_height
            .checked_mul(row_bytes)
            .ok_or_else(|| anyhow::anyhow!("stitched image allocation overflow"))?;
        let mut out = Vec::with_capacity(output_bytes);

        // Preserve only the final fixed edge, then consume each retained band
        // as it is copied. This avoids holding the complete source history at
        // the same time as the complete output allocation.
        let final_trailing = if trailing > 0 {
            let last = bands
                .last()
                .expect("a stationary edge requires at least two frames");
            let local_start = content_bottom - last.source_axis_start;
            let byte_start = local_start * row_bytes;
            last.rgba[byte_start..byte_start + trailing * row_bytes].to_vec()
        } else {
            Vec::new()
        };

        out.extend_from_slice(&first_rgba[..content_bottom * row_bytes]);
        drop(first_rgba);
        for band in bands {
            let source_start = content_bottom - band.delta;
            let local_start = source_start - band.source_axis_start;
            let byte_start = local_start * row_bytes;
            let byte_end = byte_start + band.delta * row_bytes;
            out.extend_from_slice(&band.rgba[byte_start..byte_end]);
        }
        out.extend_from_slice(&final_trailing);
        debug_assert_eq!(out.len(), output_bytes);

        if direction == Some(StitchDirection::Reverse) {
            reverse_rgba_axis_in_place(&mut out, width, total_height, StitchAxis::Vertical);
        }

        Ok(pixbuf_from_rgba(out, width, total_height))
    }

    fn finish_horizontal(self, trailing: usize) -> Result<Pixbuf> {
        let content_right = self.width - trailing;
        let total_width = self.width + self.total_delta;
        let output_stride = total_width
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("stitched image row size overflow"))?;
        let output_bytes = self
            .height
            .checked_mul(output_stride)
            .ok_or_else(|| anyhow::anyhow!("stitched image allocation overflow"))?;
        let mut out = Vec::with_capacity(output_bytes);

        for y in 0..self.height {
            let first_row = y * self.width * 4;
            out.extend_from_slice(&self.first_rgba[first_row..first_row + content_right * 4]);
            for band in &self.bands {
                let band_width = self.width - band.source_axis_start;
                let source_start = content_right - band.delta;
                let local_start = source_start - band.source_axis_start;
                let byte_start = (y * band_width + local_start) * 4;
                out.extend_from_slice(&band.rgba[byte_start..byte_start + band.delta * 4]);
            }
            if trailing > 0 {
                let last = self
                    .bands
                    .last()
                    .expect("a stationary edge requires at least two frames");
                let band_width = self.width - last.source_axis_start;
                let local_start = content_right - last.source_axis_start;
                let byte_start = (y * band_width + local_start) * 4;
                out.extend_from_slice(&last.rgba[byte_start..byte_start + trailing * 4]);
            }
        }
        debug_assert_eq!(out.len(), output_bytes);

        if self.direction == Some(StitchDirection::Reverse) {
            reverse_rgba_axis_in_place(&mut out, total_width, self.height, StitchAxis::Horizontal);
        }

        Ok(pixbuf_from_rgba(out, total_width, self.height))
    }

    #[cfg(test)]
    fn retained_rgba_bytes(&self) -> usize {
        self.first_rgba.len() + self.bands.iter().map(|band| band.rgba.len()).sum::<usize>()
    }
}

#[derive(Clone, Copy, Debug)]
struct SearchSample {
    shift: isize,
    error: f64,
}

fn forward_candidate_set(prev: &GrayView, cur: &GrayView, axis: StitchAxis) -> ForwardCandidateSet {
    let empty = |search_max_delta| ForwardCandidateSet {
        candidates: Vec::new(),
        truncated: false,
        search_max_delta,
    };
    if prev.width != cur.width
        || prev.height != cur.height
        || prev.pixels.len() != prev.width.saturating_mul(prev.height)
        || cur.pixels.len() != cur.width.saturating_mul(cur.height)
    {
        return empty(None);
    }

    let axis_len = prev.axis_len(axis);
    let cross_len = prev.cross_len(axis);
    if axis_len < MIN_OVERLAP_PIXELS + MIN_MOTION_PIXELS || cross_len < 2 {
        return empty(None);
    }

    let min_overlap = (axis_len / MIN_OVERLAP_DEN)
        .max(MIN_OVERLAP_PIXELS)
        .min(axis_len.saturating_sub(1));
    let max_shift = axis_len.saturating_sub(min_overlap);
    let source_scale = prev.source_scale(axis);
    let search_max_delta = max_shift.checked_mul(source_scale);
    let min_shift = (MIN_MOTION_PIXELS / source_scale).saturating_add(1);
    if min_shift > max_shift {
        return empty(search_max_delta);
    }

    let match_extent = axis_len - max_shift;
    let coarse_axis_sample = (match_extent / 32).max(1);
    let coarse_cross_sample = (cross_len / 48).max(1);
    let fine_axis_sample = (match_extent / 256).max(1);
    let fine_cross_sample = (cross_len / 256).max(1);

    // Search only the physically possible direction. This is intentionally
    // not a local delta bound: every forward shift that preserves the global
    // minimum overlap remains eligible.
    let mut coarse = Vec::with_capacity(max_shift - min_shift + 1);
    for shift in min_shift..=max_shift {
        coarse.push(SearchSample {
            shift: shift as isize,
            error: score_shift(
                prev,
                cur,
                axis,
                shift as isize,
                max_shift,
                coarse_axis_sample,
                coarse_cross_sample,
            ),
        });
    }
    coarse.sort_by(|a, b| a.error.total_cmp(&b.error));
    let Some(coarse_best) = coarse.first().copied() else {
        return empty(search_max_delta);
    };
    if !coarse_best.error.is_finite() || coarse_best.error > MAX_MATCH_ERROR {
        return empty(search_max_delta);
    }

    let coarse_limit = plausible_error_limit(coarse_best.error).min(MAX_MATCH_ERROR);
    let peak_neighborhood = (axis_len / 256).clamp(6, 16) as isize;
    let mut peaks: Vec<SearchSample> = Vec::new();
    let mut truncated = false;
    for sample in coarse {
        if sample.error > coarse_limit {
            break;
        }
        if peaks
            .iter()
            .any(|peak| (peak.shift - sample.shift).abs() <= peak_neighborhood)
        {
            continue;
        }
        if peaks.len() == MAX_FORWARD_CANDIDATES {
            truncated = true;
            break;
        }
        peaks.push(sample);
    }

    let max_shift_signed = max_shift as isize;
    let min_shift_signed = min_shift as isize;
    let mut refined: Vec<ForwardMatchCandidate> = Vec::with_capacity(peaks.len());
    for peak in peaks {
        let start = (peak.shift - 1).max(min_shift_signed);
        let end = (peak.shift + 1).min(max_shift_signed);
        let mut best = SearchSample {
            shift: peak.shift,
            error: f64::INFINITY,
        };
        for shift in start..=end {
            let error = score_shift(
                prev,
                cur,
                axis,
                shift,
                max_shift,
                fine_axis_sample,
                fine_cross_sample,
            );
            if error < best.error {
                best = SearchSample { shift, error };
            }
        }
        let delta = best.shift.unsigned_abs().saturating_mul(source_scale);
        if delta <= MIN_MOTION_PIXELS || best.error > MAX_MATCH_ERROR {
            continue;
        }
        if let Some(existing) = refined
            .iter_mut()
            .find(|candidate| candidate.delta.abs_diff(delta) <= peak_neighborhood as usize)
        {
            if best.error < existing.error {
                *existing = ForwardMatchCandidate {
                    delta,
                    error: best.error,
                };
            }
        } else {
            refined.push(ForwardMatchCandidate {
                delta,
                error: best.error,
            });
        }
    }

    refined.sort_by(forward_candidate_order);
    if let Some(best) = refined.first().copied() {
        refined.retain(|candidate| {
            candidate == &best || !errors_are_distinct(best.error, candidate.error)
        });
    }
    ForwardCandidateSet {
        candidates: refined,
        truncated,
        search_max_delta,
    }
}

fn plausible_error_limit(best: f64) -> f64 {
    (best * MIN_CONFIDENCE).max(best + MIN_ERROR_MARGIN)
}

fn errors_are_distinct(best: f64, runner: f64) -> bool {
    if !best.is_finite() || !runner.is_finite() {
        return runner.is_infinite() && best.is_finite();
    }
    let confidence = if best <= f64::EPSILON {
        if runner <= f64::EPSILON {
            1.0
        } else {
            f64::INFINITY
        }
    } else {
        runner / best
    };
    confidence >= MIN_CONFIDENCE && runner - best >= MIN_ERROR_MARGIN
}

fn forward_candidate_order(
    a: &ForwardMatchCandidate,
    b: &ForwardMatchCandidate,
) -> std::cmp::Ordering {
    a.error
        .total_cmp(&b.error)
        .then_with(|| a.delta.cmp(&b.delta))
}

fn forward_path_order(a: &ForwardMatchPath, b: &ForwardMatchPath) -> std::cmp::Ordering {
    a.total_error
        .total_cmp(&b.total_error)
        .then_with(|| a.first_delta.cmp(&b.first_delta))
        .then_with(|| a.second_delta.cmp(&b.second_delta))
}

fn consistent_forward_paths(
    first: &ForwardCandidateSet,
    second: &ForwardCandidateSet,
    cumulative: &ForwardCandidateSet,
) -> Vec<ForwardMatchPath> {
    let mut paths = Vec::new();
    for first_candidate in &first.candidates {
        for second_candidate in &second.candidates {
            let Some(sum) = first_candidate.delta.checked_add(second_candidate.delta) else {
                continue;
            };
            let Some(total_candidate) = cumulative
                .candidates
                .iter()
                .filter(|candidate| candidate.delta.abs_diff(sum) <= PATH_DELTA_TOLERANCE)
                .min_by(|a, b| forward_candidate_order(a, b))
            else {
                continue;
            };
            let consistency_error = total_candidate.delta.abs_diff(sum) as f64;
            paths.push(ForwardMatchPath {
                first_delta: first_candidate.delta,
                second_delta: second_candidate.delta,
                total_error: first_candidate.error
                    + second_candidate.error
                    + total_candidate.error
                    + consistency_error,
            });
        }
    }
    paths
}

fn resolve_forward_candidate_sets(
    first: &ForwardCandidateSet,
    second: &ForwardCandidateSet,
    cumulative: &ForwardCandidateSet,
    allow_physical_prior: bool,
) -> ForwardLookaheadResolution {
    let mut paths = consistent_forward_paths(first, second, cumulative);
    paths.sort_by(forward_path_order);
    let best_consistent = paths.first().copied();
    let fallback = best_consistent.or_else(|| best_independent_path(first, second));

    let Some(best) = best_consistent else {
        if allow_physical_prior
            && let Some(path) = best_unverifiable_auto_path(first, second, cumulative)
        {
            return ForwardLookaheadResolution::LowErrorPeriodic(path);
        }
        return ForwardLookaheadResolution::Unresolved {
            best_effort: fallback,
        };
    };

    let unique = paths
        .get(1)
        .is_none_or(|runner| errors_are_distinct(best.total_error, runner.total_error));
    if unique && !first.truncated && !second.truncated && !cumulative.truncated {
        return ForwardLookaheadResolution::Resolved(best);
    }

    if allow_physical_prior
        && let Some(path) = best_matcher_grade_auto_path(first, second, cumulative)
    {
        return ForwardLookaheadResolution::LowErrorPeriodic(path);
    }

    ForwardLookaheadResolution::Unresolved {
        best_effort: Some(best),
    }
}

/// Select a cumulatively consistent path for known-forward automatic capture.
/// Candidate errors are filtered individually before constructing paths, so a
/// very good pair cannot mask a poor seam in the summed score. Candidate-set
/// truncation is not itself a rejection: a retained path still describes
/// three matcher-grade forward comparisons that agree cumulatively.
fn best_matcher_grade_auto_path(
    first: &ForwardCandidateSet,
    second: &ForwardCandidateSet,
    cumulative: &ForwardCandidateSet,
) -> Option<ForwardMatchPath> {
    let matcher_grade_set = |set: &ForwardCandidateSet| ForwardCandidateSet {
        candidates: set
            .candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.error.is_finite() && candidate.error <= MAX_MATCH_ERROR)
            .collect(),
        truncated: set.truncated,
        search_max_delta: set.search_max_delta,
    };
    let first = matcher_grade_set(first);
    let second = matcher_grade_set(second);
    let cumulative = matcher_grade_set(cumulative);

    let mut paths = consistent_forward_paths(&first, &second, &cumulative);
    paths.sort_by(auto_periodic_path_order);
    paths.first().copied()
}

/// If F0→F1 and F1→F2 are both sound but their sum is beyond the largest
/// F0→F2 displacement that can retain the minimum overlap, absence of a
/// cumulative candidate is not contradictory evidence. Known-forward auto
/// capture may keep the best adjacent path in that one structural case.
fn best_unverifiable_auto_path(
    first: &ForwardCandidateSet,
    second: &ForwardCandidateSet,
    cumulative: &ForwardCandidateSet,
) -> Option<ForwardMatchPath> {
    let search_max_delta = cumulative.search_max_delta?;
    let mut paths = Vec::new();
    for first_candidate in &first.candidates {
        if !matcher_grade_candidate(first_candidate) {
            continue;
        }
        for second_candidate in &second.candidates {
            if !matcher_grade_candidate(second_candidate) {
                continue;
            }
            let Some(sum) = first_candidate.delta.checked_add(second_candidate.delta) else {
                continue;
            };
            if sum <= search_max_delta {
                continue;
            }
            paths.push(ForwardMatchPath {
                first_delta: first_candidate.delta,
                second_delta: second_candidate.delta,
                total_error: first_candidate.error + second_candidate.error,
            });
        }
    }
    paths.sort_by(auto_periodic_path_order);
    paths.first().copied()
}

fn matcher_grade_candidate(candidate: &ForwardMatchCandidate) -> bool {
    candidate.error.is_finite() && candidate.error <= MAX_MATCH_ERROR
}

fn auto_periodic_path_order(a: &ForwardMatchPath, b: &ForwardMatchPath) -> std::cmp::Ordering {
    a.total_error
        .total_cmp(&b.total_error)
        .then_with(|| auto_cadence_prior_score(a).total_cmp(&auto_cadence_prior_score(b)))
        .then_with(|| a.first_delta.cmp(&b.first_delta))
        .then_with(|| a.second_delta.cmp(&b.second_delta))
}

fn auto_cadence_prior_score(path: &ForwardMatchPath) -> f64 {
    let ratio = path.first_delta as f64 / path.second_delta as f64;
    // The automatic worker uses either 3 wheel notches followed by a
    // 1-notch probe, or its keyboard fallback of 5 arrows followed by 2.
    (ratio - 3.0).abs().min((ratio - 2.5).abs())
}

fn best_independent_path(
    first: &ForwardCandidateSet,
    second: &ForwardCandidateSet,
) -> Option<ForwardMatchPath> {
    let first = first.candidates.first()?;
    let second = second.candidates.first()?;
    first.delta.checked_add(second.delta)?;
    Some(ForwardMatchPath {
        first_delta: first.delta,
        second_delta: second.delta,
        total_error: first.error + second.error,
    })
}

fn unmatchable() -> MotionEstimate {
    MotionEstimate {
        motion: Motion::Unmatchable,
        error: f64::INFINITY,
        confidence: 0.0,
    }
}

/// Mean absolute error for a signed candidate shift. Every shift uses
/// `axis_len - max_shift` pixels along the motion axis, centered within the
/// available overlap, so scores have equal sample areas.
fn score_shift(
    prev: &GrayView,
    cur: &GrayView,
    axis: StitchAxis,
    shift: isize,
    max_shift: usize,
    axis_sample_step: usize,
    cross_sample_step: usize,
) -> f64 {
    let axis_len = prev.axis_len(axis);
    let cross_len = prev.cross_len(axis);
    let magnitude = shift.unsigned_abs();
    if magnitude > max_shift || max_shift >= axis_len {
        return f64::INFINITY;
    }

    let match_extent = axis_len - max_shift;
    let available = axis_len - magnitude;
    if available < match_extent || match_extent == 0 {
        return f64::INFINITY;
    }
    let centered = (available - match_extent) / 2;
    let (prev_start, cur_start) = if shift >= 0 {
        (centered + magnitude, centered)
    } else {
        (centered, centered + magnitude)
    };

    let cross_margin = (cross_len / 12).min(cross_len.saturating_sub(1) / 2);
    let cross_start = cross_margin;
    let cross_end = cross_len - cross_margin;
    let axis_step = axis_sample_step.max(1);
    let cross_step = cross_sample_step.max(1);
    let mut total = 0u64;
    let mut count = 0usize;

    match axis {
        StitchAxis::Vertical => {
            for along in (0..match_extent).step_by(axis_step) {
                let prev_row = (prev_start + along) * prev.width;
                let cur_row = (cur_start + along) * cur.width;
                for cross in (cross_start..cross_end).step_by(cross_step) {
                    total +=
                        prev.pixels[prev_row + cross].abs_diff(cur.pixels[cur_row + cross]) as u64;
                    count += 1;
                }
            }
        }
        StitchAxis::Horizontal => {
            for along in (0..match_extent).step_by(axis_step) {
                let prev_column = prev_start + along;
                let cur_column = cur_start + along;
                for cross in (cross_start..cross_end).step_by(cross_step) {
                    total += prev.pixels[cross * prev.width + prev_column]
                        .abs_diff(cur.pixels[cross * cur.width + cur_column])
                        as u64;
                    count += 1;
                }
            }
        }
    }

    if count == 0 {
        f64::INFINITY
    } else {
        total as f64 / count as f64
    }
}

#[cfg(test)]
fn score_shift_reference(
    prev: &GrayView,
    cur: &GrayView,
    axis: StitchAxis,
    shift: isize,
    max_shift: usize,
    axis_sample_step: usize,
    cross_sample_step: usize,
) -> f64 {
    let axis_len = prev.axis_len(axis);
    let cross_len = prev.cross_len(axis);
    let magnitude = shift.unsigned_abs();
    if magnitude > max_shift || max_shift >= axis_len {
        return f64::INFINITY;
    }

    let match_extent = axis_len - max_shift;
    let available = axis_len - magnitude;
    if available < match_extent || match_extent == 0 {
        return f64::INFINITY;
    }
    let centered = (available - match_extent) / 2;
    let (prev_start, cur_start) = if shift >= 0 {
        (centered + magnitude, centered)
    } else {
        (centered, centered + magnitude)
    };

    let cross_margin = (cross_len / 12).min(cross_len.saturating_sub(1) / 2);
    let cross_start = cross_margin;
    let cross_end = cross_len - cross_margin;
    let axis_step = axis_sample_step.max(1);
    let cross_step = cross_sample_step.max(1);
    let mut total = 0u64;
    let mut count = 0usize;

    for along in (0..match_extent).step_by(axis_step) {
        for cross in (cross_start..cross_end).step_by(cross_step) {
            let (prev_idx, cur_idx) = match axis {
                StitchAxis::Vertical => (
                    (prev_start + along) * prev.width + cross,
                    (cur_start + along) * cur.width + cross,
                ),
                StitchAxis::Horizontal => (
                    cross * prev.width + prev_start + along,
                    cross * cur.width + cur_start + along,
                ),
            };
            total += prev.pixels[prev_idx].abs_diff(cur.pixels[cur_idx]) as u64;
            count += 1;
        }
    }

    if count == 0 {
        f64::INFINITY
    } else {
        total as f64 / count as f64
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn validate_frames(frames: &[Pixbuf]) -> Result<()> {
    let Some(first) = frames.first() else {
        bail!("nothing to stitch (no frames captured)");
    };
    validate_frame(first, None)?;
    let expected = (first.width() as usize, first.height() as usize);
    for frame in &frames[1..] {
        validate_frame(frame, Some(expected))?;
    }
    Ok(())
}

fn validate_frame(frame: &Pixbuf, expected: Option<(usize, usize)>) -> Result<()> {
    if frame.width() <= 0 || frame.height() <= 0 {
        bail!("frames have invalid dimensions");
    }
    if frame.n_channels() != 4 {
        bail!("scroll capture requires four-channel RGBA frames");
    }

    let width = frame.width() as usize;
    let height = frame.height() as usize;
    if expected.is_some_and(|dimensions| dimensions != (width, height)) {
        bail!("frames have inconsistent dimensions or pixel formats");
    }

    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| anyhow::anyhow!("frame row size overflow"))?;
    let stride = frame.rowstride() as usize;
    if stride < row_bytes {
        bail!("frame rowstride is smaller than its pixel width");
    }
    let required = (height - 1)
        .checked_mul(stride)
        .and_then(|bytes| bytes.checked_add(row_bytes))
        .ok_or_else(|| anyhow::anyhow!("frame pixel extent overflow"))?;
    if frame.read_pixel_bytes().len() < required {
        bail!("frame pixel buffer is shorter than its dimensions require");
    }
    Ok(())
}

fn validate_delta_and_extent(
    axis: StitchAxis,
    axis_len: usize,
    total_delta: usize,
    delta: usize,
) -> Result<usize> {
    if delta == 0 || delta >= axis_len {
        bail!("invalid {axis:?} stitch delta {delta} for frame extent {axis_len}");
    }

    let new_total = total_delta
        .checked_add(delta)
        .ok_or_else(|| anyhow::anyhow!("stitched image extent overflow"))?;
    let output_extent = axis_len
        .checked_add(new_total)
        .ok_or_else(|| anyhow::anyhow!("stitched image extent overflow"))?;
    if output_extent > i32::MAX as usize {
        bail!("stitched image extent exceeds the supported size");
    }
    Ok(new_total)
}

/// Reflect tight row-major RGBA pixels across the requested axis without
/// allocating a second image-sized buffer.
fn reverse_rgba_axis_in_place(pixels: &mut [u8], width: usize, height: usize, axis: StitchAxis) {
    debug_assert_eq!(pixels.len(), width.saturating_mul(height).saturating_mul(4));
    match axis {
        StitchAxis::Vertical => {
            let row_bytes = width * 4;
            for top in 0..height / 2 {
                let bottom = height - top - 1;
                let bottom_start = bottom * row_bytes;
                let (before_bottom, bottom_and_after) = pixels.split_at_mut(bottom_start);
                let top_row = &mut before_bottom[top * row_bytes..(top + 1) * row_bytes];
                let bottom_row = &mut bottom_and_after[..row_bytes];
                top_row.swap_with_slice(bottom_row);
            }
        }
        StitchAxis::Horizontal => {
            let row_bytes = width * 4;
            for row in pixels.chunks_exact_mut(row_bytes) {
                for left in 0..width / 2 {
                    let right = width - left - 1;
                    for channel in 0..4 {
                        row.swap(left * 4 + channel, right * 4 + channel);
                    }
                }
            }
        }
    }
}

fn copy_frame_tight(frame: &Pixbuf) -> Result<Vec<u8>> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| anyhow::anyhow!("frame row size overflow"))?;
    let capacity = height
        .checked_mul(row_bytes)
        .ok_or_else(|| anyhow::anyhow!("frame allocation overflow"))?;
    let source_bytes = frame.read_pixel_bytes();
    let source = source_bytes.as_ref();
    let source_stride = frame.rowstride() as usize;
    let mut tight = Vec::with_capacity(capacity);
    for y in 0..height {
        let start = y * source_stride;
        tight.extend_from_slice(&source[start..start + row_bytes]);
    }
    Ok(tight)
}

fn copy_tail_band(
    frame: &Pixbuf,
    axis: StitchAxis,
    width: usize,
    height: usize,
    source_axis_start: usize,
) -> Result<Vec<u8>> {
    let source_bytes = frame.read_pixel_bytes();
    let source = source_bytes.as_ref();
    let source_stride = frame.rowstride() as usize;

    match axis {
        StitchAxis::Vertical => {
            let row_bytes = width
                .checked_mul(4)
                .ok_or_else(|| anyhow::anyhow!("retained band row size overflow"))?;
            let band_height = height - source_axis_start;
            let capacity = band_height
                .checked_mul(row_bytes)
                .ok_or_else(|| anyhow::anyhow!("retained scroll band allocation overflow"))?;
            let mut band = Vec::with_capacity(capacity);
            for y in source_axis_start..height {
                let start = y * source_stride;
                band.extend_from_slice(&source[start..start + row_bytes]);
            }
            Ok(band)
        }
        StitchAxis::Horizontal => {
            let band_width = width - source_axis_start;
            let band_row_bytes = band_width
                .checked_mul(4)
                .ok_or_else(|| anyhow::anyhow!("retained band row size overflow"))?;
            let capacity = height
                .checked_mul(band_row_bytes)
                .ok_or_else(|| anyhow::anyhow!("retained scroll band allocation overflow"))?;
            let mut band = Vec::with_capacity(capacity);
            for y in 0..height {
                let start = y * source_stride + source_axis_start * 4;
                band.extend_from_slice(&source[start..start + band_row_bytes]);
            }
            Ok(band)
        }
    }
}

fn pixbuf_from_rgba(pixels: Vec<u8>, width: usize, height: usize) -> Pixbuf {
    let bytes = Bytes::from_owned(pixels);
    Pixbuf::from_bytes(
        &bytes,
        Colorspace::Rgb,
        true,
        8,
        width as i32,
        height as i32,
        (width * 4) as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW_W: usize = 64;
    const VIEW_H: usize = 180;
    const BORDER: [u8; 4] = [128, 159, 191, 255];

    /// Manual diagnosis harness: replay frames dumped by
    /// `TENSAKU_CAPTURE_DEBUG_DIR` through the real classifier + stitcher.
    /// Run with:
    ///   TENSAKU_STITCH_REPLAY_DIR=~/.cache/tensaku/capture-debug \
    ///   TENSAKU_STITCH_REPLAY_OUT=/tmp/replay.png \
    ///   cargo test --release replay_dumped_capture_frames -- --ignored --nocapture
    #[test]
    #[ignore = "manual harness; needs TENSAKU_STITCH_REPLAY_DIR"]
    fn replay_dumped_capture_frames() {
        let dir = std::env::var("TENSAKU_STITCH_REPLAY_DIR")
            .expect("set TENSAKU_STITCH_REPLAY_DIR to the dumped-frame directory");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("replay dir must be readable")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "png")
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("frame-"))
            })
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no frame-*.png files in {dir}");

        let axis = StitchAxis::Vertical;
        let first = Pixbuf::from_file(&files[0]).expect("first frame must load");
        eprintln!(
            "replay: {} frames of {}x{}",
            files.len(),
            first.width(),
            first.height()
        );
        let mut accumulator =
            StitchAccumulator::new(&first, axis).expect("accumulator must initialize");
        let mut last_gray = downsample_to_gray_for_axis(&first, axis);
        let mut skipped = 0usize;
        for path in &files[1..] {
            let frame = Pixbuf::from_file(path).expect("frame must load");
            let gray = downsample_to_gray_for_axis(&frame, axis);
            let delta = match classify_forward_with_lookahead(&last_gray, &gray, axis) {
                ForwardMatch::Classified(estimate) => match estimate.motion {
                    Motion::Forward(delta) => Some(delta),
                    other => {
                        eprintln!("replay: {path:?}: unhandled motion {other:?}");
                        None
                    }
                },
                ForwardMatch::Ambiguous(_) => {
                    eprintln!("replay: {path:?}: ambiguous — skipped");
                    None
                }
            };
            match delta {
                Some(delta) => {
                    accumulator
                        .push_forward(&frame, delta)
                        .expect("push_forward must succeed");
                    last_gray = gray;
                }
                None => skipped += 1,
            }
        }
        let result = accumulator.finish().expect("finish must succeed");
        eprintln!(
            "replay: stitched {}x{} ({} of {} frames skipped)",
            result.width(),
            result.height(),
            skipped,
            files.len()
        );
        if let Ok(out) = std::env::var("TENSAKU_STITCH_REPLAY_OUT") {
            result.savev(&out, "png", &[]).expect("result must save");
            eprintln!("replay: saved {out}");
        }
    }

    #[test]
    fn classifies_varying_forward_deltas_without_a_hint() {
        let offsets = [0, 37, 101, 158];
        let frames: Vec<_> = offsets
            .iter()
            .map(|&y| viewport(0, y, VIEW_W, VIEW_H, None))
            .collect();

        for (pair, expected) in frames.windows(2).zip([37, 64, 57]) {
            let estimate = classify_motion(
                &downsample_to_gray(&pair[0]),
                &downsample_to_gray(&pair[1]),
                StitchAxis::Vertical,
            );
            assert_eq!(estimate.motion, Motion::Forward(expected), "{estimate:?}");
        }

        let stitched = stitch_with_deltas(&frames, &[37, 64, 57], StitchAxis::Vertical).unwrap();
        assert_eq!(stitched.width(), VIEW_W as i32);
        assert_eq!(stitched.height(), (VIEW_H + 158) as i32);
        assert_vertical_document(&stitched, VIEW_H + 158);
    }

    #[test]
    fn classifies_duplicate_and_reverse_motion() {
        let at_80 = viewport(0, 80, VIEW_W, VIEW_H, None);
        let duplicate = viewport(0, 80, VIEW_W, VIEW_H, None);
        let at_23 = viewport(0, 23, VIEW_W, VIEW_H, None);

        let stationary = classify_motion(
            &downsample_to_gray(&at_80),
            &downsample_to_gray(&duplicate),
            StitchAxis::Vertical,
        );
        assert_eq!(stationary.motion, Motion::Stationary);

        let reverse = classify_motion(
            &downsample_to_gray(&at_80),
            &downsample_to_gray(&at_23),
            StitchAxis::Vertical,
        );
        assert_eq!(reverse.motion, Motion::Reverse(57), "{reverse:?}");
    }

    #[test]
    fn accumulator_stitches_reverse_vertical_frames_in_document_order() {
        let offsets = [101, 64, 0];
        let deltas = [37, 64];
        let frames: Vec<_> = offsets
            .iter()
            .map(|&y| viewport(0, y, VIEW_W, VIEW_H, None))
            .collect();
        let mut accumulator = StitchAccumulator::new(&frames[0], StitchAxis::Vertical).unwrap();
        accumulator.push_reverse(&frames[1], deltas[0]).unwrap();
        accumulator.push_reverse(&frames[2], deltas[1]).unwrap();

        let stitched = accumulator.finish().unwrap();
        assert_eq!(stitched.width(), VIEW_W as i32);
        assert_eq!(stitched.height(), (VIEW_H + 101) as i32);
        assert_vertical_document(&stitched, VIEW_H + 101);
    }

    #[test]
    fn reverse_accumulator_keeps_a_fixed_top_strip_once() {
        const FIXED_TOP: usize = 4;
        let offsets = [101, 64, 0];
        let deltas = [37, 64];
        let frames: Vec<_> = offsets
            .iter()
            .map(|&y| viewport_with_fixed_top(y, VIEW_W, VIEW_H, FIXED_TOP))
            .collect();
        let mut accumulator = StitchAccumulator::new(&frames[0], StitchAxis::Vertical).unwrap();
        accumulator.push_reverse(&frames[1], deltas[0]).unwrap();
        accumulator.push_reverse(&frames[2], deltas[1]).unwrap();
        assert_eq!(accumulator.stationary_trailing_strip(), FIXED_TOP);

        let stitched = accumulator.finish().unwrap();
        let expected_height = VIEW_H + deltas.iter().sum::<usize>();
        assert_eq!(stitched.width(), VIEW_W as i32);
        assert_eq!(stitched.height(), expected_height as i32);
        let bytes = stitched.read_pixel_bytes();
        let pixels = bytes.as_ref();
        let stride = stitched.rowstride() as usize;
        for y in 0..expected_height {
            for x in 0..VIEW_W {
                let expected = if y < FIXED_TOP {
                    BORDER
                } else {
                    doc_pixel(x, y - FIXED_TOP)
                };
                assert_eq!(
                    &pixels[y * stride + x * 4..y * stride + x * 4 + 4],
                    &expected
                );
            }
        }
    }

    #[test]
    fn accumulator_rejects_direction_mix_without_mutation() {
        let at_0 = viewport(0, 0, VIEW_W, VIEW_H, None);
        let at_37 = viewport(0, 37, VIEW_W, VIEW_H, None);

        let mut forward = StitchAccumulator::new(&at_0, StitchAxis::Vertical).unwrap();
        forward.push_forward(&at_37, 37).unwrap();
        let forward_before = (
            forward.direction,
            forward.frame_count(),
            forward.total_delta,
            forward.retained_rgba_bytes(),
            forward.first_rgba.clone(),
            forward.edge_error_sums.clone(),
            forward.edge_error_counts.clone(),
        );
        let error = forward.push_reverse(&at_0, 37).unwrap_err();
        assert!(error.to_string().contains("cannot mix"));
        assert_eq!(
            (
                forward.direction,
                forward.frame_count(),
                forward.total_delta,
                forward.retained_rgba_bytes(),
                forward.first_rgba.clone(),
                forward.edge_error_sums.clone(),
                forward.edge_error_counts.clone(),
            ),
            forward_before
        );
        assert_vertical_document(&forward.finish().unwrap(), VIEW_H + 37);

        let mut reverse = StitchAccumulator::new(&at_37, StitchAxis::Vertical).unwrap();
        reverse.push_reverse(&at_0, 37).unwrap();
        let reverse_before = (
            reverse.direction,
            reverse.frame_count(),
            reverse.total_delta,
            reverse.retained_rgba_bytes(),
            reverse.first_rgba.clone(),
            reverse.edge_error_sums.clone(),
            reverse.edge_error_counts.clone(),
        );
        let error = reverse.push_forward(&at_37, 37).unwrap_err();
        assert!(error.to_string().contains("cannot mix"));
        assert_eq!(
            (
                reverse.direction,
                reverse.frame_count(),
                reverse.total_delta,
                reverse.retained_rgba_bytes(),
                reverse.first_rgba.clone(),
                reverse.edge_error_sums.clone(),
                reverse.edge_error_counts.clone(),
            ),
            reverse_before
        );
        assert_vertical_document(&reverse.finish().unwrap(), VIEW_H + 37);
    }

    #[test]
    fn preserves_small_signed_candidate_for_ambiguous_periodic_terminal_content() {
        const DELTA: usize = 18;
        let first = periodic_terminal_gray(0);
        let second = periodic_terminal_gray(DELTA);

        let estimate = classify_motion(&first, &second, StitchAxis::Vertical);

        assert_eq!(estimate.motion, Motion::Ambiguous(DELTA as isize));
        assert!(estimate.error <= MAX_AMBIGUOUS_ERROR, "{estimate:?}");
        assert!(estimate.confidence < MIN_CONFIDENCE, "{estimate:?}");
    }

    #[test]
    fn forward_lookahead_resolves_periodic_terminal_content_with_later_evidence() {
        const FIRST_DELTA: usize = 18;
        const SECOND_DELTA: usize = 182;
        let first = periodic_terminal_with_unique_gray(0);
        let second = periodic_terminal_with_unique_gray(FIRST_DELTA);
        let third = periodic_terminal_with_unique_gray(FIRST_DELTA + SECOND_DELTA);

        let pending = match classify_forward_with_lookahead(&first, &second, StitchAxis::Vertical) {
            ForwardMatch::Ambiguous(pending) => pending,
            ForwardMatch::Classified(estimate) => {
                panic!("expected ambiguous first pair, got {estimate:?}")
            }
        };
        assert!(
            pending
                .candidates()
                .iter()
                .any(|candidate| candidate.delta == FIRST_DELTA),
            "{:?}",
            pending.candidates()
        );

        let resolution = pending.resolve(&third);
        let ForwardLookaheadResolution::Resolved(path) = resolution else {
            panic!("expected unique lookahead path, got {resolution:?}");
        };
        assert_eq!(path.first_delta, FIRST_DELTA, "{path:?}");
        assert_eq!(path.second_delta, SECOND_DELTA, "{path:?}");
    }

    #[test]
    fn forward_lookahead_probes_when_a_reverse_global_alias_beats_real_forward_motion() {
        const FORWARD_DELTA: usize = 60;
        const REVERSE_ALIAS: usize = 240;
        let first = reverse_alias_gray(0);
        let second = reverse_alias_gray(FORWARD_DELTA);

        let strict = classify_motion(&first, &second, StitchAxis::Vertical);
        assert_eq!(strict.motion, Motion::Reverse(REVERSE_ALIAS), "{strict:?}");

        let pending = match classify_forward_with_lookahead(&first, &second, StitchAxis::Vertical) {
            ForwardMatch::Ambiguous(pending) => pending,
            ForwardMatch::Classified(estimate) => {
                panic!("known-forward capture trusted a reverse alias: {estimate:?}")
            }
        };
        assert!(
            pending
                .candidates()
                .iter()
                .any(|candidate| candidate.delta == FORWARD_DELTA),
            "{:?}",
            pending.candidates()
        );

        let third = reverse_alias_gray(FORWARD_DELTA + 20);
        let ForwardLookaheadResolution::Resolved(path) = pending.resolve(&third) else {
            panic!("one smaller forward probe should resolve the reverse alias");
        };
        assert_eq!(path.first_delta, FORWARD_DELTA, "{path:?}");
        assert_eq!(path.second_delta, 20, "{path:?}");
    }

    #[test]
    fn stationary_probe_verifies_a_unique_known_forward_basin() {
        const FORWARD_DELTA: usize = 60;
        let first = reverse_alias_gray(0);
        let second = reverse_alias_gray(FORWARD_DELTA);

        let pending = match classify_forward_with_lookahead(&first, &second, StitchAxis::Vertical) {
            ForwardMatch::Ambiguous(pending) => pending,
            ForwardMatch::Classified(estimate) => {
                panic!("expected the reverse alias to require lookahead, got {estimate:?}")
            }
        };
        assert_eq!(pending.candidates().len(), 1, "{:?}", pending.candidates());

        let resolution = pending.resolve(&second);
        let ForwardLookaheadResolution::StationaryProbe {
            first_match: StationaryProbeFirstMatch::Unique(candidate),
        } = resolution
        else {
            panic!("expected a verified stationary-end seam, got {resolution:?}");
        };
        assert_eq!(candidate.delta, FORWARD_DELTA, "{candidate:?}");
    }

    #[test]
    fn stationary_probe_never_verifies_a_truncated_candidate_search() {
        let frame = unique_document_gray(0);
        let candidate = ForwardMatchCandidate {
            delta: 60,
            error: 0.5,
        };
        let pending = ForwardLookahead {
            origin: frame.clone(),
            pending: frame.clone(),
            axis: StitchAxis::Vertical,
            first: ForwardCandidateSet {
                candidates: vec![candidate],
                truncated: true,
                search_max_delta: Some(240),
            },
            estimate: MotionEstimate {
                motion: Motion::Ambiguous(candidate.delta as isize),
                error: candidate.error,
                confidence: 1.0,
            },
        };

        assert_eq!(
            pending.resolve(&frame),
            ForwardLookaheadResolution::StationaryProbe {
                first_match: StationaryProbeFirstMatch::Ambiguous {
                    best_effort: Some(candidate),
                },
            }
        );
    }

    #[test]
    fn physical_end_bound_isolates_only_the_small_repeated_content_basin() {
        const TRUE_DELTA: usize = 18;
        let first = exactly_periodic_gray(0);
        let second = exactly_periodic_gray(TRUE_DELTA);
        let pending = match classify_forward_with_lookahead(&first, &second, StitchAxis::Vertical) {
            ForwardMatch::Ambiguous(pending) => pending,
            ForwardMatch::Classified(estimate) => {
                panic!("expected exact periodicity to be ambiguous, got {estimate:?}")
            }
        };

        assert_eq!(
            pending.unique_candidate_at_most(60),
            Some(ForwardMatchCandidate {
                delta: TRUE_DELTA,
                error: 0.0,
            })
        );
        assert_eq!(
            pending.unique_candidate_at_most(100),
            None,
            "a bound containing both +18 and its +90 alias must stay ambiguous"
        );
    }

    #[test]
    fn forward_lookahead_preserves_a_unique_genuine_reverse_result() {
        const DELTA: usize = 60;
        let first = unique_document_gray(DELTA);
        let second = unique_document_gray(0);

        let ForwardMatch::Classified(estimate) =
            classify_forward_with_lookahead(&first, &second, StitchAxis::Vertical)
        else {
            panic!("unique backward movement should not enter forward lookahead");
        };
        assert_eq!(estimate.motion, Motion::Reverse(DELTA), "{estimate:?}");
    }

    #[test]
    fn forward_lookahead_does_not_guess_on_identical_periodic_content() {
        const FIRST_DELTA: usize = 18;
        const SECOND_DELTA: usize = 54;
        let first = exactly_periodic_gray(0);
        let second = exactly_periodic_gray(FIRST_DELTA);
        let third = exactly_periodic_gray(FIRST_DELTA + SECOND_DELTA);

        let pending = match classify_forward_with_lookahead(&first, &second, StitchAxis::Vertical) {
            ForwardMatch::Ambiguous(pending) => pending,
            ForwardMatch::Classified(estimate) => {
                panic!("expected ambiguous first pair, got {estimate:?}")
            }
        };
        let resolution = pending.resolve(&third);
        assert!(
            matches!(
                resolution,
                ForwardLookaheadResolution::Unresolved {
                    best_effort: Some(_)
                }
            ),
            "{resolution:?}"
        );
    }

    #[test]
    fn automatic_lookahead_uses_three_to_one_cadence_for_exact_periodicity() {
        const FIRST_DELTA: usize = 54;
        const SECOND_DELTA: usize = 18;
        let first = exactly_periodic_gray(0);
        let second = exactly_periodic_gray(FIRST_DELTA);
        let third = exactly_periodic_gray(FIRST_DELTA + SECOND_DELTA);

        let pending = match classify_forward_with_lookahead(&first, &second, StitchAxis::Vertical) {
            ForwardMatch::Ambiguous(pending) => pending,
            ForwardMatch::Classified(estimate) => {
                panic!("expected ambiguous first pair, got {estimate:?}")
            }
        };

        assert!(
            matches!(
                pending.resolve(&third),
                ForwardLookaheadResolution::Unresolved {
                    best_effort: Some(_)
                }
            ),
            "strict resolution must remain hint-free"
        );

        let ForwardLookaheadResolution::LowErrorPeriodic(path) = pending.resolve_auto(&third)
        else {
            panic!("automatic resolution should use the controlled cadence");
        };
        assert_eq!(path.first_delta, FIRST_DELTA, "{path:?}");
        assert_eq!(path.second_delta, SECOND_DELTA, "{path:?}");
    }

    #[test]
    fn automatic_lookahead_accepts_real_792_330_path_beyond_cumulative_overlap() {
        const FIRST_DELTA: usize = 792;
        const SECOND_DELTA: usize = 330;
        let first = real_periodic_terminal_gray(0);
        let second = real_periodic_terminal_gray(FIRST_DELTA);
        let third = real_periodic_terminal_gray(FIRST_DELTA + SECOND_DELTA);

        let pending = match classify_forward_with_lookahead(&first, &second, StitchAxis::Vertical) {
            ForwardMatch::Ambiguous(pending) => pending,
            ForwardMatch::Classified(estimate) => {
                panic!("expected the +792/-660 periodic aliases to require lookahead: {estimate:?}")
            }
        };
        assert_eq!(pending.first.search_max_delta, Some(1_052));
        assert!(
            matches!(
                pending.resolve(&third),
                ForwardLookaheadResolution::Unresolved {
                    best_effort: Some(ForwardMatchPath {
                        first_delta: FIRST_DELTA,
                        second_delta: SECOND_DELTA,
                        ..
                    })
                }
            ),
            "strict resolution cannot compare a +1122 cumulative displacement"
        );

        let ForwardLookaheadResolution::LowErrorPeriodic(path) = pending.resolve_auto(&third)
        else {
            panic!("known-forward auto capture should trust both adjacent seams");
        };
        assert_eq!(path.first_delta, FIRST_DELTA, "{path:?}");
        assert_eq!(path.second_delta, SECOND_DELTA, "{path:?}");
        assert_eq!(path.total_error, 0.0, "{path:?}");
    }

    #[test]
    fn automatic_lookahead_probes_matcher_grade_periodicity_above_two_gray_levels() {
        const FIRST_DELTA: usize = 54;
        const SECOND_DELTA: usize = 18;
        let brighten = |mut view: GrayView, amount: u8| {
            for pixel in &mut view.pixels {
                *pixel = pixel.saturating_add(amount);
            }
            view
        };
        let first = exactly_periodic_gray(0);
        let second = brighten(exactly_periodic_gray(FIRST_DELTA), 4);
        let third = brighten(exactly_periodic_gray(FIRST_DELTA + SECOND_DELTA), 8);

        let strict = classify_motion(&first, &second, StitchAxis::Vertical);
        assert_eq!(strict.motion, Motion::Unmatchable, "{strict:?}");
        assert!(strict.error > MAX_AMBIGUOUS_ERROR, "{strict:?}");
        assert!(strict.error <= MAX_MATCH_ERROR, "{strict:?}");

        let pending = match classify_forward_with_lookahead(&first, &second, StitchAxis::Vertical) {
            ForwardMatch::Ambiguous(pending) => pending,
            ForwardMatch::Classified(estimate) => {
                panic!("matcher-grade ambiguity should receive an auto probe: {estimate:?}")
            }
        };
        assert!(matches!(
            pending.resolve_auto(&third),
            ForwardLookaheadResolution::LowErrorPeriodic(_)
        ));
    }

    #[test]
    fn automatic_periodic_resolution_rejects_an_independent_inconsistent_fallback() {
        let first = ForwardCandidateSet {
            candidates: vec![ForwardMatchCandidate {
                delta: 54,
                error: 0.0,
            }],
            truncated: false,
            search_max_delta: Some(100),
        };
        let second = ForwardCandidateSet {
            candidates: vec![ForwardMatchCandidate {
                delta: 18,
                error: 0.0,
            }],
            truncated: false,
            search_max_delta: Some(100),
        };
        let cumulative = ForwardCandidateSet {
            candidates: vec![ForwardMatchCandidate {
                delta: 80,
                error: 0.0,
            }],
            truncated: false,
            search_max_delta: Some(100),
        };

        assert_eq!(
            resolve_forward_candidate_sets(&first, &second, &cumulative, true),
            ForwardLookaheadResolution::Unresolved {
                best_effort: Some(ForwardMatchPath {
                    first_delta: 54,
                    second_delta: 18,
                    total_error: 0.0,
                }),
            }
        );
    }

    #[test]
    fn automatic_periodic_resolution_rejects_a_high_error_seam() {
        let candidates = |deltas: &[usize], error| ForwardCandidateSet {
            candidates: deltas
                .iter()
                .copied()
                .map(|delta| ForwardMatchCandidate { delta, error })
                .collect(),
            truncated: false,
            search_max_delta: Some(240),
        };
        let first = candidates(&[54, 126], 0.0);
        let second = candidates(&[18, 90], MAX_MATCH_ERROR + 0.01);
        let cumulative = candidates(&[72, 144, 216], 0.0);

        assert!(matches!(
            resolve_forward_candidate_sets(&first, &second, &cumulative, true),
            ForwardLookaheadResolution::Unresolved {
                best_effort: Some(_)
            }
        ));
    }

    #[test]
    fn automatic_periodic_resolution_accepts_a_retained_path_in_a_truncated_landscape() {
        let candidates = |deltas: &[usize], truncated| ForwardCandidateSet {
            candidates: deltas
                .iter()
                .copied()
                .map(|delta| ForwardMatchCandidate { delta, error: 0.0 })
                .collect(),
            truncated,
            search_max_delta: Some(240),
        };
        let first = candidates(&[54, 126], true);
        let second = candidates(&[18, 90], false);
        let cumulative = candidates(&[72, 144, 216], false);

        assert_eq!(
            resolve_forward_candidate_sets(&first, &second, &cumulative, true),
            ForwardLookaheadResolution::LowErrorPeriodic(ForwardMatchPath {
                first_delta: 54,
                second_delta: 18,
                total_error: 0.0,
            })
        );
    }

    #[test]
    fn automatic_periodic_resolution_accepts_noncanonical_matcher_grade_path() {
        let candidates = |values: &[(usize, f64)], truncated| ForwardCandidateSet {
            candidates: values
                .iter()
                .map(|&(delta, error)| ForwardMatchCandidate { delta, error })
                .collect(),
            truncated,
            search_max_delta: Some(1_200),
        };
        let first = candidates(&[(420, 4.0), (700, 4.2)], true);
        let second = candidates(&[(360, 5.0), (640, 5.3)], false);
        let cumulative = candidates(&[(780, 6.0), (1_060, 6.5)], false);

        assert_eq!(
            resolve_forward_candidate_sets(&first, &second, &cumulative, true),
            ForwardLookaheadResolution::LowErrorPeriodic(ForwardMatchPath {
                first_delta: 420,
                second_delta: 360,
                total_error: 15.0,
            })
        );
    }

    #[test]
    fn forward_lookahead_reports_stationary_periodic_probe_without_a_second_band() {
        const FIRST_DELTA: usize = 18;
        let first = exactly_periodic_gray(0);
        let second = exactly_periodic_gray(FIRST_DELTA);

        let pending = match classify_forward_with_lookahead(&first, &second, StitchAxis::Vertical) {
            ForwardMatch::Ambiguous(pending) => pending,
            ForwardMatch::Classified(estimate) => {
                panic!("expected ambiguous first pair, got {estimate:?}")
            }
        };
        assert!(
            pending.candidates().len() > 1,
            "exact periodicity should retain multiple forward basins: {:?}",
            pending.candidates()
        );

        // The confirmation capture is byte-for-byte F1 again. Periodicity
        // supplies plenty of convincing non-zero aliases, but none of them
        // represents real F1→F2 motion and therefore none may become a
        // synthetic second delta.
        let resolution = pending.resolve(&second);
        let ForwardLookaheadResolution::StationaryProbe {
            first_match:
                StationaryProbeFirstMatch::Ambiguous {
                    best_effort: Some(candidate),
                },
        } = resolution
        else {
            panic!("expected a stationary probe, got {resolution:?}");
        };
        assert!(
            pending.candidates().contains(&candidate),
            "{candidate:?} not in {:?}",
            pending.candidates()
        );
    }

    #[test]
    fn bounded_manual_search_recovers_signed_nearby_periodic_peak() {
        const DELTA: usize = 60;
        const MANUAL_BOUND: usize = 100;
        let first = periodic_alias_gray(0);
        let second = periodic_alias_gray(DELTA);

        // The unconstrained matcher correctly reports the lowest-error global
        // peak, but periodic content makes that a distant repetition rather
        // than the physical movement. Its behavior remains deliberately
        // strict and hint-free.
        let strict = classify_motion(&first, &second, StitchAxis::Vertical);
        assert_eq!(strict.motion, Motion::Ambiguous(360), "{strict:?}");

        let forward = classify_motion_bounded(&first, &second, StitchAxis::Vertical, MANUAL_BOUND);
        assert_eq!(forward.motion, Motion::Forward(DELTA), "{forward:?}");

        let reverse = classify_motion_bounded(&second, &first, StitchAxis::Vertical, MANUAL_BOUND);
        assert_eq!(reverse.motion, Motion::Reverse(DELTA), "{reverse:?}");
    }

    #[test]
    fn excludes_fixed_bottom_border_from_each_slice() {
        let offsets = [0, 37, 101];
        let frames: Vec<_> = offsets
            .iter()
            .map(|&y| viewport(0, y, VIEW_W, VIEW_H, Some(4)))
            .collect();
        let mut accumulator = StitchAccumulator::new(&frames[0], StitchAxis::Vertical).unwrap();
        accumulator.push_forward(&frames[1], 37).unwrap();
        accumulator.push_forward(&frames[2], 64).unwrap();
        assert_eq!(accumulator.stationary_trailing_strip(), 4);

        let stitched = accumulator.finish().unwrap();
        let content_height = VIEW_H - 4 + 101;
        assert_eq!(stitched.height(), (VIEW_H + 101) as i32);
        let bytes = stitched.read_pixel_bytes();
        let bytes = bytes.as_ref();
        let stride = stitched.rowstride() as usize;
        for y in 0..content_height {
            for x in 0..VIEW_W {
                assert_eq!(
                    &bytes[y * stride + x * 4..y * stride + x * 4 + 4],
                    &doc_pixel(x, y)
                );
            }
        }
        for y in content_height..content_height + 4 {
            for x in 0..VIEW_W {
                assert_eq!(&bytes[y * stride + x * 4..y * stride + x * 4 + 4], &BORDER);
            }
        }
    }

    #[test]
    fn accumulator_retains_incremental_bands_instead_of_full_frames() {
        let offsets = [0, 37, 101, 158];
        let deltas = [37, 64, 57];
        let frames: Vec<_> = offsets
            .iter()
            .map(|&y| viewport(0, y, VIEW_W, VIEW_H, None))
            .collect();
        let mut accumulator = StitchAccumulator::new(&frames[0], StitchAxis::Vertical).unwrap();
        for (frame, delta) in frames.iter().skip(1).zip(deltas) {
            accumulator.push_forward(frame, delta).unwrap();
        }

        let edge_halo = (VIEW_H / 8).min(MAX_STATIONARY_EDGE);
        let expected_bytes = VIEW_W * VIEW_H * 4
            + deltas
                .iter()
                .map(|delta| VIEW_W * (delta + edge_halo) * 4)
                .sum::<usize>();
        let full_frame_bytes = frames.len() * VIEW_W * VIEW_H * 4;
        assert_eq!(accumulator.frame_count(), frames.len());
        assert_eq!(accumulator.retained_rgba_bytes(), expected_bytes);
        assert!(accumulator.retained_rgba_bytes() < full_frame_bytes);

        let stitched = accumulator.finish().unwrap();
        assert_vertical_document(&stitched, VIEW_H + 158);
    }

    #[test]
    fn saved_halo_handles_a_trailing_edge_that_changes_late() {
        let frames = [
            viewport(0, 0, VIEW_W, VIEW_H, Some(8)),
            viewport(0, 37, VIEW_W, VIEW_H, Some(8)),
            viewport(0, 101, VIEW_W, VIEW_H, Some(4)),
        ];
        let deltas = [37, 64];
        let mut accumulator = StitchAccumulator::new(&frames[0], StitchAxis::Vertical).unwrap();
        accumulator.push_forward(&frames[1], deltas[0]).unwrap();
        accumulator.push_forward(&frames[2], deltas[1]).unwrap();

        // Freezing the edge after the first pair would incorrectly use eight
        // rows. Online all-frame statistics reduce the stationary strip to
        // four, and the saved halo still contains the shifted slices needed at
        // finish time. The four rows that were border in the first two frames
        // and content in the third are near-stationary (border vs content
        // when document-aligned), so the trailing extent grows back to eight:
        // those document rows are taken from the last frame, where they are
        // visible, instead of emitting hidden border rows mid-image.
        assert_eq!(accumulator.stationary_trailing_strip(), 4);
        assert_eq!(accumulator.near_stationary_trailing_zone(4), 4);
        let stitched = accumulator.finish().unwrap();
        assert_vertical_composition(&stitched, &frames, &deltas, 8);
    }

    #[test]
    fn rejects_jump_larger_than_safe_overlap() {
        let first = viewport(0, 0, VIEW_W, VIEW_H, None);
        let jumped = viewport(0, 150, VIEW_W, VIEW_H, None);
        let estimate = classify_motion(
            &downsample_to_gray(&first),
            &downsample_to_gray(&jumped),
            StitchAxis::Vertical,
        );
        assert_eq!(estimate.motion, Motion::Unmatchable, "{estimate:?}");
    }

    #[test]
    fn classifies_non_grid_motion_in_a_large_high_frequency_viewport() {
        const HEIGHT: usize = 2000;
        const DELTA: usize = 719;
        let first = viewport(0, 0, VIEW_W, HEIGHT, None);
        let second = viewport(0, DELTA, VIEW_W, HEIGHT, None);
        let estimate = classify_motion(
            &downsample_to_gray(&first),
            &downsample_to_gray(&second),
            StitchAxis::Vertical,
        );
        assert_eq!(estimate.motion, Motion::Forward(DELTA), "{estimate:?}");
    }

    /// A sparse light page: white with irregular dark text rows, so most
    /// pixels carry no alignment signal — the case where a fixed band's
    /// constant error floor used to swamp the true shift.
    fn sparse_doc_pixel(x: usize, y: usize) -> [u8; 4] {
        let line = y / 5;
        let mut h = (line as u64).wrapping_mul(0x9E37_79B1_85EB_CA77);
        h ^= h >> 29;
        let text_row = h % 4 == 0;
        let ink = text_row && (doc_pixel(x, line)[0] % 3 != 0);
        if ink {
            [20, 20, 24, 255]
        } else {
            [250, 250, 250, 255]
        }
    }

    /// Viewport over the sparse page with a dark sticky header and a bright
    /// fixed footer that stay put while the content between them scrolls.
    fn viewport_with_chrome(document_y: usize, header: usize, footer: usize) -> Pixbuf {
        let mut pixels = Vec::with_capacity(VIEW_W * VIEW_H * 4);
        for y in 0..VIEW_H {
            for x in 0..VIEW_W {
                let pixel = if y < header {
                    [28 + (x % 5) as u8, 28, 30, 255]
                } else if y >= VIEW_H - footer {
                    [255, 214, 10, 255]
                } else {
                    sparse_doc_pixel(x, document_y + y - header)
                };
                pixels.extend_from_slice(&pixel);
            }
        }
        pixbuf_from_rgba(pixels, VIEW_W, VIEW_H)
    }

    /// Viewport with an opaque fixed footer whose translucent drop shadow
    /// darkens the `shadow` document rows above it, strongest next to the
    /// footer. `shaded == false` gives the same view without the shadow.
    fn viewport_with_shadowed_footer(
        document_y: usize,
        footer: usize,
        shadow: usize,
        shaded: bool,
    ) -> Pixbuf {
        let mut pixels = Vec::with_capacity(VIEW_W * VIEW_H * 4);
        for y in 0..VIEW_H {
            for x in 0..VIEW_W {
                let pixel = if y >= VIEW_H - footer {
                    BORDER
                } else {
                    let mut pixel = doc_pixel(x, document_y + y);
                    let distance = VIEW_H - footer - y; // 1 = touching the footer
                    if shaded && distance <= shadow {
                        let tint = ((shadow + 1 - distance) * 6) as u8;
                        for channel in &mut pixel[..3] {
                            *channel = channel.saturating_sub(tint);
                        }
                    }
                    pixel
                };
                pixels.extend_from_slice(&pixel);
            }
        }
        pixbuf_from_rgba(pixels, VIEW_W, VIEW_H)
    }

    #[test]
    fn footer_shadow_is_emitted_once_not_at_every_seam() {
        const FOOTER: usize = 10;
        const SHADOW: usize = 8;
        let offsets = [0usize, 37, 78, 107];
        let deltas = [37usize, 41, 29];
        let frames: Vec<_> = offsets
            .iter()
            .map(|&y| viewport_with_shadowed_footer(y, FOOTER, SHADOW, true))
            .collect();

        let mut accumulator = StitchAccumulator::new(&frames[0], StitchAxis::Vertical).unwrap();
        for (frame, &delta) in frames.iter().skip(1).zip(&deltas) {
            accumulator.push_forward(frame, delta).unwrap();
        }
        let strip = accumulator.stationary_trailing_strip();
        assert_eq!(strip, FOOTER);
        let zone = accumulator.near_stationary_trailing_zone(strip);
        assert_eq!(zone, SHADOW, "shadow rows above the footer");

        // Every content row before the final footer + shadow equals the
        // untinted document, i.e. no seam carries the shadow.
        let stitched = accumulator.finish().unwrap();
        let total: usize = deltas.iter().sum::<usize>() + VIEW_H;
        assert_eq!(stitched.height() as usize, total);
        let bytes = stitched.read_pixel_bytes();
        let stride = stitched.rowstride() as usize;
        for y in 0..total - FOOTER - SHADOW {
            for x in 0..VIEW_W {
                let off = y * stride + x * 4;
                assert_eq!(
                    &bytes[off..off + 3],
                    &doc_pixel(x, y)[..3],
                    "row {y} col {x} carries a tint"
                );
            }
        }
        // The last frame's shadow and footer close the image, once.
        let tail_row = total - 1;
        let footer_off = tail_row * stride;
        assert_eq!(&bytes[footer_off..footer_off + 3], &BORDER[..3]);
        let shaded_row = total - FOOTER - 1;
        let shaded_off = shaded_row * stride;
        assert!(
            bytes[shaded_off] < doc_pixel(0, shaded_row)[0] || doc_pixel(0, shaded_row)[0] < 6,
            "final shadow row {shaded_row} should be tinted"
        );
    }

    #[test]
    fn plain_translation_has_no_near_stationary_zone() {
        let frames = [
            viewport(0, 0, VIEW_W, VIEW_H, Some(10)),
            viewport(0, 40, VIEW_W, VIEW_H, Some(10)),
            viewport(0, 85, VIEW_W, VIEW_H, Some(10)),
        ];
        let mut accumulator = StitchAccumulator::new(&frames[0], StitchAxis::Vertical).unwrap();
        accumulator.push_forward(&frames[1], 40).unwrap();
        accumulator.push_forward(&frames[2], 45).unwrap();
        let strip = accumulator.stationary_trailing_strip();
        assert_eq!(strip, 10);
        assert_eq!(accumulator.near_stationary_trailing_zone(strip), 0);
    }

    #[test]
    fn horizontal_edge_shadow_is_emitted_once() {
        // Fixed right edge (12 columns) with a 6-column shadow fading left.
        const EDGE: usize = 12;
        const SHADOW: usize = 6;
        let (w, h) = (180usize, 64usize);
        let frame = |document_x: usize| {
            let mut pixels = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    let pixel = if x >= w - EDGE {
                        BORDER
                    } else {
                        let mut pixel = doc_pixel(document_x + x, y);
                        let distance = w - EDGE - x;
                        if distance <= SHADOW {
                            let tint = ((SHADOW + 1 - distance) * 8) as u8;
                            for channel in &mut pixel[..3] {
                                *channel = channel.saturating_sub(tint);
                            }
                        }
                        pixel
                    };
                    pixels.extend_from_slice(&pixel);
                }
            }
            pixbuf_from_rgba(pixels, w, h)
        };
        let frames = [frame(0), frame(43), frame(95)];
        let mut accumulator = StitchAccumulator::new(&frames[0], StitchAxis::Horizontal).unwrap();
        accumulator.push_forward(&frames[1], 43).unwrap();
        accumulator.push_forward(&frames[2], 52).unwrap();
        let strip = accumulator.stationary_trailing_strip();
        assert_eq!(strip, EDGE);
        assert_eq!(accumulator.near_stationary_trailing_zone(strip), SHADOW);
        let stitched = accumulator.finish().unwrap();
        assert_eq!(stitched.width() as usize, w + 95);
        let bytes = stitched.read_pixel_bytes();
        let stride = stitched.rowstride() as usize;
        for y in 0..h {
            for x in 0..w + 95 - EDGE - SHADOW {
                let off = y * stride + x * 4;
                assert_eq!(
                    &bytes[off..off + 3],
                    &doc_pixel(x, y)[..3],
                    "col {x} row {y}"
                );
            }
        }
    }

    #[test]
    fn scoring_crops_high_contrast_fixed_header_and_footer() {
        let first = downsample_to_gray(&viewport_with_chrome(0, 24, 16));
        let second = downsample_to_gray(&viewport_with_chrome(53, 24, 16));
        // Blank rows adjoining the footer are unchanged too and may extend the
        // trailing crop; they carry no signal, so that is harmless.
        let (lead, trail) = stationary_scoring_edges(&first, &second, StitchAxis::Vertical);
        assert_eq!(lead, 24);
        assert!(trail >= 16, "trail {trail}");
        let estimate = classify_motion(&first, &second, StitchAxis::Vertical);
        assert_eq!(estimate.motion, Motion::Forward(53), "{estimate:?}");
        assert!(estimate.confidence >= MIN_CONFIDENCE, "{estimate:?}");
        assert!(estimate.error <= MAX_AMBIGUOUS_ERROR, "{estimate:?}");
    }

    #[test]
    fn scoring_edges_stay_bounded_for_a_stationary_pair() {
        let frame = downsample_to_gray(&viewport_with_chrome(40, 24, 16));
        let (lead, trail) = stationary_scoring_edges(&frame, &frame, StitchAxis::Vertical);
        assert_eq!(lead, VIEW_H / MAX_STATIONARY_SCORING_EDGE_DEN);
        assert_eq!(trail, VIEW_H / MAX_STATIONARY_SCORING_EDGE_DEN);
        let estimate = classify_motion(&frame, &frame, StitchAxis::Vertical);
        assert_eq!(estimate.motion, Motion::Stationary, "{estimate:?}");
    }

    #[test]
    fn scoring_crops_a_fixed_sidebar_for_horizontal_motion() {
        // Fixed 12-column sidebar on the left; content scrolls right by 43.
        let frame = |document_x: usize| {
            let (w, h) = (180usize, 64usize);
            let mut pixels = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    let pixel = if x < 12 {
                        [40, 44, 52, 255]
                    } else {
                        sparse_doc_pixel(y, document_x + x - 12)
                    };
                    pixels.extend_from_slice(&pixel);
                }
            }
            pixbuf_from_rgba(pixels, w, h)
        };
        let first = downsample_to_gray_for_axis(&frame(0), StitchAxis::Horizontal);
        let second = downsample_to_gray_for_axis(&frame(43), StitchAxis::Horizontal);
        assert_eq!(
            stationary_scoring_edges(&first, &second, StitchAxis::Horizontal).0,
            12
        );
        let estimate = classify_motion(&first, &second, StitchAxis::Horizontal);
        assert_eq!(estimate.motion, Motion::Forward(43), "{estimate:?}");
    }

    #[test]
    fn ignores_sticky_header_and_small_dynamic_badge() {
        let first = viewport_with_sticky_header(0, 1);
        let second = viewport_with_sticky_header(53, 2);
        let estimate = classify_motion(
            &downsample_to_gray(&first),
            &downsample_to_gray(&second),
            StitchAxis::Vertical,
        );
        assert_eq!(estimate.motion, Motion::Forward(53), "{estimate:?}");
    }

    #[test]
    fn classifies_and_stitches_horizontal_motion() {
        let offsets = [0, 43, 95];
        let frames: Vec<_> = offsets
            .iter()
            .map(|&x| viewport(x, 0, 180, 64, None))
            .collect();
        let first = classify_motion(
            &downsample_to_gray_for_axis(&frames[0], StitchAxis::Horizontal),
            &downsample_to_gray_for_axis(&frames[1], StitchAxis::Horizontal),
            StitchAxis::Horizontal,
        );
        assert_eq!(first.motion, Motion::Forward(43), "{first:?}");

        let stitched = stitch_with_deltas(&frames, &[43, 52], StitchAxis::Horizontal).unwrap();
        assert_eq!(stitched.width(), 275);
        assert_eq!(stitched.height(), 64);
        let bytes = stitched.read_pixel_bytes();
        let bytes = bytes.as_ref();
        let stride = stitched.rowstride() as usize;
        for y in 0..64 {
            for x in 0..275 {
                assert_eq!(
                    &bytes[y * stride + x * 4..y * stride + x * 4 + 4],
                    &doc_pixel(x, y)
                );
            }
        }
    }

    #[test]
    fn accumulator_excludes_a_fixed_right_border_from_horizontal_slices() {
        let offsets = [0, 44, 96];
        let frames: Vec<_> = offsets
            .iter()
            .map(|&x| viewport_with_fixed_right(x, 180, 64, 4))
            .collect();
        let mut accumulator = StitchAccumulator::new(&frames[0], StitchAxis::Horizontal).unwrap();
        accumulator.push_forward(&frames[1], 44).unwrap();
        accumulator.push_forward(&frames[2], 52).unwrap();
        assert_eq!(accumulator.stationary_trailing_strip(), 4);

        let stitched = accumulator.finish().unwrap();
        let content_width = 180 - 4 + 96;
        let bytes = stitched.read_pixel_bytes();
        let bytes = bytes.as_ref();
        let stride = stitched.rowstride() as usize;
        for y in 0..64 {
            for x in 0..content_width {
                assert_eq!(
                    &bytes[y * stride + x * 4..y * stride + x * 4 + 4],
                    &doc_pixel(x, y)
                );
            }
            for x in content_width..content_width + 4 {
                assert_eq!(&bytes[y * stride + x * 4..y * stride + x * 4 + 4], &BORDER);
            }
        }
    }

    #[test]
    fn accumulator_copies_padded_pixbuf_rows_tightly() {
        const WIDTH: usize = 17;
        const HEIGHT: usize = 40;
        let first = padded_viewport(0, WIDTH, HEIGHT, 12);
        let second = padded_viewport(9, WIDTH, HEIGHT, 12);
        let mut accumulator = StitchAccumulator::new(&first, StitchAxis::Vertical).unwrap();
        accumulator.push_forward(&second, 9).unwrap();

        let stitched = accumulator.finish().unwrap();
        assert_eq!(stitched.rowstride(), (WIDTH * 4) as i32);
        assert_vertical_document(&stitched, HEIGHT + 9);
    }

    /// Development microbenchmark for the viewport size from the terminal
    /// regression fixture. Kept ignored so normal test runs stay fast; run
    /// with `cargo test benchmark_tall_motion_matcher -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn benchmark_tall_motion_matcher() {
        const WIDTH: usize = 530;
        const HEIGHT: usize = 3270;
        const DELTA: usize = 330;
        const RUNS: usize = 5;

        let frame = |document_y: usize| {
            let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let pixel = doc_pixel(x, document_y + y);
                    pixels.push(
                        ((pixel[0] as u32 * 77 + pixel[1] as u32 * 150 + pixel[2] as u32 * 29) >> 8)
                            as u8,
                    );
                }
            }
            GrayView {
                pixels,
                width: WIDTH,
                height: HEIGHT,
                source_width: WIDTH * DOWNSAMPLE_CROSS,
            }
        };
        let first = frame(0);
        let second = frame(DELTA);

        let rgba_width = WIDTH * DOWNSAMPLE_CROSS;
        let mut rgba = Vec::with_capacity(rgba_width * HEIGHT * 4);
        for y in 0..HEIGHT {
            for x in 0..rgba_width {
                rgba.extend_from_slice(&doc_pixel(x, y));
            }
        }
        let pixbuf = pixbuf_from_rgba(rgba, rgba_width, HEIGHT);
        let downsample_started = std::time::Instant::now();
        for _ in 0..RUNS {
            std::hint::black_box(downsample_to_gray_for_axis(
                std::hint::black_box(&pixbuf),
                StitchAxis::Vertical,
            ));
        }
        let downsample_elapsed = downsample_started.elapsed();
        eprintln!(
            "tall downsample: {RUNS} runs in {downsample_elapsed:?}, average {:?}",
            downsample_elapsed / RUNS as u32
        );

        let started = std::time::Instant::now();
        let mut estimate = unmatchable();
        for _ in 0..RUNS {
            estimate = std::hint::black_box(classify_motion(
                std::hint::black_box(&first),
                std::hint::black_box(&second),
                StitchAxis::Vertical,
            ));
        }
        let elapsed = started.elapsed();
        eprintln!(
            "tall matcher: {RUNS} runs in {elapsed:?}, average {:?}, result {estimate:?}",
            elapsed / RUNS as u32
        );
        assert_eq!(estimate.motion, Motion::Forward(DELTA));
    }

    #[test]
    fn specialized_shift_scoring_is_bit_exact() {
        let view = |salt: usize| {
            let width = 53;
            let height = 71;
            let pixels = (0..width * height)
                .map(|index| {
                    let x = index % width;
                    let y = index / width;
                    doc_pixel(x + salt * 7, y + salt * 11)[(x + y) % 3]
                })
                .collect();
            GrayView {
                pixels,
                width,
                height,
                source_width: width * DOWNSAMPLE_CROSS,
            }
        };
        let prev = view(0);
        let cur = view(1);

        for axis in [StitchAxis::Vertical, StitchAxis::Horizontal] {
            let axis_len = prev.axis_len(axis);
            for max_shift in [3, 7, axis_len / 3] {
                for shift in -(max_shift as isize)..=max_shift as isize {
                    for axis_step in [0, 1, 2, 5, 13] {
                        for cross_step in [0, 1, 3, 8] {
                            let optimized = score_shift(
                                &prev, &cur, axis, shift, max_shift, axis_step, cross_step,
                            );
                            let reference = score_shift_reference(
                                &prev, &cur, axis, shift, max_shift, axis_step, cross_step,
                            );
                            assert_eq!(
                                optimized.to_bits(),
                                reference.to_bits(),
                                "{axis:?} shift={shift} max={max_shift} axis_step={axis_step} cross_step={cross_step}"
                            );
                        }
                    }
                }
            }
        }
    }

    fn viewport(
        document_x: usize,
        document_y: usize,
        width: usize,
        height: usize,
        fixed_bottom: Option<usize>,
    ) -> Pixbuf {
        let mut pixels = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let pixel = if fixed_bottom.is_some_and(|rows| y >= height - rows) {
                    BORDER
                } else {
                    doc_pixel(document_x + x, document_y + y)
                };
                pixels.extend_from_slice(&pixel);
            }
        }
        pixbuf_from_rgba(pixels, width, height)
    }

    fn viewport_with_fixed_top(
        document_y: usize,
        width: usize,
        height: usize,
        fixed_top: usize,
    ) -> Pixbuf {
        let mut pixels = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let pixel = if y < fixed_top {
                    BORDER
                } else {
                    doc_pixel(x, document_y + y - fixed_top)
                };
                pixels.extend_from_slice(&pixel);
            }
        }
        pixbuf_from_rgba(pixels, width, height)
    }

    /// Terminal-like rows repeat in blocks while a faint translucent desktop
    /// background stays fixed in viewport coordinates. The true small shift
    /// and shifts one block away consequently have very similar errors. A
    /// tiny per-block marker makes the true peak the best one without making
    /// it distinct enough to satisfy the normal confidence threshold.
    fn periodic_terminal_gray(document_y: usize) -> GrayView {
        const WIDTH: usize = 96;
        const HEIGHT: usize = 360;
        const BLOCK_HEIGHT: usize = 144;
        const BACKGROUND_PERIOD: usize = 48;

        let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
        for screen_y in 0..HEIGHT {
            let doc_y = document_y + screen_y;
            let block = doc_y / BLOCK_HEIGHT;
            let in_block = doc_y % BLOCK_HEIGHT;
            let line = in_block / 12;
            let glyph_row = (2..7).contains(&(in_block % 12));

            for x in 0..WIDTH {
                let background_phase = (screen_y / 6 + x / 12) % (BACKGROUND_PERIOD / 12);
                let mut value = 20 + background_phase as u8;

                // Sparse, high-contrast runs stand in for glyphs on each
                // terminal row. Their layout repeats every command block.
                let glyph_start = 8 + (line * 7) % 28;
                let glyph_end = (glyph_start + 28 + (line * 5) % 34).min(WIDTH - 8);
                if glyph_row
                    && (glyph_start..glyph_end).contains(&x)
                    && !(x + line).is_multiple_of(5)
                {
                    value = 150 + (line % 6) as u8 * 8;
                }

                // This small prompt/metadata mark varies between repeated
                // blocks, like timestamps or a shell prompt would.
                if (4..8).contains(&in_block) && (12..36).contains(&x) {
                    value = 72 + (block % 4) as u8 * 5;
                }
                pixels.push(value);
            }
        }

        GrayView {
            pixels,
            width: WIDTH,
            height: HEIGHT,
            source_width: WIDTH,
        }
    }

    /// The first pair's equal-area search sees the repeated command blocks,
    /// while the third frame moves this non-repeating status line into the
    /// cumulative comparison area. It models a terminal where later output
    /// contains a unique prompt or diagnostic between repeated commands.
    fn periodic_terminal_with_unique_gray(document_y: usize) -> GrayView {
        const UNIQUE_TOP: usize = 328;
        const UNIQUE_BOTTOM: usize = 344;
        let mut view = periodic_terminal_gray(document_y);
        for screen_y in 0..view.height {
            let absolute_y = document_y + screen_y;
            if !(UNIQUE_TOP..UNIQUE_BOTTOM).contains(&absolute_y) {
                continue;
            }
            for x in 14..view.width - 10 {
                let value = (absolute_y as u64)
                    .wrapping_mul(0x9E37_79B1)
                    .wrapping_add((x as u64).wrapping_mul(0x85EB_CA77));
                view.pixels[screen_y * view.width + x] = 48 + (value % 190) as u8;
            }
        }
        view
    }

    /// Every document row repeats exactly. There is no viewport-fixed signal,
    /// block marker, or later unique content that could distinguish aliases.
    fn exactly_periodic_gray(document_y: usize) -> GrayView {
        const WIDTH: usize = 96;
        const HEIGHT: usize = 360;
        const PERIOD: usize = 72;
        let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
        for screen_y in 0..HEIGHT {
            let row = (document_y + screen_y) % PERIOD;
            for x in 0..WIDTH {
                let mut value = (row as u64)
                    .wrapping_mul(0x9E37_79B1)
                    .wrapping_add((x as u64).wrapping_mul(0x85EB_CA77));
                value ^= value >> 17;
                pixels.push(24 + (value % 210) as u8);
            }
        }
        GrayView {
            pixels,
            width: WIDTH,
            height: HEIGHT,
            source_width: WIDTH,
        }
    }

    /// Exact geometry from the reported terminal capture: a 1578px viewport,
    /// 1452px repeated blocks, a +792px normal step, and a +330px probe. The
    /// adjacent seams retain safe overlap, while their +1122px sum exceeds the
    /// cumulative matcher's 1052px ceiling.
    fn real_periodic_terminal_gray(document_y: usize) -> GrayView {
        const WIDTH: usize = 48;
        const HEIGHT: usize = 1_578;
        const PERIOD: usize = 1_452;
        let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
        for screen_y in 0..HEIGHT {
            let row = (document_y + screen_y) % PERIOD;
            for x in 0..WIDTH {
                let mut value = (row as u64)
                    .wrapping_mul(0x9E37_79B1)
                    .wrapping_add((x as u64).wrapping_mul(0x85EB_CA77));
                value ^= value >> 17;
                pixels.push(24 + (value % 210) as u8);
            }
        }
        GrayView {
            pixels,
            width: WIDTH,
            height: HEIGHT,
            source_width: WIDTH,
        }
    }

    /// A periodic document over a faint viewport-fixed pattern. The document
    /// moves by 60px, but its 300px repetition and the background's 360px
    /// repetition make +360px the global minimum. A manual small-motion bound
    /// excludes that remote alias and exposes the true signed peak.
    fn periodic_alias_gray(document_y: usize) -> GrayView {
        const WIDTH: usize = 96;
        const HEIGHT: usize = 900;
        const DOCUMENT_PERIOD: usize = 300;
        const BACKGROUND_PERIOD: usize = 360;

        let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
        for screen_y in 0..HEIGHT {
            let absolute_doc_y = document_y + screen_y;
            let doc_y = absolute_doc_y % DOCUMENT_PERIOD;
            let document_block = absolute_doc_y / DOCUMENT_PERIOD;
            let background = u8::from(screen_y % BACKGROUND_PERIOD >= BACKGROUND_PERIOD / 2);
            for x in 0..WIDTH {
                let mut value = (doc_y as u64)
                    .wrapping_mul(0x9E37_79B1)
                    .wrapping_add((x as u64).wrapping_mul(0x85EB_CA77));
                value ^= value >> 17;
                value = value.wrapping_mul(0xC2B2_AE3D);
                let content = 24 + (value % 180) as u8;
                let block_marker = if (8..12).contains(&x) && !document_block.is_multiple_of(2) {
                    4
                } else {
                    0
                };
                pixels.push(content + block_marker + background);
            }
        }

        GrayView {
            pixels,
            width: WIDTH,
            height: HEIGHT,
            source_width: WIDTH,
        }
    }

    /// The document repeats every 300px, while a faint viewport-fixed layer
    /// repeats every 240px. Advancing the document by 60px therefore makes
    /// -240px the distinct global minimum even though +60px is the physical
    /// motion. This models repeated terminal rows over a translucent desktop.
    fn reverse_alias_gray(document_y: usize) -> GrayView {
        const WIDTH: usize = 96;
        const HEIGHT: usize = 900;
        const DOCUMENT_PERIOD: usize = 300;
        const BACKGROUND_PERIOD: usize = 240;

        let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
        for screen_y in 0..HEIGHT {
            let doc_y = (document_y + screen_y) % DOCUMENT_PERIOD;
            let background = 3 * u8::from(screen_y % BACKGROUND_PERIOD >= BACKGROUND_PERIOD / 2);
            for x in 0..WIDTH {
                let mut value = (doc_y as u64)
                    .wrapping_mul(0x9E37_79B1)
                    .wrapping_add((x as u64).wrapping_mul(0x85EB_CA77));
                value ^= value >> 17;
                value = value.wrapping_mul(0xC2B2_AE3D);
                pixels.push(24 + (value % 180) as u8 + background);
            }
        }

        GrayView {
            pixels,
            width: WIDTH,
            height: HEIGHT,
            source_width: WIDTH,
        }
    }

    fn unique_document_gray(document_y: usize) -> GrayView {
        const WIDTH: usize = 96;
        const HEIGHT: usize = 360;
        let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
        for screen_y in 0..HEIGHT {
            let absolute_y = document_y + screen_y;
            for x in 0..WIDTH {
                let mut value = (absolute_y as u64)
                    .wrapping_mul(0x9E37_79B1)
                    .wrapping_add((x as u64).wrapping_mul(0x85EB_CA77));
                value ^= value >> 15;
                value = value.wrapping_mul(0xC2B2_AE3D);
                pixels.push(16 + (value % 220) as u8);
            }
        }
        GrayView {
            pixels,
            width: WIDTH,
            height: HEIGHT,
            source_width: WIDTH,
        }
    }

    fn viewport_with_sticky_header(document_y: usize, badge_value: u8) -> Pixbuf {
        const HEADER_HEIGHT: usize = 24;
        let mut pixels = Vec::with_capacity(VIEW_W * VIEW_H * 4);
        for y in 0..VIEW_H {
            for x in 0..VIEW_W {
                let pixel = if y < HEADER_HEIGHT {
                    if (4..12).contains(&x) && (4..12).contains(&y) {
                        [badge_value, 220, 90, 255]
                    } else {
                        [30 + (x % 17) as u8, 42, 58, 255]
                    }
                } else {
                    doc_pixel(x, document_y + y - HEADER_HEIGHT)
                };
                pixels.extend_from_slice(&pixel);
            }
        }
        pixbuf_from_rgba(pixels, VIEW_W, VIEW_H)
    }

    fn viewport_with_fixed_right(
        document_x: usize,
        width: usize,
        height: usize,
        fixed_right: usize,
    ) -> Pixbuf {
        let mut pixels = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let pixel = if x >= width - fixed_right {
                    BORDER
                } else {
                    doc_pixel(document_x + x, y)
                };
                pixels.extend_from_slice(&pixel);
            }
        }
        pixbuf_from_rgba(pixels, width, height)
    }

    fn padded_viewport(document_y: usize, width: usize, height: usize, padding: usize) -> Pixbuf {
        let rowstride = width * 4 + padding;
        let mut pixels = vec![0xA5; rowstride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * rowstride + x * 4;
                pixels[offset..offset + 4].copy_from_slice(&doc_pixel(x, document_y + y));
            }
        }
        let bytes = Bytes::from_owned(pixels);
        Pixbuf::from_bytes(
            &bytes,
            Colorspace::Rgb,
            true,
            8,
            width as i32,
            height as i32,
            rowstride as i32,
        )
    }

    fn doc_pixel(x: usize, y: usize) -> [u8; 4] {
        let mut value = (x as u64)
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add((y as u64).wrapping_mul(0x85EB_CA77));
        value ^= value >> 17;
        value = value.wrapping_mul(0xC2B2_AE3D);
        value ^= value >> 13;
        [
            value as u8,
            value.rotate_left(19) as u8,
            value.rotate_left(41) as u8,
            255,
        ]
    }

    fn assert_vertical_document(pixbuf: &Pixbuf, height: usize) {
        let bytes = pixbuf.read_pixel_bytes();
        let bytes = bytes.as_ref();
        let stride = pixbuf.rowstride() as usize;
        for y in 0..height {
            for x in 0..pixbuf.width() as usize {
                assert_eq!(
                    &bytes[y * stride + x * 4..y * stride + x * 4 + 4],
                    &doc_pixel(x, y)
                );
            }
        }
    }

    fn assert_vertical_composition(
        stitched: &Pixbuf,
        frames: &[Pixbuf],
        deltas: &[usize],
        trailing: usize,
    ) {
        let width = frames[0].width() as usize;
        let frame_height = frames[0].height() as usize;
        let content_bottom = frame_height - trailing;
        let expected_height = frame_height + deltas.iter().sum::<usize>();
        assert_eq!(stitched.width(), width as i32);
        assert_eq!(stitched.height(), expected_height as i32);

        let mut expected = Vec::with_capacity(width * expected_height * 4);
        let row_bytes = width * 4;
        let first_bytes = frames[0].read_pixel_bytes();
        let first_stride = frames[0].rowstride() as usize;
        for y in 0..content_bottom {
            expected.extend_from_slice(
                &first_bytes.as_ref()[y * first_stride..y * first_stride + row_bytes],
            );
        }
        for (frame, &delta) in frames.iter().skip(1).zip(deltas) {
            let bytes = frame.read_pixel_bytes();
            let stride = frame.rowstride() as usize;
            for y in content_bottom - delta..content_bottom {
                expected.extend_from_slice(&bytes.as_ref()[y * stride..y * stride + row_bytes]);
            }
        }
        if trailing > 0 {
            let last = frames.last().unwrap();
            let bytes = last.read_pixel_bytes();
            let stride = last.rowstride() as usize;
            for y in content_bottom..frame_height {
                expected.extend_from_slice(&bytes.as_ref()[y * stride..y * stride + row_bytes]);
            }
        }

        assert_eq!(stitched.read_pixel_bytes().as_ref(), expected);
    }
}
