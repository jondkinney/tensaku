use anyhow::Result;
use glow::HasContext;
use std::{
    cell::{RefCell, RefMut},
    collections::HashSet,
    num::NonZeroU32,
    path::PathBuf,
    rc::Rc,
};

use femtovg::{
    Canvas, CompositeOperation, FontId, ImageFlags, ImageSource, Paint, Path, PixelFormat,
    RenderTarget, Transform2D,
    imgref::{Img, ImgVec},
    renderer,
    rgb::{RGB, RGBA, RGBA8},
};
use fontconfig::Fontconfig;
use gtk::{glib, prelude::*, subclass::prelude::*};
use relm4::gtk::gdk_pixbuf::Pixbuf;
use relm4::{Sender, gtk};
use resource::resource;

use crate::{
    APP_CONFIG,
    configuration::Action,
    math::{Vec2D, rect_ensure_in_bounds, rect_round},
    sketch_board::SketchBoardInput,
    tools::{CanvasTransform, CropTool, Drawable, DrawableId, Stacked, Tool, UndoAction},
};

use super::{CANVAS_PADDING_CSS, font_stack, set_font_stack};

const TRANSPARENCY_SQUARE_SIZE: usize = 64;

/// Lowest auto-fit zoom a vertical window resize will shrink the
/// image to. Enforced two ways so it holds under any window manager:
/// `apply_vertical_resize_floor` pins a minimum height on `outer_box`
/// so a *floating* window can't be dragged shorter, and
/// `auto_fit_scale` floors the vertical fit term at render time so a
/// *tiled* window — whose compositor ignores that min-size request —
/// clips the image instead of squeezing it past this zoom.
const MIN_AUTO_FIT_ZOOM: f32 = 0.10;
/// Ambient ("contact") shadow: a tight, even halo right at the image
/// edge. No vertical offset — sells the "the image is sitting on the
/// surface" half of the macOS shadow model.
const SHADOW_AMBIENT_BLUR_CSS: f32 = 12.0;
const SHADOW_AMBIENT_ALPHA: f32 = 0.25;
/// Key ("elevation") shadow: wider, offset downward. This is the
/// layer that actually reads as "the window is floating above the
/// desktop" — the look macOS uses. Layered on top of the
/// ambient so the two combine into a soft, asymmetric shadow that
/// pools more below than above.
const SHADOW_KEY_BLUR_CSS: f32 = 40.0;
const SHADOW_KEY_OFFSET_Y_CSS: f32 = 14.0;
const SHADOW_KEY_ALPHA: f32 = 0.45;
/// Maximum overshoot (CSS px) the rubber-band rendering will display
/// when the user pans past the image edge. The hyperbolic damping
/// in `rubber_band` asymptotes to this value, so it's the "elastic
/// stretch" budget.
const RUBBER_BAND_MAX_OVERSHOOT_CSS: f32 = 100.0;
/// Hyperbolic-damping constant. Apple's reference value is 0.55;
/// lower numbers add more resistance (1 px of drag produces less
/// visible motion at the limit), higher numbers feel loose. 0.30
/// gives the canvas a heavier, more deliberate feel when you push
/// past the edge — less of a flick, more of a tug.
const RUBBER_BAND_RESISTANCE: f32 = 0.30;
/// How long the user must be idle before the spring-back animation
/// kicks in. Short enough that releasing your fingers feels snappy,
/// long enough that mid-gesture pauses don't trigger a jarring
/// retreat.
const SPRING_BACK_IDLE_MS: u128 = 40;
/// Tick interval for the spring-back timer. ~60 fps so the recovery
/// looks fluid on standard refresh rates.
pub const SPRING_BACK_TICK_MS: u64 = 16;
/// Natural angular frequency of the spring-back animation
/// (`ω = sqrt(k/m)` with m=1). Settling time for a critically
/// damped spring is roughly `4/ω`. ω=18 ≈ 220 ms settling — a hair
/// faster than macOS' overscroll recovery so the long exponential
/// tail doesn't drag on. Raise it for snappier, lower for softer.
const SPRING_BACK_OMEGA: f32 = 18.0;
/// Position-snap threshold (image-space px). Past this proximity to
/// the limit, force `drag_offset` to exactly the limit so we stop
/// drifting on subpixel residuals from the long exponential tail.
const SPRING_BACK_SNAP_EPS: f32 = 0.5;
/// Fraction of the initial displacement that triggers the snap
/// (combined with `SPRING_BACK_SNAP_EPS` so we stop the timer once
/// we're effectively at the target rather than chasing the
/// asymptotic tail forever).
const SPRING_BACK_DONE_FRACTION: f32 = 0.004;
/// Hyperbolic rubber-band damping. Returns the rendered offset for a
/// given raw `value`: untouched while within `±limit`, then damped
/// past the limit so the visible offset asymptotes at `limit +
/// max_overshoot`. Matches the curve used by UIScrollView's elastic
/// scrolling — the further the user pulls, the more resistance.
fn rubber_band(value: f32, limit: f32, max_overshoot: f32) -> f32 {
    if value.abs() <= limit || max_overshoot <= 0.0 {
        return value;
    }
    let sign = value.signum();
    let beyond = value.abs() - limit;
    let damped =
        max_overshoot * (1.0 - 1.0 / (1.0 + beyond * RUBBER_BAND_RESISTANCE / max_overshoot));
    sign * (limit + damped)
}

/// Inverse of `rubber_band`: given the desired visible offset, return
/// the `drag_offset` value that would produce it. Used by the
/// spring-back animation so we can drive the VISIBLE offset on a
/// smooth curve and let the renderer's rubber-band map handle the
/// rest — animating `drag_offset` directly through this non-linear
/// map produced the "stuck then snap" feel (most of the curve was
/// spent near the asymptote where visible motion barely changes).
fn inverse_rubber_band(visible: f32, limit: f32, max_overshoot: f32) -> f32 {
    if visible.abs() <= limit || max_overshoot <= 0.0 || RUBBER_BAND_RESISTANCE <= 0.0 {
        return visible;
    }
    let sign = visible.signum();
    let v_over = (visible.abs() - limit).min(max_overshoot - 0.001);
    let drag_over = max_overshoot * v_over / (RUBBER_BAND_RESISTANCE * (max_overshoot - v_over));
    sign * (limit + drag_over)
}

/// Closed-form critically damped spring response. Returns the
/// fraction of the initial displacement that REMAINS at time `t`
/// (seconds). At `t = 0` the value is `1` (no movement yet); as `t`
/// grows the value approaches `0` (fully recovered). The curve has
/// zero slope at `t = 0` (gentle start), accelerates quickly, then
/// decelerates into the target via a long exponential tail — same
/// shape UIScrollView uses for overscroll release.
fn critically_damped_remaining(t: f32) -> f32 {
    let wt = SPRING_BACK_OMEGA * t;
    (1.0 + wt) * (-wt).exp()
}

/// Compute the spring-back position for a single axis given the
/// animation's start position and elapsed time. `start` is where
/// `drag_offset` sat the moment the animation began; the target is
/// the nearest hard limit (or `start` itself if already inside).
/// Returns `(new_value, done)` where `done` is true once we're
/// either snapped to the limit or the spring has decayed enough
/// that the residual is invisible.
fn spring_back_progress(start: f32, limit: f32, elapsed_ms: f32) -> (f32, bool) {
    if start.abs() <= limit {
        return (start, true);
    }
    let target = if start > 0.0 { limit } else { -limit };
    let t = elapsed_ms / 1000.0;
    let remaining = critically_damped_remaining(t);
    let value = target + (start - target) * remaining;
    let snapped =
        remaining < SPRING_BACK_DONE_FRACTION || (value - target).abs() < SPRING_BACK_SNAP_EPS;
    if snapped {
        (target, true)
    } else {
        (value, false)
    }
}

/// Slack allocated around the background raster so a run of small
/// grows can re-view the same memory instead of reallocating and
/// copying. Costs ~15 MB on a 6144x3456 RGB capture, and absorbs a few
/// hundred single-pixel nudges before the next allocation.
const RASTER_GROWTH_PAD: i32 = 256;

/// Geometry of a raster resize: which part of the old raster survives,
/// where it lands in the new one, and how thick the extension strip on
/// each side is.
///
/// Split out from the resize itself because two callers need it: the
/// path that allocates a new raster and copies into it, and the path
/// that just re-views a larger allocation and so has nothing to copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResizeLayout {
    /// Surviving rect in the OLD raster's coordinates.
    src_x: i32,
    src_y: i32,
    copy_w: i32,
    copy_h: i32,
    /// Where that rect lands in the new raster.
    dst_x: i32,
    dst_y: i32,
    grow_left: i32,
    grow_top: i32,
    grow_right: i32,
    grow_bottom: i32,
}

impl ResizeLayout {
    fn new(src_x: i32, src_y: i32, new_w: i32, new_h: i32, orig_w: i32, orig_h: i32) -> Self {
        let isec_x = src_x.max(0);
        let isec_y = src_y.max(0);
        let isec_w = ((src_x + new_w).min(orig_w) - isec_x).max(0);
        let isec_h = ((src_y + new_h).min(orig_h) - isec_y).max(0);
        Self {
            src_x: isec_x,
            src_y: isec_y,
            copy_w: isec_w,
            copy_h: isec_h,
            dst_x: isec_x - src_x,
            dst_y: isec_y - src_y,
            grow_left: (-src_x).max(0),
            grow_top: (-src_y).max(0),
            grow_right: ((src_x + new_w) - orig_w).max(0),
            grow_bottom: ((src_y + new_h) - orig_h).max(0),
        }
    }
}

/// How far each side of a raster extends past the original capture.
/// The background shadow stays anchored to that capture across every resize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CaptureInset {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl CaptureInset {
    fn from_rect(rect: crate::math::Rect, image: &Pixbuf) -> Self {
        Self {
            left: rect.pos.x.floor().max(0.0) as i32,
            top: rect.pos.y.floor().max(0.0) as i32,
            right: (image.width() as f32 - (rect.pos.x + rect.size.x).ceil()).max(0.0) as i32,
            bottom: (image.height() as f32 - (rect.pos.y + rect.size.y).ceil()).max(0.0) as i32,
        }
    }
}

fn extend_background(dst: &Pixbuf, src: &Pixbuf, layout: ResizeLayout, inset: CaptureInset) {
    if layout.grow_left == 0
        && layout.grow_top == 0
        && layout.grow_right == 0
        && layout.grow_bottom == 0
    {
        return;
    }

    // Screenshot pixels stay intact. Only the surrounding canvas grows, with
    // a neutral fill and a shadow around the original screenshot rectangle.
    let extension = CanvasBackground {
        x: layout.dst_x - layout.src_x + inset.left,
        y: layout.dst_y - layout.src_y + inset.top,
        width: src.width() - inset.left - inset.right,
        height: src.height() - inset.top - inset.bottom,
    };

    let (w, h) = (dst.width(), dst.height());
    // Four disjoint rectangles cover only newly exposed pixels. The shadow is
    // a function of distance to the source, so growing in small steps has the
    // same result as growing once. Existing pixels and undo views survive.
    extension.fill(dst, 0, 0, w, layout.grow_top);
    extension.fill(dst, 0, h - layout.grow_bottom, w, layout.grow_bottom);
    let middle_height = h - layout.grow_top - layout.grow_bottom;
    extension.fill(dst, 0, layout.grow_top, layout.grow_left, middle_height);
    extension.fill(
        dst,
        w - layout.grow_right,
        layout.grow_top,
        layout.grow_right,
        middle_height,
    );
}

/// Resize `old` to `(src_x, src_y, new_w, new_h)`, re-viewing `alloc`
/// when the request still fits inside it and allocating a padded
/// replacement when it doesn't.
///
/// Returns the new view together with the allocation and origin to
/// remember for next time. The view's pixels are identical to what
/// `resize_pixbuf_to_rect` would have produced — that equivalence is
/// what `view_resize_matches_copy_resize` pins down — it just avoids
/// the copy whenever the surviving pixels are already in place.
#[allow(clippy::too_many_arguments)]
fn resize_raster_in_alloc(
    old: &Pixbuf,
    alloc: Option<&Pixbuf>,
    origin: (i32, i32),
    src_x: i32,
    src_y: i32,
    new_w: i32,
    new_h: i32,
    inset: CaptureInset,
) -> Option<(Pixbuf, Pixbuf, (i32, i32))> {
    if new_w <= 0 || new_h <= 0 {
        return None;
    }
    let layout = ResizeLayout::new(src_x, src_y, new_w, new_h, old.width(), old.height());

    if let Some(alloc) = alloc {
        let nx = origin.0 + src_x;
        let ny = origin.1 + src_y;
        if nx >= 0 && ny >= 0 && nx + new_w <= alloc.width() && ny + new_h <= alloc.height() {
            let view = alloc.new_subpixbuf(nx, ny, new_w, new_h);
            // The surviving pixels are already in place, so only the
            // strips the view newly covers need painting. The fill doesn't
            // read source pixels, so overlapping views are safe.
            extend_background(&view, old, layout, inset);
            return Some((view, alloc.clone(), (nx, ny)));
        }
    }

    // Doesn't fit: allocate with slack on every side so the next
    // several resizes take the path above, and pay one copy.
    let pad = RASTER_GROWTH_PAD;
    let fresh = Pixbuf::new(
        old.colorspace(),
        old.has_alpha(),
        old.bits_per_sample(),
        new_w + 2 * pad,
        new_h + 2 * pad,
    )?;
    if layout.copy_w > 0 && layout.copy_h > 0 {
        old.copy_area(
            layout.src_x,
            layout.src_y,
            layout.copy_w,
            layout.copy_h,
            &fresh,
            pad + layout.dst_x,
            pad + layout.dst_y,
        );
    }
    let view = fresh.new_subpixbuf(pad, pad, new_w, new_h);
    extend_background(&view, old, layout, inset);
    Some((view, fresh, (pad, pad)))
}

/// Resize by allocating a fresh raster and copying the surviving pixels
/// into it.
///
/// Kept as the reference the view-based path is checked against: it is
/// the straightforward implementation, so if the two ever disagree it
/// is the fast one that is wrong.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn resize_pixbuf_to_rect(
    original: &Pixbuf,
    src_x: i32,
    src_y: i32,
    new_w: i32,
    new_h: i32,
    inset: CaptureInset,
) -> Option<Pixbuf> {
    if new_w <= 0 || new_h <= 0 {
        return None;
    }
    let new = Pixbuf::new(
        original.colorspace(),
        original.has_alpha(),
        original.bits_per_sample(),
        new_w,
        new_h,
    )?;
    // No clear first: the copy below plus the strips `extend_background`
    // paints cover the new raster exactly, which `resize_covers_every_
    // pixel` pins down.
    let layout = ResizeLayout::new(
        src_x,
        src_y,
        new_w,
        new_h,
        original.width(),
        original.height(),
    );
    if layout.copy_w > 0 && layout.copy_h > 0 {
        original.copy_area(
            layout.src_x,
            layout.src_y,
            layout.copy_w,
            layout.copy_h,
            &new,
            layout.dst_x,
            layout.dst_y,
        );
    }
    extend_background(&new, original, layout, inset);
    Some(new)
}

#[cfg(test)]
fn read_pixel(p: &Pixbuf, x: i32, y: i32, has_alpha: bool) -> (u8, u8, u8, u8) {
    let stride = p.rowstride() as usize;
    let bpp = if has_alpha { 4 } else { 3 };
    let idx = y as usize * stride + x as usize * bpp;
    unsafe {
        let buf = p.pixels();
        let r = buf[idx];
        let g = buf[idx + 1];
        let b = buf[idx + 2];
        let a = if has_alpha { buf[idx + 3] } else { 255 };
        (r, g, b, a)
    }
}

/// RGBA colour sample used by the edge-extension fills.
type Rgba = (u8, u8, u8, u8);

/// Write `row` — already `w * bytes_per_pixel` bytes — into `h`
/// consecutive rows starting at `(x, y)`.
///
/// The one place the extension fills touch raw Pixbuf memory. The
/// obvious `put_pixel` loop costs a GObject call per pixel, which
/// showed up as ~5 ms of every canvas auto-grow at 6144x3456: the
/// strips a grow fills are full-width, so the pixel count is large
/// even when the strip is thin.
fn blit_row(p: &Pixbuf, x: i32, y: i32, h: i32, row: &[u8]) {
    if h <= 0 || x < 0 || y < 0 || row.is_empty() {
        return;
    }
    let bpp = if p.has_alpha() { 4 } else { 3 };
    let stride = p.rowstride() as usize;
    let start_col = x as usize * bpp;
    if start_col + row.len() > stride || (y + h) as usize > p.height() as usize {
        return;
    }

    // SAFETY: `pixels()` hands back the Pixbuf's live buffer. The
    // bounds check above guarantees every write below stays inside the
    // row it targets and inside the last line.
    unsafe {
        let buf = p.pixels();
        for yy in y..(y + h) {
            let offset = yy as usize * stride + start_col;
            buf[offset..offset + row.len()].copy_from_slice(row);
        }
    }
}

/// The bytes of one pixel, in this Pixbuf's layout.
fn pixel_bytes(p: &Pixbuf, (r, g, b, a): Rgba) -> Vec<u8> {
    if p.has_alpha() {
        vec![r, g, b, a]
    } else {
        vec![r, g, b]
    }
}

/// Flood `w`x`h` at `(x, y)` with a solid colour.
fn fill_rect(p: &Pixbuf, x: i32, y: i32, w: i32, h: i32, color: Rgba) {
    if w <= 0 {
        return;
    }
    blit_row(p, x, y, h, &pixel_bytes(p, color).repeat(w as usize));
}

/// Neutral canvas around the screenshot. This is also the editor's backdrop,
/// so expanding the canvas reveals annotation space without stretching pixels.
const CANVAS_GRAY: u8 = 0x24;
const CANVAS_COLOR: Rgba = (CANVAS_GRAY, CANVAS_GRAY, CANVAS_GRAY, 255);
const SHADOW_EXTENT: i32 = (SHADOW_KEY_BLUR_CSS + SHADOW_KEY_OFFSET_Y_CSS) as i32;

struct CanvasBackground {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl CanvasBackground {
    fn color(&self, x: i32, y: i32) -> Rgba {
        // Match the two box-gradient layers used by the editor shadow. Pixel
        // centers and signed distance give smooth corners without sampling or
        // smearing any screenshot content into the new canvas.
        let shadow = |offset_y: f32, blur: f32, alpha: f32| {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5 - offset_y;
            let dx = (self.x as f32 - px).max(px - (self.x + self.width) as f32);
            let dy = (self.y as f32 - py).max(py - (self.y + self.height) as f32);
            let distance = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0);
            alpha * (0.5 - distance / blur).clamp(0.0, 1.0)
        };
        let ambient = shadow(0.0, SHADOW_AMBIENT_BLUR_CSS, SHADOW_AMBIENT_ALPHA);
        let key = shadow(
            SHADOW_KEY_OFFSET_Y_CSS,
            SHADOW_KEY_BLUR_CSS,
            SHADOW_KEY_ALPHA,
        );
        let value = (CANVAS_GRAY as f32 * (1.0 - ambient) * (1.0 - key)).round() as u8;
        (value, value, value, 255)
    }

    fn fill(&self, dst: &Pixbuf, x: i32, y: i32, w: i32, h: i32) {
        if w <= 0 || h <= 0 {
            return;
        }
        fill_rect(dst, x, y, w, h, CANVAS_COLOR);
        // The rest is a flat fill; compute shadow pixels only near the source.
        let x0 = x.max(self.x - SHADOW_EXTENT);
        let y0 = y.max(self.y - SHADOW_EXTENT);
        let x1 = (x + w).min(self.x + self.width + SHADOW_EXTENT);
        let y1 = (y + h).min(self.y + self.height + SHADOW_EXTENT);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let has_alpha = dst.has_alpha();
        let bpp = if has_alpha { 4 } else { 3 };
        let mut row = Vec::with_capacity((x1 - x0) as usize * bpp);
        for yy in y0..y1 {
            row.clear();
            for xx in x0..x1 {
                let (r, g, b, a) = self.color(xx, yy);
                row.extend_from_slice(&[r, g, b]);
                if has_alpha {
                    row.push(a);
                }
            }
            blit_row(dst, x0, yy, 1, &row);
        }
    }
}

/// Repaint only the margins of a newly transformed raster. Rotating/resampling
/// the old shadow would leave it pointing sideways or at a different scale
/// from the next canvas expansion. Call only on a fresh, unshared image.
fn repaint_background_margins(image: &Pixbuf, source_rect: crate::math::Rect) {
    let inset = CaptureInset::from_rect(source_rect, image);
    let background = CanvasBackground {
        x: inset.left,
        y: inset.top,
        width: image.width() - inset.left - inset.right,
        height: image.height() - inset.top - inset.bottom,
    };
    background.fill(image, 0, 0, image.width(), inset.top);
    background.fill(
        image,
        0,
        image.height() - inset.bottom,
        image.width(),
        inset.bottom,
    );
    background.fill(image, 0, inset.top, inset.left, background.height);
    background.fill(
        image,
        image.width() - inset.right,
        inset.top,
        inset.right,
        background.height,
    );
}

/// Dark gray fill behind the screenshot (replaces solid black). Matches
/// the surrounding toolbar chrome so the canvas reads as part of the
/// app surface, not a void.
const CANVAS_BG: femtovg::Color = femtovg::Color {
    r: CANVAS_GRAY as f32 / 255.0,
    g: CANVAS_GRAY as f32 / 255.0,
    b: CANVAS_GRAY as f32 / 255.0,
    a: 1.0,
};

#[derive(Default)]
pub struct FemtoVGArea {
    canvas: RefCell<Option<femtovg::Canvas<femtovg::renderer::OpenGl>>>,
    font: RefCell<Option<FontId>>,
    inner: RefCell<Option<FemtoVgAreaMut>>,
    request_render: RefCell<Option<Vec<Action>>>,
    sender: RefCell<Option<Sender<SketchBoardInput>>>,
    /// Last `scale_factor` we emitted to the parent so we can suppress
    /// redundant `ZoomDisplayChanged` notifications during steady-state
    /// frame rendering.
    last_emitted_scale: RefCell<f32>,
    /// Last `PanInfo` we emitted upstream. Stops us forwarding the same
    /// scrollbar-update payload on every `update_transformation` —
    /// without dedup, every spring-back / pinch / scroll tick fired a
    /// fresh PanChanged → sync_scrollbars cycle even when nothing had
    /// actually moved, which showed up as visible UI stutter.
    last_emitted_pan: RefCell<Option<crate::sketch_board::PanInfo>>,
    /// Active spring-back timer source. Started on each pan when the
    /// drag offset is past its hard limit, cleared once the offset
    /// has fully recovered — keeps the timer from running forever
    /// while there's no rubber-band stretch to recover.
    pub spring_back_timer: RefCell<Option<gtk::glib::SourceId>>,
    /// `GL_MAX_TEXTURE_SIZE` of the context the canvas was created on.
    /// 0 until `setup_canvas` has run; consumers fall back to a safe
    /// minimum. Textures above this limit "succeed" at the API level
    /// but are storage-less: sampling them returns black and attaching
    /// them to an FBO fails — femtovg then silently keeps the previous
    /// render target, which is how oversized scroll captures used to
    /// export as solid-white canvas-sized images.
    max_texture_size: std::cell::Cell<usize>,
}

