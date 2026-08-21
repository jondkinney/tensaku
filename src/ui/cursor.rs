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

/// How far the arrow pointer reaches from its tip, used to size the
/// counter cursor's texture when the badge itself is smaller.
const ARROW_EXTENT: f64 = 18.0;

/// Below this the digit is a smudge rather than a number, so the badge
/// shows as a plain disc — still the right color and size, just without
/// an illegible glyph in it.
const MIN_DIGIT_PX: f64 = 9.0;

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

/// Build the Counter tool's cursor: a preview of the badge a click
/// would stamp — its real color, its real size, and the number that is
/// actually next — centered on the pointer, because that is where the
/// badge lands. A crosshair said "you are about to click"; this says
/// what, and where.
///
/// The arrow's tip sits at that same center rather than beside the
/// badge, so the two halves of the cursor can't disagree about the
/// placement point. The badge is drawn translucent and the arrow
/// carries its own light-on-dark outline, so the digit stays readable
/// underneath it.
///
/// Returns `None` when the badge would exceed GDK's cursor size limit
/// (a very large counter, or a zoomed-in canvas), leaving the caller
/// on the stock crosshair.
pub fn build_marker_cursor(
    style: &Style,
    number: u16,
    render_scale: f64,
    device_pixel_ratio: f64,
) -> Option<gdk::Cursor> {
    let surface = render_marker_cursor(style, number, render_scale, device_pixel_ratio)?;
    let total = surface.width();
    let pixbuf: Pixbuf = gdk::pixbuf_get_from_surface(&surface, 0, 0, total, total)?;
    let texture = gdk::Texture::for_pixbuf(&pixbuf);
    let hot = total / 2;
    Some(gdk::Cursor::from_texture(&texture, hot, hot, None))
}

/// Paint the counter cursor onto a fresh surface. Split from
/// `build_marker_cursor` so a test can render it and inspect the
/// pixels without a display server in the loop.
fn render_marker_cursor(
    style: &Style,
    number: u16,
    render_scale: f64,
    device_pixel_ratio: f64,
) -> Option<cairo::ImageSurface> {
    let dpr = device_pixel_ratio.max(1.0);
    // Same image→cursor-texture mapping the other builders use, so the
    // preview tracks the zoom the badge will actually be drawn at.
    let scale = render_scale / dpr;
    let text_size_img = crate::tools::marker_text_size(style.size, style.annotation_size_factor, 1.0);
    let radius = crate::tools::marker_radius(text_size_img, number) as f64 * scale;
    let text_px = text_size_img as f64 * scale;

    // Shrink the arrow on a small badge. At full size it swallows a
    // 20 px disc whole, and the number — the entire point of the
    // preview — disappears under it.
    let arrow_scale = (radius / 26.0).clamp(0.55, 1.0);
    // The texture is square and the hotspot is its center, so it has to
    // hold whichever reaches further from that center: the badge, or
    // the arrow hanging off toward the lower right.
    let half = (radius + RING_PAD).max(ARROW_EXTENT * arrow_scale + RING_PAD);
    let total = (half * 2.0).ceil() as i32;
    if total > 128 {
        return None;
    }

    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, total, total).ok()?;
    let ctx = cairo::Context::new(&surface).ok()?;
    let c = total as f64 / 2.0;

    let (r, g, b) = (
        style.color.r as f64 / 255.0,
        style.color.g as f64 / 255.0,
        style.color.b as f64 / 255.0,
    );

    // Disc, translucent so the arrow on top of it stays legible while
    // the color still reads as the color you picked.
    ctx.new_path();
    ctx.arc(c, c, radius.max(1.0), 0.0, 2.0 * PI);
    ctx.set_source_rgba(r, g, b, 0.72);
    let _ = ctx.fill_preserve();
    // Double ring, same dark-then-light trick as the brush cursor, so
    // the badge's edge survives on a background of any brightness.
    ctx.set_source_rgba(0.0, 0.0, 0.0, 0.65);
    ctx.set_line_width(OUTER_LINE_WIDTH);
    let _ = ctx.stroke_preserve();
    ctx.set_source_rgba(1.0, 1.0, 1.0, 0.95);
    ctx.set_line_width(INNER_LINE_WIDTH);
    let _ = ctx.stroke();

    // The number, in the same luminance-picked ink the real badge uses.
    // https://en.wikipedia.org/wiki/Luma_(video)
    if text_px >= MIN_DIGIT_PX {
        let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let ink = if luminance > 0.5 { 0.0 } else { 1.0 };
        let label = format!("{number}");
        ctx.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        ctx.set_font_size(text_px);
        if let Ok(ext) = ctx.text_extents(&label) {
            // Nudged up and left, out from under the arrow that hangs
            // down-right off the same point. The badge itself stays
            // centered — only the glyph inside it shifts, and only far
            // enough to stay readable.
            let dodge = ARROW_EXTENT * arrow_scale * 0.22;
            ctx.move_to(
                c - dodge - ext.width() / 2.0 - ext.x_bearing(),
                c - dodge * 0.7 - ext.height() / 2.0 - ext.y_bearing(),
            );
            ctx.set_source_rgba(ink, ink, ink, 0.95);
            let _ = ctx.show_text(&label);
        }
    }

    draw_arrow_at(&ctx, c, c, arrow_scale);

    drop(ctx);
    Some(surface)
}

