use anyhow::Result;
use femtovg::{FontId, Path};
use relm4::{
    Sender,
    gtk::gdk::{Key, ModifierType},
};

use crate::{
    math::{Rect, Vec2D},
    sketch_board::{MouseButton, MouseEventMsg, MouseEventType, SketchBoardInput},
    style::Style,
};

use super::{
    CanvasTransform, Drawable, DrawableClone, GLOW_COLOR, Handle, HandleId, Tool, ToolUpdateResult,
    Tools, bbox_handles, bbox_resize, halo_in_image_units,
};

#[derive(Clone, Copy, Debug)]
pub struct Ellipse {
    origin: Vec2D,
    middle: Vec2D,
    radii: Option<Vec2D>,
    style: Style,
    centered: bool,
    finishing: bool,
}

impl Ellipse {
    /// Half the stroke plus the caller's slack: how far either side of
    /// the mathematical curve still counts as "on the line".
    fn outline_pad(&self, tolerance: f32) -> f32 {
        let stroke = self
            .style
            .size
            .to_line_width(self.style.annotation_size_factor);
        stroke / 2.0 + tolerance
    }

    /// Inside the curve, padded outward — the silhouette plus slack.
    fn inside_outer_edge(&self, point: Vec2D, tolerance: f32) -> bool {
        let Some(r) = self.radii else {
            return false;
        };
        let (rx, ry) = (r.x.abs(), r.y.abs());
        if rx < f32::EPSILON || ry < f32::EPSILON {
            return false;
        }
        let pad = self.outline_pad(tolerance);
        let dx = (point.x - self.middle.x) / (rx + pad);
        let dy = (point.y - self.middle.y) / (ry + pad);
        dx * dx + dy * dy <= 1.0
    }

    /// On the ring straddling the curve: inside the outward-padded
    /// ellipse but outside the inward-padded one. When the padding
    /// swallows a radius there is no hollow middle left, so the whole
    /// silhouette is outline.
    fn on_outline(&self, point: Vec2D, tolerance: f32) -> bool {
        if !self.inside_outer_edge(point, tolerance) {
            return false;
        }
        let Some(r) = self.radii else {
            return false;
        };
        let pad = self.outline_pad(tolerance);
        let inner_rx = r.x.abs() - pad;
        let inner_ry = r.y.abs() - pad;
        if inner_rx <= 0.0 || inner_ry <= 0.0 {
            return true;
        }
        let dx = (point.x - self.middle.x) / inner_rx;
        let dy = (point.y - self.middle.y) / inner_ry;
        dx * dx + dy * dy > 1.0
    }
}

