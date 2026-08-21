use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;

use femtovg::{Color, Paint, Path};

use crate::sketch_board::{MouseButton, MouseEventType, SketchBoardInput};
use crate::style::Style;
use crate::{
    math::{Rect, Vec2D},
    sketch_board::MouseEventMsg,
};

use super::{
    CanvasTransform, Drawable, DrawableClone, GLOW_COLOR, Handle, HandleId, Tool, ToolUpdateResult,
    Tools, halo_in_image_units,
};
use relm4::Sender;

pub struct MarkerTool {
    style: Style,
    next_number: Rc<RefCell<u16>>,
    input_enabled: bool,
    sender: Option<Sender<SketchBoardInput>>,
    /// The badge a click would stamp, parked under the pointer. Drawn
    /// through the same path as a committed one — real font, real
    /// size, real zoom — just translucent and never exported.
    ghost: Option<Marker>,
}

#[derive(Clone, Debug)]
pub struct Marker {
    pos: Vec2D,
    /// Uniform resize multiplier on top of the style-derived size.
    /// 1.0 is the size picked in the toolbar; dragging a selected
    /// marker's handles adjusts this.
    scale: f32,
    number: u16,
    style: Style,
    tool_next_number: Rc<RefCell<u16>>,
    /// Hover preview rather than a real annotation: same geometry,
    /// drawn see-through so it reads as "this is what you'd get".
    ghost: bool,
}

/// Ghost opacity. Solid enough to read the number against a busy
/// screenshot, sheer enough that it never looks already-placed.
const GHOST_ALPHA: f32 = 0.72;

impl Drawable for Marker {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn kind_label(&self) -> &'static str {
        "Marker"
    }
    fn icon_name(&self) -> &'static str {
        "number-circle-1-regular"
    }

    fn is_hover_preview(&self) -> bool {
        self.ghost
    }

    fn draw(
        &self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        font: femtovg::FontId,
        _bounds: (Vec2D, Vec2D),
    ) -> anyhow::Result<()> {
        let text = format!("{}", self.number);

        let mut marker_color: Color = self.style.color.into();
        // https://en.wikipedia.org/wiki/Luma_(video)
        let luminance = 0.2126 * marker_color.r + 0.7152 * marker_color.g + 0.0722 * marker_color.b;
        let mut text_color = if luminance > 0.5 {
            Color::black()
        } else {
            Color::white()
        };
        if self.ghost {
            marker_color.a *= GHOST_ALPHA;
            text_color.a *= GHOST_ALPHA;
        }

        let mut paint = Paint::color(text_color);

        // Marker font size — tighter than the generic Style::to_text_size scale
        // since the marker is a self-contained badge, not a paragraph of text.
        let text_size = self.marker_text_size();

        paint.set_font(&[font]);
        paint.set_font_size(text_size);
        paint.set_text_align(femtovg::Align::Center);
        paint.set_text_baseline(femtovg::Baseline::Middle);

        let text_metrics = canvas.measure_text(self.pos.x, self.pos.y, &text, &paint)?;

        // Solid filled disc — no outer ring. Padding so the digit doesn't
        // touch the edge.
        let circle_radius = ((text_metrics.width() * text_metrics.width()
            + text_metrics.height() * text_metrics.height())
        .sqrt()
            * 0.65)
            .max(text_size * 0.55);

        let mut disc = Path::new();
        disc.arc(
            self.pos.x,
            self.pos.y,
            circle_radius,
            0.0,
            2.0 * PI as f32,
            femtovg::Solidity::Solid,
        );

        let disc_paint = Paint::color(marker_color);

        canvas.save();
        canvas.fill_path(&disc, &disc_paint);
        canvas.fill_text(self.pos.x, self.pos.y, &text, &paint)?;
        canvas.restore();
        Ok(())
    }

    fn handle_undo(&mut self) {
        *self.tool_next_number.borrow_mut() = self.number;
    }

    fn handle_redo(&mut self) {
        *self.tool_next_number.borrow_mut() = self.number + 1;
    }

    fn bounds(&self) -> Option<Rect> {
        let r = self.approx_radius();
        Some(Rect {
            pos: Vec2D::new(self.pos.x - r, self.pos.y - r),
            size: Vec2D::new(r * 2.0, r * 2.0),
        })
    }

    fn hit_test(&self, point: Vec2D, tolerance: f32) -> bool {
        let r = self.approx_radius() + tolerance;
        self.pos.distance_to(&point) <= r
    }

    fn translate(&mut self, delta: Vec2D) {
        self.pos += delta;
    }

    fn apply_canvas_transform(&mut self, t: CanvasTransform, w: f32, h: f32) {
        // The badge is a circle — only its center moves; the radius is a
        // scalar unaffected by a flip / 90° turn.
        self.pos = t.map_point(self.pos, w, h);
    }

    /// Four corner handles around the marker's bounding box. Markers
    /// scale uniformly, so only corners are exposed — side handles
    /// would imply a non-uniform resize the round badge can't take.
    fn handles(&self) -> Vec<Handle> {
        let Some(b) = self.bounds() else {
            return Vec::new();
        };
        vec![
            Handle::new(HandleId::TopLeft, b.top_left()),
            Handle::new(HandleId::TopRight, b.top_right()),
            Handle::new(HandleId::BottomLeft, b.bottom_left()),
            Handle::new(HandleId::BottomRight, b.bottom_right()),
        ]
    }

    /// Uniform resize about the marker's center. `pos` is a meaningful
    /// anchor — the spot being numbered — so it stays put while the
    /// badge scales. A bbox corner sits r·√2 from the center, so the
    /// dragged point's distance from center sets the new radius.
    fn move_handle(&mut self, _handle: HandleId, to: Vec2D) {
        let current_r = self.approx_radius();
        if current_r <= f32::EPSILON {
            return;
        }
        let target_r = self.pos.distance_to(&to) * std::f32::consts::FRAC_1_SQRT_2;
        self.scale = (self.scale * target_r / current_r).clamp(0.2, 10.0);
    }

    fn set_style(&mut self, style: Style) {
        self.style = style;
    }

    fn style(&self) -> Option<Style> {
        Some(self.style)
    }

    fn tool_type(&self) -> Option<Tools> {
        Some(Tools::Marker)
    }

    fn render_glow(
        &self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        _font: femtovg::FontId,
        _bounds: (Vec2D, Vec2D),
        device_pixel_ratio: f32,
    ) -> anyhow::Result<()> {
        // Fill a circle slightly larger than the marker disc. Drawn under the
        // marker itself, so only the outer ring (HALO_PAD wide) shows as a halo.
        let halo = halo_in_image_units(canvas, device_pixel_ratio);
        let glow_radius = self.approx_radius() + halo;
        let mut path = Path::new();
        path.arc(
            self.pos.x,
            self.pos.y,
            glow_radius,
            0.0,
            2.0 * PI as f32,
            femtovg::Solidity::Solid,
        );
        let paint = Paint::color(GLOW_COLOR);
        canvas.fill_path(&path, &paint);
        Ok(())
    }
}