/// Trace the classic arrow pointer with its tip at `(x, y)`, body
/// hanging toward the lower right, and paint it light-on-dark so it
/// reads over both the badge and whatever the badge is translucent
/// over.
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
    ctx.set_line_width(1.3 * scale.max(0.8));
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
/// `marker_number` is the number the Counter would stamp next — only
/// that tool uses it, and without one it keeps the stock crosshair.
pub fn drawing_tool_cursor(
    tool: crate::tools::Tools,
    style: &Style,
    render_scale: f64,
    device_pixel_ratio: f64,
    band_height_image_px: Option<f32>,
    band_vertical_offset_image_px: f32,
    marker_number: Option<u16>,
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
        Tools::Marker => {
            build_marker_cursor(style, marker_number?, render_scale, device_pixel_ratio)
        }
        _ => None,
    }
}

// Suppress "unused" if Size is referenced only for the public API.
#[allow(dead_code)]
fn _force_use_size(_: Size) {}

#[cfg(test)]
mod marker_cursor_tests {
    use super::render_marker_cursor;
    use crate::style::{Color, Size, Style};

    fn style(size: Size) -> Style {
        Style {
            size,
            color: Color {
                r: 240,
                g: 60,
                b: 50,
                a: 255,
            },
            ..Default::default()
        }
    }

    /// Dump the cursor at a few sizes for eyeballing. Ignored — it
    /// writes files and proves nothing on its own.
    #[test]
    #[ignore]
    fn dump_marker_cursor_pngs() {
        let dir = std::env::var("TENSAKU_CURSOR_DUMP_DIR").unwrap_or("/tmp".to_string());
        for (name, size, number, scale) in [
            ("small-1", Size::Small, 1u16, 1.0),
            ("medium-7", Size::Medium, 7, 1.0),
            ("medium-12", Size::Medium, 12, 1.0),
            ("large-3", Size::Large, 3, 1.0),
            ("medium-3-zoomed-out", Size::Medium, 3, 0.4),
        ] {
            let mut surface = render_marker_cursor(&style(size), number, scale, 1.0)
                .unwrap_or_else(|| panic!("{name} produced no surface"));
            let w = surface.width();
            let h = surface.height();
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            // Raw BGRA rows, tightly packed: cairo's `png` feature
            // isn't on, and any image viewer takes this with a size.
            let mut raw = Vec::with_capacity((w * h * 4) as usize);
            for row in 0..h as usize {
                raw.extend_from_slice(&data[row * stride..row * stride + (w as usize) * 4]);
            }
            std::fs::write(format!("{dir}/cursor-{name}-{w}x{h}.bgra"), &raw).unwrap();
        }
    }

    /// The badge has to grow with the size picker, or the preview is
    /// lying about what lands.
    #[test]
    fn the_badge_tracks_the_size_picker() {
        let small = render_marker_cursor(&style(Size::Small), 1, 1.0, 1.0).unwrap();
        let large = render_marker_cursor(&style(Size::Large), 1, 1.0, 1.0).unwrap();
        assert!(
            large.width() > small.width(),
            "large {} should exceed small {}",
            large.width(),
            small.width()
        );
    }

    /// Past GDK's cursor limit there is no texture to hand back, and
    /// the caller falls through to the stock crosshair.
    #[test]
    fn an_oversized_badge_declines_rather_than_returning_a_clipped_one() {
        assert!(render_marker_cursor(&style(Size::XXLarge), 888, 4.0, 1.0).is_none());
    }
}
