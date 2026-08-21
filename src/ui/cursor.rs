//! Custom drawing-tool cursors. Both Brush and Highlighter use a
//! "double ring" cursor (dark outer, light inner) so the cursor stays
//! readable on any background — same reasoning as Mac apps using a
//! drawing cursors.
//!
//! - Brush: circular, diameter = stroke line width. Tells the user
//!   exactly how thick the next pen stroke will be.
//! - Highlighter: vertical capsule (chisel tip) whose outer height —
//!   the top of one rounded cap to the top of the other — equals the
//!   width of the highlight stripe that gets laid down. The cursor's
//!   width is a thin marker-tip value so it still reads as a chisel
//!   rather than a fat capsule.
//!
//! On HiDPI displays, GTK4 paints cursor textures at a larger
//! on-screen size than their texture pixel count would suggest, so the
//! cursor builders divide by DPR to compensate — without this, a
//! 30 px highlight stroke rendered at 2x DPR drew at 30 CSS px on
//! screen while the cursor texture (30 px) showed at 60 CSS px, and
//! the highlight came out at ~60 % of the cursor.
//!
//! Cursors are recreated whenever the relevant style inputs change
//! (size, annotation_size_factor); neither cursor encodes color, so
//! changing the picker color does not require regeneration.

use relm4::gtk::cairo;
use relm4::gtk::gdk;
use relm4::gtk::gdk_pixbuf::Pixbuf;
use std::f64::consts::PI;

use crate::style::{Size, Style};

/// Padding around the cursor shape, in pixels, to leave room for the
/// outer ring stroke without it being clipped at the texture edge.
const RING_PAD: f64 = 2.0;

/// Outer ring stroke width — slightly thicker than the inner ring so
/// the dark outline reads clearly on light backgrounds.
const OUTER_LINE_WIDTH: f64 = 1.6;
const INNER_LINE_WIDTH: f64 = 1.0;

/// Don't render cursors smaller than this — a 2px wide cursor would
/// be invisible after the rings are drawn. Floors XSmall to a
/// reasonable minimum.
const MIN_CURSOR_PX: f64 = 8.0;

/// How far down-right the Counter's arrow sits from the point it
/// marks, so it clears the digit centered there. Big enough to
/// uncover the number at the sizes people actually use, small enough
/// that the arrow still reads as pointing at the badge.
const POINTER_DODGE: f64 = 14.0;

/// Arrow art extents, from its tip.
const ARROW_W: f64 = 10.6;
const ARROW_H: f64 = 17.4;


/// Build a circular double-ring cursor for the Brush tool. Diameter
/// matches the brush's stroke line width AS RENDERED on screen —
/// `render_scale` is the renderer's image→canvas multiplier, and
/// `device_pixel_ratio` divides out the extra scaling GTK4 applies
/// to cursor textures on HiDPI surfaces (without this, a HiDPI
/// cursor reads ~DPR× larger than the stroke that comes out of it).
pub fn build_brush_cursor(
    style: &Style,
    render_scale: f64,
    device_pixel_ratio: f64,
) -> Option<gdk::Cursor> {
    let dpr = device_pixel_ratio.max(1.0);
    let diameter =
        style.size.to_line_width(style.annotation_size_factor) as f64 * render_scale / dpr;
    let diameter = diameter.max(MIN_CURSOR_PX);
    // Brush cursor is always centered on the pointer; no vertical
    // hotspot offset.
    build_double_ring_cursor(diameter, diameter, 0.0)
}