/// One GPU tile of the background image. Long scroll captures exceed
/// `GL_MAX_TEXTURE_SIZE` (16384 on common desktop GPUs), so the
/// background is split into a grid of tiles no larger than the limit,
/// each drawn at its image-space rect.
struct BackgroundTile {
    id: femtovg::ImageId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Tightly-packed bytes for the `(x, y, w, h)` region of a raster.
///
/// femtovg pins `UNPACK_ROW_LENGTH` to the image width and ignores an
/// `ImgRef`'s stride, so every upload has to arrive packed at exactly
/// `w` pixels per row. When the region spans the full width of a raster
/// with no row padding it already is that, and borrows; otherwise its
/// rows are packed into a fresh buffer.
fn region_bytes(
    src: &[u8],
    raster: RasterLayout,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> std::borrow::Cow<'_, [u8]> {
    let RasterLayout {
        stride,
        bytes_per_pixel,
        width,
    } = raster;
    if stride == width * bytes_per_pixel && x == 0 && w == width {
        return std::borrow::Cow::Borrowed(&src[y * stride..(y + h) * stride]);
    }
    let row_length = w * bytes_per_pixel;
    let mut packed = Vec::with_capacity(row_length * h);
    for row in y..y + h {
        let offset = row * stride + x * bytes_per_pixel;
        packed.extend_from_slice(&src[offset..offset + row_length]);
    }
    std::borrow::Cow::Owned(packed)
}

/// How a raster's bytes are arranged, for [`region_bytes`].
#[derive(Clone, Copy)]
struct RasterLayout {
    /// Bytes per row, which a Pixbuf may pad beyond `width`.
    stride: usize,
    bytes_per_pixel: usize,
    width: usize,
}

impl RasterLayout {
    fn of(image: &Pixbuf) -> Self {
        Self {
            stride: image.rowstride() as usize,
            bytes_per_pixel: if image.has_alpha() { 4 } else { 3 },
            width: image.width() as usize,
        }
    }
}

/// Rects of the new raster that the old one doesn't cover, after a
/// resize that placed the old raster at `translation` inside a
/// `new_w` x `new_h` image.
///
/// Emitted as full-width bands above and below the old raster plus the
/// left/right remainder beside it, so the four never overlap — an
/// overlap would upload the same pixels twice and stack redundant
/// tiles. Shrinking sides contribute nothing; a pure shrink returns
/// none.
fn background_grow_strips(
    translation: Vec2D,
    old_w: f32,
    old_h: f32,
    new_w: f32,
    new_h: f32,
) -> Vec<(f32, f32, f32, f32)> {
    let (ox, oy) = (translation.x, translation.y);
    // The old raster clipped to the new one — bands are measured
    // against what is actually still visible, not where it nominally
    // sits, so a simultaneous grow on one axis and shrink on the other
    // can't emit a band that overlaps the copied region.
    let top = oy.clamp(0.0, new_h);
    let bottom = (oy + old_h).clamp(0.0, new_h);
    let left = ox.clamp(0.0, new_w);
    let right = (ox + old_w).clamp(0.0, new_w);

    [
        (0.0, 0.0, new_w, top),
        (0.0, bottom, new_w, new_h - bottom),
        (0.0, top, left, bottom - top),
        (right, top, new_w - right, bottom - top),
    ]
    .into_iter()
    .filter(|(_, _, w, h)| *w > 0.0 && *h > 0.0)
    .collect()
}

/// Split `total` into consecutive `(start, len)` ranges of at most
/// `limit` each. Ranges are non-empty; a zero `total` yields none.
fn tile_ranges(total: usize, limit: usize) -> Vec<(usize, usize)> {
    let limit = limit.max(1);
    let mut ranges = Vec::with_capacity(total.div_ceil(limit));
    let mut start = 0;
    while start < total {
        let len = limit.min(total - start);
        ranges.push((start, len));
        start += len;
    }
    ranges
}

pub struct FemtoVgAreaMut {
    background_image: Pixbuf,
    /// Backing allocation for `background_image` when that is a *view*
    /// into a larger raster — see `resize_raster`. `None` means the
    /// image owns its buffer and the next resize must allocate.
    background_alloc: Option<Pixbuf>,
    /// Origin of `background_image` inside `background_alloc`.
    background_origin: (i32, i32),
    background_tiles: Vec<BackgroundTile>,
    /// Image-space rects whose pixels are on the CPU but not yet on the
    /// GPU. Set by an incremental canvas grow, consumed by the next
    /// `render_background_image`. Empty means the tile set is current.
    pending_tile_strips: Vec<(f32, f32, f32, f32)>,
    /// Propagated from the GL context via `ensure_canvas`; see
    /// `FemtoVGArea::max_texture_size`. Defaults to a conservative
    /// 8192 until the real limit is known.
    max_texture_size: usize,
    /// Image-space rect of the original (pre-auto-extension)
    /// screenshot inside the current `background_image`. Initially
    /// `(0, 0, orig_w, orig_h)`. When the canvas auto-extends, the
    /// origin shifts (for left/top extensions) and the size stays
    /// fixed. `auto_resize_for_drawables` uses this rect as the
    /// "must keep visible" floor — it never crops away original
    /// screenshot pixels, even if the user deletes every drawable.
    original_rect: crate::math::Rect,
    transparent_background_id: Option<femtovg::ImageId>,
    active_tool: Rc<RefCell<dyn Tool>>,
    /// The pointer tool is consulted alongside the active tool so implicit
    /// selection (clicking a shape while a drawing tool is active) renders
    /// handles, glow, and live drag visuals.
    pointer_tool: Rc<RefCell<dyn Tool>>,
    crop_tool: Rc<RefCell<CropTool>>,
    scale_factor: f32,
    offset: Vec2D,
    drawables: Vec<Stacked>,
    undo_stack: Vec<UndoAction>,
    redo_stack: Vec<UndoAction>,
    next_drawable_id: u64,
    /// Per-kind monotonic counter for `Stacked::auto_label_index`.
    /// Incremented at every commit, never decremented — so a layer's
    /// ordinal stays stable across reorders (and across delete + redo
    /// chains, which carry the original index in `UndoAction::Remove`).
    next_label_index: std::collections::HashMap<&'static str, u32>,
    zoom_scale: f32,
    last_scale: f32,
    pointer_offset: Vec2D,
    last_offset: Vec2D,
    drag_offset: Vec2D,
    is_drag: bool,
    is_reset: bool,
    /// Set by `set_zoom_scale_at` to tell `update_transformation`
    /// to KEEP the freshly-computed `drag_offset` (which positions
    /// the image so the user's anchor point stays under the cursor
    /// after a zoom). Without this flag, the centering logic at
    /// line ~2003 would zero out drag_offset on the very tick the
    /// zoom takes effect, defeating the anchor.
    zoom_anchor_pending: bool,
    /// Device pixel ratio of the host display (1 on standard DPI, 2 on
    /// retina). Updated on `resize`. Used so per-frame UI elements
    /// (selection handles) can render at constant CSS-pixel size while
    /// still looking sharp on HiDPI screens.
    device_pixel_ratio: f32,
    /// Global darkness for the spotlight overlay (0.10–0.90, slider
    /// range). Sketch_board pushes the toolbar slider value here on
    /// every change; the renderer's spotlight pass reads it directly
    /// at render time so the overlay updates live without redrawing
    /// each spotlight Drawable.
    spotlight_darkness: f32,
    spotlight_magnification: f32,
    /// Scale + offset actually used by the most recent on-screen
    /// render. Equal to `scale_factor` / `offset` in the normal case,
    /// but switches to a "fit the committed crop into the canvas"
    /// transform whenever a committed crop is present. Coordinate
    /// conversions read these so a click on the zoomed crop lands at
    /// the right image-space position.
    effective_scale: f32,
    effective_offset: Vec2D,
    /// Canvas-pixel rect of the visible content (full image in the
    /// regular view; the cropped region in committed-crop mode).
    /// Captured during `render_framebuffer` so the drop-shadow path
    /// in `render` can draw a shadow around whichever rect is
    /// actually on screen without re-deriving it.
    display_rect_origin: Vec2D,
    display_rect_size: Vec2D,
    /// Canvas pixel dimensions captured at the last
    /// `update_transformation` call. Used by
    /// `set_pan_from_scrollbar` to translate a scrollbar adjustment
    /// value (which is expressed in canvas pixels) into the
    /// renderer's centered `drag_offset` representation without
    /// having to thread the canvas reference all the way down.
    last_canvas_size: Vec2D,
    /// User-applied zoom multiplier ON TOP of the committed-crop's
    /// fit-to-canvas scale. 1.0 = exactly fit, 2.0 = 2× the fit.
    /// Lives separately from `zoom_scale` because committed crop has
    /// its own base scale (the fit) and we want wheel-zoom inputs to
    /// scale that base, not the underlying image's full-resolution
    /// scale. Reset to 1.0 whenever the crop is dropped.
    crop_zoom: f32,
    /// Timestamp of the most recent pan input (wheel scroll or
    /// trackpad swipe). `update_transformation` only applies the
    /// spring-back lerp when this is older than `SPRING_BACK_IDLE_MS`
    /// — otherwise we'd fight the user's active gesture.
    last_pan_input: std::time::Instant,
    /// In-flight spring-back animation, if any. `Some((start_time,
    /// start_visible))` once the user has been idle long enough that
    /// we start easing the canvas back to the limit; cleared on
    /// `pan_by` and when the animation completes. `start_visible` is
    /// the **rendered** offset at release (after rubber-band), not
    /// the raw `drag_offset` — we animate that smoothly to the
    /// nearest limit and back-solve a `drag_offset` per frame via
    /// `inverse_rubber_band`. Animating the raw offset directly was
    /// where the "stuck-then-snap" recovery came from: the
    /// nonlinear rubber-band map ate most of the visible motion in
    /// the first half of the curve, then released it in the second.
    spring_back_anim: Option<(std::time::Instant, Vec2D)>,
}

#[glib::object_subclass]
impl ObjectSubclass for FemtoVGArea {
    const NAME: &'static str = "FemtoVGArea";
    type Type = super::FemtoVGArea;
    type ParentType = gtk::GLArea;
}

impl ObjectImpl for FemtoVGArea {
    fn constructed(&self) {
        self.parent_constructed();
        let area = self.obj();
        area.set_has_stencil_buffer(true);
        area.queue_render();
    }
}

impl WidgetImpl for FemtoVGArea {
    fn realize(&self) {
        self.parent_realize();
        if super::perf::spin() {
            // Free-running profiling mode: drive a render every frame
            // tick so steady-state canvas cost is measurable without a
            // human moving the mouse. A `queue_render` from inside
            // `render` would be swallowed as already-in-progress, so
            // the request has to come from the frame clock instead.
            self.obj().add_tick_callback(|area, _| {
                area.queue_render();
                glib::ControlFlow::Continue
            });
        }
    }

    fn unrealize(&self) {
        self.obj().make_current();
        if let Some(id) = self.spring_back_timer.borrow_mut().take() {
            id.remove();
        }
        self.canvas.borrow_mut().take();
        self.parent_unrealize();
    }
}

impl GLAreaImpl for FemtoVGArea {
    fn resize(&self, width: i32, height: i32) {
        self.ensure_canvas();

        let mut bc = self.canvas.borrow_mut();
        let canvas = bc.as_mut().unwrap(); // this unwrap is safe as long as we call "ensure_canvas" before

        let w = canvas.width();
        let h = canvas.height();

        let dpr = self.obj().scale_factor() as f32;
        canvas.set_size(
            if width == 0 { w } else { width as u32 },
            if height == 0 { h } else { height as u32 },
            dpr,
        );

        // update scale factor + pan; capture the snapshot we need
        // for the upstream notifications BEFORE releasing the inner
        // borrow so the emit paths don't have to re-acquire it.
        // The zoom indicator shows a *user-facing* zoom where 100% is
        // the image at its true 1× logical size. Internally that's
        // `effective_scale` (a render scale whose 100% is
        // `natural_scale()`, not 1.0), so divide it back out before
        // emitting — otherwise a fractional-scaling output would read
        // e.g. "187%" at the fit-to-window view.
        let (display_zoom, pan_info, min_canvas_h) = {
            let mut inner_ref = self.inner();
            let inner = inner_ref
                .as_mut()
                .expect("Did you call init before using FemtoVgArea?");
            inner.device_pixel_ratio = dpr;
            inner.update_transformation(canvas);
            // Keep the crop tool's cached image→canvas scale fresh so
            // its handle hit-testing stays screen-constant as the user
            // zooms.
            let eff_scale_for_crop = inner.effective_scale;
            let crop_tool = inner.crop_tool.clone();
            {
                let mut ct = crop_tool.borrow_mut();
                if ct.is_active_edit() {
                    ct.set_render_scale(eff_scale_for_crop);
                }
            }
            let image_w = inner.background_image.width() as f32;
            let image_h = inner.background_image.height() as f32;
            // A committed crop is a view-window — the user can only pan WITHIN
            // the crop, so the scrollable content is the crop region, not the
            // full image. Take the crop size when one is committed (else the
            // full image), scaled by `effective_scale` (the real on-screen
            // scale, which folds in crop_zoom). Using `scale_factor` × the full
            // image was the bug: `scale_factor` is the full-image auto-fit, so
            // after a crop / un-crop / grow sequence it left the scrollbars
            // thinking the whole original was pannable and showed spurious
            // bars. For the non-crop view `effective_scale == scale_factor`, so
            // that case is unchanged.
            let (content_w, content_h) = crop_tool
                .borrow()
                .get_committed_rect()
                .filter(|(_, s)| s.x > 0.0 && s.y > 0.0)
                .map(|(_, s)| (s.x, s.y))
                .unwrap_or((image_w, image_h));
            let pan_info = crate::sketch_board::PanInfo {
                drag_x: inner.drag_offset.x,
                drag_y: inner.drag_offset.y,
                image_w_scaled: content_w * inner.effective_scale,
                image_h_scaled: content_h * inner.effective_scale,
                canvas_w: canvas.width() as f32,
                canvas_h: canvas.height() as f32,
            };
            (
                inner.effective_scale / inner.natural_scale(),
                pan_info,
                inner.min_canvas_height_logical(),
            )
        };
        self.notify_zoom_display(display_zoom);
        self.notify_pan_display(pan_info);
        self.apply_vertical_resize_floor(min_canvas_h);
    }
    fn render(&self, _context: &gtk::gdk::GLContext) -> glib::Propagation {
        self.ensure_canvas();

        let mut bc = self.canvas.borrow_mut();
        let canvas = bc.as_mut().unwrap(); // this unwrap is safe as long as we call "ensure_canvas" before
        let font = self.font.borrow().unwrap(); // this unwrap is safe as long as we call "ensure_canvas" before
        let mut actions = self.request_render.borrow_mut();

        // if we got requested to render a frame
        if let Some(a) = actions.take() {
            // render image
            let image = match self
                .inner()
                .as_mut()
                .expect("Did you call init before using FemtoVgArea?")
                .render_native_resolution(canvas, font)
            {
                Ok(t) => t,
                Err(e) => {
                    println!("Error while rendering image: {e}");
                    return glib::Propagation::Stop;
                }
            };

            // send result
            self.sender
                .borrow()
                .as_ref()
                .expect("Did you call init before using FemtoVgArea?")
                .emit(SketchBoardInput::RenderResult(image, a));

            // reset request
            *actions = None;
        }
        if let Err(e) = self
            .inner()
            .as_mut()
            .expect("Did you call init before using FemtoVgArea?")
            .render_framebuffer(canvas, font)
        {
            println!("Error rendering to framebuffer: {e}");
        }
        glib::Propagation::Stop
    }
}
impl FemtoVGArea {
    /// Forward a `ZoomDisplayChanged` event to the parent component when
    /// the rendered scale factor changes. Idempotent: skips emission when
    /// the value matches what we sent last time.
    /// Pin the window's content (`outer_box`) to a minimum height so
    /// a vertical resize can't shrink the image past
    /// `MIN_AUTO_FIT_ZOOM`. The floor is `min_canvas_h` (the canvas
    /// height for that zoom) plus the *measured* chrome — the live
    /// `outer_box` height minus this canvas's height — so it stays
    /// correct no matter how tall the toolbars currently are (the top
    /// bar's height changes when it wraps). Setting the request on
    /// `outer_box` rather than the window keeps it clear of the
    /// launch-time `set_size_request` size-pinning, which targets the
    /// window itself.
    fn apply_vertical_resize_floor(&self, min_canvas_h: f32) {
        let canvas = self.obj();
        let mut node = canvas.parent();
        let outer = loop {
            match node {
                Some(w) if w.has_css_class("outer_box") => break Some(w),
                Some(w) => node = w.parent(),
                None => break None,
            }
        };
        let Some(outer) = outer else { return };
        let chrome = (outer.height() - canvas.height()).max(0);
        let mut floor = min_canvas_h.ceil() as i32 + chrome;
        // The floor exists to stop the user shrinking the image below
        // 10% zoom — it must never instead FORCE the window taller
        // than the screen (a 30k-pixel scroll capture's 10% is taller
        // than a 1728px display). When the clamp engages, the canvas
        // clips the image at the floored zoom and panning reaches the
        // remainder.
        if let Some(monitor_h) = self.monitor_logical_height() {
            floor = floor.min((monitor_h as f32 * 0.85) as i32);
        }
        if outer.height_request() != floor {
            outer.set_size_request(outer.width_request(), floor);
        }
    }

    /// Logical height of the monitor showing this widget, resolved
    /// with the same fallback chain as the window-sizing code: the
    /// surface's own monitor, else the focused Hyprland monitor
    /// (cached — this runs on every canvas resize), else any monitor
    /// GTK knows about.
    fn monitor_logical_height(&self) -> Option<i32> {
        let widget = self.obj();
        let display = WidgetExt::display(widget.as_ref());
        widget
            .native()
            .and_then(|native| native.surface())
            .and_then(|surface| display.monitor_at_surface(&surface))
            .map(|monitor| monitor.geometry().height())
            .or_else(|| {
                crate::display::hyprland_focused_logical_size_cached().map(|(_, height)| height)
            })
            .or_else(|| {
                display
                    .monitors()
                    .item(0)
                    .and_then(|obj| obj.downcast::<gtk::gdk::Monitor>().ok())
                    .map(|monitor| monitor.geometry().height())
            })
    }

    fn notify_zoom_display(&self, scale_factor: f32) {
        let mut last = self.last_emitted_scale.borrow_mut();
        if (*last - scale_factor).abs() > 0.0005 {
            *last = scale_factor;
            if let Some(sender) = self.sender.borrow().as_ref() {
                sender.emit(SketchBoardInput::ZoomDisplayChanged(scale_factor));
            }
        }
    }

    /// Forward a `PanDisplayChanged` event so the App's scrollbars
    /// can sync their visibility + values. Deduped against the last
    /// emitted value — `update_transformation` runs on every render
    /// tick (including animation timers), and forwarding identical
    /// PanInfo through SketchBoard → App → sync_scrollbars on each
    /// tick was producing measurable UI lag on every relayout.
    fn notify_pan_display(&self, info: crate::sketch_board::PanInfo) {
        {
            let mut last = self.last_emitted_pan.borrow_mut();
            if last.as_ref() == Some(&info) {
                return;
            }
            *last = Some(info);
        }
        if let Some(sender) = self.sender.borrow().as_ref() {
            sender.emit(SketchBoardInput::PanDisplayChanged(info));
        }
    }

    pub fn init(
        &self,
        sender: Sender<SketchBoardInput>,
        crop_tool: Rc<RefCell<CropTool>>,
        active_tool: Rc<RefCell<dyn Tool>>,
        pointer_tool: Rc<RefCell<dyn Tool>>,
        background_image: Pixbuf,
    ) {
        let original_rect = crate::math::Rect::new(
            Vec2D::zero(),
            Vec2D::new(
                background_image.width() as f32,
                background_image.height() as f32,
            ),
        );
        self.inner().replace(FemtoVgAreaMut {
            background_image,
            background_alloc: None,
            background_origin: (0, 0),
            background_tiles: Vec::new(),
            pending_tile_strips: Vec::new(),
            max_texture_size: 8192,
            original_rect,
            transparent_background_id: None,
            active_tool,
            pointer_tool,
            crop_tool,
            scale_factor: 1.0,
            offset: Vec2D::zero(),
            drawables: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            next_drawable_id: 0,
            next_label_index: std::collections::HashMap::new(),
            zoom_scale: 0.0,
            pointer_offset: Vec2D::zero(),
            last_offset: Vec2D::zero(),
            drag_offset: Vec2D::zero(),
            last_scale: 0.0,
            is_drag: false,
            is_reset: false,
            zoom_anchor_pending: false,
            device_pixel_ratio: 1.0,
            spotlight_darkness: 0.50,
            spotlight_magnification: 1.0,
            effective_scale: 1.0,
            effective_offset: Vec2D::zero(),
            display_rect_origin: Vec2D::zero(),
            display_rect_size: Vec2D::zero(),
            last_canvas_size: Vec2D::zero(),
            crop_zoom: 1.0,
            last_pan_input: std::time::Instant::now(),
            spring_back_anim: None,
        });
        self.sender.borrow_mut().replace(sender);
    }
    fn ensure_canvas(&self) {
        if self.canvas.borrow().is_none() {
            let c = self
                .setup_canvas()
                .expect("Cannot setup renderer and canvas");
            self.canvas.borrow_mut().replace(c);
        }

        // Propagate the context's real texture limit to the renderer
        // state so background tiling and export tiling divide against
        // the actual hardware bound instead of the 8192 default.
        let max_texture_size = self.max_texture_size.get();
        if max_texture_size >= 1024
            && let Some(inner) = self.inner().as_mut()
        {
            inner.max_texture_size = max_texture_size;
        }

        if self.font.borrow().is_none()
            && let Some(first) = font_stack().first()
        {
            self.font.borrow_mut().replace(*first);
        }
    }