/// Marker-specific text size. Smaller than Style::to_text_size — markers
/// are compact badges, not paragraphs.
///
/// A free function rather than a method because the cursor previews the
/// badge before any `Marker` exists to ask, and a preview sized by its
/// own copy of these numbers would drift from the badge it promises.
fn marker_text_size(size: crate::style::Size, factor: f32, scale: f32) -> f32 {
    let base = match size {
        crate::style::Size::XSmall => 14.0,
        crate::style::Size::Small => 22.0,
        crate::style::Size::Medium => 36.0,
        crate::style::Size::Large => 50.0,
        crate::style::Size::XLarge => 70.0,
        crate::style::Size::XXLarge => 96.0,
    };
    base * factor * scale
}

/// Approximate badge radius for `text_size`, without canvas-bound text
/// metrics — wider for a two- or three-digit number, same as the real
/// disc. Shared with the cursor preview.
fn marker_radius(text_size: f32, number: u16) -> f32 {
    let digits = number.to_string().len() as f32;
    let w = text_size * 0.7 * digits.max(1.0);
    let h = text_size;
    (w * w + h * h).sqrt() * 0.65
}

impl Marker {
    fn marker_text_size(&self) -> f32 {
        marker_text_size(self.style.size, self.style.annotation_size_factor, self.scale)
    }

    fn approx_radius(&self) -> f32 {
        marker_radius(self.marker_text_size(), self.number)
    }
}

impl Tool for MarkerTool {
    fn input_enabled(&self) -> bool {
        self.input_enabled
    }

    fn set_input_enabled(&mut self, value: bool) {
        self.input_enabled = value;
    }

    fn get_tool_type(&self) -> super::Tools {
        Tools::Marker
    }

    fn get_drawable(&self) -> Option<&dyn Drawable> {
        self.ghost.as_ref().map(|m| m as &dyn Drawable)
    }

    fn set_hover_preview(&mut self, pos: Option<Vec2D>) -> bool {
        let Some(pos) = pos else {
            return self.ghost.take().is_some();
        };
        let number = *self.next_number.borrow();
        // Skip the redraw when nothing about the badge would differ —
        // the pointer sitting still, or a motion event that rounds to
        // the same image pixel.
        if let Some(g) = &self.ghost
            && g.pos == pos
            && g.number == number
            && g.style.color == self.style.color
            && g.style.size == self.style.size
            && g.style.annotation_size_factor == self.style.annotation_size_factor
        {
            return false;
        }
        self.ghost = Some(Marker {
            pos,
            scale: 1.0,
            number,
            style: self.style,
            tool_next_number: self.next_number.clone(),
            ghost: true,
        });
        true
    }

    fn handle_style_event(&mut self, style: Style) -> ToolUpdateResult {
        self.style = style;
        ToolUpdateResult::Unmodified
    }

    fn handle_mouse_event(&mut self, event: MouseEventMsg) -> ToolUpdateResult {
        match event.type_ {
            MouseEventType::Click => {
                if event.button == MouseButton::Primary {
                    // Land exactly where the ghost was drawn. The ghost
                    // sits offset from the pointer so the cursor doesn't
                    // cover its number, and a badge that ignored that
                    // offset would jump on click — the preview would be
                    // showing one place and the stamp landing in
                    // another.
                    let pos = self.ghost.as_ref().map_or(event.pos, |g| g.pos);
                    let marker = Marker {
                        pos,
                        scale: 1.0,
                        number: *self.next_number.borrow(),
                        style: self.style,
                        tool_next_number: self.next_number.clone(),
                        ghost: false,
                    };

                    // increment for next
                    *self.next_number.borrow_mut() += 1;

                    ToolUpdateResult::Commit(marker.clone_box())
                } else {
                    ToolUpdateResult::Unmodified
                }
            }
            _ => ToolUpdateResult::Unmodified,
        }
    }

    fn set_sender(&mut self, sender: Sender<SketchBoardInput>) {
        self.sender = Some(sender);
    }
}

impl Default for MarkerTool {
    fn default() -> Self {
        Self {
            style: Default::default(),
            next_number: Rc::new(RefCell::new(1)),
            input_enabled: true,
            sender: None,
            ghost: None,
        }
    }
}