/// Build a vertical-capsule (chisel-tip) double-ring cursor for the
/// Highlighter tool. The capsule's outer HEIGHT — from the top of one
/// rounded cap to the top of the other — equals the highlight stroke
/// width that will be laid down. Width is a proportionally narrow
/// marker-tip value (~1/6 the height, floored at 4 px so XSmall
/// doesn't degenerate to a vertical line).
///
/// `render_scale` is the image→canvas multiplier (zoom); `device_pixel_ratio`
/// undoes the on-screen upscaling GTK4 applies to cursor textures on
/// HiDPI surfaces. When `band_height_image_px` is `Some`, the cursor's
/// height comes from the detected text band under the pointer instead
/// of the style's size — that's the "smart highlighter" preview that
/// shows the user what a click here would highlight. The value is in
/// IMAGE pixels (matching `style.size.to_highlight_width`'s units) so
/// the same `* render_scale / dpr` conversion applies.
pub fn build_highlighter_cursor(
    style: &Style,
    render_scale: f64,
    device_pixel_ratio: f64,
    band_height_image_px: Option<f32>,
    band_vertical_offset_image_px: f32,
) -> Option<gdk::Cursor> {
    let dpr = device_pixel_ratio.max(1.0);
    let style_height = style.size.to_highlight_width(style.annotation_size_factor) as f64;
    let base_height = band_height_image_px
        .map(|h| h as f64)
        .unwrap_or(style_height);
    let height = (base_height * render_scale / dpr).max(MIN_CURSOR_PX);
    // Vertical hotspot offset: move the cursor texture so its visual
    // center sits at the band's CENTER on screen, not at the
    // pointer's position. Without this, hovering anywhere inside a
    // band (e.g. near its top edge) renders the cursor centered on
    // the pointer — so the preview drifts above the text it's
    // supposed to cover. The offset is (band.center_y - pointer_y)
    // in image pixels; convert to cursor-texture pixels via the same
    // `render_scale / dpr` mapping the height uses.
    let hotspot_offset_tex_px = if band_height_image_px.is_some() {
        band_vertical_offset_image_px as f64 * render_scale / dpr
    } else {
        0.0
    };
    if band_height_image_px.is_some() {
        // Text-locked context — the cursor is an I-beam scaled to
        // the band's height, with the bar's hotspot anchored to the
        // band's center. Reads as a "text-selection" cursor that
        // matches the snap-to-text-row behavior of the tool. Falls
        // back to the chisel capsule when no band is detected
        // (TextLocked mode hovering over non-text) so the user can
        // still see the freehand size that would apply on click.
        build_ibeam_cursor(height, hotspot_offset_tex_px)
    } else {
        // Marker-tip width: about a sixth of the height, but never
        // narrower than 4 px so smaller sizes still read as a tip
        // rather than a vertical line. For XSmall the floor wins;
        // for XXLarge we get ~15 px.
        let width = (height / 6.0).max(4.0).min(height);
        build_double_ring_cursor(width, height, hotspot_offset_tex_px)
    }
}

/// The Counter's pointer, nudged clear of its own preview.
///
/// The ghost badge is centered on the click point with the number at
/// its middle, so a cursor drawn AT that point covers exactly the
/// thing the preview exists to show. The hotspot stays on the true
/// point — only the arrow art moves down and right, out of the
/// digit's way.
pub fn build_offset_pointer_cursor(device_pixel_ratio: f64) -> Option<gdk::Cursor> {
    let dpr = device_pixel_ratio.max(1.0);
    // GTK4 paints cursor textures at a DPR-scaled on-screen size, so
    // everything here is divided down to keep the arrow the same
    // apparent size (and the same apparent nudge) on any display.
    let scale = 1.0 / dpr;
    let dodge = POINTER_DODGE * scale;
    let total_w = ((dodge + ARROW_W * scale).ceil() + RING_PAD) as i32;
    let total_h = ((dodge + ARROW_H * scale).ceil() + RING_PAD) as i32;

    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, total_w, total_h).ok()?;
    let ctx = cairo::Context::new(&surface).ok()?;
    draw_arrow_at(&ctx, dodge, dodge, scale);
    drop(ctx);

    let pixbuf: Pixbuf = gdk::pixbuf_get_from_surface(&surface, 0, 0, total_w, total_h)?;
    let texture = gdk::Texture::for_pixbuf(&pixbuf);
    // Hotspot at the texture's corner: the click point itself carries
    // no art, because the badge under it is the art.
    Some(gdk::Cursor::from_texture(&texture, 0, 0, None))
}

