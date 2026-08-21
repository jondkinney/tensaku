//! Direct GL access for the two things femtovg's `Canvas` API can't
//! express: reading back a *sub-rectangle* of the framebuffer, and
//! deferring texture deletion to a point where a canvas is in hand.

use std::cell::RefCell;

use femtovg::{
    ImageId,
    imgref::ImgVec,
    rgb::{ComponentBytes, RGBA8},
};
use glow::HasContext;

thread_local! {
    /// Built lazily on the GTK main thread while the GLArea's context
    /// is current, then reused for the process lifetime.
    static GL: RefCell<Option<glow::Context>> = const { RefCell::new(None) };

    /// Textures whose owners dropped them somewhere without a canvas.
    /// Drained once per frame by `drain_deleted_images`.
    static PENDING_DELETES: RefCell<Vec<ImageId>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` with the cached glow context.
///
/// # Safety contract
/// Callers must be inside the GLArea's render path, so the widget's GL
/// context is current — that is where the loader resolves symbols and
/// where every command below is issued.
fn with_context<R>(f: impl FnOnce(&glow::Context) -> R) -> R {
    GL.with(|cell| {
        let mut slot = cell.borrow_mut();
        let ctx = slot.get_or_insert_with(|| unsafe {
            glow::Context::from_loader_function(|s| epoxy::get_proc_addr(s) as *const _)
        });
        f(ctx)
    })
}

/// Read `(x, y, w, h)` — top-left origin, canvas pixels — out of the
/// currently bound framebuffer.
///
/// `Canvas::screenshot` reads the *entire* framebuffer and then flips
/// it, which on a large display costs far more than the region a
/// caller actually wants: at 5528x2886 a full readback measured
/// 9-10 ms, and the Blur tool was paying it once per frame of a drag
/// to fetch a rectangle a fraction of that size. Reading just the
/// region keeps the same pixels with a fraction of the transfer.
///
/// Returns `None` when the region is empty or falls outside the
/// framebuffer; the caller is expected to have clamped it already, so
/// this is a guard rather than a code path.
///
/// The caller must have flushed the canvas first — a `glReadPixels`
/// sees only what has actually been submitted.
pub fn read_framebuffer_region(
    canvas_height: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> Option<ImgVec<RGBA8>> {
    if w == 0 || h == 0 {
        return None;
    }
    // GL's framebuffer origin is bottom-left; ours is top-left.
    let gl_y = canvas_height.checked_sub(y.checked_add(h)?)?;

    let mut rows = vec![RGBA8::new(0, 0, 0, 255); w * h];
    with_context(|ctx| unsafe {
        ctx.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        ctx.read_pixels(
            x as i32,
            gl_y as i32,
            w as i32,
            h as i32,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(rows.as_bytes_mut())),
        );
    });

    // Rows arrive bottom-up.
    let mut flipped = Vec::with_capacity(w * h);
    for row in rows.chunks_exact(w).rev() {
        flipped.extend_from_slice(row);
    }
    Some(ImgVec::new(flipped, w, h))
}

/// Hand back a texture whose owner has no canvas to delete it with.
///
/// `Drawable` mutators (`translate`, `move_handle`, `set_style`) drop
/// cached GPU images but are handed no canvas, so without this the
/// texture leaked — a blur drag invalidated its cache every frame and
/// leaked two textures per frame for the length of the drag.
pub fn queue_image_deletion(id: ImageId) {
    PENDING_DELETES.with(|q| q.borrow_mut().push(id));
}

/// Delete everything handed to `queue_image_deletion` since the last
/// call. Runs once per frame from the render path, which is late
/// enough that any queued texture has already been consumed by the
/// draw commands that referenced it.
pub fn drain_deleted_images(canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>) {
    let ids: Vec<ImageId> = PENDING_DELETES.with(|q| std::mem::take(&mut *q.borrow_mut()));
    for id in ids {
        canvas.delete_image(id);
    }
}

/// One-shot self-check for [`read_framebuffer_region`], run when
/// `TENSAKU_VERIFY_READBACK=1`.
///
/// The region read has to line up exactly with what femtovg's
/// whole-framebuffer `screenshot()` would have produced for the same
/// rectangle — same origin, same row order (GL's framebuffer is
/// bottom-up and ours is top-down), same channel order. Rather than
/// trust that by inspection, compare the two directly for a handful of
/// rectangles and report the first mismatch.
pub fn verify_readback_matches_screenshot(canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>) {
    if !std::env::var("TENSAKU_VERIFY_READBACK").is_ok_and(|v| v != "0") {
        return;
    }
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }

    canvas.flush();
    let full = match canvas.screenshot() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("verify-readback: screenshot failed: {e}");
            return;
        }
    };
    let (cw, ch) = (canvas.width() as usize, canvas.height() as usize);

    // Corners, an edge, and an interior rect — enough to catch a flip,
    // an off-by-one origin, or a transposed axis.
    let cases = [
        (0usize, 0usize, 32usize, 16usize),
        (cw - 40, 0, 40, 20),
        (0, ch - 24, 24, 24),
        (cw - 64, ch - 48, 64, 48),
        (cw / 3, ch / 4, 57, 31),
    ];

    for (x, y, w, h) in cases {
        let Some(region) = read_framebuffer_region(ch, x, y, w, h) else {
            eprintln!("verify-readback: FAIL region ({x},{y},{w},{h}) returned None");
            continue;
        };
        let mut mismatches = 0usize;
        for row in 0..h {
            for col in 0..w {
                let a = region.buf()[row * w + col];
                let b = full[(x + col, y + row)];
                if a.r != b.r || a.g != b.g || a.b != b.b {
                    mismatches += 1;
                }
            }
        }
        if mismatches == 0 {
            eprintln!("verify-readback: ok ({x},{y},{w},{h})");
        } else {
            eprintln!(
                "verify-readback: FAIL ({x},{y},{w},{h}) {mismatches}/{} pixels differ",
                w * h
            );
        }
    }
}

/// One-shot uniformity probe, run when `TENSAKU_VERIFY_FLAT=1`.
///
/// Reads a horizontal strip straight out of the framebuffer and
/// reports its colour runs. Point it at a capture that is a single
/// flat colour: anything but one run means the canvas itself is not
/// rendering flat. Distinguishes a rendering problem from one
/// introduced later by the compositor or the screenshot path, which a
/// screen grab alone cannot.
pub fn verify_flat_render(canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>) {
    if !std::env::var("TENSAKU_VERIFY_FLAT").is_ok_and(|v| v != "0") {
        return;
    }
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    canvas.flush();
    let (w, h) = (canvas.width() as usize, canvas.height() as usize);
    for frac in [3, 2] {
        let y = h / frac;
        let Some(strip) = read_framebuffer_region(h, 0, y, w, 1) else {
            continue;
        };
        let mut runs: Vec<(usize, usize, RGBA8)> = Vec::new();
        for (x, px) in strip.buf().iter().enumerate() {
            match runs.last_mut() {
                Some(last) if last.2 == *px => last.1 = x,
                _ => runs.push((x, x, *px)),
            }
        }
        let long: Vec<String> = runs
            .iter()
            .filter(|(a, b, _)| b - a > 40)
            .map(|(a, b, c)| format!("{a}-{b} #{:02X}{:02X}{:02X}", c.r, c.g, c.b))
            .collect();
        eprintln!(
            "verify-flat: framebuffer row y={y}: {} runs; long: {long:?}",
            runs.len()
        );
        // Alpha matters as much as colour: a canvas that renders the
        // right RGB but leaves alpha below 255 is composited against
        // whatever sits behind the window, which shows up as faint
        // bands tracking the shapes of the windows underneath.
        let mut alpha_runs: Vec<(usize, usize, u8)> = Vec::new();
        for (x, px) in strip.buf().iter().enumerate() {
            match alpha_runs.last_mut() {
                Some(last) if last.2 == px.a => last.1 = x,
                _ => alpha_runs.push((x, x, px.a)),
            }
        }
        let alphas: Vec<String> = alpha_runs
            .iter()
            .filter(|(a, b, _)| b - a > 40)
            .map(|(a, b, v)| format!("{a}-{b} a={v}"))
            .collect();
        eprintln!(
            "verify-flat: alpha y={y}: {} runs; long: {alphas:?}",
            alpha_runs.len()
        );
    }
}