impl Drawable for Ellipse {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn kind_label(&self) -> &'static str {
        "Ellipse"
    }
    fn icon_name(&self) -> &'static str {
        "circle-regular"
    }
    fn panel_preview(&self) -> crate::tools::PanelPreview {
        crate::tools::PanelPreview::Ellipse {
            filled: self.style.fill,
        }
    }

    fn draw(
        &self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        _font: FontId,
        _bounds: (Vec2D, Vec2D),
    ) -> Result<()> {
        let radii = match self.radii {
            Some(s) => s,
            None => return Ok(()), // early exit if none
        };

        canvas.save();
        let mut path = Path::new();
        path.ellipse(self.middle.x, self.middle.y, radii.x, radii.y);

        if self.style.fill {
            canvas.fill_path(&path, &self.style.into());
        } else {
            canvas.stroke_path(&path, &self.style.into());
        }
        canvas.restore();

        Ok(())
    }

    fn bounds(&self) -> Option<Rect> {
        let r = self.radii?;
        let rx = r.x.abs();
        let ry = r.y.abs();
        Some(Rect {
            pos: Vec2D::new(self.middle.x - rx, self.middle.y - ry),
            size: Vec2D::new(rx * 2.0, ry * 2.0),
        })
    }

    /// Border-only picking: a filled ellipse covers real canvas, so its interior must stay
    /// available to whichever drawing tool is armed. See
    /// `Drawable::edge_hit_test`.
    ///
    /// The band hugs the ellipse's own curve rather than its bounding
    /// box. A box-shaped band is nowhere near the outline along the
    /// diagonals — at 45° the curve sits roughly 30% of the radius
    /// inside the corner — so grabbing an ellipse anywhere but its
    /// four extremes meant aiming at a band that wasn't under the
    /// line being aimed at.
    fn edge_hit_test(&self, point: Vec2D, tolerance: f32) -> bool {
        self.on_outline(point, tolerance)
    }

    fn hit_test(&self, point: Vec2D, tolerance: f32) -> bool {
        // Filled: anywhere inside the silhouette is a hit. Unfilled:
        // only the stroke band, which is the same ring the border grab
        // uses — one implementation, so picking and grabbing can't
        // disagree about where the outline is.
        if self.style.fill {
            self.inside_outer_edge(point, tolerance)
        } else {
            self.on_outline(point, tolerance)
        }
    }

    fn translate(&mut self, delta: Vec2D) {
        self.middle += delta;
        self.origin += delta;
    }

    fn apply_canvas_transform(&mut self, t: CanvasTransform, w: f32, h: f32) {
        match self.radii {
            Some(r) => {
                let rx = r.x.abs();
                let ry = r.y.abs();
                let bbox = Rect::new(
                    Vec2D::new(self.middle.x - rx, self.middle.y - ry),
                    Vec2D::new(rx * 2.0, ry * 2.0),
                );
                let m = t.map_rect(bbox, w, h);
                self.middle = m.center();
                self.radii = Some(Vec2D::new(m.size.x / 2.0, m.size.y / 2.0));
                self.origin = m.pos;
            }
            None => {
                self.middle = t.map_point(self.middle, w, h);
                self.origin = t.map_point(self.origin, w, h);
            }
        }
    }

    fn handles(&self) -> Vec<Handle> {
        self.bounds().map(bbox_handles).unwrap_or_default()
    }

    fn move_handle(&mut self, handle: HandleId, to: Vec2D) {
        let Some(cur) = self.bounds() else { return };
        let new = bbox_resize(cur, handle, to);
        self.middle = new.center();
        self.radii = Some(Vec2D::new(new.size.x / 2.0, new.size.y / 2.0));
        self.origin = new.pos;
    }

    fn set_style(&mut self, style: Style) {
        self.style = style;
    }

    fn style(&self) -> Option<Style> {
        Some(self.style)
    }

    fn tool_type(&self) -> Option<Tools> {
        Some(Tools::Ellipse)
    }

    fn render_glow(
        &self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        _font: FontId,
        _bounds: (Vec2D, Vec2D),
        device_pixel_ratio: f32,
    ) -> Result<()> {
        let Some(radii) = self.radii else {
            return Ok(());
        };
        let rx = radii.x.abs();
        let ry = radii.y.abs();
        let halo = halo_in_image_units(canvas, device_pixel_ratio);
        canvas.save();
        if self.style.fill {
            let inflate = halo / 2.0;
            let mut path = Path::new();
            path.ellipse(self.middle.x, self.middle.y, rx + inflate, ry + inflate);
            let mut paint = femtovg::Paint::color(GLOW_COLOR);
            paint.set_line_width(halo);
            canvas.stroke_path(&path, &paint);
        } else {
            let line_width = self
                .style
                .size
                .to_line_width(self.style.annotation_size_factor);
            let mut path = Path::new();
            path.ellipse(self.middle.x, self.middle.y, rx, ry);
            let mut paint = femtovg::Paint::color(GLOW_COLOR);
            paint.set_line_width(line_width + 2.0 * halo);
            canvas.stroke_path(&path, &paint);
        }
        canvas.restore();
        Ok(())
    }
}

impl Ellipse {
    fn calculate_shape(&mut self, event: &MouseEventMsg) {
        self.centered = event.modifier & ModifierType::ALT_MASK == ModifierType::ALT_MASK;
        match event.modifier & (ModifierType::ALT_MASK | ModifierType::SHIFT_MASK) {
            v if v == ModifierType::ALT_MASK | ModifierType::SHIFT_MASK => {
                self.middle = self.origin;
                let max_size = event.pos.x.abs().max(event.pos.y.abs());
                self.radii = Some(Vec2D {
                    x: max_size * event.pos.x.signum(),
                    y: max_size * event.pos.y.signum(),
                });
            }
            ModifierType::ALT_MASK => {
                self.middle = self.origin;
                self.radii = Some(event.pos);
            }
            ModifierType::SHIFT_MASK => {
                let max_size = (event.pos.x / 2.0).abs().max((event.pos.y / 2.0).abs());
                self.radii = Some(Vec2D {
                    x: max_size * event.pos.x.signum(),
                    y: max_size * event.pos.y.signum(),
                });
                self.middle.x = self.origin.x + max_size * event.pos.x.signum();
                self.middle.y = self.origin.y + max_size * event.pos.y.signum();
            }
            _ => {
                self.radii = Some(Vec2D {
                    x: event.pos.x / 2.0,
                    y: event.pos.y / 2.0,
                });
                self.middle.x = self.origin.x + event.pos.x / 2.0;
                self.middle.y = self.origin.y + event.pos.y / 2.0;
            }
        }
    }
}

#[derive(Default)]
pub struct EllipseTool {
    ellipse: Option<Ellipse>,
    style: Style,
    input_enabled: bool,
    sender: Option<Sender<SketchBoardInput>>,
}

impl Tool for EllipseTool {
    fn input_enabled(&self) -> bool {
        self.input_enabled
    }

    fn set_input_enabled(&mut self, value: bool) {
        self.input_enabled = value;
    }

    fn get_tool_type(&self) -> super::Tools {
        Tools::Ellipse
    }