/// Trace the classic arrow pointer with its tip at `(x, y)`, painted
/// light-on-dark so it stays legible over the badge it sits beside.
fn draw_arrow_at(ctx: &cairo::Context, x: f64, y: f64, scale: f64) {
    const POINTS: [(f64, f64); 7] = [
        (0.0, 0.0),
        (0.0, 15.4),
        (3.6, 12.0),
        (6.1, 17.4),
        (8.5, 16.3),
        (5.9, 11.1),
        (10.6, 10.7),
    ];
    ctx.new_path();
    for (i, (dx, dy)) in POINTS.iter().enumerate() {
        if i == 0 {
            ctx.move_to(x + dx * scale, y + dy * scale);
        } else {
            ctx.line_to(x + dx * scale, y + dy * scale);
        }
    }
    ctx.close_path();
    ctx.set_source_rgba(1.0, 1.0, 1.0, 0.98);
    let _ = ctx.fill_preserve();
    ctx.set_source_rgba(0.0, 0.0, 0.0, 0.85);
    ctx.set_line_width(1.3 * scale.max(0.7));
    let _ = ctx.stroke();
}

/// Build a thick I-beam (text-selection style) cursor scaled to
/// `height` texture pixels tall. Used for the Highlighter's
/// Text-locked mode so the cursor reads as "select this line of
/// text" — mirrors the OS text-edit cursor at the band's measured
/// height, with the band's center mapping to the cursor's vertical
/// center (and `hotspot_y_offset_tex_px` shifting that to the
/// band's actual on-screen y).
///
/// The shape is a vertical spine plus top + bottom serifs, traced as
/// one closed path so a single stroke pass produces the full
/// outline. Outer dark + inner light stroke (same double-ring trick
/// the capsule cursor uses) keeps the I-beam legible on any
/// background.
fn build_ibeam_cursor(height: f64, hotspot_y_offset_tex_px: f64) -> Option<gdk::Cursor> {
    // I-beam geometry. Spine is thick enough to read as a "thick"
    // cursor (per the user's request); serifs extend a few pixels
    // either side. All scaled mildly with height so very tall text
    // bands get proportionally bigger serifs.
    let half_h = height / 2.0;
    let spine_half_w = 1.5_f64;
    let serif_half_w = (height * 0.18).clamp(5.0, 9.0);
    let serif_h = (height * 0.08).clamp(2.0, 4.0);

    let total_w = (serif_half_w * 2.0 + RING_PAD * 2.0).ceil() as i32;
    let total_h = (height + RING_PAD * 2.0).ceil() as i32;
    if total_w > 128 || total_h > 128 {
        return None;
    }
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, total_w, total_h).ok()?;
    let ctx = cairo::Context::new(&surface).ok()?;

    let cx = total_w as f64 / 2.0;
    let cy = total_h as f64 / 2.0;
    let top = cy - half_h;
    let bot = cy + half_h;
    let inner_top = top + serif_h;
    let inner_bot = bot - serif_h;

    // Trace the I-beam outline clockwise as a closed polygon: top
    // serif → step in to spine → spine → step out to bottom serif →
    // bottom serif → mirror back up. Order matters for join
    // continuity in the stroke pass.
    ctx.move_to(cx - serif_half_w, top);
    ctx.line_to(cx + serif_half_w, top);
    ctx.line_to(cx + serif_half_w, inner_top);
    ctx.line_to(cx + spine_half_w, inner_top);
    ctx.line_to(cx + spine_half_w, inner_bot);
    ctx.line_to(cx + serif_half_w, inner_bot);
    ctx.line_to(cx + serif_half_w, bot);
    ctx.line_to(cx - serif_half_w, bot);
    ctx.line_to(cx - serif_half_w, inner_bot);
    ctx.line_to(cx - spine_half_w, inner_bot);
    ctx.line_to(cx - spine_half_w, inner_top);
    ctx.line_to(cx - serif_half_w, inner_top);
    ctx.close_path();

    // White fill so the cursor body is visible on dark backgrounds;
    // dark outline gives contrast on light. Matches the double-ring
    // capsule's "readable on any background" intent.
    ctx.set_source_rgba(1.0, 1.0, 1.0, 0.95);
    let _ = ctx.fill_preserve();
    ctx.set_source_rgba(0.0, 0.0, 0.0, 0.85);
    ctx.set_line_width(OUTER_LINE_WIDTH);
    let _ = ctx.stroke();

    drop(ctx);

    let pixbuf: Pixbuf = gdk::pixbuf_get_from_surface(&surface, 0, 0, total_w, total_h)?;
    let texture = gdk::Texture::for_pixbuf(&pixbuf);
    let hot_x = total_w / 2;
    let hot_y_raw = (total_h as f64 / 2.0 - hotspot_y_offset_tex_px).round() as i32;
    let hot_y = hot_y_raw.clamp(0, total_h - 1);
    Some(gdk::Cursor::from_texture(&texture, hot_x, hot_y, None))
}