    fn build_text_context(&self) -> Result<(femtovg::TextContext, Vec<FontId>)> {
        let text_context = femtovg::TextContext::default();
        let mut loaded_fonts = Vec::new();
        let mut loaded_paths = HashSet::<(PathBuf, u32)>::new();

        let app_config = APP_CONFIG.read();
        let fontconfig = Fontconfig::new();

        let mut load_font = |family: &str, style: Option<&str>| -> Result<FontId> {
            let font = fontconfig
                .as_ref()
                .and_then(|fc| fc.find(family, style))
                .ok_or_else(|| anyhow::anyhow!("Font family '{}' not found", family))?;

            let face_index = font.index.unwrap_or(0).max(0) as u32;

            if !loaded_paths.insert((font.path.clone(), face_index)) {
                return Err(anyhow::anyhow!("Font '{}' already loaded", family));
            }
            let data = std::fs::read(&font.path)
                .map_err(|e| anyhow::anyhow!("Failed to read font file: {}", e))?;

            text_context
                .add_shared_font_with_index(data, face_index)
                .map_err(|e| anyhow::anyhow!("Failed to load font: {}", e))
        };

        // Prefer the user-configured font ONLY when they've explicitly
        // set `font.family`. With no override we skip straight to the
        // bundled Inter Display SemiBold below — the previous code's
        // `unwrap_or("")` flow let fontconfig substitute whatever system
        // default it picked (often a generic sans-serif that looked
        // visually unrelated), defeating the point of bundling a
        // font.
        if let Some(family) = app_config.font().family() {
            match load_font(family, app_config.font().style()) {
                Ok(id) => {
                    loaded_fonts.push(id);
                }
                Err(e) => {
                    eprintln!("Primary font: {}", e);
                }
            }
        }

        if loaded_fonts.is_empty() {
            // Bundled Inter Display SemiBold — a clean sans-serif that
            // reads well at small annotation-label sizes. Ships in
            // `src/assets/`, license at `Inter-LICENSE.txt`.
            let fallback = text_context
                .add_font_mem(&resource!("src/assets/InterDisplay-SemiBold.ttf"))
                .expect("Cannot add font");
            loaded_fonts.push(fallback);
        }

        for family in app_config.font().fallback() {
            match load_font(family, None) {
                Ok(id) => {
                    loaded_fonts.push(id);
                }
                Err(e) => {
                    eprintln!("Fallback font: {}", e);
                }
            }
        }

        Ok((text_context, loaded_fonts))
    }

    fn setup_canvas(&self) -> Result<femtovg::Canvas<femtovg::renderer::OpenGl>> {
        let widget = self.obj();
        widget.attach_buffers();

        static LOAD_FN: fn(&str) -> *const std::ffi::c_void =
            |s| epoxy::get_proc_addr(s) as *const _;
        // SAFETY: Need to get the framebuffer id that gtk expects us to draw into, so
        // femtovg knows which framebuffer to bind. This is safe as long as we
        // call attach_buffers beforehand. Also unbind it here just in case,
        // since this can be called outside render.
        let (mut renderer, fbo) = unsafe {
            let renderer =
                renderer::OpenGl::new_from_function(LOAD_FN).expect("Cannot create renderer");
            let ctx = glow::Context::from_loader_function(LOAD_FN);
            let id = NonZeroU32::new(ctx.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) as u32)
                .expect("No GTK provided framebuffer binding");
            let max_texture_size = ctx.get_parameter_i32(glow::MAX_TEXTURE_SIZE);
            // GL guarantees at least 1024; anything smaller means the
            // query itself failed, so keep the conservative default.
            if max_texture_size >= 1024 {
                self.max_texture_size.set(max_texture_size as usize);
            }
            // `TENSAKU_MAX_TEXTURE=N` forces a smaller limit so the
            // multi-tile background path can be exercised without a
            // capture big enough to exceed the real one. Tiling
            // normally only engages on very long scroll captures, which
            // makes it easy for a seam between tiles to go unnoticed.
            if let Some(forced) = std::env::var("TENSAKU_MAX_TEXTURE")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v >= 64)
            {
                eprintln!("tensaku: forcing GL_MAX_TEXTURE_SIZE to {forced}");
                self.max_texture_size.set(forced);
            }
            ctx.bind_framebuffer(glow::FRAMEBUFFER, None);
            (renderer, glow::NativeFramebuffer(id))
        };
        renderer.set_screen_target(Some(fbo));

        let (text_context, loaded_fonts) = self.build_text_context()?;
        let canvas = Canvas::new_with_text_context(renderer, text_context)?;

        set_font_stack(loaded_fonts.clone());
        if let Some(first) = loaded_fonts.first() {
            self.font.borrow_mut().replace(*first);
        }

        Ok(canvas)
    }

    pub fn inner(&self) -> RefMut<'_, Option<FemtoVgAreaMut>> {
        self.inner.borrow_mut()
    }
    pub fn request_render(&self, actions: &[Action]) {
        self.request_render.borrow_mut().replace(actions.into());
        self.obj().queue_render();
    }
    pub fn set_parent_sender(&self, sender: Sender<SketchBoardInput>) {
        self.sender.borrow_mut().replace(sender);
    }
}

/// Auto-fit scale that fits `content` (device px) inside the padded
/// `inner` area, capped at `max_scale` — the render scale at which the
/// image displays at its true 1× logical size (see
/// `FemtoVgAreaMut::natural_scale`; it's 1.0 on integer-scaled
/// displays and >1 on fractional-scaling outputs). The *vertical* fit
/// term is floored at `MIN_AUTO_FIT_ZOOM`: shrinking the window's
/// height — including a tiling-WM resize that ignores our `outer_box`
/// min-size request — can't squeeze the image past that zoom; the
/// canvas clips it instead. The horizontal term is left unfloored,
/// matching the height-only `min_canvas_height_logical` /
/// size-request floor.
fn auto_fit_scale(
    inner_w: f32,
    inner_h: f32,
    content_w: f32,
    content_h: f32,
    max_scale: f32,
) -> f32 {
    let fit_h = (inner_h / content_h).max(MIN_AUTO_FIT_ZOOM);
    (inner_w / content_w).min(fit_h).min(max_scale)
}

/// Add one spotlight's punch-out path to `paths`, and — when it
/// magnifies — a second copy to `magnifiers` with the rect to sample
/// and the factor to enlarge it by.
fn collect_spotlight(
    drawable: &dyn Drawable,
    paths: &mut Vec<Path>,
    magnifiers: &mut Vec<(Path, crate::math::Rect)>,
    magnifying: bool,
) {
    let mut punch = Path::new();
    drawable.append_spotlight_path(&mut punch);
    if magnifying
        && let Some(rect) = drawable.bounds()
        // A shape still being dragged out has no area to sample yet.
        && rect.size.x >= 1.0
        && rect.size.y >= 1.0
    {
        let mut lens = Path::new();
        drawable.append_spotlight_path(&mut lens);
        magnifiers.push((lens, rect));
    }
    paths.push(punch);
}

impl FemtoVgAreaMut {
    pub fn commit(&mut self, drawable: Box<dyn Drawable>) -> DrawableId {
        let id = DrawableId(self.next_drawable_id);
        self.next_drawable_id += 1;
        // Assign + bump the per-kind ordinal. Indices start at 1 so the
        // first rectangle reads as "Rectangle 1" not "Rectangle 0".
        let kind = drawable.kind_label();
        let counter = self.next_label_index.entry(kind).or_insert(1);
        let label_index = *counter;
        *counter += 1;
        // Land at the top of this drawable's band rather than the top
        // of the stack, so a filled rectangle drawn over a label goes
        // behind it instead of swallowing it. Only the landing spot —
        // reordering afterwards moves it anywhere and stays put.
        let bands: Vec<crate::tools::StackBand> = self
            .drawables
            .iter()
            .map(|s| s.drawable.stack_band())
            .collect();
        let at = crate::tools::StackBand::insert_position(&bands, drawable.stack_band());
        self.drawables
            .insert(at, Stacked::new(id, drawable, label_index));
        self.undo_stack.push(UndoAction::Add(id));
        self.redo_stack.clear();
        id
    }

    /// After a drawable mutation, re-fit the canvas so it tightly
    /// contains `original_rect` (the un-extended screenshot) plus the
    /// union of all current drawable bounds. Grows the background
    /// Pixbuf (with a neutral background and screenshot shadow) when a
    /// drawable spills past the current image, and shrinks it back
    /// toward `original_rect` when no drawable still needs the
    /// previously-added strips. Translates all drawables EXCEPT those
    /// in `ids_to_exclude` by the resulting shift, and wraps the most
    /// recent undo entry with a `ResizeCanvas` action inside a `Batch`
    /// so one Ctrl+Z reverses both. The excluded ids are the
    /// drawables whose just-pushed Add/Modify/Remove carries
    /// pre-resize state (translating them would double-apply on
    /// redo). Returns the new `(width, height)` if a resize happened,
    /// else `None`.
    /// Returns `Some((applied_offset, new_w, new_h))` when a resize
    /// happened. `applied_offset` is the translation added to every
    /// drawable when top/left strips were prepended (zero for pure
    /// right/bottom growth); callers that track image-space rects
    /// outside the drawable list (e.g. a committed crop rect) must shift
    /// them by it to stay aligned.
    pub fn auto_resize_for_drawables(
        &mut self,
        ids_to_exclude: &[DrawableId],
    ) -> Option<(Vec2D, f32, f32)> {
        if self.undo_stack.is_empty() {
            return None;
        }
        // Tight rect we want the new image to cover, in CURRENT image
        // coordinates. Always includes the original screenshot rect
        // (we never crop into the user's actual screenshot pixels).
        let mut tight = self.original_rect;
        for s in &self.drawables {
            if let Some(b) = s.drawable.bounds() {
                tight = tight.union(b);
            }
        }
        let cur_w = self.background_image.width() as f32;
        let cur_h = self.background_image.height() as f32;
        let dx_min = tight.pos.x.floor() as i32;
        let dy_min = tight.pos.y.floor() as i32;
        let dx_max = (tight.pos.x + tight.size.x).ceil() as i32;
        let dy_max = (tight.pos.y + tight.size.y).ceil() as i32;
        if dx_min == 0 && dy_min == 0 && dx_max == cur_w as i32 && dy_max == cur_h as i32 {
            return None;
        }
        let new_w = dx_max - dx_min;
        let new_h = dy_max - dy_min;
        if new_w <= 0 || new_h <= 0 {
            return None;
        }
        let prev_image = self.background_image.clone();
        // Profiled: this reallocates and edge-fills the whole
        // background raster, and its cost scales with the capture's
        // pixel count (~30 ms at 6144x3456). It runs from
        // `auto_resize_canvas`, so any drawable whose bounds reach the
        // image edge pays it once per commit or end-of-drag — and once
        // per EVENT for a keyboard nudge, which repeats faster than the
        // frame clock.
        let resized = super::perf::timed("canvas-grow", || {
            self.resize_raster(dx_min, dy_min, new_w, new_h)
        })?;
        let translation = Vec2D::new(-dx_min as f32, -dy_min as f32);
        self.original_rect.pos += translation;
        let exclude: HashSet<DrawableId> = ids_to_exclude.iter().copied().collect();
        let mut translated_ids: Vec<DrawableId> = Vec::new();
        for s in &mut self.drawables {
            s.drawable.translate(translation);
            if !exclude.contains(&s.id) {
                translated_ids.push(s.id);
            }
        }
        let old_w = cur_w;
        let old_h = cur_h;
        self.background_image = resized;
        self.reuse_background_tiles(translation, old_w, old_h, new_w as f32, new_h as f32);

        let resize = UndoAction::ResizeCanvas {
            prev_image,
            applied_offset: translation,
            translated_ids,
        };
        let prior = self
            .undo_stack
            .pop()
            .expect("auto_resize called with empty undo stack");
        self.undo_stack.push(UndoAction::Batch(vec![resize, prior]));
        Some((translation, new_w as f32, new_h as f32))
    }

    /// Replace the drawable with `id` in-place. Records a Modify undo action.
    /// Returns true if the id was found.
    pub fn modify(&mut self, id: DrawableId, new: Box<dyn Drawable>) -> bool {
        let Some(pos) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        let prev = std::mem::replace(&mut self.drawables[pos].drawable, new);
        self.undo_stack.push(UndoAction::Modify { id, prev });
        self.redo_stack.clear();
        true
    }

    /// Remove the drawable with `id` from the stack. Records a Remove undo
    /// action so the deletion can be undone.
    pub fn delete(&mut self, id: DrawableId) -> bool {
        let Some(pos) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        let stacked = self.drawables.remove(pos);
        self.undo_stack.push(UndoAction::Remove {
            id: stacked.id,
            idx: pos,
            drawable: stacked.drawable,
            visible: stacked.visible,
            locked: stacked.locked,
            custom_name: stacked.custom_name,
            auto_label_index: stacked.auto_label_index,
        });
        self.redo_stack.clear();
        true
    }

    /// Replace the drawable with `id` in-place, folding the change into
    /// the top of the undo stack when that top is already a `Modify`
    /// for the same id. The "first" prev (i.e. the state before the
    /// burst started) is preserved, so a single Ctrl+Z reverses the
    /// whole burst. Falls back to `modify` when the top doesn't match
    /// — e.g. an unrelated action slipped in between.
    pub fn modify_coalesce(&mut self, id: DrawableId, new: Box<dyn Drawable>) -> bool {
        let Some(pos) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        let top_matches = matches!(
            self.undo_stack.last(),
            Some(UndoAction::Modify { id: top_id, .. }) if *top_id == id
        );
        if top_matches {
            // Keep the existing Modify's `prev` (the burst's original
            // state) and just swap the live drawable forward.
            self.drawables[pos].drawable = new;
            true
        } else {
            self.modify(id, new)
        }
    }

    /// Multi-select counterpart of `modify_coalesce`. Coalesces only
    /// when the top undo entry is a `Batch` whose contained `Modify`
    /// ids match the requested update set exactly.
    pub fn modify_many_coalesce(&mut self, updates: Vec<(DrawableId, Box<dyn Drawable>)>) -> bool {
        let top_matches = if let Some(UndoAction::Batch(actions)) = self.undo_stack.last() {
            let top_ids: Vec<DrawableId> = actions
                .iter()
                .filter_map(|a| {
                    if let UndoAction::Modify { id, .. } = a {
                        Some(*id)
                    } else {
                        None
                    }
                })
                .collect();
            top_ids.len() == actions.len()
                && top_ids.len() == updates.len()
                && updates.iter().all(|(id, _)| top_ids.contains(id))
        } else {
            false
        };
        if !top_matches {
            return self.modify_many(updates);
        }
        for (id, new) in updates {
            if let Some(pos) = self.drawables.iter().position(|s| s.id == id) {
                self.drawables[pos].drawable = new;
            }
        }
        true
    }

    /// Move the drawable with `id` to the top of the stack. Records a
    /// `Reorder` undo entry; if the previous undo entry is already a
    /// `Reorder` for the same id, the older entry's `prev_order` is reused
    /// and the new entry replaces it — so a chain of consecutive raises of
    /// one shape unwinds in a single Ctrl+Z.
    ///
    /// Returns true if anything moved. No-ops (already topmost, missing id)
    /// don't touch undo state.
    pub fn reorder_to_top_coalesce(&mut self, id: DrawableId) -> bool {
        let Some(pos) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        if pos + 1 == self.drawables.len() {
            return false;
        }
        let mut snapshot: Vec<DrawableId> = self.drawables.iter().map(|s| s.id).collect();
        let stacked = self.drawables.remove(pos);
        self.drawables.push(stacked);

        let coalesce_with_prior = matches!(
            self.undo_stack.last(),
            Some(UndoAction::Reorder { last_raised: Some(prev_id), .. }) if *prev_id == id
        );
        if coalesce_with_prior
            && let Some(UndoAction::Reorder { prev_order, .. }) = self.undo_stack.pop()
        {
            snapshot = prev_order;
        }
        self.undo_stack.push(UndoAction::Reorder {
            prev_order: snapshot,
            last_raised: Some(id),
        });
        self.redo_stack.clear();
        true
    }

    /// Replace many drawables atomically (single Batch undo).
    pub fn modify_many(&mut self, updates: Vec<(DrawableId, Box<dyn Drawable>)>) -> bool {
        let mut actions = Vec::new();
        for (id, new) in updates {
            if let Some(pos) = self.drawables.iter().position(|s| s.id == id) {
                let prev = std::mem::replace(&mut self.drawables[pos].drawable, new);
                actions.push(UndoAction::Modify { id, prev });
            }
        }
        if actions.is_empty() {
            return false;
        }
        self.undo_stack.push(UndoAction::Batch(actions));
        self.redo_stack.clear();
        true
    }

    /// Remove a set of drawables atomically. Records a single Batch undo
    /// action so one Ctrl+Z brings them all back.
    pub fn delete_many(&mut self, ids: &[DrawableId]) -> bool {
        let mut actions = Vec::new();
        // Sort by position descending so removing earlier ids doesn't shift
        // later ones.
        let mut positions: Vec<(usize, DrawableId)> = ids
            .iter()
            .filter_map(|&id| {
                self.drawables
                    .iter()
                    .position(|s| s.id == id)
                    .map(|pos| (pos, id))
            })
            .collect();
        positions.sort_by_key(|p| std::cmp::Reverse(p.0));
        for (pos, id) in positions {
            let stacked = self.drawables.remove(pos);
            actions.push(UndoAction::Remove {
                id,
                idx: pos,
                drawable: stacked.drawable,
                visible: stacked.visible,
                locked: stacked.locked,
                custom_name: stacked.custom_name,
                auto_label_index: stacked.auto_label_index,
            });
        }
        if actions.is_empty() {
            return false;
        }
        // Apply order matters for the undo (Insert): the original order was
        // back-to-front, so reverse the per-removal actions to insert in the
        // right order on undo.
        actions.reverse();
        self.undo_stack.push(UndoAction::Batch(actions));
        self.redo_stack.clear();
        true
    }

    /// Drawable ids whose AABB bounds overlap `rect` (image coords). Used
    /// for marquee / drag-rect selection.
    pub fn drawables_in_rect(&self, rect: crate::math::Rect) -> Vec<DrawableId> {
        self.drawables
            .iter()
            .filter(|s| s.visible && !s.locked)
            .filter(|s| {
                s.drawable
                    .bounds()
                    .map(|b| b.intersects(rect))
                    .unwrap_or(false)
            })
            .map(|s| s.id)
            .collect()
    }

    /// All drawable ids in stacking order (back-to-front).
    pub fn all_drawable_ids(&self) -> Vec<DrawableId> {
        self.drawables.iter().map(|s| s.id).collect()
    }

    /// Per-instance UI state for a drawable. `None` if `id` isn't in the
    /// stack. Both fields default to (visible=true, locked=false) at
    /// commit time and are persisted across undo/redo via `Remove` and
    /// `SetLayerFlags` action variants.
    pub fn drawable_flags(&self, id: DrawableId) -> Option<(bool, bool)> {
        self.drawables
            .iter()
            .find(|s| s.id == id)
            .map(|s| (s.visible, s.locked))
    }

