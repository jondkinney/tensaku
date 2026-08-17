use anyhow::Result;
use relm4::gtk::gdk_pixbuf::Pixbuf;

pub mod outputs;
pub mod wlr_screencopy;

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Capture a whole output. `output` is the connector name (e.g. `DP-3`) to
/// capture; `None` falls back to the first advertised output.
pub fn capture_output(output: Option<&str>) -> Result<Pixbuf> {
    wlr_screencopy::capture(None, output)
}

/// Capture `rect` (logical, output-relative) from the named output.
pub fn capture_region(rect: Rect, output: Option<&str>) -> Result<Pixbuf> {
    wlr_screencopy::capture(Some(rect), output)
}
