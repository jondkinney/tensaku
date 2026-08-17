use anyhow::{Context, Result, anyhow, bail};
use relm4::gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use relm4::gtk::glib::Bytes;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, delegate_shm, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_shm};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use super::Rect;
use super::outputs::OutputTracker;

struct CaptureState {
    registry_state: RegistryState,
    shm: Shm,
    outputs: OutputTracker,

    // Frame negotiation
    shm_buffer_info: Option<ShmBufferInfo>,
    buffer_done: bool,

    // Completion
    ready: bool,
    failed: bool,
    fail_reason: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ShmBufferInfo {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

/// Capture `region` (or the whole output) from the output named
/// `output_name`; `None` falls back to the first advertised output.
pub fn capture(region: Option<Rect>, output_name: Option<&str>) -> Result<Pixbuf> {
    let conn = Connection::connect_to_env()
        .context("failed to connect to Wayland display (is WAYLAND_DISPLAY set?)")?;

    let (globals, mut event_queue) =
        registry_queue_init::<CaptureState>(&conn).context("failed to init registry")?;
    let qh = event_queue.handle();

    let registry_state = RegistryState::new(&globals);
    let shm = Shm::bind(&globals, &qh).context("compositor does not expose wl_shm")?;
    let screencopy_manager = globals
        .bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(&qh, 1..=3, ())
        .context("compositor does not expose zwlr_screencopy_manager_v1 (wlroots-only)")?;

    let mut outputs = OutputTracker::default();
    outputs.bind_all(&globals, &qh);

    let mut state = CaptureState {
        registry_state,
        shm,
        outputs,
        shm_buffer_info: None,
        buffer_done: false,
        ready: false,
        failed: false,
        fail_reason: None,
    };
    // Output names and modes arrive with the initial wl_output events.
    event_queue
        .roundtrip(&mut state)
        .context("roundtrip for wl_output info")?;
    let (output, _) = state.outputs.select(output_name)?;
    let output = output.clone();

    // Request capture. Cursor not included (overlay_cursor = 0).
    let frame = match region {
        None => screencopy_manager.capture_output(0, &output, &qh, ()),
        Some(r) => screencopy_manager.capture_output_region(
            0,
            &output,
            r.x,
            r.y,
            r.width,
            r.height,
            &qh,
            (),
        ),
    };

    // Pump events until the compositor finishes announcing buffer formats.
    while !state.buffer_done && !state.failed {
        event_queue
            .blocking_dispatch(&mut state)
            .context("dispatch failed during buffer negotiation")?;
    }
    if state.failed {
        bail!(
            "screencopy failed during negotiation: {}",
            state.fail_reason.unwrap_or_else(|| "(no reason)".into())
        );
    }

    let info = state
        .shm_buffer_info
        .ok_or_else(|| anyhow!("compositor did not announce an SHM buffer"))?;

    // Allocate an SHM buffer of the announced size and submit it.
    let pool_size = (info.stride as usize) * (info.height as usize);
    let mut pool = SlotPool::new(pool_size, &state.shm).context("failed to create SHM pool")?;
    let (buffer, _canvas) = pool
        .create_buffer(
            info.width as i32,
            info.height as i32,
            info.stride as i32,
            info.format,
        )
        .context("failed to allocate SHM buffer")?;

    frame.copy(buffer.wl_buffer());

    while !state.ready && !state.failed {
        event_queue
            .blocking_dispatch(&mut state)
            .context("dispatch failed during copy")?;
    }
    if state.failed {
        bail!(
            "screencopy failed during copy: {}",
            state.fail_reason.unwrap_or_else(|| "(no reason)".into())
        );
    }

    // Read the pixels back from the pool. canvas() borrows mutably; that's fine,
    // we've already received the Ready event so the buffer is fully written.
    let canvas = buffer
        .canvas(&mut pool)
        .ok_or_else(|| anyhow!("pool canvas unavailable"))?;

    let pixbuf = to_pixbuf(canvas, &info)?;

    // Cleanup. The frame, buffer, and pool are dropped here.
    frame.destroy();

    Ok(pixbuf)
}

fn to_pixbuf(canvas: &[u8], info: &ShmBufferInfo) -> Result<Pixbuf> {
    let width = info.width as usize;
    let height = info.height as usize;
    let stride = info.stride as usize;
    let row_bytes = width * 4;
    let mut rgba = Vec::with_capacity(row_bytes * height);

    // wl_shm formats are little-endian. XRGB8888/ARGB8888 mean the bytes in
    // memory are B, G, R, X/A. A captured output is already compositor-flattened
    // and therefore always opaque. Some compositors leave the alpha byte of an
    // ARGB screencopy undefined when a transparent layer surface is present;
    // preserving that byte can turn a valid capture transparent in the editor.
    let swap_bgra = match info.format {
        wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => true,
        wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => false,
        other => bail!("unsupported wl_shm format: {:?}", other),
    };

    for y in 0..height {
        let row = &canvas[y * stride..y * stride + row_bytes];
        for px in row.chunks_exact(4) {
            let (r, g, b) = if swap_bgra {
                (px[2], px[1], px[0])
            } else {
                (px[0], px[1], px[2])
            };
            rgba.push(r);
            rgba.push(g);
            rgba.push(b);
            rgba.push(255);
        }
    }

    let bytes = Bytes::from_owned(rgba);
    Ok(Pixbuf::from_bytes(
        &bytes,
        Colorspace::Rgb,
        true, // has_alpha
        8,    // bits_per_sample
        info.width as i32,
        info.height as i32,
        (info.width * 4) as i32, // rowstride (packed)
    ))
}

// ---- wayland dispatch glue ----

impl Dispatch<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, ()> for CaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        _event: zwlr_screencopy_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events from the manager.
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event::*;
        match event {
            Buffer {
                format: wayland_client::WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                state.shm_buffer_info = Some(ShmBufferInfo {
                    format,
                    width,
                    height,
                    stride,
                });
            }
            BufferDone => {
                state.buffer_done = true;
            }
            Flags { .. } => {}
            Ready { .. } => {
                state.ready = true;
            }
            Failed => {
                state.failed = true;
                state.fail_reason = Some("compositor reported failed".into());
            }
            Damage { .. } => {}
            LinuxDmabuf { .. } => {
                // We don't use DMABUF in v1; ignore.
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for CaptureState {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        state.outputs.handle(proxy, event);
    }
}

impl ProvidesRegistryState for CaptureState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![];
}

impl ShmHandler for CaptureState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(CaptureState);
delegate_shm!(CaptureState);

#[cfg(test)]
mod tests {
    use super::*;

    fn converted(format: wl_shm::Format, canvas: &[u8]) -> Vec<u8> {
        let info = ShmBufferInfo {
            format,
            width: 2,
            height: 1,
            stride: 8,
        };
        to_pixbuf(canvas, &info)
            .expect("conversion should succeed")
            .read_pixel_bytes()
            .as_ref()
            .to_vec()
    }

    #[test]
    fn output_capture_is_opaque_for_argb_and_xrgb() {
        let memory = [3, 2, 1, 0, 6, 5, 4, 73];
        let expected = vec![1, 2, 3, 255, 4, 5, 6, 255];
        assert_eq!(converted(wl_shm::Format::Xrgb8888, &memory), expected);
        assert_eq!(converted(wl_shm::Format::Argb8888, &memory), expected);
    }

    #[test]
    fn output_capture_is_opaque_for_abgr_and_xbgr() {
        let memory = [1, 2, 3, 0, 4, 5, 6, 73];
        let expected = vec![1, 2, 3, 255, 4, 5, 6, 255];
        assert_eq!(converted(wl_shm::Format::Xbgr8888, &memory), expected);
        assert_eq!(converted(wl_shm::Format::Abgr8888, &memory), expected);
    }

    #[test]
    fn conversion_ignores_source_row_padding() {
        let info = ShmBufferInfo {
            format: wl_shm::Format::Xrgb8888,
            width: 1,
            height: 2,
            stride: 8,
        };
        let memory = [30, 20, 10, 0, 99, 99, 99, 99, 60, 50, 40, 0, 88, 88, 88, 88];
        let pixels = to_pixbuf(&memory, &info)
            .expect("conversion should succeed")
            .read_pixel_bytes();
        assert_eq!(pixels.as_ref(), [10, 20, 30, 255, 40, 50, 60, 255]);
    }
}