    pub fn drawable_custom_name(&self, id: DrawableId) -> Option<String> {
        self.drawables
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.custom_name.clone())
    }

    /// Auto-label ordinal assigned at commit. Stable across reorders so
    /// the layer panel can show "Rectangle 3" regardless of where the
    /// row currently sits in the panel. `None` if `id` isn't in the
    /// stack.
    pub fn drawable_auto_label_index(&self, id: DrawableId) -> Option<u32> {
        self.drawables
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.auto_label_index)
    }

    /// Set or clear the custom panel name for `id`. Records a `Rename`
    /// undo entry; no-op when the new value matches the current one.
    pub fn set_drawable_custom_name(&mut self, id: DrawableId, name: Option<String>) -> bool {
        let Some(pos) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        if self.drawables[pos].custom_name == name {
            return false;
        }
        let prev = self.drawables[pos].custom_name.take();
        self.drawables[pos].custom_name = name;
        self.undo_stack.push(UndoAction::Rename { id, prev });
        self.redo_stack.clear();
        true
    }

    /// Set the visible+locked flags for `id`, recording a `SetLayerFlags`
    /// undo entry when anything actually changes. Returns true on apply.
    pub fn set_drawable_flags(&mut self, id: DrawableId, visible: bool, locked: bool) -> bool {
        let Some(pos) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        let prev_visible = self.drawables[pos].visible;
        let prev_locked = self.drawables[pos].locked;
        if prev_visible == visible && prev_locked == locked {
            return false;
        }
        self.drawables[pos].visible = visible;
        self.drawables[pos].locked = locked;
        self.undo_stack.push(UndoAction::SetLayerFlags {
            id,
            prev_visible,
            prev_locked,
        });
        self.redo_stack.clear();
        true
    }

    /// Move `id` one position toward the top of the stack (forward in the
    /// Vec). Records a non-coalescing `Reorder` undo entry. Returns true
    /// on apply; false if `id` is missing or already at the top.
    pub fn move_drawable_up(&mut self, id: DrawableId) -> bool {
        let Some(pos) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        if pos + 1 == self.drawables.len() {
            return false;
        }
        let snapshot: Vec<DrawableId> = self.drawables.iter().map(|s| s.id).collect();
        self.drawables.swap(pos, pos + 1);
        self.undo_stack.push(UndoAction::Reorder {
            prev_order: snapshot,
            last_raised: None,
        });
        self.redo_stack.clear();
        true
    }

    /// Move `id` one position toward the bottom of the stack.
    pub fn move_drawable_down(&mut self, id: DrawableId) -> bool {
        let Some(pos) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        if pos == 0 {
            return false;
        }
        let snapshot: Vec<DrawableId> = self.drawables.iter().map(|s| s.id).collect();
        self.drawables.swap(pos, pos - 1);
        self.undo_stack.push(UndoAction::Reorder {
            prev_order: snapshot,
            last_raised: None,
        });
        self.redo_stack.clear();
        true
    }

    /// Send `id` all the way to the bottom of the stack.
    pub fn move_drawable_to_bottom(&mut self, id: DrawableId) -> bool {
        let Some(pos) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        if pos == 0 {
            return false;
        }
        let snapshot: Vec<DrawableId> = self.drawables.iter().map(|s| s.id).collect();
        let stacked = self.drawables.remove(pos);
        self.drawables.insert(0, stacked);
        self.undo_stack.push(UndoAction::Reorder {
            prev_order: snapshot,
            last_raised: None,
        });
        self.redo_stack.clear();
        true
    }

    /// Bring `id` all the way to the top of the stack. Non-coalescing
    /// counterpart of `reorder_to_top_coalesce` — used by the explicit
    /// "Front" button so a deliberate button press never collapses into
    /// a prior auto-raise of the same id.
    pub fn move_drawable_to_top(&mut self, id: DrawableId) -> bool {
        let Some(pos) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        if pos + 1 == self.drawables.len() {
            return false;
        }
        let snapshot: Vec<DrawableId> = self.drawables.iter().map(|s| s.id).collect();
        let stacked = self.drawables.remove(pos);
        self.drawables.push(stacked);
        self.undo_stack.push(UndoAction::Reorder {
            prev_order: snapshot,
            last_raised: None,
        });
        self.redo_stack.clear();
        true
    }

    /// Replace the stack order with `new_order` if it's a permutation of
    /// the current ids. Used by drag-to-reorder. Records a single
    /// `Reorder` undo entry.
    pub fn reorder_to(&mut self, new_order: Vec<DrawableId>) -> bool {
        if new_order.len() != self.drawables.len() {
            return false;
        }
        let cur: std::collections::HashSet<DrawableId> =
            self.drawables.iter().map(|s| s.id).collect();
        if !new_order.iter().all(|id| cur.contains(id)) {
            return false;
        }
        let snapshot: Vec<DrawableId> = self.drawables.iter().map(|s| s.id).collect();
        if snapshot == new_order {
            return false;
        }
        let mut by_id: std::collections::HashMap<DrawableId, Stacked> =
            self.drawables.drain(..).map(|s| (s.id, s)).collect();
        for id in &new_order {
            if let Some(s) = by_id.remove(id) {
                self.drawables.push(s);
            }
        }
        self.undo_stack.push(UndoAction::Reorder {
            prev_order: snapshot,
            last_raised: None,
        });
        self.redo_stack.clear();
        true
    }

    /// True if some other *visible* drawable above `id` in the stack has
    /// bounds that intersect `id`'s bounds. Hidden drawables are skipped
    /// (nothing to see); locked drawables still count (visually present).
    pub fn has_visible_overlapper_above(&self, id: DrawableId) -> bool {
        let Some(my_idx) = self.drawables.iter().position(|s| s.id == id) else {
            return false;
        };
        let Some(my_bounds) = self.drawables[my_idx].drawable.bounds() else {
            return false;
        };
        self.drawables.iter().skip(my_idx + 1).any(|s| {
            if !s.visible {
                return false;
            }
            s.drawable
                .bounds()
                .map(|b| b.intersects(my_bounds))
                .unwrap_or(false)
        })
    }

    /// Returns `(did_something, canvas_transform)`. The second element is
    /// `Some((transform, w, h))` when the reversed action was a whole-
    /// canvas op — the caller applies the same transform to the crop rect
    /// (which isn't part of undo history) so it tracks the image.
    pub fn undo(&mut self) -> (bool, Option<(CanvasTransform, f32, f32)>) {
        let Some(action) = self.undo_stack.pop() else {
            return (false, None);
        };
        let applied = match &action {
            UndoAction::CanvasOp {
                transform, w, h, ..
            } => Some((*transform, *w, *h)),
            _ => None,
        };
        let inverse = self.apply_inverse(action);
        self.redo_stack.push(inverse);
        (true, applied)
    }

    pub fn redo(&mut self) -> (bool, Option<(CanvasTransform, f32, f32)>) {
        let Some(action) = self.redo_stack.pop() else {
            return (false, None);
        };
        let applied = match &action {
            UndoAction::CanvasOp {
                transform, w, h, ..
            } => Some((*transform, *w, *h)),
            _ => None,
        };
        let inverse = self.apply_inverse(action);
        self.undo_stack.push(inverse);
        (true, applied)
    }

    /// Apply the inverse of `action`, returning the action that should be pushed
    /// on the opposite stack. Shared between undo() and redo().
    fn apply_inverse(&mut self, action: UndoAction) -> UndoAction {
        match action {
            UndoAction::Add(id) => {
                let pos = self
                    .drawables
                    .iter()
                    .position(|s| s.id == id)
                    .expect("Add references missing drawable");
                let mut stacked = self.drawables.remove(pos);
                stacked.drawable.handle_undo();
                UndoAction::Remove {
                    id,
                    idx: pos,
                    drawable: stacked.drawable,
                    visible: stacked.visible,
                    locked: stacked.locked,
                    custom_name: stacked.custom_name,
                    auto_label_index: stacked.auto_label_index,
                }
            }
            UndoAction::Remove {
                id,
                idx,
                mut drawable,
                visible,
                locked,
                custom_name,
                auto_label_index,
            } => {
                drawable.handle_redo();
                let insert_at = idx.min(self.drawables.len());
                self.drawables.insert(
                    insert_at,
                    Stacked {
                        id,
                        drawable,
                        visible,
                        locked,
                        custom_name,
                        auto_label_index,
                    },
                );
                UndoAction::Add(id)
            }
            UndoAction::Modify { id, prev } => {
                let pos = self
                    .drawables
                    .iter()
                    .position(|s| s.id == id)
                    .expect("Modify references missing drawable");
                let cur = std::mem::replace(&mut self.drawables[pos].drawable, prev);
                UndoAction::Modify { id, prev: cur }
            }
            UndoAction::Batch(actions) => {
                // Reverse order while inverting so insert/remove indices stay
                // consistent. The result is also a Batch; pushing it onto the
                // opposite stack lets one Ctrl+Z/Y restore the whole group.
                let mut inverses: Vec<UndoAction> = actions
                    .into_iter()
                    .rev()
                    .map(|a| self.apply_inverse(a))
                    .collect();
                inverses.reverse();
                UndoAction::Batch(inverses)
            }
            UndoAction::SetLayerFlags {
                id,
                prev_visible,
                prev_locked,
            } => {
                let pos = self
                    .drawables
                    .iter()
                    .position(|s| s.id == id)
                    .expect("SetLayerFlags references missing drawable");
                let cur_visible = self.drawables[pos].visible;
                let cur_locked = self.drawables[pos].locked;
                self.drawables[pos].visible = prev_visible;
                self.drawables[pos].locked = prev_locked;
                UndoAction::SetLayerFlags {
                    id,
                    prev_visible: cur_visible,
                    prev_locked: cur_locked,
                }
            }
            UndoAction::Rename { id, prev } => {
                let pos = self
                    .drawables
                    .iter()
                    .position(|s| s.id == id)
                    .expect("Rename references missing drawable");
                let cur = self.drawables[pos].custom_name.take();
                self.drawables[pos].custom_name = prev;
                UndoAction::Rename { id, prev: cur }
            }
            UndoAction::Reorder {
                prev_order,
                last_raised,
            } => {
                let cur_order: Vec<DrawableId> = self.drawables.iter().map(|s| s.id).collect();
                // Rebuild stack in `prev_order`. Move-by-take with a HashMap so
                // each Stacked transfers exactly once and drawables not named
                // in `prev_order` (shouldn't happen, but defensive) end up at
                // the top in their original relative order.
                let mut by_id: std::collections::HashMap<DrawableId, Stacked> =
                    self.drawables.drain(..).map(|s| (s.id, s)).collect();
                for id in &prev_order {
                    if let Some(s) = by_id.remove(id) {
                        self.drawables.push(s);
                    }
                }
                // Anything that survived isn't in prev_order — push at top.
                for (_, s) in by_id.drain() {
                    self.drawables.push(s);
                }
                // Preserve `last_raised` on the inverse so a later live raise
                // can still coalesce against this entry if it ends up back on
                // the undo stack after a redo.
                UndoAction::Reorder {
                    prev_order: cur_order,
                    last_raised,
                }
            }
            UndoAction::ResizeCanvas {
                prev_image,
                applied_offset,
                translated_ids,
            } => {
                let cur_image = self.adopt_background_image(prev_image);
                let translated_set: HashSet<DrawableId> = translated_ids.iter().copied().collect();
                for s in &mut self.drawables {
                    if translated_set.contains(&s.id) {
                        s.drawable.translate(-applied_offset);
                    }
                }
                self.original_rect.pos -= applied_offset;
                UndoAction::ResizeCanvas {
                    prev_image: cur_image,
                    applied_offset: -applied_offset,
                    translated_ids,
                }
            }
            UndoAction::CanvasOp {
                image,
                original_rect,
                transform,
                w,
                h,
            } => {
                // Swap the raster + protected rect back, and remap every
                // live drawable by `transform` (the inverse of the op we
                // recorded). The returned action redoes it: swap the
                // post-op image back, apply the inverse-of-this (the
                // forward op) at the resulting (pre-`transform`) dims.
                let cur_image = self.adopt_background_image(image);
                let cur_rect = std::mem::replace(&mut self.original_rect, original_rect);
                for s in &mut self.drawables {
                    s.drawable.apply_canvas_transform(transform, w, h);
                }
                let (rw, rh) = transform.new_size(w, h);
                UndoAction::CanvasOp {
                    image: cur_image,
                    original_rect: cur_rect,
                    transform: transform.inverse(),
                    w: rw,
                    h: rh,
                }
            }
        }
    }

    pub fn reset(&mut self) -> bool {
        let mut any = false;
        while !self.drawables.is_empty() && self.undo().0 {
            any = true;
        }
        any
    }

    /// Topmost drawable hit by `point` (image coords). Iterates back-to-front so
    /// the most recently drawn (visually on top) wins. Drawables hidden via
    /// either tool's `dragging_drawable_id` are skipped — they're effectively
    /// invisible (working copy renders on top), so they shouldn't be hit-test
    /// targets either. `try_borrow` falls back to no filter when a tool is
    /// already mutably borrowed (e.g. when PointerTool itself is calling
    /// hit_test from inside its own handler), which is the safe direction:
    /// worst case we hit-test more drawables than strictly necessary.
    pub fn hit_test(&self, point: Vec2D, tolerance: f32) -> Option<DrawableId> {
        let dragging_active = self
            .active_tool
            .try_borrow()
            .ok()
            .and_then(|t| t.dragging_drawable_id());
        let dragging_pointer = self
            .pointer_tool
            .try_borrow()
            .ok()
            .and_then(|t| t.dragging_drawable_id());
        for s in self.drawables.iter().rev() {
            if dragging_active == Some(s.id) || dragging_pointer == Some(s.id) {
                continue;
            }
            // Hidden drawables can't be hit (they're invisible) and locked
            // drawables can't be hit (they're a fixed background that the
            // pointer should pass through to whatever's beneath).
            if !s.visible || s.locked {
                continue;
            }
            if s.drawable.hit_test(point, tolerance) {
                return Some(s.id);
            }
        }
        None
    }

    /// Borrow the live drawable for a given id, if it exists in the stack.
    pub fn drawable(&self, id: DrawableId) -> Option<&dyn Drawable> {
        self.drawables
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.drawable.as_ref())
    }

    pub fn set_active_tool(&mut self, active_tool: Rc<RefCell<dyn Tool>>) {
        self.active_tool = active_tool;
    }

    pub fn render_native_resolution(
        &mut self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        font: FontId,
    ) -> anyhow::Result<ImgVec<RGBA8>> {
        // Publish DPR for text/UI sizing during the offscreen render
        // (used for save/clipboard export).
        super::set_current_device_pixel_ratio(self.device_pixel_ratio);
        super::set_current_render_scale(self.effective_scale_or_fallback());
        let bounds = (
            Vec2D::zero(),
            Vec2D::new(
                self.background_image.width() as f32,
                self.background_image.height() as f32,
            ),
        );
        // get offset and size of the area in question
        let (pos, size) = self
            .crop_tool
            .borrow()
            .get_crop()
            .map(|c| c.get_rectangle())
            .map(|rect| rect_ensure_in_bounds(rect, bounds))
            .map(rect_round)
            .filter(|(_, size)| !size.is_zero())
            .unwrap_or(bounds);

        // Render in tiles no larger than the GL texture limit. A single
        // full-size render target silently breaks above the limit: the
        // texture allocates without storage, its framebuffer is
        // incomplete, femtovg keeps the previous (screen) target bound,
        // and `screenshot()` then returns its white-initialized,
        // canvas-sized buffer — which is exactly how oversized scroll
        // captures used to export as solid white. Long captures easily
        // exceed the limit, so tile unconditionally.
        let out_w = size.x as usize;
        let out_h = size.y as usize;
        let limit = self.max_texture_size.max(1024);
        let mut result = ImgVec::new(vec![RGBA8::new(0, 0, 0, 0); out_w * out_h], out_w, out_h);

        for (tile_y, tile_h) in tile_ranges(out_h, limit) {
            for (tile_x, tile_w) in tile_ranges(out_w, limit) {
                let image_id = canvas.create_image_empty(
                    tile_w,
                    tile_h,
                    PixelFormat::Rgba8,
                    ImageFlags::empty(),
                )?;
                // Execute anything pending against the old target before
                // switching, so this tile's readback can't observe it.
                canvas.flush();
                canvas.set_render_target(RenderTarget::Image(image_id));

                // Map image coordinates so this tile's region lands at
                // the target's origin.
                let mut transform = Transform2D::identity();
                transform.translate(-(pos.x + tile_x as f32), -(pos.y + tile_y as f32));
                canvas.reset_transform();
                canvas.set_transform(&transform);
                // `render()`'s own clear uses the on-screen canvas
                // dimensions, which don't match the tile — clear the
                // full tile here instead and pass `clear_canvas: false`.
                canvas.clear_rect(
                    0,
                    0,
                    tile_w as u32,
                    tile_h as u32,
                    femtovg::Color::rgbaf(0.0, 0.0, 0.0, 0.0),
                );

                self.render(
                    canvas,
                    font,
                    false,
                    femtovg::Color::rgbaf(0.0, 0.0, 0.0, 0.0),
                    false,
                    false,
                    RenderTarget::Image(image_id),
                    transform,
                    None,
                )?;

                let tile_pixels = canvas.screenshot()?;
                let tile_ok = tile_pixels.width() == tile_w && tile_pixels.height() == tile_h;
                if !tile_ok {
                    let got_w = tile_pixels.width();
                    let got_h = tile_pixels.height();
                    canvas.set_render_target(RenderTarget::Screen);
                    canvas.delete_image(image_id);
                    anyhow::bail!(
                        "export tile readback returned {got_w}x{got_h}, expected \
                         {tile_w}x{tile_h} — refusing to produce a corrupt export"
                    );
                }

                let src_stride = tile_pixels.stride();
                let src = tile_pixels.buf();
                let dst = result.buf_mut();
                for row in 0..tile_h {
                    let src_start = row * src_stride;
                    let dst_start = (tile_y + row) * out_w + tile_x;
                    dst[dst_start..dst_start + tile_w]
                        .copy_from_slice(&src[src_start..src_start + tile_w]);
                }

                canvas.set_render_target(RenderTarget::Screen);
                canvas.delete_image(image_id);
            }
        }

        Ok(result)
    }

    pub fn render_framebuffer(
        &mut self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        font: FontId,
    ) -> Result<()> {
        super::perf::begin_frame();
        // Reclaim textures released since the last frame. Deferred to
        // here because the `Drawable` mutators that drop cached images
        // have no canvas, and because the previous frame's draw
        // commands have flushed by now.
        super::gl::drain_deleted_images(canvas);
        canvas.set_render_target(RenderTarget::Screen);
        // Publish current DPR so drawables can size CSS-pixel UI
        // (text editing handles, outlines) inside `Drawable::draw`
        // without us having to thread it through every impl.
        super::set_current_device_pixel_ratio(self.device_pixel_ratio);
        super::set_current_render_scale(self.effective_scale_or_fallback());

        // Choose between the regular pan/zoom transform and a
        // committed-crop fit transform. The crop fit centers and
        // scales the cropped region into the canvas; combined with
        // a scissor at the same canvas-space rect, anything outside
        // the crop renders as the canvas's clear color (black) so
        // the user sees only the cropped image.
        let canvas_w = canvas.width() as f32;
        let canvas_h = canvas.height() as f32;
        let (transform, scissor, eff_scale, eff_offset) = if let Some((crop_pos, crop_size)) =
            self.crop_tool.borrow().get_committed_rect()
            && crop_size.x > 0.0
            && crop_size.y > 0.0
        {
            // Defensive clamp: a committed crop rect can outlive the
            // raster that backed its extended region — e.g. an auto-grow
            // edge-extended the image, then an undo retracted the raster
            // (a `ResizeCanvas` undo step) but the crop rect (NOT an undo
            // step) stayed grown. Clamp the crop to the live image so the
            // scissor/fit/shadow never frame pixels that no longer exist
            // (those rendered as a black, drop-shadowed strip).
            let img_w = self.background_image.width() as f32;
            let img_h = self.background_image.height() as f32;
            let cx0 = crop_pos.x.clamp(0.0, img_w);
            let cy0 = crop_pos.y.clamp(0.0, img_h);
            let cx1 = (crop_pos.x + crop_size.x).clamp(0.0, img_w);
            let cy1 = (crop_pos.y + crop_size.y).clamp(0.0, img_h);
            let crop_pos = Vec2D::new(cx0, cy0);
            let crop_size = Vec2D::new((cx1 - cx0).max(0.0), (cy1 - cy0).max(0.0));
            // Render the committed crop at 1:1 when it fits in the
            // canvas with padding, with reduced padding when it just
            // fits the canvas, and scaled down only when it can't fit
            // at all. Mirror image of `update_transformation`'s
            // non-crop branch — same "100 % first, shrink padding,
            // scale only as last resort" cascade. Main.rs resizes the
            // window to fit (cropped + padding) on commit so the
            // canvas usually has enough room for the 1:1 path.
            // `crop_zoom` still multiplies on top so Ctrl+scroll
            // zooms further once cropped.
            let pad = CANVAS_PADDING_CSS * self.device_pixel_ratio.max(0.0001);
            let inner_w = (canvas_w - 2.0 * pad).max(canvas_w * 0.5).max(1.0);
            let inner_h = (canvas_h - 2.0 * pad).max(canvas_h * 0.5).max(1.0);
            let base_scale = auto_fit_scale(
                inner_w,
                inner_h,
                crop_size.x,
                crop_size.y,
                self.natural_scale(),
            );
            let scale = base_scale * self.crop_zoom;
            let crop_canvas_w = crop_size.x * scale;
            let crop_canvas_h = crop_size.y * scale;
            // pad_* can be negative when crop_zoom × base_scale > 1
            // (zoomed past the canvas edge); the scissor below clips
            // back to the visible window.
            let pad_x = (canvas_w - crop_canvas_w) / 2.0;
            let pad_y = (canvas_h - crop_canvas_h) / 2.0;
            // Clamp the user's pan to the in-bounds range for the
            // zoomed crop. If the crop fits entirely (excess ≤ 0)
            // there's no room to scroll, so the pan is pinned to 0
            // and the crop stays centered.
            let excess_x = (crop_canvas_w - canvas_w).max(0.0);
            let excess_y = (crop_canvas_h - canvas_h).max(0.0);
            self.drag_offset.x = self.drag_offset.x.clamp(-excess_x / 2.0, excess_x / 2.0);
            self.drag_offset.y = self.drag_offset.y.clamp(-excess_y / 2.0, excess_y / 2.0);
            self.last_offset = self.drag_offset;
            // Round to whole device pixels for the same reason as the
            // non-crop path in `update_transformation`: a half-pixel
            // translation blurs the texture through bilinear sampling.
            let offset_x = (pad_x - scale * crop_pos.x + self.drag_offset.x).round();
            let offset_y = (pad_y - scale * crop_pos.y + self.drag_offset.y).round();
            // Visible-content canvas-pixel rect — used by the
            // drop-shadow path so the shadow falls around the
            // cropped region, not the full background image
            // (whose edges are off-canvas / scissored out).
            self.display_rect_origin =
                Vec2D::new(pad_x + self.drag_offset.x, pad_y + self.drag_offset.y);
            self.display_rect_size = Vec2D::new(crop_canvas_w, crop_canvas_h);
            let mut t = Transform2D::identity();
            t.scale(scale, scale);
            t.translate(offset_x, offset_y);
            // Scissor takes coords in the CURRENT transform's space —
            // i.e., image space once the crop-fit transform is applied.
            // Passing canvas-pixel values here would silently mis-clip
            // every drawable whose geometry extends past the crop edges
            // (the background image alone clips correctly by virtue of
            // the transform mapping its non-crop pixels off-canvas;
            // strokes that crossed the crop boundary leaked through).
            (
                t,
                Some((crop_pos.x, crop_pos.y, crop_size.x, crop_size.y)),
                scale,
                Vec2D::new(offset_x, offset_y),
            )
        } else {
            // Leaving committed-crop view (or never entered) — reset
            // the user's crop-zoom multiplier so the next commit
            // starts cleanly at 100 % (1.0×). Without this, a user
            // who zoomed inside a crop, reverted, and re-cropped
            // would land in the new committed view at the OLD zoom
            // multiplier (surprising).
            self.crop_zoom = 1.0;
            // Non-crop view: visible rect is the full background image.
            let image_w = self.background_image.width() as f32;
            let image_h = self.background_image.height() as f32;
            self.display_rect_origin = self.offset;
            self.display_rect_size =
                Vec2D::new(image_w * self.scale_factor, image_h * self.scale_factor);
            let mut t = Transform2D::identity();
            t.scale(self.scale_factor, self.scale_factor);
            t.translate(self.offset.x, self.offset.y);
            let scissor = APP_CONFIG
                .read()
                .fixed_canvas()
                .then_some((0.0, 0.0, image_w, image_h));
            (t, scissor, self.scale_factor, self.offset)
        };

        // (Effective-scale → zoom indicator emit happens in the
        //  outer FemtoVGArea::render after this returns, because the
        //  parent sender lives there.)

        // Cache the effective transform so input-coord conversion
        // routes through the same scale/offset the user is seeing.
        self.effective_scale = eff_scale;
        self.effective_offset = eff_offset;

        // Pre-scissor stage: fill the full canvas with CANVAS_BG and
        // draw the drop shadow in canvas-pixel space. Doing this here
        // (rather than inside `render`'s clear + shadow path) is what
        // lets the soft shadow blur fall OUTSIDE a committed crop's
        // scissor rectangle — if we cleared and drew the shadow after
        // setting the scissor, the blur would be clipped and the
        // cropped view would have no visible shadow.
        canvas.reset_transform();
        canvas.clear_rect(0, 0, canvas.width(), canvas.height(), CANVAS_BG);

        {
            let dpr = self.device_pixel_ratio.max(0.0001);
            let has_margins = self.original_rect.pos != Vec2D::zero()
                || self.original_rect.size
                    != Vec2D::new(
                        self.background_image.width() as f32,
                        self.background_image.height() as f32,
                    );
            let (shadow_pos, shadow_size, shadow_scale) = if has_margins {
                // The source screenshot casts the shadow, including where it
                // reaches the viewport's backdrop. The expanded canvas has no
                // shadow of its own. Use image units to match the exported fill.
                let mut source = self.original_rect;
                if let Some((x, y, w, h)) = scissor {
                    let right = (source.pos.x + source.size.x).min(x + w);
                    let bottom = (source.pos.y + source.size.y).min(y + h);
                    source.pos.x = source.pos.x.max(x);
                    source.pos.y = source.pos.y.max(y);
                    source.size = Vec2D::new(
                        (right - source.pos.x).max(0.0),
                        (bottom - source.pos.y).max(0.0),
                    );
                }
                (
                    eff_offset + source.pos * eff_scale,
                    source.size * eff_scale,
                    eff_scale,
                )
            } else {
                (self.display_rect_origin, self.display_rect_size, dpr)
            };
            let (img_x, img_y) = (shadow_pos.x, shadow_pos.y);
            let (img_w, img_h) = (shadow_size.x, shadow_size.y);

            let mut draw_layer = |center_x: f32, center_y: f32, blur: f32, alpha: f32| {
                if img_w <= 0.0 || img_h <= 0.0 {
                    return;
                }
                let mut path = Path::new();
                path.rect(
                    center_x - blur,
                    center_y - blur,
                    img_w + 2.0 * blur,
                    img_h + 2.0 * blur,
                );
                let paint = Paint::box_gradient(
                    center_x,
                    center_y,
                    img_w,
                    img_h,
                    0.0,
                    blur,
                    femtovg::Color::rgbaf(0.0, 0.0, 0.0, alpha),
                    femtovg::Color::rgbaf(0.0, 0.0, 0.0, 0.0),
                );
                canvas.fill_path(&path, &paint);
            };

            // Ambient (contact) layer — tight halo, no offset.
            draw_layer(
                img_x,
                img_y,
                SHADOW_AMBIENT_BLUR_CSS * shadow_scale,
                SHADOW_AMBIENT_ALPHA,
            );

            // Key (elevation) layer — wide, offset downward.
            draw_layer(
                img_x,
                img_y + SHADOW_KEY_OFFSET_Y_CSS * shadow_scale,
                SHADOW_KEY_BLUR_CSS * shadow_scale,
                SHADOW_KEY_ALPHA,
            );
        }

        // Phase boundary: canvas clear + both drop-shadow layers.
        super::perf::mark();

        canvas.reset_transform();
        canvas.set_transform(&transform);

        if let Some((sx, sy, sw, sh)) = scissor {
            canvas.scissor(sx, sy, sw, sh);
        }

        // Canvas + shadow are already painted above; tell `render`
        // to skip its own clear_rect so the shadow survives until
        // the image is drawn over it.
        self.render(
            canvas,
            font,
            true,
            CANVAS_BG,
            true,
            false,
            RenderTarget::Screen,
            transform,
            scissor,
        )?;

        if scissor.is_some() {
            canvas.reset_scissor();
        }

        super::gl::verify_readback_matches_screenshot(canvas);
        super::gl::verify_flat_render(canvas);

        if super::perf::readback_probe() {
            // Both shapes of readback, so the cost of the Blur tool's
            // old whole-framebuffer grab can be read against the region
            // grab that replaced it.
            let _ = super::perf::timed("readback-full", || canvas.screenshot());
            let (rw, rh) = (
                700.min(canvas.width() as usize),
                420.min(canvas.height() as usize),
            );
            let ch = canvas.height() as usize;
            let _ = super::perf::timed("readback-region", || {
                super::gl::read_framebuffer_region(ch, 0, 0, rw, rh)
            });
        }

        super::perf::end_frame(
            canvas.width(),
            canvas.height(),
            self.device_pixel_ratio,
            self.drawables.len(),
        );

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        font: FontId,
        render_crop: bool,
        outside_bg_color: femtovg::Color,
        onscreen: bool,
        clear_canvas: bool,
        restore_target: RenderTarget,
        restore_transform: Transform2D,
        restore_scissor: Option<(f32, f32, f32, f32)>,
    ) -> Result<()> {
        let _decorations = super::RenderDecorationsGuard::new(onscreen);
        // Clear canvas. Skipped when the caller has already filled
        // the canvas + drawn the drop shadow pre-scissor (the
        // `render_framebuffer` path does this so the shadow blur can
        // fall outside a committed-crop's scissor without being
        // clipped).
        if clear_canvas {
            canvas.clear_rect(0, 0, canvas.width(), canvas.height(), outside_bg_color);
        }

        // render background
        self.render_background_image(canvas, onscreen)?;
        if onscreen {
            super::perf::mark();
        }

        // Debug overlay: when `TENSAKU_DEBUG_BANDS=1`, draw a faint
        // colored stripe at each detected text band so we can
        // visually correlate the cursor's anchored position against
        // the heuristic's output. Temporary — strip once the
        // detector is dialed in.
        if onscreen && std::env::var("TENSAKU_DEBUG_BANDS").is_ok() {
            for b in crate::text_bands::bands() {
                let mut path = femtovg::Path::new();
                path.rect(
                    0.0,
                    b.y_start,
                    self.background_image.width() as f32,
                    b.height(),
                );
                let paint = femtovg::Paint::color(femtovg::Color::rgba(255, 60, 60, 50));
                canvas.fill_path(&path, &paint);
                // Solid edge lines at top/bottom for sharp visual.
                let mut edge = femtovg::Path::new();
                edge.move_to(0.0, b.y_start);
                edge.line_to(self.background_image.width() as f32, b.y_start);
                edge.move_to(0.0, b.y_end);
                edge.line_to(self.background_image.width() as f32, b.y_end);
                let mut edge_paint = femtovg::Paint::color(femtovg::Color::rgba(255, 60, 60, 200));
                edge_paint.set_line_width(1.0);
                canvas.stroke_path(&edge, &edge_paint);
            }
        }

        let bounds = (
            Vec2D::zero(),
            Vec2D::new(
                self.background_image.width() as f32,
                self.background_image.height() as f32,
            ),
        );
        // Spotlight pass runs BEFORE the annotation loop so the dark
        // overlay sits BENEATH every drawable. Annotations (arrows,
        // text, shapes) need to stay legible regardless of spotlight
        // darkness — running this pass after them would dim every
        // annotation outside the spotlight cutout, including labels
        // the user explicitly placed there to point at the focused
        // region. Inside the cutout the punch-through still shows
        // the background untouched, so the spotlight effect on the
        // focus area itself is unchanged.
        //
        // Multiple spotlight shapes still union into one dark layer
        // because the punch-out happens against an offscreen image
        // first; running this pass earlier doesn't change that.
        self.render_spotlight_overlay(
            canvas,
            bounds,
            restore_target,
            restore_transform,
            restore_scissor,
        )?;
        if onscreen {
            super::perf::mark();
        }

        // Skip rendering of any drawable currently being dragged by either
        // tool — the tool will render the moved/transformed copy below.
        let dragging_active = self.active_tool.borrow().dragging_drawable_id();
        let dragging_pointer = self.pointer_tool.borrow().dragging_drawable_id();
        // Extra members of a group/move drag — skipped here so only their
        // moved copies (drawn below) show.
        let extra_dragging = self.pointer_tool.borrow().extra_dragging_ids();
        let selected_ids = self.pointer_tool.borrow().selected_drawables();

        for s in &mut self.drawables {
            if dragging_active == Some(s.id)
                || dragging_pointer == Some(s.id)
                || extra_dragging.contains(&s.id)
            {
                continue;
            }
            // Layer-panel visibility: hidden drawables stay in the stack
            // (so canvas auto-resize still includes their bounds, and undo
            // restores them exactly) but don't render.
            if !s.visible {
                continue;
            }
            // Spotlights themselves don't `draw()` — their contribution
            // is the punch-out path collected by the spotlight pass
            // above. Skip so the loop only renders the annotation
            // stack on top of the (already-composited) overlay.
            if s.drawable.is_spotlight() {
                continue;
            }
            let is_selected = onscreen && selected_ids.contains(&s.id);
            // Render the selection glow underneath each selected drawable so
            // the wide blue trace is half-clipped by the drawable on top —
            // leaving only an outer halo.
            if is_selected {
                s.drawable
                    .render_glow(canvas, font, bounds, self.device_pixel_ratio)?;
            }
            // Publish selection state so drawables that draw their
            // own selection decorations (e.g. text's outline) can
            // see fresh layout in the same draw call.
            super::set_current_drawable_is_selected(is_selected);
            s.drawable.draw(canvas, font, bounds)?;
            super::set_current_drawable_is_selected(false);
        }

        let pointer_is_active = Rc::ptr_eq(&self.active_tool, &self.pointer_tool);

        // In-progress drawable from the active tool (e.g. the shape currently
        // being drawn). When the pointer tool is the active tool *and* it's
        // mid-drag, this is the selection's working copy — drawn without the
        // selection glow so the user can see exactly where the edges land.
        //
        // Exception: during a handle *resize* we still publish the selection
        // flag. A text box's dashed outline IS the box being resized — hiding
        // it would leave nothing to aim the drag with. Only `Text::draw`
        // reads the flag, so this is a no-op for every other drawable, and
        // the glow stays off either way (`render_glow` isn't called here).
        // A move drag (`DragMode::Body`) keeps the old no-decoration look.
        {
            let at = self.active_tool.borrow();
            // Hover-only previews (the Counter's ghost badge) are
            // editor furniture, not annotations — they must never
            // reach an export or the clipboard.
            if let Some(d) = at.get_drawable()
                && (onscreen || !d.is_hover_preview())
            {
                let resizing = onscreen && at.is_resizing();
                super::set_current_drawable_is_selected(resizing);
                d.draw(canvas, font, bounds)?;
                super::set_current_drawable_is_selected(false);
            }
            // Other members of a group/move drag (Pointer as active tool).
            for d in at.extra_dragging_drawables() {
                d.draw(canvas, font, bounds)?;
            }
        }

        // The pointer tool's working copy during an implicit-mode drag (active
        // tool is something else, like Arrow). Same treatment as the
        // active-tool branch above — including the resize exception that
        // keeps a text box's dashed outline visible while its handles drag.
        if !pointer_is_active {
            let pt = self.pointer_tool.borrow();
            if let Some(d) = pt.get_drawable() {
                super::set_current_drawable_is_selected(onscreen && pt.is_resizing());
                d.draw(canvas, font, bounds)?;
                super::set_current_drawable_is_selected(false);
            }
            // Other members of a group/move drag (implicit-mode pointer).
            for d in pt.extra_dragging_drawables() {
                d.draw(canvas, font, bounds)?;
            }
        }

        // Selection overlay (marquee + handles for single selection).
        // The spotlight overlay already ran before the annotation
        // loop, so handles and marquee draw on top of the dim layer
        // at full brightness without needing extra ordering tricks
        // here.
        let single_selected_drawable = if selected_ids.len() == 1 {
            self.drawables
                .iter()
                .find(|s| s.id == selected_ids[0])
                // A hidden layer draws nothing, so it shouldn't show its
                // selection handles either — pass None so `build_overlay`
                // skips the handles (the marquee path is unaffected).
                .filter(|s| s.visible)
                .map(|s| s.drawable.as_ref())
        } else {
            None
        };
        if onscreen
            && let Some(o) = self
                .pointer_tool
                .borrow()
                .build_overlay(single_selected_drawable, self.device_pixel_ratio)
        {
            o.draw(canvas, font, bounds)?;
        }

        // render crop tool
        if render_crop && let Some(c) = self.crop_tool.borrow().get_crop() {
            c.draw(canvas, font, bounds)?;
        }
        if onscreen {
            super::perf::mark();
        }

        canvas.flush();
        if onscreen {
            super::perf::mark();
        }
        Ok(())
    }

    /// Build the inverse-mask dark overlay and composite it on top of
    /// the current canvas. No-ops when there are no spotlight shapes
    /// or when darkness rounds to zero. Multiple spotlight shapes
    /// union correctly because the punch-out happens against an
    /// offscreen layer first — doing it directly on the main canvas
    /// would erase the underlying screenshot in the punched regions.
    ///
    /// `restore_target` is the render target the caller had set
    /// before invoking this pass. We switch to a temporary offscreen
    /// image to build the punched overlay, then restore to
    /// `restore_target` and composite back. The caller's transform
    /// is re-established here too (image-space → canvas-space) so
    /// callers don't need to re-set their transform afterward.
    ///
    /// `restore_scissor` is the caller's clip rect (image-space
    /// coords, as passed to `Canvas::scissor`), or `None` when the
    /// caller had no scissor set. The offscreen pass must run with
    /// the clip *off* — the overlay buffer spans the whole image, so
    /// inheriting the committed-crop scissor would dark-fill only a
    /// misplaced sub-rectangle of it. We clear the scissor for the
    /// offscreen pass and re-apply this rect before compositing, so
    /// the final paint still clips to the crop and the clip stays
    /// active for the annotation pass that follows.
    fn render_spotlight_overlay(
        &self,
        canvas: &mut Canvas<renderer::OpenGl>,
        bounds: (Vec2D, Vec2D),
        restore_target: RenderTarget,
        restore_transform: Transform2D,
        restore_scissor: Option<(f32, f32, f32, f32)>,
    ) -> Result<()> {
        let darkness = self.spotlight_darkness.clamp(0.0, 1.0);
        if darkness < 0.001 {
            return Ok(());
        }

        // Collect every spotlight path (committed + the active tool's
        // in-progress one, if any). Pointer-tool drag previews can
        // also be spotlights when a user grabs an existing spotlight
        // to move it — surface those too so the live drag follows.
        let mut paths: Vec<Path> = Vec::new();
        // Openings to magnify, kept beside the punch-out paths: same
        // shape, plus the rect to sample. Collected only when the
        // factor asks for it, and empty otherwise so an ordinary
        // spotlight costs nothing extra.
        let magnifying = self.spotlight_magnification > 1.0;
        let mut magnifiers: Vec<(Path, crate::math::Rect)> = Vec::new();
        let dragging_active = self.active_tool.borrow().dragging_drawable_id();
        let dragging_pointer = self.pointer_tool.borrow().dragging_drawable_id();
        let extra_dragging = self.pointer_tool.borrow().extra_dragging_ids();
        for s in &self.drawables {
            if dragging_active == Some(s.id)
                || dragging_pointer == Some(s.id)
                || extra_dragging.contains(&s.id)
            {
                continue;
            }
            if !s.visible {
                continue;
            }
            if s.drawable.is_spotlight() {
                collect_spotlight(s.drawable.as_ref(), &mut paths, &mut magnifiers, magnifying);
            }
        }
        // In-flight spotlight copies from a group/move drag.
        for d in self.pointer_tool.borrow().extra_dragging_drawables() {
            if d.is_spotlight() {
                collect_spotlight(d, &mut paths, &mut magnifiers, magnifying);
            }
        }
        {
            let at = self.active_tool.borrow();
            if let Some(d) = at.get_drawable()
                && d.is_spotlight()
            {
                collect_spotlight(d, &mut paths, &mut magnifiers, magnifying);
            }
        }
        if !Rc::ptr_eq(&self.active_tool, &self.pointer_tool)
            && let Some(d) = self.pointer_tool.borrow().get_drawable()
            && d.is_spotlight()
        {
            collect_spotlight(d, &mut paths, &mut magnifiers, magnifying);
        }
        if paths.is_empty() {
            return Ok(());
        }

        let img_w = (bounds.1.x - bounds.0.x).max(1.0) as usize;
        let img_h = (bounds.1.y - bounds.0.y).max(1.0) as usize;

        // The punched dark sheet is built as a grid of offscreen tiles
        // no larger than the GL texture limit — the same treatment the
        // background image and export targets get. A single full-image
        // buffer above the limit is storage-less: its render-target
        // switch silently fails and the dark fill would land on the
        // CALLER'S target and corrupt it (long scroll captures exceed
        // the limit routinely). Tiles are seamless because per-pixel
        // coverage is computed by exactly one tile.
        let limit = self.max_texture_size.max(1024);
        let dark = Paint::color(femtovg::Color::rgbaf(0.0, 0.0, 0.0, darkness));
        let punch = Paint::color(femtovg::Color::rgbaf(1.0, 1.0, 1.0, 1.0));

        struct OverlayTile {
            id: femtovg::ImageId,
            x: f32,
            y: f32,
            w: f32,
            h: f32,
        }
        let mut tiles: Vec<OverlayTile> = Vec::new();

        // Build every tile before compositing any of them, so the
        // render target switches stay batched. Collected in a closure
        // so an allocation failure part-way still falls through to the
        // caller-state restore + tile cleanup below instead of leaving
        // the canvas pointed at a half-built tile.
        let build_result = (|| -> Result<()> {
            for (tile_y, tile_h) in tile_ranges(img_h, limit) {
                for (tile_x, tile_w) in tile_ranges(img_w, limit) {
                    // FLIP_Y because GL framebuffer-attached textures
                    // are bottom-up; without it the composited tile
                    // lands upside-down on the final target.
                    let overlay_id = canvas.create_image_empty(
                        tile_w,
                        tile_h,
                        PixelFormat::Rgba8,
                        ImageFlags::FLIP_Y,
                    )?;
                    tiles.push(OverlayTile {
                        id: overlay_id,
                        x: tile_x as f32,
                        y: tile_y as f32,
                        w: tile_w as f32,
                        h: tile_h as f32,
                    });

                    canvas.flush();
                    canvas.set_render_target(RenderTarget::Image(overlay_id));
                    canvas.reset_transform();
                    // Drop any scissor the caller left set. femtovg
                    // keeps the scissor in canvas state across
                    // `set_render_target`, so in committed-crop mode
                    // the clip would still be the on-screen crop rect —
                    // and it would clip the dark fill below to that
                    // rect *inside* this offscreen buffer, leaving the
                    // overlay dark only in a misplaced sub-rectangle.
                    // The clip is re-applied before the composite.
                    canvas.reset_scissor();
                    canvas.clear_rect(
                        0,
                        0,
                        tile_w as u32,
                        tile_h as u32,
                        femtovg::Color::rgbaf(0.0, 0.0, 0.0, 0.0),
                    );

                    // Map image coordinates into this tile so the
                    // spotlight paths (image-space) punch at the right
                    // spots; shapes outside the tile clip away.
                    let mut tile_transform = Transform2D::identity();
                    tile_transform.translate(-(tile_x as f32), -(tile_y as f32));
                    canvas.set_transform(&tile_transform);

                    // Dark fill across this tile's slice of the image.
                    let mut fill = Path::new();
                    fill.rect(tile_x as f32, tile_y as f32, tile_w as f32, tile_h as f32);
                    canvas.fill_path(&fill, &dark);

                    // Punch the spotlight shapes out. The composite
                    // operation only cares about the source's alpha;
                    // any opaque color works.
                    canvas.global_composite_operation(CompositeOperation::DestinationOut);
                    for p in &paths {
                        canvas.fill_path(p, &punch);
                    }
                    canvas.global_composite_operation(CompositeOperation::SourceOver);
                    canvas.flush();
                }
            }
            Ok(())
        })();

        // Restore the caller's target + transform BEFORE acting on any
        // build error, so a failure can never leave the canvas pointed
        // at an overlay tile.
        canvas.set_render_target(restore_target);
        canvas.reset_transform();
        canvas.set_transform(&restore_transform);
        // Re-apply the caller's scissor that the offscreen pass
        // cleared. `Canvas::scissor` bakes in the current transform,
        // so this has to follow `set_transform`. It clips the
        // composite to the committed crop and leaves the clip active
        // for the annotation pass that runs after this returns.
        if let Some((sx, sy, sw, sh)) = restore_scissor {
            canvas.scissor(sx, sy, sw, sh);
        }

        // Sample each loupe's region BEFORE the dim sheet lands on it:
        // the opening shows undimmed content, and a rounded corner or a
        // freehand edge would otherwise pull dimmed pixels in from the
        // parts of the bounding box the shape doesn't cover.
        //
        // The flush is what makes the readback read the right surface.
        // `set_render_target` above only records the intent — femtovg
        // binds it at flush time — and `read_framebuffer_region` is a
        // raw `glReadPixels` on whatever is bound. Without this it
        // samples the last overlay tile instead of the canvas.
        if !magnifiers.is_empty() {
            canvas.flush();
        }
        let magnification = self.spotlight_magnification;
        let lenses: Vec<(&Path, femtovg::ImageId, crate::math::Rect)> = magnifiers
            .iter()
            .filter_map(|(path, rect)| {
                let (x, y, w, h) = crate::tools::canvas_region(canvas, rect.pos, rect.size)?;
                let sub = super::read_framebuffer_region(canvas.height() as usize, x, y, w, h)?;
                let id = canvas
                    .create_image(sub.as_ref(), ImageFlags::empty())
                    .ok()?;
                Some((path, id, *rect))
            })
            .collect();

        if build_result.is_ok() {
            for tile in &tiles {
                let mut final_path = Path::new();
                final_path.rect(tile.x, tile.y, tile.w, tile.h);
                let composited = Paint::image(tile.id, tile.x, tile.y, tile.w, tile.h, 0.0, 1.0);
                canvas.fill_path(&final_path, &composited);
            }
            // Then the loupes, over the sheet and inside their own
            // openings: each region painted into a rect that many times
            // its size about the same centre, so the fill clips away
            // everything but the enlarged middle.
            for (path, id, rect) in &lenses {
                let grown = rect.size * magnification;
                let centre = rect.pos + rect.size * 0.5;
                canvas.fill_path(
                    path,
                    &Paint::image(
                        *id,
                        centre.x - grown.x / 2.0,
                        centre.y - grown.y / 2.0,
                        grown.x,
                        grown.y,
                        0.0,
                        1.0,
                    ),
                );
            }
            canvas.flush();
        }
        for (_, id, _) in &lenses {
            canvas.delete_image(*id);
        }
        for tile in &tiles {
            canvas.delete_image(tile.id);
        }
        build_result
    }

    /// Update the global spotlight darkness used by the next render.
    /// Sketch_board calls this on every slider change; the change
    /// becomes visible after the next `request_render`.
    pub fn set_spotlight_darkness(&mut self, value: f32) {
        self.spotlight_darkness = value.clamp(0.0, 1.0);
    }

    /// Update the loupe factor used by the next render. Global, like
    /// darkness: one overlay, one opening treatment. Sliding it moves
    /// every spotlight at once, which is what makes it usable without
    /// first hunting for the shape and selecting it.
    pub fn set_spotlight_magnification(&mut self, value: f32) {
        self.spotlight_magnification = value.clamp(
            crate::tools::MIN_SPOTLIGHT_MAGNIFICATION,
            crate::tools::MAX_SPOTLIGHT_MAGNIFICATION,
        );
    }

    /// Current global spotlight darkness (0.0–1.0). Read by the
    /// layer panel to render a swatch that matches the dim overlay
    /// the spotlight effect actually paints.
    pub fn spotlight_darkness(&self) -> f32 {
        self.spotlight_darkness
    }

    /// Mirror the background image horizontally and invalidate the
    /// uploaded GL texture so the next render uploads the flipped
    /// pixels. Existing drawables keep their image-space positions
    /// (so a flip immediately followed by drawing lands annotations
    /// over the mirrored content; a flip AFTER drawing leaves the
    /// annotations where they were, no longer tracking the image
    /// content — that's a documented limitation, fixable by
    /// extending the Drawable trait with a mirror op later if it
    /// shows up as friction).
    ///
    /// Returns `true` when the flip succeeded; `false` when the
    /// Pixbuf couldn't be flipped (out of memory).
    /// Apply a whole-canvas flip/rotate to the background raster AND
    /// every drawable (plus the undo/redo snapshots and the protected
    /// `original_rect`), so annotations move with the image instead of
    /// staying put — non-destructively, by remapping geometry rather
    /// than rasterizing. Returns the NEW `(width, height)` (swapped for a
    /// rotate), or `None` if the pixbuf transform failed.
    fn apply_canvas_transform_all(&mut self, t: CanvasTransform) -> Option<(f32, f32)> {
        let old_w = self.background_image.width() as f32;
        let old_h = self.background_image.height() as f32;
        // Transform the background pixels first (this can fail / alloc).
        // Only flip/rotate route through here; a `Scale` (image resize)
        // is handled by `resize_image` with `scale_simple`.
        let new_bg = match t {
            CanvasTransform::FlipHorizontal => self.background_image.flip(true)?,
            CanvasTransform::RotateCcw => self
                .background_image
                .rotate_simple(gtk::gdk_pixbuf::PixbufRotation::Counterclockwise)?,
            // RotateCw (undo of a rotate) and Scale (resize) restore the
            // raster from the stored snapshot, not by re-deriving it here.
            CanvasTransform::RotateCw | CanvasTransform::Scale { .. } => return None,
        };
        // Swap in the transformed raster (keeping the old one for undo),
        // remap the live drawables + the protected rect, and record the
        // undoable op. No history remap needed: the op sits on the undo
        // stack, so LIFO reverses it before any older snapshot is used.
        let prev_image = self.adopt_background_image(new_bg);
        let prev_rect = self.original_rect;
        for s in self.drawables.iter_mut() {
            s.drawable.apply_canvas_transform(t, old_w, old_h);
        }
        self.original_rect = t.map_rect(self.original_rect, old_w, old_h);
        repaint_background_margins(&self.background_image, self.original_rect);
        let new_w = self.background_image.width() as f32;
        let new_h = self.background_image.height() as f32;
        self.record_canvas_op(prev_image, prev_rect, t, new_w, new_h);
        Some((new_w, new_h))
    }

    /// Flip the whole canvas (background + annotations) left↔right.
    /// Returns the new `(width, height)` (unchanged for a flip) or
    /// `None` if the pixbuf flip failed.
    pub fn flip_image_horizontal(&mut self) -> Option<(f32, f32)> {
        self.apply_canvas_transform_all(CanvasTransform::FlipHorizontal)
    }

    /// Rotate the whole canvas (background + annotations) 90°
    /// counter-clockwise. Returns the NEW `(width, height)` (swapped) so
    /// the caller can resize the window / update crop bounds.
    pub fn rotate_image_ccw(&mut self) -> Option<(f32, f32)> {
        self.apply_canvas_transform_all(CanvasTransform::RotateCcw)
    }

    /// Resample the background image to the target pixel dimensions
    /// via `Pixbuf::scale_simple` (bilinear). Invalidates the
    /// uploaded GL texture so the next render uploads the resampled
    /// pixels. Returns the new `(width, height)` once the resample
    /// succeeds; `None` on a degenerate request (zero / negative
    /// dim) or out-of-memory failure inside `scale_simple`.
    ///
    /// Drawables don't scale with the image — same limitation as
    /// the other transforms in this section. Resizing typically
    /// happens before annotating; flatten-into-image first if you
    /// need to ship pre-annotated artwork at a smaller dim.
    pub fn resize_image(&mut self, new_w: i32, new_h: i32) -> Option<(f32, f32)> {
        if new_w <= 0 || new_h <= 0 {
            return None;
        }
        let old_w = self.background_image.width() as f32;
        let old_h = self.background_image.height() as f32;
        let resized = self.background_image.scale_simple(
            new_w,
            new_h,
            gtk::gdk_pixbuf::InterpType::Bilinear,
        )?;
        let w = resized.width() as f32;
        let h = resized.height() as f32;
        // Scale every annotation (and the protected original rect) by the
        // same factor so they stay aligned to the resampled background — a
        // circle keeps circling what it circled. (Stroke widths / font
        // sizes are styles, not geometry, so they're left as-is, matching
        // handle-resize.) Recorded as an undoable canvas op.
        if old_w > 0.0 && old_h > 0.0 {
            let prev_image = self.background_image.clone();
            let prev_rect = self.original_rect;
            let t = CanvasTransform::Scale {
                sx: w / old_w,
                sy: h / old_h,
            };
            for s in self.drawables.iter_mut() {
                s.drawable.apply_canvas_transform(t, old_w, old_h);
            }
            self.original_rect = t.map_rect(self.original_rect, old_w, old_h);
            self.record_canvas_op(prev_image, prev_rect, t, w, h);
        }
        repaint_background_margins(&resized, self.original_rect);
        self.adopt_background_image(resized);
        Some((w, h))
    }

    /// Push an undoable `CanvasOp` for a just-applied whole-canvas
    /// transform: store the pre-op raster + protected rect and the
    /// *inverse* transform (with the post-op dims it maps in), and clear
    /// the redo stack. `forward` is the transform that was applied to the
    /// live drawables; `new_w`/`new_h` are the post-op image dimensions.
    fn record_canvas_op(
        &mut self,
        prev_image: Pixbuf,
        prev_rect: crate::math::Rect,
        forward: CanvasTransform,
        new_w: f32,
        new_h: f32,
    ) {
        self.undo_stack.push(UndoAction::CanvasOp {
            image: prev_image,
            original_rect: prev_rect,
            transform: forward.inverse(),
            w: new_w,
            h: new_h,
        });
        self.redo_stack.clear();
    }

    /// Current image-space dimensions of the background. Used by
    /// the toolbar's "Image size: W×H" label to show what the
    /// resize popover would default the W/H inputs to.
    pub fn image_dimensions(&self) -> (i32, i32) {
        (
            self.background_image.width(),
            self.background_image.height(),
        )
    }

    /// Canvas height (CSS px) at which the auto-fit zoom's vertical
    /// term equals `MIN_AUTO_FIT_ZOOM` — the shortest the canvas may
    /// get before a window resize would shrink the image past that
    /// zoom. The content is the committed crop region when one is
    /// active, otherwise the full image.
    ///
    /// This inverts `update_transformation`'s height term exactly,
    /// including its `inner_h = (canvas − 2·pad).max(canvas · 0.5)`
    /// degenerate guard: the guard wins for short content, so the
    /// answer is the smaller of the two candidate heights.
    fn min_canvas_height_logical(&self) -> f32 {
        let content_h = self
            .crop_tool
            .borrow()
            .get_committed_rect()
            .map(|(_, size)| size.y)
            .filter(|h| *h > 0.0)
            .unwrap_or(self.background_image.height() as f32);
        let dpr = self.device_pixel_ratio.max(0.0001);
        let pad = CANVAS_PADDING_CSS * dpr;
        // `inner_h = 0.1·content_h` solved for the canvas DEVICE
        // height, once via `canvas − 2·pad` and once via the
        // `canvas · 0.5` guard; the consistent root is the smaller.
        let device =
            (MIN_AUTO_FIT_ZOOM * content_h + 2.0 * pad).min(2.0 * MIN_AUTO_FIT_ZOOM * content_h);
        device / dpr
    }

    fn render_background_image(
        &mut self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        onscreen: bool,
    ) -> Result<()> {
        if self.background_tiles.is_empty() {
            // Profiled: a full CPU→GPU re-upload of the screenshot.
            // Anything that invalidates the tile set outright (flip,
            // rotate, resample, undo of those) pays it on the next
            // frame, and its cost scales with the capture's pixel
            // count. An auto-grow does NOT land here — it reuses the
            // existing textures and only uploads its new strips below.
            self.pending_tile_strips.clear();
            self.background_tiles = super::perf::timed("bg-upload", || {
                Self::upload_background_tiles(canvas, &self.background_image, self.max_texture_size)
            })?;
        } else if !self.pending_tile_strips.is_empty() {
            let strips = std::mem::take(&mut self.pending_tile_strips);
            let image = self.background_image.clone();
            let limit = self.max_texture_size;
            let new_tiles = super::perf::timed("bg-upload-strip", || {
                Self::upload_background_strips(canvas, &image, limit, &strips)
            })?;
            self.background_tiles.extend(new_tiles);
        }

        let transparency_bg_id = match self.transparent_background_id {
            Some(id) if onscreen => Some(id),
            None => {
                if let Some(id) = Self::create_transparency_bg(canvas) {
                    self.transparent_background_id.replace(id);
                    Some(id)
                } else {
                    None
                }
            }
            _ => None,
        };

        let w = self.background_image.width() as f32;
        let h = self.background_image.height() as f32;

        // (The on-screen drop shadow is drawn pre-scissor by
        //  `render_framebuffer` so it doesn't get clipped to the
        //  cropped region in committed-crop mode — see the shadow
        //  block at the top of that function. Saved exports skip
        //  shadow entirely.)

        // render the image
        let mut path = Path::new();
        path.rect(0.0, 0.0, w, h);

        if let Some(id) = transparency_bg_id {
            canvas.fill_path(
                &path,
                &Paint::image(
                    id,
                    0f32,
                    0f32,
                    TRANSPARENCY_SQUARE_SIZE as f32,
                    TRANSPARENCY_SQUARE_SIZE as f32,
                    0f32,
                    1f32,
                ),
            );
        }

        for tile in &self.background_tiles {
            // Clip the drawn rect to the live image. After a shrink a
            // reused tile can overhang the raster, and painting it
            // whole would spill background pixels outside the canvas.
            // The paint keeps the tile's full geometry, so the texture
            // still samples at the right offset for the clipped path.
            let x0 = tile.x.max(0.0);
            let y0 = tile.y.max(0.0);
            let x1 = (tile.x + tile.w).min(w);
            let y1 = (tile.y + tile.h).min(h);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let mut tile_path = Path::new();
            tile_path.rect(x0, y0, x1 - x0, y1 - y0);
            canvas.fill_path(
                &tile_path,
                &Paint::image(tile.id, tile.x, tile.y, tile.w, tile.h, 0f32, 1f32),
            );
        }

        Ok(())
    }

    /// Resize the background raster to `(src_x, src_y, new_w, new_h)`,
    /// stated relative to the current raster.
    ///
    /// Growing used to allocate a whole new raster and `copy_area` the
    /// old one into it — 16 ms of the ~18 ms an auto-grow costs at
    /// 6144x3456, paid on every commit or end-of-drag that reaches the
    /// image edge, and once per frame while a held arrow key nudges a
    /// drawable there.
    ///
    /// The copy exists only because the raster is exactly the logical
    /// image. So allocate it with slack instead and let
    /// `background_image` be a *view* into that allocation: a resize
    /// that still fits re-views the same memory, leaving the surviving
    /// pixels exactly where they already are, and paints only the newly
    /// exposed strips. Nothing outside this function sees a difference
    /// — a sub-Pixbuf reports its own width/height and the parent's
    /// rowstride, which every reader here already handles.
    ///
    /// Undo snapshots stay valid: they hold a view of the same
    /// allocation, and a later grow writes only *outside* the region
    /// that view covers.
    fn resize_raster(&mut self, src_x: i32, src_y: i32, new_w: i32, new_h: i32) -> Option<Pixbuf> {
        if new_w <= 0 || new_h <= 0 {
            return None;
        }
        let old = self.background_image.clone();
        let inset = CaptureInset::from_rect(self.original_rect, &old);
        let (view, alloc, origin) = resize_raster_in_alloc(
            &old,
            self.background_alloc.as_ref(),
            self.background_origin,
            src_x,
            src_y,
            new_w,
            new_h,
            inset,
        )?;
        self.background_alloc = Some(alloc);
        self.background_origin = origin;
        Some(view)
    }

    /// Adopt a raster that this type didn't carve out of its own
    /// allocation (a flip, a rotate, a resample, or an undo restoring
    /// an earlier one). The allocation is dropped so the next resize
    /// starts a fresh one rather than assuming an origin into it.
    fn adopt_background_image(&mut self, image: Pixbuf) -> Pixbuf {
        self.background_alloc = None;
        self.background_origin = (0, 0);
        self.invalidate_background_tiles();
        std::mem::replace(&mut self.background_image, image)
    }

    /// Keep the uploaded background textures across an auto-grow /
    /// shrink instead of re-uploading the whole capture.
    ///
    /// A resize only ever moves the old raster within the new one (by
    /// `translation`) and exposes fresh strips around it — the old
    /// pixels are unchanged. Tiles carry their own image-space rect, so
    /// shifting those rects re-registers the existing textures at their
    /// new coordinates for free, and only the newly exposed strips need
    /// uploading. At 6144x3456 that is the difference between a ~23 ms
    /// whole-image upload and a fraction of a millisecond, on an
    /// operation a keyboard nudge at the image edge repeats at key-repeat
    /// rate.
    ///
    /// Shrink needs no work here: `render_background_image` clips each
    /// tile's *path* to the image, and the paint keeps its own
    /// geometry, so the texture still samples correctly.
    ///
    /// Falls back to a full rebuild when there is nothing uploaded yet,
    /// or when a long grow sequence has accumulated more strips than is
    /// worth tracking.
    fn reuse_background_tiles(
        &mut self,
        translation: Vec2D,
        old_w: f32,
        old_h: f32,
        new_w: f32,
        new_h: f32,
    ) {
        /// Beyond this, rebuild the background as one clean texture
        /// instead of accumulating strips.
        ///
        /// Kept low deliberately. Every extra tile is another boundary
        /// bilinear sampling cannot cross, and a long run of grows —
        /// nudging an annotation outward — appends one per event. The
        /// overlap above stops a boundary showing, but fewer boundaries
        /// is better than more, and a rebuild is ~20 ms against the
        /// ~1 ms a grow costs, so it is affordable a few times a second.
        const MAX_TILES: usize = 8;

        // Count strips still queued from earlier grows: several can
        // land between two frames (a keyboard nudge at the image edge
        // outruns the frame clock), and each becomes a tile.
        if self.background_tiles.is_empty()
            || self.background_tiles.len() + self.pending_tile_strips.len() + 4 > MAX_TILES
            || self.max_texture_size == 0
        {
            self.invalidate_background_tiles();
            return;
        }

        for tile in &mut self.background_tiles {
            tile.x += translation.x;
            tile.y += translation.y;
        }
        // Queued strips are image-space rects too, so they move with
        // everything else when a grow prepends a top/left band.
        for strip in &mut self.pending_tile_strips {
            strip.0 += translation.x;
            strip.1 += translation.y;
        }

        self.pending_tile_strips.extend(background_grow_strips(
            translation,
            old_w,
            old_h,
            new_w,
            new_h,
        ));
    }

    /// Drop every uploaded background texture so the next render
    /// re-uploads from scratch. For changes that rewrite the raster's
    /// contents (flip, rotate, resample, undo of those) rather than
    /// just extending it.
    fn invalidate_background_tiles(&mut self) {
        // Hand the textures back rather than just dropping the ids —
        // `ImageId` is a plain handle, so clearing the vec on its own
        // leaked one texture per tile on every flip, rotate, resample
        // or undo of those.
        for tile in self.background_tiles.drain(..) {
            super::gl::queue_image_deletion(tile.id);
        }
        self.pending_tile_strips.clear();
    }

    /// Upload just `strips` (image-space rects) as additional tiles,
    /// each split to respect `max_texture_size`. Used by the
    /// incremental grow path, where everything except these rects is
    /// already resident on the GPU.
    fn upload_background_strips(
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        image: &Pixbuf,
        max_texture_size: usize,
        strips: &[(f32, f32, f32, f32)],
    ) -> Result<Vec<BackgroundTile>> {
        let img_w = image.width() as usize;
        let img_h = image.height() as usize;
        let limit = max_texture_size.max(1024);
        let mut tiles = Vec::new();

        for &(sx, sy, sw, sh) in strips {
            // Clamp to the raster: a strip is derived from float rects
            // and must never index past the Pixbuf. Inflate by a pixel
            // first so neighbouring tiles OVERLAP: each tile is its own
            // texture, and bilinear sampling cannot reach across two of
            // them, so a boundary between abutting tiles shows as a
            // seam once the canvas is minified enough for a destination
            // pixel to span more than one texel. The overlap costs one
            // row/column per strip and the duplicated pixels are
            // identical, so whichever tile draws last is right.
            let x0 = ((sx - 1.0).max(0.0) as usize).min(img_w);
            let y0 = ((sy - 1.0).max(0.0) as usize).min(img_h);
            let x1 = (((sx + sw + 1.0).max(0.0)).ceil() as usize).min(img_w);
            let y1 = (((sy + sh + 1.0).max(0.0)).ceil() as usize).min(img_h);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            for (ty, th) in tile_ranges(y1 - y0, limit) {
                for (tx, tw) in tile_ranges(x1 - x0, limit) {
                    tiles.extend(Self::upload_region(
                        canvas,
                        image,
                        x0 + tx,
                        y0 + ty,
                        tw,
                        th,
                    )?);
                }
            }
        }
        Ok(tiles)
    }

    /// Upload `image`'s `(x, y, w, h)` region as one tile.
    ///
    /// femtovg pins `UNPACK_ROW_LENGTH` to the image width and ignores
    /// an `ImgRef`'s stride, so the upload has to be tightly packed at
    /// `w` pixels per row. When the region spans the full image width
    /// of a Pixbuf with no row padding the Pixbuf's buffer already is
    /// that layout and goes straight to GL; otherwise its rows are
    /// packed into `scratch` first.
    fn upload_region(
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        image: &Pixbuf,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) -> Result<Option<BackgroundTile>> {
        if w == 0 || h == 0 {
            return Ok(None);
        }
        let format = if image.has_alpha() {
            PixelFormat::Rgba8
        } else {
            PixelFormat::Rgb8
        };
        let id = canvas.create_image_empty(w, h, format, ImageFlags::empty())?;

        // SAFETY: `pixels()` borrows the Pixbuf's live buffer and we only
        // read from it. `align_to` is safe on either source because
        // RGB<u8>/RGBA<u8> are `repr(C)` byte structs with alignment 1,
        // so the whole slice comes back as the middle segment.
        unsafe {
            let src = image.pixels();
            let bytes = region_bytes(src, RasterLayout::of(image), x, y, w, h);
            let bytes: &[u8] = &bytes;
            if image.has_alpha() {
                let img = Img::new(bytes.align_to::<RGBA<u8>>().1, w, h);
                canvas.update_image(id, ImageSource::Rgba(img), 0, 0)?;
            } else {
                let img = Img::new(bytes.align_to::<RGB<u8>>().1, w, h);
                canvas.update_image(id, ImageSource::Rgb(img), 0, 0)?;
            }
        }

        Ok(Some(BackgroundTile {
            id,
            x: x as f32,
            y: y as f32,
            w: w as f32,
            h: h as f32,
        }))
    }

    /// Upload `image` as a grid of GPU tiles no larger than
    /// `max_texture_size` on either axis. A single texture would be
    /// storage-less above that limit (long scroll captures routinely
    /// exceed it), sampling as black on screen and breaking offscreen
    /// exports — see `BackgroundTile`.
    fn upload_background_tiles(
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        image: &Pixbuf,
        max_texture_size: usize,
    ) -> Result<Vec<BackgroundTile>> {
        let format = if image.has_alpha() {
            PixelFormat::Rgba8
        } else {
            PixelFormat::Rgb8
        };

        let width = image.width() as usize;
        let height = image.height() as usize;
        let limit = max_texture_size.max(1024);

        // femtovg pins `UNPACK_ROW_LENGTH` to the image width rather
        // than honouring an `ImgRef`'s stride, so every upload has to
        // be tightly packed at `tile_w` pixels per row. When a tile
        // spans the full image width AND the Pixbuf has no row padding
        // (the usual case — gdk-pixbuf only pads to a 4-byte boundary),
        // the Pixbuf's own buffer is already in exactly that layout and
        // we can hand it straight to GL. Otherwise we pack the tile's
        // rows into a scratch buffer first.
        let mut tiles = Vec::new();
        // SAFETY: `image.pixels()` borrows the pixbuf's live buffer; we
        // only read from it. `align_to` is safe on either source
        // because RGB<u8>/RGBA<u8> are `repr(C)` byte structs with
        // alignment 1, so the whole slice comes back as the middle
        // segment with no padding.
        unsafe {
            let src_buffer = image.pixels();
            for (tile_y, tile_h) in tile_ranges(height, limit) {
                for (tile_x, tile_w) in tile_ranges(width, limit) {
                    let id =
                        canvas.create_image_empty(tile_w, tile_h, format, ImageFlags::empty())?;

                    let bytes = region_bytes(
                        src_buffer,
                        RasterLayout::of(image),
                        tile_x,
                        tile_y,
                        tile_w,
                        tile_h,
                    );
                    let bytes: &[u8] = &bytes;

                    if image.has_alpha() {
                        let img = Img::new(bytes.align_to::<RGBA<u8>>().1, tile_w, tile_h);
                        canvas.update_image(id, ImageSource::Rgba(img), 0, 0)?;
                    } else {
                        let img = Img::new(bytes.align_to::<RGB<u8>>().1, tile_w, tile_h);
                        canvas.update_image(id, ImageSource::Rgb(img), 0, 0)?;
                    }

                    tiles.push(BackgroundTile {
                        id,
                        x: tile_x as f32,
                        y: tile_y as f32,
                        w: tile_w as f32,
                        h: tile_h as f32,
                    });
                }
            }
        }

        Ok(tiles)
    }

    fn create_transparency_bg(
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
    ) -> Option<femtovg::ImageId> {
        let tile: usize = TRANSPARENCY_SQUARE_SIZE * 2;
        let mut pixels = vec![RGBA8::new(204, 204, 204, 255); tile * tile];

        for y in 0..tile {
            for x in 0..tile {
                if (x / TRANSPARENCY_SQUARE_SIZE + y / TRANSPARENCY_SQUARE_SIZE) % 2 == 1 {
                    pixels[y * tile + x] = RGBA8::new(153, 153, 153, 255);
                }
            }
        }
        let img = Img::new(pixels, tile, tile);

        match canvas.create_image(
            ImageSource::Rgba(img.as_ref()),
            ImageFlags::REPEAT_X | ImageFlags::REPEAT_Y,
        ) {
            Ok(id) => Some(id),
            Err(_) => {
                eprintln!("Could not create transparency background image");
                None
            }
        }
    }

    /// Render scale at which the background image displays at its true
    /// 1× logical size — i.e. what "100% zoom" means for this capture.
    ///
    /// The image arrives as capture-native (device) pixels. The GL
    /// canvas renders into a framebuffer `device_pixel_ratio`× the
    /// logical canvas. So an image drawn at render scale `s` occupies
    /// `image_px · s / device_pixel_ratio` logical pixels. For that to
    /// equal the image's own logical size (`image_px / capture_scale`)
    /// the render scale must be `device_pixel_ratio / capture_scale`.
    ///
    /// On integer-scaled displays the capture scale equals the GL DPR,
    /// so this is exactly `1.0` and nothing changes. On a fractional
    /// output (e.g. 1.07× — where GTK rounds the GL DPR up to 2) it's
    /// `2 / 1.07 ≈ 1.87`, which is what stops the screenshot from
    /// rendering at roughly half size.
    fn natural_scale(&self) -> f32 {
        let dpr = self.device_pixel_ratio.max(0.0001);
        let capture = crate::display::capture_scale().unwrap_or(dpr).max(0.0001);
        dpr / capture
    }

    pub fn update_transformation(
        &mut self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
    ) {
        let image_width = self.background_image.width() as f32;
        let image_height = self.background_image.height() as f32;

        let canvas_width = canvas.width() as f32;
        let canvas_height = canvas.height() as f32;
        self.last_canvas_size = Vec2D::new(canvas_width, canvas_height);

        // update scale_factor
        if self.zoom_scale != 0.0 {
            if self.zoom_scale != self.last_scale {
                self.last_scale = self.zoom_scale;
                self.scale_factor = self.zoom_scale;

                if !self.is_reset && !self.zoom_anchor_pending {
                    // Keep the image centered on zoom — clear the
                    // accumulated drag offset so `center_offset`
                    // (below) places the image at the canvas's
                    // middle. Skipped when the zoom came from
                    // `set_zoom_scale_at`, which has already
                    // computed a `drag_offset` that anchors the
                    // image under the cursor.
                    self.drag_offset = Vec2D::zero();
                    self.store_last_offset();
                }
                self.zoom_anchor_pending = false;
            } else {
                self.scale_factor = self.zoom_scale;
            }
        } else {
            // Auto-fit branch (no user zoom yet): always reserve
            // `CANVAS_PADDING_CSS` of breathing room on every side.
            // If the image fits inside that padded area at 1:1, render
            // at 100 %. Otherwise scale it down so it still fits
            // inside the padded area — never go edge-to-edge
            // automatically. The user can pinch / scroll to zoom in
            // past this if they want. `auto_fit_scale` floors the
            // result at `MIN_AUTO_FIT_ZOOM` so a window squeezed
            // shorter than that clips the image instead of shrinking
            // it further. The `.max(canvas * 0.5)` floor is a
            // degenerate-case guard for canvases smaller than 2 × pad
            // — keeps the inner area positive so the computed scale
            // stays finite during initial layout.
            let pad = CANVAS_PADDING_CSS * self.device_pixel_ratio.max(0.0001);
            let inner_w = (canvas_width - 2.0 * pad).max(canvas_width * 0.5).max(1.0);
            let inner_h = (canvas_height - 2.0 * pad)
                .max(canvas_height * 0.5)
                .max(1.0);
            self.scale_factor = auto_fit_scale(
                inner_w,
                inner_h,
                image_width,
                image_height,
                self.natural_scale(),
            );
        }

        // `effective_scale` is what the zoom indicator should show:
        // it's the on-screen scale a fresh `render_framebuffer` will
        // actually use. For the regular view that's just
        // `scale_factor`. For a committed crop we re-run the same
        // auto-fit-with-padding cascade `render_framebuffer` uses —
        // 100 % when the crop fits with padding, scaled-down only
        // when it can't — multiplied by `crop_zoom`. Computing this
        // here (rather than waiting for `render_framebuffer`) means
        // the existing `notify_zoom_display` path in `resize` picks
        // up the right value without special-casing crop mode.
        let committed_crop = self
            .crop_tool
            .borrow()
            .get_committed_rect()
            .filter(|(_, s)| s.x > 0.0 && s.y > 0.0);
        self.effective_scale = if let Some((_, crop_size)) = committed_crop {
            let pad = CANVAS_PADDING_CSS * self.device_pixel_ratio.max(0.0001);
            let inner_w = (canvas_width - 2.0 * pad).max(canvas_width * 0.5).max(1.0);
            let inner_h = (canvas_height - 2.0 * pad)
                .max(canvas_height * 0.5)
                .max(1.0);
            auto_fit_scale(
                inner_w,
                inner_h,
                crop_size.x,
                crop_size.y,
                self.natural_scale(),
            ) * self.crop_zoom
        } else {
            self.scale_factor
        };

        let center_offset = Vec2D::new(
            (canvas_width - image_width * self.scale_factor) / 2.0,
            (canvas_height - image_height * self.scale_factor) / 2.0,
        );

        // When the image fully fits the canvas on an axis (`excess
        // == 0`) there's no scroll affordance at all, so we hard-pin
        // the drag offset to zero on that axis — rubber-banding a
        // fit-to-canvas image would let the user pull a perfectly
        // centered screenshot off-center for no reason.
        //
        // For axes that DO have excess we let `drag_offset` grow
        // freely; the rubber-band map below produces an ever-
        // diminishing visible overshoot so further pulling at the
        // extreme still translates to a sliver of motion (matches
        // macOS' bottomless-elastic behavior) instead of slamming
        // into a hard cap. The clamp at a very large multiple of
        // `max_overshoot` is purely a guard against runaway float
        // growth from minutes of held-down scrolling.
        let excess_x = (image_width * self.scale_factor - canvas_width).max(0.0);
        let excess_y = (image_height * self.scale_factor - canvas_height).max(0.0);
        let limit_x = excess_x / 2.0;
        let limit_y = excess_y / 2.0;
        let max_overshoot = RUBBER_BAND_MAX_OVERSHOOT_CSS * self.device_pixel_ratio.max(0.0001);
        let runaway_cap = 100.0 * max_overshoot.max(1.0);
        if excess_x <= 0.0 {
            self.drag_offset.x = 0.0;
        } else {
            self.drag_offset.x = self
                .drag_offset
                .x
                .clamp(-limit_x - runaway_cap, limit_x + runaway_cap);
        }
        if excess_y <= 0.0 {
            self.drag_offset.y = 0.0;
        } else {
            self.drag_offset.y = self
                .drag_offset
                .y
                .clamp(-limit_y - runaway_cap, limit_y + runaway_cap);
        }

        // Spring-back: once the user is idle, ease `drag_offset` back
        // toward the nearest hard limit so the rubber-band stretch
        // recovers smoothly. Skipped while a gesture is mid-flight
        // (we'd fight the user's input) or while a drawable is being
        // dragged (`is_drag`).
        let idle_ms = std::time::Instant::now()
            .duration_since(self.last_pan_input)
            .as_millis();
        if idle_ms > SPRING_BACK_IDLE_MS && !self.is_drag {
            // Lock in the recovery start state — the VISIBLE offset
            // at release (rubber-banded), not the raw drag_offset.
            // Subsequent ticks ease this value toward the limit on a
            // smooth curve, and we back-solve a drag_offset that
            // reproduces the eased visible offset via the
            // rubber-band map.
            let (start_time, start_visible) = match self.spring_back_anim {
                Some(s) => s,
                None => {
                    let visible = Vec2D::new(
                        rubber_band(self.drag_offset.x, limit_x, max_overshoot),
                        rubber_band(self.drag_offset.y, limit_y, max_overshoot),
                    );
                    let s = (std::time::Instant::now(), visible);
                    self.spring_back_anim = Some(s);
                    s
                }
            };
            let elapsed_ms = start_time.elapsed().as_millis() as f32;
            let (vis_x, done_x) = spring_back_progress(start_visible.x, limit_x, elapsed_ms);
            let (vis_y, done_y) = spring_back_progress(start_visible.y, limit_y, elapsed_ms);
            // Back-solve drag_offset so the rubber-band render below
            // reproduces the eased visible value.
            self.drag_offset.x = inverse_rubber_band(vis_x, limit_x, max_overshoot);
            self.drag_offset.y = inverse_rubber_band(vis_y, limit_y, max_overshoot);
            if done_x && done_y {
                self.spring_back_anim = None;
            }
        } else {
            // Active gesture (or no overshoot) — drop any pending
            // animation so the next idle stretch starts a fresh
            // recovery from the user's release point.
            self.spring_back_anim = None;
        }
        self.last_offset = self.drag_offset;

        // Rubber-band map for rendering: even with `drag_offset` past
        // the limit, the OFFSET we hand to the canvas is damped via a
        // hyperbolic curve that asymptotes at `limit + max_overshoot`.
        // Pulling past the edge feels stretchy instead of slamming.
        let effective_x = rubber_band(self.drag_offset.x, limit_x, max_overshoot);
        let effective_y = rubber_band(self.drag_offset.y, limit_y, max_overshoot);

        if self.is_reset {
            //centered
            self.is_reset = false;
            self.offset = center_offset;
        } else {
            //dragged
            self.offset = center_offset + Vec2D::new(effective_x, effective_y);
        }

        // Snap the image origin to whole device pixels. `center_offset`
        // is `(canvas − image) / 2`, which lands on a half-pixel
        // whenever `canvas − image` is odd — and a half-pixel
        // translation makes the GPU sample the background texture
        // between texels, blurring the *entire* image through bilinear
        // filtering even at an exact 1:1 scale. Rounding here keeps a
        // 100%-zoom screenshot pixel-perfect; at a fractional render
        // scale the image is resampled regardless, so the round is
        // harmless. Drawables share this transform, so they stay
        // pinned to the image.
        self.offset.x = self.offset.x.round();
        self.offset.y = self.offset.y.round();
    }

    /// Pan the canvas by `(dx, dy)` canvas-space pixels. Accumulates
    /// into `drag_offset`; the next `update_transformation` applies
    /// rubber-band damping to the render side and (once the user is
    /// idle) drives the ease-in-out recovery back inside the limit.
    pub fn pan_by(&mut self, dx: f32, dy: f32) {
        self.drag_offset.x += dx;
        self.drag_offset.y += dy;
        self.last_offset = self.drag_offset;
        self.last_pan_input = std::time::Instant::now();
        // User took over — abandon any in-flight recovery so the
        // next idle stretch starts fresh from the new release point.
        self.spring_back_anim = None;
    }

    /// True when `drag_offset` is currently outside the hard pan
    /// limits — i.e. the rubber-band stretch is non-zero and the
    /// spring-back timer should keep ticking until it's recovered.
    pub fn drag_offset_overshoots(&self) -> bool {
        let canvas_w = self.last_canvas_size.x;
        let canvas_h = self.last_canvas_size.y;
        if canvas_w <= 0.0 || canvas_h <= 0.0 {
            return false;
        }
        let image_w = self.background_image.width() as f32 * self.scale_factor;
        let image_h = self.background_image.height() as f32 * self.scale_factor;
        let limit_x = (image_w - canvas_w).max(0.0) / 2.0;
        let limit_y = (image_h - canvas_h).max(0.0) / 2.0;
        self.drag_offset.x.abs() > limit_x + SPRING_BACK_SNAP_EPS
            || self.drag_offset.y.abs() > limit_y + SPRING_BACK_SNAP_EPS
    }

    /// Which axes have anything to scroll to: `(horizontal, vertical)`.
    /// An axis has slack when the scaled image is wider (or taller)
    /// than the canvas showing it.
    pub fn pan_slack(&self) -> (bool, bool) {
        let image_w = self.background_image.width() as f32 * self.scale_factor;
        let image_h = self.background_image.height() as f32 * self.scale_factor;
        (
            image_w - self.last_canvas_size.x > 1.0,
            image_h - self.last_canvas_size.y > 1.0,
        )
    }

    /// Apply a scrollbar value to one axis. Scrollbar values run
    /// 0..=excess (where excess = image*scale − canvas), counted
    /// from the top/left of the scaled image. Our `drag_offset` is
    /// centered: `-excess/2` means the image is fully shifted left
    /// (right edge visible), `+excess/2` is fully shifted right.
    /// So `drag = excess/2 − value`. If the canvas size hasn't been
    /// captured yet (no `update_transformation` has run), this is a
    /// no-op — there's nothing to scroll on a zero-sized canvas.
    pub fn set_pan_from_scrollbar(&mut self, is_horizontal: bool, value: f32) {
        let image_w = self.background_image.width() as f32 * self.scale_factor;
        let image_h = self.background_image.height() as f32 * self.scale_factor;
        if is_horizontal {
            let excess = (image_w - self.last_canvas_size.x).max(0.0);
            if excess <= 0.0 {
                return;
            }
            self.drag_offset.x = (excess / 2.0 - value).clamp(-excess / 2.0, excess / 2.0);
        } else {
            let excess = (image_h - self.last_canvas_size.y).max(0.0);
            if excess <= 0.0 {
                return;
            }
            self.drag_offset.y = (excess / 2.0 - value).clamp(-excess / 2.0, excess / 2.0);
        }
        self.last_offset = self.drag_offset;
    }

    /// Current image-to-canvas scale used for the most recent render.
    /// Falls back to `scale_factor` if `update_transformation` hasn't
    /// run yet (which would leave `effective_scale` at its 1.0 init).
    pub fn effective_scale_or_fallback(&self) -> f32 {
        if self.effective_scale > 0.0 {
            self.effective_scale
        } else {
            self.scale_factor.max(1.0)
        }
    }

    /// The renderer's current image→canvas transform: effective scale
    /// and offset. The crop tool reads the scale on activation and after
    /// transform-changing gestures to keep handle hit-testing
    /// screen-constant.
    pub fn render_transform(&self) -> (f32, Vec2D) {
        (self.effective_scale, self.effective_offset)
    }

    pub fn abs_canvas_to_image_coordinates(&self, input: Vec2D, dpi_scale_factor: f32) -> Vec2D {
        Vec2D::new(
            (input.x * dpi_scale_factor - self.effective_offset.x) / self.effective_scale,
            (input.y * dpi_scale_factor - self.effective_offset.y) / self.effective_scale,
        )
    }
    pub fn rel_canvas_to_image_coordinates(&self, input: Vec2D, dpi_scale_factor: f32) -> Vec2D {
        Vec2D::new(
            input.x * dpi_scale_factor / self.effective_scale,
            input.y * dpi_scale_factor / self.effective_scale,
        )
    }

    pub fn set_zoom_scale(&mut self, factor: f32, abs: bool) {
        if self.is_drag {
            return;
        }

        // In committed-crop mode the base scale is the fit-to-canvas
        // calculation done at render time, not `scale_factor`. Route
        // the user's zoom into `crop_zoom` (a multiplier on top of
        // the fit) so wheel-up makes the crop larger and wheel-down
        // makes it smaller. Clamp to 0.5×–8× so the user can't lose
        // the image off-screen at one extreme or zoom out so far it
        // becomes a dot at the other.
        if self.crop_tool.borrow().get_committed_rect().is_some() {
            if abs {
                self.crop_zoom = factor.clamp(0.5, 8.0);
            } else {
                self.crop_zoom = (self.crop_zoom * factor).clamp(0.5, 8.0);
            }
            return;
        }

        // User-zoom range: 10% to 500%. Anything outside is either too
        // dot-like to make out (below 10%) or so blown up that the
        // user can only see a sliver of the image (above 500%).
        // `factor == 0.0` is the FitCanvas sentinel — preserved as-is
        // so `update_transformation` re-enters the auto-fit branch.
        //
        // `zoom_scale` is stored as a *render* scale (the transform
        // fed to `t.scale`), where `natural_scale()` — not 1.0 — is
        // 100%. So the user-facing limits and an absolute `factor`
        // (which arrives in user-zoom terms, 1.0 = 100%) are both
        // mapped through `natural`. A relative `factor` is a plain
        // ratio and needs no mapping; only its clamp bounds do.
        const MIN_ZOOM: f32 = 0.10;
        const MAX_ZOOM: f32 = 5.00;
        let natural = self.natural_scale();
        let (min_render, max_render) = (MIN_ZOOM * natural, MAX_ZOOM * natural);

        if abs {
            if factor == 0.0 {
                self.zoom_scale = 0.0;
            } else {
                self.zoom_scale = (factor * natural).clamp(min_render, max_render);
            }
        } else {
            if self.zoom_scale == 0.0 {
                self.zoom_scale = self.scale_factor;
            }

            self.zoom_scale = (self.zoom_scale * factor).clamp(min_render, max_render);
        }
    }

    pub fn set_pointer_offset(&mut self, offset: Vec2D) {
        self.pointer_offset = offset;
    }

    /// Last known cursor position in canvas (physical) pixels. The
    /// Motion controller pushes this via `set_pointer_offset` on
    /// every move, so it tracks the user's cursor across the canvas
    /// continuously — used by `set_zoom_scale_at_cursor` to anchor
    /// wheel-zoom on whatever the user is hovering over.
    pub fn pointer_offset(&self) -> Vec2D {
        self.pointer_offset
    }

    /// Zoom while keeping `anchor_canvas` (in canvas physical pixels,
    /// same units as `pointer_offset` / `drag_offset`) under the same
    /// canvas position before and after. Reduces to `set_zoom_scale`
    /// when the scale doesn't actually change, or when committed crop
    /// is active (the crop view has its own zoom semantics).
    pub fn set_zoom_scale_at(&mut self, factor: f32, abs: bool, anchor_canvas: Vec2D) {
        if self.is_drag {
            return;
        }
        // Committed-crop mode routes zoom through `crop_zoom` (a
        // multiplier on top of the fit) — no drag_offset to adjust,
        // so just defer to the existing path.
        if self.crop_tool.borrow().get_committed_rect().is_some() {
            self.set_zoom_scale(factor, abs);
            return;
        }
        // Capture pre-zoom state so we can solve for the new
        // drag_offset that keeps the anchor pinned.
        let canvas_w = self.last_canvas_size.x;
        let canvas_h = self.last_canvas_size.y;
        let image_w = self.background_image.width() as f32;
        let image_h = self.background_image.height() as f32;
        let old_scale = self.scale_factor;
        if canvas_w <= 0.0 || canvas_h <= 0.0 || old_scale <= 0.0 {
            self.set_zoom_scale(factor, abs);
            return;
        }
        let old_center = Vec2D::new(
            (canvas_w - image_w * old_scale) / 2.0,
            (canvas_h - image_h * old_scale) / 2.0,
        );
        // Image-space point (in original image pixels) currently
        // displayed at `anchor_canvas`.
        let image_pt = (anchor_canvas - old_center - self.drag_offset) * (1.0 / old_scale);

        // Apply the zoom request through the standard path so the
        // crop-zoom branch + min/max clamps + FitCanvas sentinel
        // all stay centralised.
        self.set_zoom_scale(factor, abs);

        // `set_zoom_scale` writes `zoom_scale`; `scale_factor`
        // doesn't update until the next `update_transformation`.
        // Compute the future scale ourselves so we can set the
        // matching `drag_offset` right now (avoids a one-frame
        // flicker where the image briefly recenters).
        let new_scale = if self.zoom_scale > 0.0 {
            self.zoom_scale
        } else {
            // FitCanvas / cold start — let auto-fit run, no anchor.
            return;
        };
        if (new_scale - old_scale).abs() < 1e-4 {
            return;
        }
        let new_center = Vec2D::new(
            (canvas_w - image_w * new_scale) / 2.0,
            (canvas_h - image_h * new_scale) / 2.0,
        );
        self.drag_offset = anchor_canvas - new_center - image_pt * new_scale;
        self.store_last_offset();
        // Tell update_transformation NOT to zero this drag_offset
        // when it picks up the new scale on the next render tick.
        self.zoom_anchor_pending = true;
    }

    pub fn set_drag_offset(&mut self, offset: Vec2D) {
        self.drag_offset = self.last_offset + offset;
    }

    pub fn reset_drag_offset(&mut self) {
        self.drag_offset = Vec2D::zero();
        self.store_last_offset();
        self.is_reset = true;
    }

    pub fn store_last_offset(&mut self) {
        self.last_offset = self.drag_offset;
    }

    pub fn set_is_drag(&mut self, is_drag: bool) {
        self.is_drag = is_drag;
    }
}