/// Render a capsule (rounded rectangle with full-end semicircles) of
/// the given pixel width and height with the dark+light double ring
/// applied as outline strokes. Returns a `gdk::Cursor` with hotspot
/// at the geometric center, optionally shifted vertically by
/// `hotspot_y_offset_tex_px` so the cursor renders anchored above /
/// below the pointer position (positive = cursor appears below
/// pointer). Used by the highlighter to align its preview capsule to
/// a detected text band's center rather than to the pointer.
fn build_double_ring_cursor(
    width: f64,
    height: f64,
    hotspot_y_offset_tex_px: f64,
) -> Option<gdk::Cursor> {
    let total_w = (width + RING_PAD * 2.0).ceil() as i32;
    let total_h = (height + RING_PAD * 2.0).ceil() as i32;

    // GTK / GDK refuses huge cursors silently on some compositors.
    // Cap the dimensions so XXLarge highlighter (~90px) plus padding
    // doesn't exceed typical 128px cursor support.
    if total_w > 128 || total_h > 128 {
        return None;
    }

    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, total_w, total_h).ok()?;
    let ctx = cairo::Context::new(&surface).ok()?;

    let cx = total_w as f64 / 2.0;
    let cy = total_h as f64 / 2.0;
    let half_w = width / 2.0;
    let half_h = height / 2.0;
    // Capsule rounding radius = the smaller half-dimension. For a
    // square (width == height), this naturally degenerates into a
    // full circle — the brush case.
    let r = half_w.min(half_h);

    // Outer ring (dark) drawn first; inner light ring overlays it.
    draw_capsule_path(&ctx, cx, cy, half_w, half_h, r);
    ctx.set_source_rgba(0.0, 0.0, 0.0, 0.65);
    ctx.set_line_width(OUTER_LINE_WIDTH);
    let _ = ctx.stroke_preserve();

    // Inner ring stroked along the same path so the two rings are
    // perfectly concentric. Cairo strokes are centered on the path,
    // so a thinner light ring renders inside the dark one.
    ctx.set_source_rgba(1.0, 1.0, 1.0, 0.95);
    ctx.set_line_width(INNER_LINE_WIDTH);
    let _ = ctx.stroke();

    drop(ctx);

    let pixbuf: Pixbuf = gdk::pixbuf_get_from_surface(&surface, 0, 0, total_w, total_h)?;
    let texture = gdk::Texture::for_pixbuf(&pixbuf);
    // Hotspot Y starts at the geometric center; the caller can
    // push it up so the rendered cursor lands BELOW the pointer
    // (a positive offset means "show the cursor `offset` px below
    // the pointer"). Clamp inside the texture bounds — GDK rejects
    // hotspots outside the texture and would fall back to the
    // default cursor silently.
    let hot_x = total_w / 2;
    let hot_y_raw = (total_h as f64 / 2.0 - hotspot_y_offset_tex_px).round() as i32;
    let hot_y = hot_y_raw.clamp(0, total_h - 1);
    Some(gdk::Cursor::from_texture(&texture, hot_x, hot_y, None))
}