    fn handle_mouse_event(&mut self, event: MouseEventMsg) -> ToolUpdateResult {
        match event.type_ {
            MouseEventType::BeginDrag => {
                if event.button == MouseButton::Middle {
                    return ToolUpdateResult::Unmodified;
                }

                // start new
                self.ellipse = Some(Ellipse {
                    origin: event.pos,
                    middle: event.pos,
                    radii: None,
                    style: self.style,
                    centered: true,
                    finishing: false,
                });

                ToolUpdateResult::Redraw
            }
            MouseEventType::EndDrag => {
                if event.button == MouseButton::Middle {
                    return ToolUpdateResult::Unmodified;
                }

                if let Some(ellipse) = &mut self.ellipse {
                    ellipse.finishing = true;
                    if event.pos == Vec2D::zero() {
                        self.ellipse = None;

                        ToolUpdateResult::Redraw
                    } else {
                        ellipse.calculate_shape(&event);
                        let result = ellipse.clone_box();
                        self.ellipse = None;
                        ToolUpdateResult::Commit(result)
                    }
                } else {
                    ToolUpdateResult::Unmodified
                }
            }
            MouseEventType::UpdateDrag => {
                if event.button == MouseButton::Middle {
                    return ToolUpdateResult::Unmodified;
                }

                if let Some(ellipse) = &mut self.ellipse {
                    if event.pos == Vec2D::zero() {
                        return ToolUpdateResult::Unmodified;
                    }
                    ellipse.calculate_shape(&event);
                    ToolUpdateResult::Redraw
                } else {
                    ToolUpdateResult::Unmodified
                }
            }
            _ => ToolUpdateResult::Unmodified,
        }
    }

    fn handle_key_event(&mut self, event: crate::sketch_board::KeyEventMsg) -> ToolUpdateResult {
        if event.key == Key::Escape && self.ellipse.is_some() {
            self.ellipse = None;
            ToolUpdateResult::Redraw
        } else {
            ToolUpdateResult::Unmodified
        }
    }

    fn handle_style_event(&mut self, style: Style) -> ToolUpdateResult {
        self.style = style;
        ToolUpdateResult::Unmodified
    }

    fn get_drawable(&self) -> Option<&dyn Drawable> {
        match &self.ellipse {
            Some(d) => Some(d),
            None => None,
        }
    }

    fn set_sender(&mut self, sender: Sender<SketchBoardInput>) {
        self.sender = Some(sender);
    }
}

#[cfg(test)]
mod outline_tests {
    use super::Ellipse;
    use crate::math::Vec2D;
    use crate::style::{Size, Style};
    use crate::tools::Drawable;

    /// A 400x200 ellipse centered at (300, 200), thin stroke.
    fn ellipse(fill: bool) -> Ellipse {
        Ellipse {
            origin: Vec2D::new(100.0, 100.0),
            middle: Vec2D::new(300.0, 200.0),
            radii: Some(Vec2D::new(200.0, 100.0)),
            style: Style {
                size: Size::XSmall,
                fill,
                ..Default::default()
            },
            centered: false,
            finishing: false,
        }
    }

    /// The point that motivated this: 45° along the curve, far from
    /// the bounding box the band used to follow.
    #[test]
    fn the_diagonal_of_the_curve_is_grabbable() {
        let e = ellipse(false);
        let d = std::f32::consts::FRAC_1_SQRT_2;
        let on_curve = Vec2D::new(300.0 + 200.0 * d, 200.0 + 100.0 * d);
        assert!(e.edge_hit_test(on_curve, 8.0));
        // The bounding box corner, meanwhile, is nowhere near the
        // line — grabbing there would be grabbing empty canvas.
        assert!(!e.edge_hit_test(Vec2D::new(500.0, 300.0), 8.0));
    }

    /// The interior stays clear so an armed drawing tool can use it,
    /// filled or not.
    #[test]
    fn the_middle_is_not_the_outline() {
        for fill in [false, true] {
            let e = ellipse(fill);
            assert!(
                !e.edge_hit_test(Vec2D::new(300.0, 200.0), 8.0),
                "fill={fill}"
            );
        }
    }

    /// Slack applies on both sides of the curve, so a near miss from
    /// either direction still grabs.
    #[test]
    fn the_band_straddles_the_curve() {
        let e = ellipse(false);
        assert!(
            e.edge_hit_test(Vec2D::new(496.0, 200.0), 8.0),
            "just inside"
        );
        assert!(
            e.edge_hit_test(Vec2D::new(504.0, 200.0), 8.0),
            "just outside"
        );
        assert!(
            !e.edge_hit_test(Vec2D::new(560.0, 200.0), 8.0),
            "well outside"
        );
    }

    /// A filled ellipse is hit anywhere inside; an unfilled one only
    /// on its stroke. That distinction predates the shared ring and
    /// has to survive it.
    #[test]
    fn filling_changes_hit_test_but_not_the_outline() {
        let middle = Vec2D::new(300.0, 200.0);
        assert!(ellipse(true).hit_test(middle, 8.0));
        assert!(!ellipse(false).hit_test(middle, 8.0));
    }
}