#[cfg(test)]
mod tests {
    use super::{Pixbuf, Vec2D, resize_pixbuf_to_rect, tile_ranges};

    #[test]
    #[ignore = "Requires a GTK display and OpenGL"]
    fn selecting_an_arrow_does_not_change_export_pixels() {
        use super::*;
        use crate::sketch_board::{MouseButton, MouseEventMsg, MouseEventType};
        use crate::tools::{ArrowTool, ToolUpdateResult, Tools, ToolsManager};

        gtk::init().unwrap();
        crate::load_gl().unwrap();
        let background = Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 128, 128).unwrap();
        background.fill(0xffffffff);
        let tools = ToolsManager::new();
        let pointer = tools.get(&Tools::Pointer);
        let (sender, _receiver) = relm4::channel();
        let mut area = super::super::FemtoVGArea::default();
        area.init(
            sender,
            Rc::new(RefCell::new(CropTool::default())),
            pointer.clone(),
            pointer.clone(),
            background,
        );
        let window = gtk::Window::builder()
            .default_width(256)
            .default_height(256)
            .child(&area)
            .build();
        window.present();
        area.make_current();
        assert!(area.error().is_none(), "OpenGL context must be available");
        let imp = area.imp();
        imp.ensure_canvas();

        let mut arrow = ArrowTool::default();
        let event = |type_, pos| MouseEventMsg {
            type_,
            pos,
            button: MouseButton::Primary,
            n_pressed: 1,
            modifier: gtk::gdk::ModifierType::empty(),
            release: false,
        };
        arrow.handle_mouse_event(event(MouseEventType::BeginDrag, Vec2D::new(30.0, 30.0)));
        let ToolUpdateResult::Commit(drawable) =
            arrow.handle_mouse_event(event(MouseEventType::EndDrag, Vec2D::new(60.0, 60.0)))
        else {
            panic!("arrow gesture should commit");
        };
        {
            let mut inner = imp.inner();
            let inner = inner.as_mut().unwrap();
            let id = inner.commit(drawable);
            let mut canvas = imp.canvas.borrow_mut();
            let canvas = canvas.as_mut().unwrap();
            let font = imp.font.borrow().unwrap();
            let unselected = inner.render_native_resolution(canvas, font).unwrap();
            pointer.borrow_mut().set_selected_drawables(vec![id]);
            let selected = inner.render_native_resolution(canvas, font).unwrap();
            assert_eq!(
                unselected.buf(),
                selected.buf(),
                "selection must not affect saved pixels"
            );
            assert!(
                selected
                    .buf()
                    .iter()
                    .any(|p| p.r < 240 || p.g < 240 || p.b < 240),
                "the arrow itself must still be exported"
            );

            // Check that the same selection still decorates the editor view.
            let target = canvas
                .create_image_empty(128, 128, PixelFormat::Rgba8, ImageFlags::empty())
                .unwrap();
            canvas.set_render_target(RenderTarget::Image(target));
            canvas.reset_transform();
            inner
                .render(
                    canvas,
                    font,
                    false,
                    femtovg::Color::white(),
                    true,
                    true,
                    RenderTarget::Image(target),
                    Transform2D::identity(),
                    None,
                )
                .unwrap();
            let decorated = canvas.screenshot().unwrap();
            assert_ne!(
                decorated.buf(),
                selected.buf(),
                "editor selection should remain visible"
            );
            canvas.set_render_target(RenderTarget::Screen);
            canvas.delete_image(target);

            // Export the same background and source shadow that the editor uses.
            let mut moved = inner.drawables[0].drawable.clone_box();
            moved.translate(Vec2D::new(220.0, 220.0));
            let moved_id = inner.commit(moved);
            inner.auto_resize_for_drawables(&[moved_id]).unwrap();
            let expanded = inner.render_native_resolution(canvas, font).unwrap();
            for (x, y) in [(140, 30), (140, 140), (expanded.width() - 1, 20)] {
                let expected = read_pixel(&inner.background_image, x as i32, y as i32, false);
                let actual = expanded.buf()[y * expanded.stride() + x];
                assert_eq!((actual.r, actual.g, actual.b, actual.a), expected);
            }
        }
        window.close();
    }

    #[test]
    fn tile_ranges_covers_exactly_once() {
        for (total, limit) in [
            (1usize, 16384usize),
            (16384, 16384),
            (16385, 16384),
            (35011, 16384),
            (5000, 1024),
        ] {
            let ranges = tile_ranges(total, limit);
            let mut expected_start = 0;
            for (start, len) in &ranges {
                assert_eq!(*start, expected_start);
                assert!(*len > 0 && *len <= limit);
                expected_start = start + len;
            }
            assert_eq!(
                expected_start, total,
                "ranges must cover total for {total}/{limit}"
            );
        }
    }

    #[test]
    fn tile_ranges_zero_total_is_empty() {
        assert!(tile_ranges(0, 16384).is_empty());
    }

    #[test]
    fn tile_ranges_guards_zero_limit() {
        assert_eq!(tile_ranges(3, 0), vec![(0, 1), (1, 1), (2, 1)]);
    }

    /// `fill_rect` writes whole rows through the raw Pixbuf buffer, so
    /// pin down that it paints exactly the requested rect: correct
    /// colour inside, untouched outside, and no bleed into the
    /// neighbouring row (the failure mode if the row stride were
    /// mishandled). Runs for both a padded RGB stride and RGBA.
    #[test]
    fn fill_rect_paints_only_the_requested_rect() {
        for has_alpha in [false, true] {
            // Width 5 with 3 bytes per pixel gives a 15-byte row that
            // gdk-pixbuf pads to 16 — so the RGB case exercises a
            // stride wider than the pixel data.
            let p =
                Pixbuf::new(relm4::gtk::gdk_pixbuf::Colorspace::Rgb, has_alpha, 8, 5, 4).unwrap();
            p.fill(0x000000ff);
            super::fill_rect(&p, 1, 1, 3, 2, (10, 20, 30, 255));

            for y in 0..4 {
                for x in 0..5 {
                    let inside = (1..4).contains(&x) && (1..3).contains(&y);
                    let expected = if inside {
                        (10, 20, 30, 255)
                    } else {
                        (0, 0, 0, 255)
                    };
                    let got = super::read_pixel(&p, x, y, has_alpha);
                    assert_eq!(
                        (got.0, got.1, got.2),
                        (expected.0, expected.1, expected.2),
                        "has_alpha={has_alpha} at ({x},{y})"
                    );
                }
            }
        }
    }

    /// Out-of-bounds rects are dropped rather than corrupting the
    /// neighbouring row or running off the end of the buffer.
    #[test]
    fn fill_rect_rejects_out_of_bounds() {
        let p = Pixbuf::new(relm4::gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 4, 4).unwrap();
        p.fill(0x000000ff);
        super::fill_rect(&p, 3, 0, 4, 1, (255, 0, 0, 255)); // runs past the row
        super::fill_rect(&p, 0, 3, 1, 4, (255, 0, 0, 255)); // runs past the last line
        super::fill_rect(&p, -1, 0, 2, 1, (255, 0, 0, 255)); // negative origin
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(super::read_pixel(&p, x, y, false).0, 0, "at ({x},{y})");
            }
        }
    }

    /// `region_bytes` feeds GL directly, so a wrong offset shows up as
    /// a sheared or shifted patch of screenshot. Check it against a
    /// naive per-pixel reference for both the borrowing fast path
    /// (full width, no row padding) and the packing path (sub-width,
    /// and/or a padded stride).
    #[test]
    fn region_bytes_matches_a_naive_reference() {
        const BPP: usize = 3;
        let width = 7;
        let height = 5;
        for pad in [0usize, 2] {
            let stride = width * BPP + pad;
            // Distinct value per (x, y, channel) so any misalignment shows.
            let mut src = vec![0u8; stride * height];
            for y in 0..height {
                for x in 0..width {
                    for c in 0..BPP {
                        src[y * stride + x * BPP + c] = (y * 40 + x * 5 + c) as u8;
                    }
                }
            }
            for (x, y, w, h) in [
                (0, 0, width, height), // fast path when pad == 0
                (0, 1, width, 3),
                (2, 1, 4, 3),
                (width - 1, height - 1, 1, 1),
            ] {
                let layout = super::RasterLayout {
                    stride,
                    bytes_per_pixel: BPP,
                    width,
                };
                let got = super::region_bytes(&src, layout, x, y, w, h);
                let mut want = Vec::with_capacity(w * h * BPP);
                for row in y..y + h {
                    for col in x..x + w {
                        for c in 0..BPP {
                            want.push(src[row * stride + col * BPP + c]);
                        }
                    }
                }
                assert_eq!(&got[..], &want[..], "pad={pad} rect=({x},{y},{w},{h})");
            }
        }
    }

    /// The strips an incremental grow uploads must exactly cover the
    /// part of the new raster the old one doesn't, and must not overlap
    /// each other — a gap leaves stale background on screen, an overlap
    /// stacks redundant tiles. Verified by rasterising the strips onto
    /// a grid and comparing against the old raster's footprint.
    type Strips = Vec<(f32, f32, f32, f32)>;

    fn strip_coverage(
        t: Vec2D,
        old_w: i32,
        old_h: i32,
        new_w: i32,
        new_h: i32,
    ) -> (Vec<u8>, Strips) {
        let strips = super::background_grow_strips(
            t,
            old_w as f32,
            old_h as f32,
            new_w as f32,
            new_h as f32,
        );
        let mut hits = vec![0u8; (new_w * new_h) as usize];
        for &(x, y, w, h) in &strips {
            for py in (y as i32)..((y + h) as i32) {
                for px in (x as i32)..((x + w) as i32) {
                    if (0..new_w).contains(&px) && (0..new_h).contains(&py) {
                        hits[(py * new_w + px) as usize] += 1;
                    }
                }
            }
        }
        (hits, strips)
    }

    #[test]
    fn grow_strips_cover_exactly_the_new_region() {
        // (translation, old size, new size) — grow on each side, on
        // both, an asymmetric grow, and a grow-plus-shrink.
        let cases = [
            (Vec2D::new(0.0, 0.0), 10, 8, 14, 8),  // right only
            (Vec2D::new(0.0, 0.0), 10, 8, 10, 12), // bottom only
            (Vec2D::new(3.0, 0.0), 10, 8, 13, 8),  // left only
            (Vec2D::new(0.0, 2.0), 10, 8, 10, 10), // top only
            (Vec2D::new(3.0, 2.0), 10, 8, 16, 13), // all four
            (Vec2D::new(0.0, 2.0), 10, 8, 14, 11), // top + right
            (Vec2D::new(0.0, 0.0), 10, 8, 14, 6),  // grow x, shrink y
        ];
        for (t, ow, oh, nw, nh) in cases {
            let (hits, strips) = strip_coverage(t, ow, oh, nw, nh);
            for py in 0..nh {
                for px in 0..nw {
                    // Inside the old raster's new position => already on
                    // the GPU, must NOT be re-uploaded. Outside => must
                    // be uploaded exactly once.
                    let in_old = px >= t.x as i32
                        && px < t.x as i32 + ow
                        && py >= t.y as i32
                        && py < t.y as i32 + oh;
                    let want = if in_old { 0 } else { 1 };
                    assert_eq!(
                        hits[(py * nw + px) as usize],
                        want,
                        "({px},{py}) for {t:?} {ow}x{oh} -> {nw}x{nh}, strips {strips:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn grow_strips_are_empty_for_a_pure_shrink() {
        let (hits, strips) = strip_coverage(Vec2D::new(0.0, 0.0), 10, 8, 6, 5);
        assert!(strips.is_empty(), "{strips:?}");
        assert!(hits.iter().all(|h| *h == 0));
    }

    #[test]
    fn expanded_canvas_preserves_source_pixels_and_uses_a_neutral_background() {
        let (w, h, pad) = (80, 60, 100);
        for has_alpha in [false, true] {
            let src =
                Pixbuf::new(relm4::gtk::gdk_pixbuf::Colorspace::Rgb, has_alpha, 8, w, h).unwrap();
            for y in 0..h {
                for x in 0..w {
                    src.put_pixel(
                        x as u32,
                        y as u32,
                        (x * 3) as u8,
                        (y * 4) as u8,
                        90,
                        (x * 3) as u8,
                    );
                }
            }
            let out = resize_pixbuf_to_rect(
                &src,
                -pad,
                -pad,
                w + pad * 2,
                h + pad * 2,
                Default::default(),
            )
            .unwrap();
            for y in 0..out.height() {
                for x in 0..out.width() {
                    let pixel = super::read_pixel(&out, x, y, has_alpha);
                    if (pad..pad + w).contains(&x) && (pad..pad + h).contains(&y) {
                        assert_eq!(
                            pixel,
                            super::read_pixel(&src, x - pad, y - pad, has_alpha),
                            "every source pixel, including alpha, must be unchanged"
                        );
                    } else {
                        assert_eq!(pixel.0, pixel.1);
                        assert_eq!(pixel.1, pixel.2);
                        assert_eq!(pixel.3, 255, "the background must be opaque");
                        assert!(pixel.0 <= super::CANVAS_GRAY);
                        if x == 0 || y == 0 || x == out.width() - 1 || y == out.height() - 1 {
                            assert_eq!(
                                pixel,
                                super::CANVAS_COLOR,
                                "no shadow around the expanded canvas edge"
                            );
                        }
                    }
                }
            }
            let top = super::read_pixel(&out, pad + w / 2, pad - 1, has_alpha).0;
            let bottom = super::read_pixel(&out, pad + w / 2, pad + h, has_alpha).0;
            assert!(
                bottom < top && top < super::CANVAS_GRAY,
                "shadow belongs to the screenshot, with a downward offset"
            );
            let left = |distance| super::read_pixel(&out, pad - distance, pad + h / 2, has_alpha).0;
            assert!(
                left(1) < left(8) && left(8) < left(16) && left(16) < left(40),
                "shadow must soften into the neutral background"
            );
        }
    }

    #[test]
    fn canvas_background_is_independent_of_screenshot_colors() {
        let a = Pixbuf::new(relm4::gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 60, 60).unwrap();
        let b = a.copy().unwrap();
        a.fill(0xff0000ff);
        b.fill(0x00ffffff);
        let extend = |source: &Pixbuf| {
            resize_pixbuf_to_rect(source, -80, -80, 220, 220, Default::default()).unwrap()
        };
        let (a, b) = (extend(&a), extend(&b));
        for y in 0..220 {
            for x in 0..220 {
                if !(80..140).contains(&x) || !(80..140).contains(&y) {
                    assert_eq!(
                        super::read_pixel(&a, x, y, false),
                        super::read_pixel(&b, x, y, false)
                    );
                }
            }
        }
    }

    #[test]
    fn rotated_canvas_keeps_the_shadow_below_the_screenshot() {
        use super::*;
        let source = Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 60, 40).unwrap();
        source.fill(0x8090a0ff);
        let grown = resize_pixbuf_to_rect(&source, -20, -30, 120, 140, Default::default()).unwrap();
        let rotated = grown
            .rotate_simple(gtk::gdk_pixbuf::PixbufRotation::Counterclockwise)
            .unwrap();
        let rect = CanvasTransform::RotateCcw.map_rect(
            crate::math::Rect::new(Vec2D::new(20.0, 30.0), Vec2D::new(60.0, 40.0)),
            120.0,
            140.0,
        );
        repaint_background_margins(&rotated, rect);
        // A further grow must agree with placing the rotated original screenshot
        // onto a fresh background. No rotated shadow may remain in the old margins.
        let next = resize_pixbuf_to_rect(
            &rotated,
            -15,
            -10,
            170,
            140,
            CaptureInset::from_rect(rect, &rotated),
        )
        .unwrap();
        let source = source
            .rotate_simple(gtk::gdk_pixbuf::PixbufRotation::Counterclockwise)
            .unwrap();
        let reference = resize_pixbuf_to_rect(
            &source,
            -(rect.pos.x as i32) - 15,
            -(rect.pos.y as i32) - 10,
            170,
            140,
            Default::default(),
        )
        .unwrap();
        for y in 0..next.height() {
            for x in 0..next.width() {
                assert_eq!(
                    read_pixel(&next, x, y, false),
                    read_pixel(&reference, x, y, false)
                );
            }
        }
    }

    #[test]
    fn canvas_background_is_independent_of_resize_steps() {
        for has_alpha in [false, true] {
            let (w, h) = (64, 48);
            let src =
                Pixbuf::new(relm4::gtk::gdk_pixbuf::Colorspace::Rgb, has_alpha, 8, w, h).unwrap();
            for y in 0..h {
                for x in 0..w {
                    src.put_pixel(x as u32, y as u32, (x * 3) as u8, (y * 4) as u8, 40, 200);
                }
            }
            let mut view = src.clone();
            let mut alloc = None;
            let mut origin = (0, 0);
            let mut inset = super::CaptureInset::default();
            // Small asymmetric grows, shrink/regrow, then an allocation change.
            for (left, top, right, bottom) in [
                (1, 1, 1, 1),
                (5, 1, 9, 1),
                (5, 13, 9, 11),
                (33, 25, 37, 29),
                (9, 6, 10, 7),
                (320, 280, 300, 290),
            ] {
                let (next, backing, next_origin) = super::resize_raster_in_alloc(
                    &view,
                    alloc.as_ref(),
                    origin,
                    inset.left - left,
                    inset.top - top,
                    w + left + right,
                    h + top + bottom,
                    inset,
                )
                .unwrap();
                let once = resize_pixbuf_to_rect(
                    &src,
                    -left,
                    -top,
                    next.width(),
                    next.height(),
                    Default::default(),
                )
                .unwrap();
                for y in 0..next.height() {
                    for x in 0..next.width() {
                        assert_eq!(
                            super::read_pixel(&next, x, y, has_alpha),
                            super::read_pixel(&once, x, y, has_alpha),
                            "resize history changed pixel ({x},{y}), padding {left},{top},{right},{bottom}, alpha={has_alpha}"
                        );
                    }
                }
                view = next;
                alloc = Some(backing);
                origin = next_origin;
                inset = super::CaptureInset {
                    left,
                    top,
                    right,
                    bottom,
                };
            }
        }
    }

    /// The view-based resize must produce exactly the pixels the
    /// straightforward allocate-and-copy resize would, across a run of
    /// resizes that exercises both its paths: ones that fit the
    /// existing allocation (no copy) and ones that outgrow it (realloc
    /// + copy), interleaved with shrinks that move the view back.
    #[test]
    fn view_resize_matches_copy_resize() {
        let (w, h) = (90, 70);
        let src = Pixbuf::new(relm4::gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, w, h).unwrap();
        for y in 0..h {
            for x in 0..w {
                src.put_pixel(
                    x as u32,
                    y as u32,
                    (x * 2) as u8,
                    (y * 3) as u8,
                    ((x + y) * 5) as u8,
                    255,
                );
            }
        }

        // (src_x, src_y, dw, dh) applied in sequence. The pad is 256, so
        // the small steps re-view and the big one forces a realloc.
        let steps = [
            (-3, -2, 5, 4),         // grow all sides a little
            (0, 0, 7, 0),           // grow right only
            (-4, 0, 4, 6),          // grow left and bottom
            (2, 1, -2, -1),         // shrink back in
            (-400, -300, 800, 600), // outgrows the allocation
            (-2, -2, 4, 4),         // small again, in the fresh allocation
        ];

        let mut view = src.clone();
        let mut reference = src.clone();
        let mut alloc: Option<Pixbuf> = None;
        let mut origin = (0, 0);
        let mut inset = super::CaptureInset::default();

        for (i, (sx, sy, dw, dh)) in steps.into_iter().enumerate() {
            let (nw, nh) = (view.width() + dw, view.height() + dh);
            let (next_view, next_alloc, next_origin) =
                super::resize_raster_in_alloc(&view, alloc.as_ref(), origin, sx, sy, nw, nh, inset)
                    .unwrap();
            let next_reference =
                super::resize_pixbuf_to_rect(&reference, sx, sy, nw, nh, inset).unwrap();

            assert_eq!(
                (next_view.width(), next_view.height()),
                (next_reference.width(), next_reference.height()),
                "step {i} dimensions"
            );
            for y in 0..next_reference.height() {
                for x in 0..next_reference.width() {
                    assert_eq!(
                        super::read_pixel(&next_view, x, y, false),
                        super::read_pixel(&next_reference, x, y, false),
                        "step {i} at ({x},{y})"
                    );
                }
            }

            view = next_view;
            alloc = Some(next_alloc);
            origin = next_origin;
            reference = next_reference;
            inset.left -= sx;
            inset.top -= sy;
            inset.right += sx + dw;
            inset.bottom += sy + dh;
        }
    }

    /// Repeated nudges must expose one continuous background. Every margin
    /// pixel is based on the original screenshot rectangle, never the last grow.
    #[test]
    fn successive_grows_leave_no_step_between_strips() {
        let (w, h) = (80, 60);
        let src = Pixbuf::new(relm4::gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, w, h).unwrap();
        for y in 0..h {
            for x in 0..w {
                let v: u8 = if x < 8 || y < 8 { 20 } else { 220 };
                src.put_pixel(x as u32, y as u32, v, v, v, 255);
            }
        }

        let mut view = src.clone();
        let mut alloc: Option<Pixbuf> = None;
        let mut origin = (0, 0);
        // Mirrors what the renderer passes: how far the raster already
        // reaches past the capture, which grows with every step.
        let mut inset = super::CaptureInset::default();
        const STEP: i32 = 20;
        const GROWS: i32 = 40;
        for _ in 0..GROWS {
            let (v, a, o) = super::resize_raster_in_alloc(
                &view,
                alloc.as_ref(),
                origin,
                -STEP,
                -STEP,
                view.width() + STEP,
                view.height() + STEP,
                inset,
            )
            .unwrap();
            view = v;
            alloc = Some(a);
            origin = o;
            inset.left += STEP;
            inset.top += STEP;
        }

        // Beyond the screenshot shadow, every strip must be the same color.
        let far = GROWS * STEP - super::SHADOW_EXTENT - STEP;
        assert!(far > STEP * 2, "test must span several strips");
        let first = super::read_pixel(&view, 0, 0, false);
        for y in 0..far {
            for x in 0..view.width() {
                assert_eq!(
                    super::read_pixel(&view, x, y, false),
                    first,
                    "step in the top extension at ({x},{y})"
                );
            }
        }
        for y in 0..view.height() {
            for x in 0..far {
                assert_eq!(
                    super::read_pixel(&view, x, y, false),
                    first,
                    "step in the left extension at ({x},{y})"
                );
            }
        }
    }

    /// An undo snapshot is a view of the same allocation as the raster
    /// that superseded it, so a later grow must not disturb it — it
    /// writes only outside the region the snapshot covers.
    #[test]
    fn resize_leaves_earlier_views_intact() {
        let (w, h) = (40, 30);
        let src = Pixbuf::new(relm4::gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, w, h).unwrap();
        src.fill(0x11223300);

        let (first, alloc, origin) = super::resize_raster_in_alloc(
            &src,
            None,
            (0, 0),
            -2,
            -2,
            w + 4,
            h + 4,
            Default::default(),
        )
        .unwrap();
        let snapshot: Vec<_> = (0..first.height())
            .flat_map(|y| (0..first.width()).map(move |x| (x, y)))
            .map(|(x, y)| super::read_pixel(&first, x, y, false))
            .collect();

        // Grow again from the same allocation.
        let (_second, _alloc2, _origin2) = super::resize_raster_in_alloc(
            &first,
            Some(&alloc),
            origin,
            -5,
            -5,
            first.width() + 10,
            first.height() + 10,
            super::CaptureInset {
                left: 2,
                top: 2,
                right: 2,
                bottom: 2,
            },
        )
        .unwrap();

        let after: Vec<_> = (0..first.height())
            .flat_map(|y| (0..first.width()).map(move |x| (x, y)))
            .map(|(x, y)| super::read_pixel(&first, x, y, false))
            .collect();
        assert_eq!(snapshot, after, "the earlier view was written through");
    }

    /// Checksum of a grown raster, for confirming that a refactor of
    /// the grow path left the produced pixels bit-identical.
    ///
    ///   TENSAKU_BENCH_IMAGE=/path/to.png \
    ///     cargo test --release grow_raster_digest -- --ignored --nocapture
    #[test]
    #[ignore = "manual; needs TENSAKU_BENCH_IMAGE"]
    fn grow_raster_digest() {
        let path = std::env::var("TENSAKU_BENCH_IMAGE")
            .expect("set TENSAKU_BENCH_IMAGE to a capture to grow");
        let src = Pixbuf::from_file(&path).expect("loading capture");
        let (w, h) = (src.width(), src.height());
        // Grow on all four sides so every fill_rect branch runs,
        // corners included.
        let pad: i32 = std::env::var("TENSAKU_BENCH_PAD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200);
        let out = resize_pixbuf_to_rect(
            &src,
            -pad,
            -pad * 3 / 4,
            w + 2 * pad,
            h + pad * 3 / 2,
            Default::default(),
        )
        .unwrap();
        let digest = unsafe {
            out.pixels().iter().fold(1469598103934665603u64, |acc, b| {
                (acc ^ *b as u64).wrapping_mul(1099511628211)
            })
        };
        println!(
            "grow_raster_digest {}x{} -> {}x{}: {:016x}",
            w,
            h,
            out.width(),
            out.height(),
            digest
        );
        if let Ok(dest) = std::env::var("TENSAKU_BENCH_OUT") {
            out.savev(&dest, "png", &[]).expect("writing grown raster");
            println!("wrote {dest}");
        }
    }

    /// Manual benchmark for the canvas auto-grow raster rebuild.
    ///
    /// `auto_resize_for_drawables` runs this on every drag motion
    /// event once a drawable's bounds spill past the image, so its
    /// cost — which scales with the capture's pixel count — is felt
    /// as drag lag on large displays. Run with the capture size you
    /// want to characterise:
    ///
    ///   TENSAKU_BENCH_W=6016 TENSAKU_BENCH_H=3384 \
    ///     cargo test --release grow_bench -- --ignored --nocapture
    #[test]
    #[ignore = "manual benchmark; prints timings"]
    fn resize_pixbuf_to_rect_grow_bench() {
        let w: i32 = std::env::var("TENSAKU_BENCH_W")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(6016);
        let h: i32 = std::env::var("TENSAKU_BENCH_H")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3384);
        let src = Pixbuf::new(relm4::gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, w, h).unwrap();
        src.fill(0x304050ff);
        // A drawable that has spilled 40 px past the right and bottom
        // edges — the shape of a grow triggered mid-drag.
        let start = std::time::Instant::now();
        let out = resize_pixbuf_to_rect(&src, 0, 0, w + 40, h + 40, Default::default()).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(out.width(), w + 40);
        println!(
            "resize_pixbuf_to_rect {w}x{h} -> +40px: {:.1} ms",
            elapsed.as_secs_f64() * 1000.0
        );

        // Breakdown, so the optimisation targets the part that costs.
        let t = |label: &str, f: &dyn Fn()| {
            let start = std::time::Instant::now();
            f();
            println!(
                "    {label}: {:.1} ms",
                start.elapsed().as_secs_f64() * 1000.0
            );
        };
        t("Pixbuf::new + fill", &|| {
            let n = Pixbuf::new(
                relm4::gtk::gdk_pixbuf::Colorspace::Rgb,
                false,
                8,
                w + 40,
                h + 40,
            )
            .unwrap();
            n.fill(0x000000ff);
        });
        let dst = Pixbuf::new(
            relm4::gtk::gdk_pixbuf::Colorspace::Rgb,
            false,
            8,
            w + 40,
            h + 40,
        )
        .unwrap();
        t("copy_area", &|| src.copy_area(0, 0, w, h, &dst, 0, 0));
        // The two resize paths at capture scale: one that has to
        // allocate and copy, and one that re-views an allocation it
        // already has.
        let (view, alloc, origin) = super::resize_raster_in_alloc(
            &src,
            None,
            (0, 0),
            -4,
            -4,
            w + 8,
            h + 8,
            Default::default(),
        )
        .unwrap();
        let start = std::time::Instant::now();
        let (view2, alloc2, origin2) = super::resize_raster_in_alloc(
            &view,
            Some(&alloc),
            origin,
            -4,
            -4,
            w + 16,
            h + 16,
            Default::default(),
        )
        .unwrap();
        println!(
            "    resize_raster (re-view, no copy): {:.1} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
        let start = std::time::Instant::now();
        let _ = super::resize_raster_in_alloc(
            &view2,
            Some(&alloc2),
            origin2,
            -600,
            -600,
            view2.width() + 1200,
            view2.height() + 1200,
            Default::default(),
        )
        .unwrap();
        println!(
            "    resize_raster (outgrows alloc):   {:.1} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );

        t("fill right+bottom strips", &|| {
            super::extend_background(
                &dst,
                &src,
                super::ResizeLayout::new(0, 0, w + 40, h + 40, w, h),
                Default::default(),
            );
        });
    }
}