/// Append a capsule (rounded rectangle with semicircular caps) to the
/// cairo context's current path. Geometry is centered on `(cx, cy)`
/// with the given half-width and half-height; `r` is the corner
/// radius (clamped to half-w for cap fullness).
fn draw_capsule_path(ctx: &cairo::Context, cx: f64, cy: f64, half_w: f64, half_h: f64, r: f64) {
    let r = r.min(half_w).min(half_h);
    let left = cx - half_w;
    let right = cx + half_w;
    let top = cy - half_h;
    let bottom = cy + half_h;

    ctx.new_path();
    // Top semicircle
    ctx.arc(cx, top + r, r, PI, 2.0 * PI);
    // Right edge (only if there's a flat region — for a circle we
    // skip directly to the bottom semicircle)
    if (half_h - r).abs() > 0.001 {
        ctx.line_to(right, bottom - r);
    }
    // Bottom semicircle
    ctx.arc(cx, bottom - r, r, 0.0, PI);
    if (half_h - r).abs() > 0.001 {
        ctx.line_to(left, top + r);
    }
    ctx.close_path();
}

/// Convenience: pick the right cursor builder for the given tool +
/// style. Returns `None` for tools that should keep their existing
/// system cursor. `band_height_image_px` is the height (in image
/// pixels) of the text band currently under the pointer — only
/// honored by the Highlighter cursor, where it replaces the
/// style-derived height so the cursor previews what a click here
/// would highlight. `None` (no band, or non-highlighter tool) keeps
/// the regular style-driven sizing.
pub fn drawing_tool_cursor(
    tool: crate::tools::Tools,
    style: &Style,
    render_scale: f64,
    device_pixel_ratio: f64,
    band_height_image_px: Option<f32>,
    band_vertical_offset_image_px: f32,
) -> Option<gdk::Cursor> {
    use crate::tools::Tools;
    match tool {
        Tools::Brush => build_brush_cursor(style, render_scale, device_pixel_ratio),
        Tools::Highlighter => build_highlighter_cursor(
            style,
            render_scale,
            device_pixel_ratio,
            band_height_image_px,
            band_vertical_offset_image_px,
        ),
        Tools::Marker => build_offset_pointer_cursor(device_pixel_ratio),
        _ => None,
    }
}

// Suppress "unused" if Size is referenced only for the public API.
#[allow(dead_code)]
fn _force_use_size(_: Size) {}

#[cfg(test)]
mod pointer_cursor_tests {
    use super::{ARROW_H, ARROW_W, POINTER_DODGE, RING_PAD, draw_arrow_at};
    use relm4::gtk::cairo;

    /// Dump the offset pointer for eyeballing. Ignored — it writes a
    /// file and proves nothing on its own.
    #[test]
    #[ignore]
    fn dump_offset_pointer() {
        let dir = std::env::var("TENSAKU_CURSOR_DUMP_DIR").unwrap_or("/tmp".to_string());
        let w = (POINTER_DODGE + ARROW_W + RING_PAD).ceil() as i32;
        let h = (POINTER_DODGE + ARROW_H + RING_PAD).ceil() as i32;
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).unwrap();
        let ctx = cairo::Context::new(&surface).unwrap();
        draw_arrow_at(&ctx, POINTER_DODGE, POINTER_DODGE, 1.0);
        drop(ctx);
        let mut surface = surface;
        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        let mut raw = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h as usize {
            raw.extend_from_slice(&data[row * stride..row * stride + (w as usize) * 4]);
        }
        std::fs::write(format!("{dir}/pointer-{w}x{h}.bgra"), &raw).unwrap();
    }

    /// Nothing may be painted in the square between the hotspot and
    /// the arrow's tip — that square is where the badge's number
    /// sits, and covering it defeats the whole preview.
    #[test]
    fn the_arrow_leaves_the_hotspot_corner_bare() {
        let side = (POINTER_DODGE + ARROW_W + RING_PAD).ceil() as i32;
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, side, side).unwrap();
        let ctx = cairo::Context::new(&surface).unwrap();
        draw_arrow_at(&ctx, POINTER_DODGE, POINTER_DODGE, 1.0);
        drop(ctx);
        let mut surface = surface;
        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        // A digit-sized square anchored at the hotspot, minus a pixel
        // of slack for the arrow outline's antialiasing.
        let clear = (POINTER_DODGE as usize).saturating_sub(1);
        for y in 0..clear {
            for x in 0..clear {
                let alpha = data[y * stride + x * 4 + 3];
                assert_eq!(alpha, 0, "pixel ({x}, {y}) covers the badge's number");
            }
        }
    }
}
