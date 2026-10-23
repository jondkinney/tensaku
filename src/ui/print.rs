//! Print dialog — place the finished image on a sheet, then hand the
//! job to the system print dialog.
//!
//! This window owns the *placement*: paper, orientation, margins,
//! scaling, rotation and position. Printer, copies and quality stay
//! with `gtk::PrintOperation`'s own dialog, which opens on Print. The
//! placement is recomputed at draw time from the page the print dialog
//! actually settles on, so switching paper there still lands correctly
//! on the sheet.

use std::cell::{Cell, RefCell};
use std::f64::consts::FRAC_PI_2;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::gdk;
use relm4::gtk::gdk::prelude::GdkCairoContextExt;
use relm4::gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};
use relm4::gtk::prelude::*;
use relm4::gtk::{PageOrientation, PageSetup, PaperSize, PrintSettings, Unit};

use crate::configuration::APP_CONFIG;
use crate::notification::log_result;

/// Screenshot pixels are screen pixels and the desktop draws at 96 dpi,
/// so "100 %" puts the image on paper at the size it had on screen.
const REFERENCE_DPI: f64 = 96.0;
const PT_PER_MM: f64 = 72.0 / 25.4;

/// The preview redraws on every option change; resampling a full 4K
/// screenshot each time is wasted work, so it draws from a copy capped
/// at this edge length. The printed image is always the original.
const PREVIEW_MAX_EDGE: i32 = 1600;

/// Half of a spin button's last displayed digit. A `GtkSpinButton`
/// re-snaps its value to the shown precision whenever focus leaves it
/// and emits `value-changed` doing so, which would otherwise read as
/// the user typing a size and flip the mode to `Fit::Custom` just for
/// tabbing past the field. A change smaller than this is that echo.
const SPIN_EPSILON: f64 = 0.05;

/// Keep a dragged image at least this many points on the sheet — one
/// parked entirely off-page prints a blank sheet with no hint why.
const MIN_ON_PAGE: f64 = 20.0;

thread_local! {
    /// Printer, copies and quality carry across prints in one session;
    /// the GTK print dialog itself starts from scratch every time.
    static LAST_SETTINGS: RefCell<Option<PrintSettings>> = const { RefCell::new(None) };
    /// Same for the sheet — re-opening Print should not throw away the
    /// paper and margins the user just set up.
    static LAST_PAGE_SETUP: RefCell<Option<PageSetup>> = const { RefCell::new(None) };
}

/// How the image is sized against the printable area.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    /// Largest size that fits entirely inside the margins.
    Page,
    /// Cover the printable area; the overflowing edges are clipped.
    Fill,
    /// 100 % — one image pixel per 1/96 inch of paper.
    Actual,
    /// Whatever the scale / width / height fields say.
    Custom,
}

impl Fit {
    const LABELS: [&'static str; 4] = ["Fit to page", "Fill page", "Actual size", "Custom"];

    fn index(self) -> u32 {
        match self {
            Self::Page => 0,
            Self::Fill => 1,
            Self::Actual => 2,
            Self::Custom => 3,
        }
    }

    fn from_index(index: u32) -> Self {
        match index {
            1 => Self::Fill,
            2 => Self::Actual,
            3 => Self::Custom,
            _ => Self::Page,
        }
    }
}

/// Where the image goes on a sheet of a given size. Holds no GTK
/// object, so the geometry stands on its own — the page it is measured
/// against is always passed in, which is also what lets the printed
/// page use the paper the print dialog settled on rather than the one
/// the preview was drawn with.
#[derive(Clone)]
struct Placement {
    /// One margin on all four sides, in millimeters. `PageSetup`
    /// carries four, but a single spin button that means what it says
    /// beats four defaults the preview would silently disagree with —
    /// the page setup's largest side becomes the uniform value, so the
    /// image never lands inside a margin the printer cannot reach.
    margin_mm: f64,
    fit: Fit,
    /// 1.0 = the image at `REFERENCE_DPI`. Only read for `Fit::Custom`.
    scale: f64,
    /// Turn the image 90° clockwise on the sheet (the paper keeps its
    /// own orientation).
    rotate: bool,
    /// Top-left corner on the page, in points. `None` = follow `align`.
    offset: Option<(f64, f64)>,
    /// Horizontal and vertical alignment inside the printable area:
    /// 0.0 = start, 0.5 = center, 1.0 = end.
    align: (f64, f64),
    image_px: (f64, f64),
}

impl Placement {
    /// The printable rectangle (x, y, width, height) in points.
    fn area(&self, page_w: f64, page_h: f64) -> (f64, f64, f64, f64) {
        let margin = self.margin_mm * PT_PER_MM;
        (
            margin,
            margin,
            (page_w - 2.0 * margin).max(1.0),
            (page_h - 2.0 * margin).max(1.0),
        )
    }

    /// The image at 100 %, in points, with rotation applied.
    fn natural(&self) -> (f64, f64) {
        let (w, h) = self.image_px;
        let (w, h) = if self.rotate { (h, w) } else { (w, h) };
        (w * 72.0 / REFERENCE_DPI, h * 72.0 / REFERENCE_DPI)
    }

    fn scale_for(&self, page_w: f64, page_h: f64) -> f64 {
        let (nw, nh) = self.natural();
        if nw <= 0.0 || nh <= 0.0 {
            return 1.0;
        }
        let (_, _, aw, ah) = self.area(page_w, page_h);
        match self.fit {
            Fit::Page => (aw / nw).min(ah / nh),
            Fit::Fill => (aw / nw).max(ah / nh),
            Fit::Actual => 1.0,
            Fit::Custom => self.scale,
        }
    }

    /// Where the image lands on the page: (x, y, width, height) in points.
    fn image_rect(&self, page_w: f64, page_h: f64) -> (f64, f64, f64, f64) {
        let scale = self.scale_for(page_w, page_h);
        let (nw, nh) = self.natural();
        let (w, h) = (nw * scale, nh * scale);
        let (ax, ay, aw, ah) = self.area(page_w, page_h);
        let (x, y) = match self.offset {
            Some(offset) => offset,
            None => (ax + (aw - w) * self.align.0, ay + (ah - h) * self.align.1),
        };
        (x, y, w, h)
    }

    fn clamp_offset(&mut self, page_w: f64, page_h: f64) {
        let Some((x, y)) = self.offset else { return };
        let (pw, ph) = (page_w, page_h);
        let (_, _, w, h) = self.image_rect(pw, ph);
        self.offset = Some((
            x.clamp(MIN_ON_PAGE - w, (pw - MIN_ON_PAGE).max(MIN_ON_PAGE - w)),
            y.clamp(MIN_ON_PAGE - h, (ph - MIN_ON_PAGE).max(MIN_ON_PAGE - h)),
        ));
    }

    /// Dots per inch the printer is asked for at the current size.
    fn effective_dpi(&self, page_w: f64, page_h: f64) -> f64 {
        let (_, _, w, _) = self.image_rect(page_w, page_h);
        if w <= 0.0 {
            return 0.0;
        }
        let px = if self.rotate {
            self.image_px.1
        } else {
            self.image_px.0
        };
        px / (w / 72.0)
    }
}

/// A `Placement` bound to the sheet it is currently shown on. Cloned
/// into the print operation, so the dialog may close while the job runs.
#[derive(Clone)]
struct Layout {
    page: PageSetup,
    image: Placement,
}

impl Layout {
    fn page_size(&self) -> (f64, f64) {
        (
            self.page.paper_width(Unit::Points),
            self.page.paper_height(Unit::Points),
        )
    }

    fn clamp_offset(&mut self) {
        let (pw, ph) = self.page_size();
        self.image.clamp_offset(pw, ph);
    }
}

/// The widest margin the page setup asks for, in millimeters —
/// applied to all four sides. Shrinking any side to match a narrower
/// one could push the image into the printer's unprintable border.
fn uniform_margin(page: &PageSetup) -> f64 {
    [
        page.top_margin(Unit::Mm),
        page.right_margin(Unit::Mm),
        page.bottom_margin(Unit::Mm),
        page.left_margin(Unit::Mm),
    ]
    .into_iter()
    .fold(0.0, f64::max)
}

/// Paper is white, and print backends vary in how they handle an alpha
/// channel; compositing here keeps that out of the job entirely.
fn flatten_on_white(image: &Pixbuf) -> Pixbuf {
    if !image.has_alpha() {
        return image.clone();
    }
    let Some(opaque) = Pixbuf::new(Colorspace::Rgb, false, 8, image.width(), image.height()) else {
        return image.clone();
    };
    opaque.fill(0xffffffff);
    image.composite(
        &opaque,
        0,
        0,
        image.width(),
        image.height(),
        0.0,
        0.0,
        1.0,
        1.0,
        InterpType::Nearest,
        255,
    );
    opaque
}

fn preview_copy(image: &Pixbuf) -> Pixbuf {
    let (w, h) = (image.width(), image.height());
    let longest = w.max(h);
    if longest <= PREVIEW_MAX_EDGE || longest <= 0 {
        return image.clone();
    }
    let factor = f64::from(PREVIEW_MAX_EDGE) / f64::from(longest);
    let scaled_w = ((f64::from(w) * factor).round() as i32).max(1);
    let scaled_h = ((f64::from(h) * factor).round() as i32).max(1);
    image
        .scale_simple(scaled_w, scaled_h, InterpType::Bilinear)
        .unwrap_or_else(|| image.clone())
}

/// Paint `image` into the (x, y, w, h) rectangle, optionally rotated a
/// quarter turn and clipped to `clip`. Shared by the preview and the
/// printed page so what is on screen is what comes out of the printer.
fn draw_image(
    cr: &cairo::Context,
    image: &Pixbuf,
    place: (f64, f64, f64, f64),
    rotate: bool,
    clip: Option<(f64, f64, f64, f64)>,
) {
    let (x, y, w, h) = place;
    let (iw, ih) = (f64::from(image.width()), f64::from(image.height()));
    if iw <= 0.0 || ih <= 0.0 || w <= 0.0 || h <= 0.0 {
        return;
    }
    let _ = cr.save();
    if let Some((cx, cy, cw, ch)) = clip {
        cr.rectangle(cx, cy, cw, ch);
        cr.clip();
    }
    if rotate {
        // Pivot around the target rectangle's top-right corner: after a
        // clockwise quarter turn the image's own width runs down the
        // page, so it spans `h`, and its height spans `w`.
        cr.translate(x + w, y);
        cr.rotate(FRAC_PI_2);
        cr.scale(h / iw, w / ih);
    } else {
        cr.translate(x, y);
        cr.scale(w / iw, h / ih);
    }
    cr.set_source_pixbuf(image, 0.0, 0.0);
    cr.source().set_filter(cairo::Filter::Good);
    cr.rectangle(0.0, 0.0, iw, ih);
    let _ = cr.fill();
    let _ = cr.restore();
}

struct Ui {
    layout: RefCell<Layout>,
    /// True while `refresh` writes the widgets from the layout, so the
    /// value-changed handlers don't read their own echo back.
    syncing: Cell<bool>,
    /// Page origin and scale of the last preview draw, in widget
    /// coordinates. The drag handler converts pointer motion with it.
    view: Cell<(f64, f64, f64)>,
    drag_start: Cell<(f64, f64)>,
    preview: gtk::DrawingArea,
    preview_image: Pixbuf,
    print_image: Pixbuf,
    paper_button: gtk::Button,
    orientation: gtk::DropDown,
    fit: gtk::DropDown,
    scale: gtk::SpinButton,
    width_mm: gtk::SpinButton,
    height_mm: gtk::SpinButton,
    margin: gtk::SpinButton,
    rotate: gtk::ToggleButton,
    align_buttons: Vec<gtk::ToggleButton>,
    info: gtk::Label,
}

/// Push the layout into every control and repaint the sheet. The single
/// direction — layout to widgets, never widget to widget — is what
/// keeps the four coupled size fields (fit, scale, width, height) from
/// chasing each other.
fn refresh(ui: &Rc<Ui>) {
    ui.syncing.set(true);
    {
        let layout = ui.layout.borrow();
        let (pw, ph) = layout.page_size();
        let (_, _, w, h) = layout.image.image_rect(pw, ph);

        ui.fit.set_selected(layout.image.fit.index());
        ui.scale.set_value(layout.image.scale_for(pw, ph) * 100.0);
        ui.width_mm.set_value(w / PT_PER_MM);
        ui.height_mm.set_value(h / PT_PER_MM);
        ui.margin.set_value(layout.image.margin_mm);
        ui.rotate.set_active(layout.image.rotate);
        ui.orientation
            .set_selected(match layout.page.orientation() {
                PageOrientation::Landscape | PageOrientation::ReverseLandscape => 1,
                _ => 0,
            });
        ui.paper_button.set_label(&format!(
            "{} — {:.0} × {:.0} mm",
            layout.page.paper_size().display_name(),
            layout.page.paper_width(Unit::Mm),
            layout.page.paper_height(Unit::Mm),
        ));

        for (index, button) in ui.align_buttons.iter().enumerate() {
            let anchor = ((index % 3) as f64 * 0.5, (index / 3) as f64 * 0.5);
            button.set_active(layout.image.offset.is_none() && layout.image.align == anchor);
        }

        ui.info.set_label(&format!(
            "{:.0} × {:.0} px  →  {:.1} × {:.1} mm at {:.0} dpi",
            layout.image.image_px.0,
            layout.image.image_px.1,
            w / PT_PER_MM,
            h / PT_PER_MM,
            layout.image.effective_dpi(pw, ph),
        ));
    }
    ui.syncing.set(false);
    ui.preview.queue_draw();
}

fn draw_preview(ui: &Rc<Ui>, cr: &cairo::Context, width: i32, height: i32) {
    let layout = ui.layout.borrow();
    let (pw, ph) = layout.page_size();
    if pw <= 0.0 || ph <= 0.0 {
        return;
    }
    let pad = 20.0;
    let avail_w = (f64::from(width) - 2.0 * pad).max(1.0);
    let avail_h = (f64::from(height) - 2.0 * pad).max(1.0);
    let scale = (avail_w / pw).min(avail_h / ph);
    let origin_x = (f64::from(width) - pw * scale) / 2.0;
    let origin_y = (f64::from(height) - ph * scale) / 2.0;
    ui.view.set((origin_x, origin_y, scale));

    let _ = cr.save();
    cr.translate(origin_x, origin_y);
    cr.scale(scale, scale);

    // The sheet: a soft drop shadow, white paper, then the image. Paper
    // is white in either theme — it is paper, not a UI surface.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.18);
    cr.rectangle(3.0 / scale, 3.0 / scale, pw, ph);
    let _ = cr.fill();
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.rectangle(0.0, 0.0, pw, ph);
    let _ = cr.fill();

    let place = layout.image.image_rect(pw, ph);
    let clip = (layout.image.fit == Fit::Fill).then(|| layout.image.area(pw, ph));
    draw_image(cr, &ui.preview_image, place, layout.image.rotate, clip);

    let (ax, ay, aw, ah) = layout.image.area(pw, ph);
    cr.set_line_width(1.0 / scale);
    cr.set_dash(&[5.0 / scale, 4.0 / scale], 0.0);
    cr.set_source_rgba(0.25, 0.5, 0.95, 0.7);
    cr.rectangle(ax, ay, aw, ah);
    let _ = cr.stroke();
    cr.set_dash(&[], 0.0);

    cr.set_source_rgba(0.0, 0.0, 0.0, 0.45);
    cr.rectangle(0.0, 0.0, pw, ph);
    let _ = cr.stroke();
    let _ = cr.restore();
}

/// Build the one-page job. Split out from `run_print` so the export
/// test drives the exact same drawing code the printer gets.
fn build_operation(layout: &Layout, image: &Pixbuf) -> gtk::PrintOperation {
    let operation = gtk::PrintOperation::new();
    operation.set_n_pages(1);
    operation.set_unit(Unit::Points);
    // Lay out against the full sheet and apply our own margins, rather
    // than letting GTK inset the context by the page setup's margins on
    // top of the ones the preview already accounted for.
    operation.set_use_full_page(true);
    operation.set_embed_page_setup(true);
    operation.set_job_name("Tensaku screenshot");
    operation.set_default_page_setup(Some(&layout.page));

    let draw_layout = layout.clone();
    let draw_image_source = image.clone();
    operation.connect_draw_page(move |_, context, _| {
        let cr = context.cairo_context();
        // The print dialog may have moved the job onto different paper,
        // so lay out against what it settled on rather than the preview.
        let (pw, ph) = (context.width(), context.height());
        let place = draw_layout.image.image_rect(pw, ph);
        let clip = (draw_layout.image.fit == Fit::Fill).then(|| draw_layout.image.area(pw, ph));
        draw_image(
            &cr,
            &draw_image_source,
            place,
            draw_layout.image.rotate,
            clip,
        );
    });

    operation
}

/// Run the system print dialog and, if the user confirms, the job.
/// Returns true when the job was handed to the printer.
fn run_print(parent: &gtk::Window, layout: &Layout, image: &Pixbuf) -> bool {
    let operation = build_operation(layout, image);
    if let Some(settings) = LAST_SETTINGS.with(|settings| settings.borrow().clone()) {
        operation.set_print_settings(Some(&settings));
    }

    let outcome = operation.run(gtk::PrintOperationAction::PrintDialog, Some(parent));

    if let Some(settings) = operation.print_settings() {
        LAST_SETTINGS.with(|slot| *slot.borrow_mut() = Some(settings));
    }
    LAST_PAGE_SETUP.with(|slot| *slot.borrow_mut() = Some(layout.page.copy()));

    match outcome {
        Ok(gtk::PrintOperationResult::Apply | gtk::PrintOperationResult::InProgress) => {
            log_result(
                "Sent to printer.",
                !APP_CONFIG.read().disable_notifications(),
            );
            true
        }
        Ok(_) => false,
        Err(error) => {
            eprintln!("Printing failed: {error}");
            let message = gtk::MessageDialog::builder()
                .modal(true)
                .message_type(gtk::MessageType::Error)
                .buttons(gtk::ButtonsType::Ok)
                .text("Could not print the image")
                .secondary_text(error.to_string())
                .build();
            message.set_transient_for(Some(parent));
            message.connect_response(|dialog, _| dialog.close());
            message.present();
            false
        }
    }
}

fn labeled_row(label: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let text = gtk::Label::builder()
        .label(label)
        .halign(gtk::Align::Start)
        .hexpand(true)
        .build();
    row.append(&text);
    row.append(control);
    row
}

fn section(title: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(title)
        .halign(gtk::Align::Start)
        .margin_top(6)
        .build();
    label.add_css_class("title-4");
    label
}

/// Open the print preview for `image`, parented on `parent`.
pub fn open(image: &Pixbuf, parent: Option<&gtk::Window>) {
    let print_image = flatten_on_white(image);
    let preview_image = preview_copy(&print_image);
    let image_px = (
        f64::from(print_image.width()),
        f64::from(print_image.height()),
    );

    let remembered = LAST_PAGE_SETUP.with(|slot| slot.borrow().as_ref().map(PageSetup::copy));
    let page = remembered.unwrap_or_else(|| {
        let setup = PageSetup::new();
        setup.set_paper_size_and_default_margins(&PaperSize::new(None));
        // A wide screenshot on portrait paper wastes half the sheet;
        // start from the orientation that matches the image and let the
        // user override it. Only on the first open — after that the
        // remembered sheet wins.
        if image_px.0 > image_px.1 {
            setup.set_orientation(PageOrientation::Landscape);
        }
        setup
    });

    let layout = Layout {
        page: page.clone(),
        image: Placement {
            margin_mm: uniform_margin(&page),
            fit: Fit::Page,
            scale: 1.0,
            rotate: false,
            offset: None,
            align: (0.5, 0.5),
            image_px,
        },
    };

    let window = gtk::Window::builder()
        .title("Print")
        .modal(true)
        .destroy_with_parent(true)
        .default_width(900)
        .default_height(720)
        .build();
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
    }

    let preview = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .content_width(400)
        .content_height(440)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .tooltip_text("Drag the image to place it on the sheet")
        .build();

    let paper_button = gtk::Button::builder().hexpand(true).build();
    let orientation = gtk::DropDown::from_strings(&["Portrait", "Landscape"]);
    let fit = gtk::DropDown::from_strings(&Fit::LABELS);
    let scale = gtk::SpinButton::with_range(1.0, 2000.0, 5.0);
    scale.set_digits(1);
    let width_mm = gtk::SpinButton::with_range(1.0, 5000.0, 1.0);
    width_mm.set_digits(1);
    let height_mm = gtk::SpinButton::with_range(1.0, 5000.0, 1.0);
    height_mm.set_digits(1);
    let margin = gtk::SpinButton::with_range(0.0, 100.0, 1.0);
    margin.set_digits(1);
    let rotate = gtk::ToggleButton::with_label("Rotate 90°");
    let info = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .wrap(true)
        .xalign(0.0)
        .build();
    info.add_css_class("dim-label");

    // Nine anchors, laid out the way they sit on the sheet.
    let align_grid = gtk::Grid::builder()
        .row_spacing(4)
        .column_spacing(4)
        .halign(gtk::Align::End)
        .build();
    let mut align_buttons = Vec::with_capacity(9);
    for (index, glyph) in ["↖", "↑", "↗", "←", "•", "→", "↙", "↓", "↘"]
        .into_iter()
        .enumerate()
    {
        let button = gtk::ToggleButton::builder()
            .label(glyph)
            .width_request(34)
            .height_request(30)
            .build();
        align_grid.attach(&button, (index % 3) as i32, (index / 3) as i32, 1, 1);
        align_buttons.push(button);
    }

    let ui = Rc::new(Ui {
        layout: RefCell::new(layout),
        syncing: Cell::new(false),
        view: Cell::new((0.0, 0.0, 1.0)),
        drag_start: Cell::new((0.0, 0.0)),
        preview: preview.clone(),
        preview_image,
        print_image,
        paper_button: paper_button.clone(),
        orientation: orientation.clone(),
        fit: fit.clone(),
        scale: scale.clone(),
        width_mm: width_mm.clone(),
        height_mm: height_mm.clone(),
        margin: margin.clone(),
        rotate: rotate.clone(),
        align_buttons: align_buttons.clone(),
        info: info.clone(),
    });

    // --- controls -------------------------------------------------
    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .width_request(300)
        .build();

    controls.append(&section("Paper"));
    controls.append(&paper_button);
    controls.append(&labeled_row("Orientation", &orientation));
    controls.append(&labeled_row("Margin (mm)", &margin));

    controls.append(&section("Size"));
    controls.append(&labeled_row("Scaling", &fit));
    controls.append(&labeled_row("Scale (%)", &scale));
    controls.append(&labeled_row("Width (mm)", &width_mm));
    controls.append(&labeled_row("Height (mm)", &height_mm));
    controls.append(&labeled_row("Image", &rotate));

    controls.append(&section("Position"));
    controls.append(&labeled_row("Anchor", &align_grid));
    controls.append(&info);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .child(&controls)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.set_vexpand(true);
    content.append(&preview);
    content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    content.append(&scroller);

    let cancel_button = gtk::Button::with_label("Cancel");
    let print_button = gtk::Button::with_label("Print…");
    print_button.add_css_class("suggested-action");
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(16)
        .margin_end(16)
        .halign(gtk::Align::End)
        .build();
    actions.append(&cancel_button);
    actions.append(&print_button);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&content);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&actions);
    window.set_child(Some(&root));

    // --- behavior -------------------------------------------------
    {
        let ui = ui.clone();
        preview.set_draw_func(move |_, cr, width, height| draw_preview(&ui, cr, width, height));
    }

    let drag = gtk::GestureDrag::new();
    {
        let ui = ui.clone();
        drag.connect_drag_begin(move |_, _, _| {
            let start = {
                let layout = ui.layout.borrow();
                let (pw, ph) = layout.page_size();
                let (x, y, _, _) = layout.image.image_rect(pw, ph);
                (x, y)
            };
            ui.drag_start.set(start);
        });
    }
    {
        let ui = ui.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let (_, _, scale) = ui.view.get();
            if scale <= 0.0 {
                return;
            }
            let (start_x, start_y) = ui.drag_start.get();
            {
                let mut layout = ui.layout.borrow_mut();
                layout.image.offset = Some((start_x + dx / scale, start_y + dy / scale));
                layout.clamp_offset();
            }
            refresh(&ui);
        });
    }
    preview.add_controller(drag);

    {
        let ui = ui.clone();
        let window = window.clone();
        paper_button.connect_clicked(move |_| {
            let settings = LAST_SETTINGS
                .with(|slot| slot.borrow().clone())
                .unwrap_or_default();
            let current = ui.layout.borrow().page.copy();
            let updated =
                gtk::print_run_page_setup_dialog(Some(&window), Some(&current), &settings);
            {
                let mut layout = ui.layout.borrow_mut();
                layout.image.margin_mm = uniform_margin(&updated);
                layout.page = updated;
                layout.clamp_offset();
            }
            refresh(&ui);
        });
    }

    {
        let ui = ui.clone();
        orientation.connect_selected_notify(move |dropdown| {
            if ui.syncing.get() {
                return;
            }
            {
                let mut layout = ui.layout.borrow_mut();
                layout.page.set_orientation(if dropdown.selected() == 1 {
                    PageOrientation::Landscape
                } else {
                    PageOrientation::Portrait
                });
                // The sheet changed shape under it; re-anchor rather
                // than leave the image hanging off the new edge.
                layout.image.offset = None;
            }
            refresh(&ui);
        });
    }

    {
        let ui = ui.clone();
        fit.connect_selected_notify(move |dropdown| {
            if ui.syncing.get() {
                return;
            }
            {
                let mut layout = ui.layout.borrow_mut();
                let (pw, ph) = layout.page_size();
                // Switching *to* Custom keeps the size it had, so the
                // image doesn't jump the moment the mode changes.
                let current = layout.image.scale_for(pw, ph);
                layout.image.fit = Fit::from_index(dropdown.selected());
                if layout.image.fit == Fit::Custom {
                    layout.image.scale = current;
                }
                layout.image.offset = None;
            }
            refresh(&ui);
        });
    }

    {
        let ui = ui.clone();
        scale.connect_value_changed(move |spin| {
            if ui.syncing.get() {
                return;
            }
            {
                let mut layout = ui.layout.borrow_mut();
                let (pw, ph) = layout.page_size();
                if (spin.value() - layout.image.scale_for(pw, ph) * 100.0).abs() < SPIN_EPSILON {
                    return;
                }
                layout.image.fit = Fit::Custom;
                layout.image.scale = (spin.value() / 100.0).max(0.001);
                layout.clamp_offset();
            }
            refresh(&ui);
        });
    }

    {
        let ui = ui.clone();
        width_mm.connect_value_changed(move |spin| {
            if ui.syncing.get() {
                return;
            }
            {
                let mut layout = ui.layout.borrow_mut();
                let (pw, ph) = layout.page_size();
                let (_, _, shown, _) = layout.image.image_rect(pw, ph);
                if (spin.value() - shown / PT_PER_MM).abs() < SPIN_EPSILON {
                    return;
                }
                let (nw, _) = layout.image.natural();
                if nw > 0.0 {
                    layout.image.fit = Fit::Custom;
                    layout.image.scale = (spin.value() * PT_PER_MM / nw).max(0.001);
                    layout.clamp_offset();
                }
            }
            refresh(&ui);
        });
    }

    {
        let ui = ui.clone();
        height_mm.connect_value_changed(move |spin| {
            if ui.syncing.get() {
                return;
            }
            {
                let mut layout = ui.layout.borrow_mut();
                let (pw, ph) = layout.page_size();
                let (_, _, _, shown) = layout.image.image_rect(pw, ph);
                if (spin.value() - shown / PT_PER_MM).abs() < SPIN_EPSILON {
                    return;
                }
                let (_, nh) = layout.image.natural();
                if nh > 0.0 {
                    layout.image.fit = Fit::Custom;
                    layout.image.scale = (spin.value() * PT_PER_MM / nh).max(0.001);
                    layout.clamp_offset();
                }
            }
            refresh(&ui);
        });
    }

    {
        let ui = ui.clone();
        margin.connect_value_changed(move |spin| {
            if ui.syncing.get() {
                return;
            }
            {
                let mut layout = ui.layout.borrow_mut();
                if (spin.value() - layout.image.margin_mm).abs() < SPIN_EPSILON {
                    return;
                }
                layout.image.margin_mm = spin.value();
                layout.clamp_offset();
            }
            refresh(&ui);
        });
    }

    {
        let ui = ui.clone();
        rotate.connect_toggled(move |button| {
            if ui.syncing.get() {
                return;
            }
            {
                let mut layout = ui.layout.borrow_mut();
                layout.image.rotate = button.is_active();
                layout.image.offset = None;
            }
            refresh(&ui);
        });
    }

    for (index, button) in align_buttons.iter().enumerate() {
        let ui = ui.clone();
        let anchor = ((index % 3) as f64 * 0.5, (index / 3) as f64 * 0.5);
        button.connect_toggled(move |button| {
            if ui.syncing.get() {
                return;
            }
            // Clicking the active anchor again is a no-op: `refresh`
            // puts it straight back, since one anchor is always in
            // effect unless the image was dragged somewhere custom.
            if button.is_active() {
                let mut layout = ui.layout.borrow_mut();
                layout.image.align = anchor;
                layout.image.offset = None;
            }
            refresh(&ui);
        });
    }

    {
        let window = window.clone();
        cancel_button.connect_clicked(move |_| window.close());
    }

    {
        let ui = ui.clone();
        let window = window.clone();
        print_button.connect_clicked(move |_| {
            let layout = ui.layout.borrow().clone();
            if run_print(&window, &layout, &ui.print_image) {
                window.close();
            }
        });
    }

    {
        let window_for_keys = window.clone();
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if key == gdk::Key::Escape && modifiers.is_empty() {
                window_for_keys.close();
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
        window.add_controller(keys);
    }

    // The draw closure holds the `Ui`, which holds the drawing area —
    // a cycle that would outlive the dialog. Drop it on close.
    {
        let preview = preview.clone();
        window.connect_close_request(move |_| {
            preview.set_draw_func(|_, _, _, _| {});
            gtk::glib::Propagation::Proceed
        });
    }

    // Enter prints, the way it confirms any other dialog. Focus has to
    // start on the button too: a focused button activates itself on
    // Enter, so leaving focus on the first control in the column would
    // hand Enter to Page Setup instead.
    print_button.set_receives_default(true);
    window.set_default_widget(Some(&print_button));
    gtk::prelude::GtkWindowExt::set_focus(&window, Some(&print_button));

    refresh(&ui);
    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A4 in points: 595.276 × 841.89.
    const A4_W: f64 = 210.0 * PT_PER_MM;
    const A4_H: f64 = 297.0 * PT_PER_MM;

    /// 10 mm margins and a 1000 × 500 px image.
    fn placement() -> Placement {
        Placement {
            margin_mm: 10.0,
            fit: Fit::Page,
            scale: 1.0,
            rotate: false,
            offset: None,
            align: (0.5, 0.5),
            image_px: (1000.0, 500.0),
        }
    }

    fn mm(points: f64) -> f64 {
        points / PT_PER_MM
    }

    #[test]
    fn fit_modes_size_the_image_against_the_printable_area() {
        let mut image = placement();
        // A4 portrait, 10 mm all round: 190 × 277 mm printable.
        let (ax, ay, aw, ah) = image.area(A4_W, A4_H);
        assert!((mm(ax) - 10.0).abs() < 0.01 && (mm(ay) - 10.0).abs() < 0.01);
        assert!((mm(aw) - 190.0).abs() < 0.01, "{}", mm(aw));
        assert!((mm(ah) - 277.0).abs() < 0.01, "{}", mm(ah));

        // A 2:1 image in a taller area — the width binds, and what is
        // left of the height is split above and below.
        let (x, y, w, h) = image.image_rect(A4_W, A4_H);
        assert!((mm(w) - 190.0).abs() < 0.01, "{}", mm(w));
        assert!((mm(h) - 95.0).abs() < 0.01, "{}", mm(h));
        assert!((mm(x) - 10.0).abs() < 0.01);
        assert!(
            (mm(y) - (10.0 + (277.0 - 95.0) / 2.0)).abs() < 0.01,
            "{}",
            mm(y)
        );

        // Fill covers the area instead, overflowing the other axis.
        image.fit = Fit::Fill;
        let (_, _, w, h) = image.image_rect(A4_W, A4_H);
        assert!((mm(w) - 554.0).abs() < 0.01, "{}", mm(w));
        assert!((mm(h) - 277.0).abs() < 0.01, "{}", mm(h));

        // Actual size: 1000 px at 96 dpi.
        image.fit = Fit::Actual;
        let (_, _, w, _) = image.image_rect(A4_W, A4_H);
        assert!((mm(w) - 1000.0 * 25.4 / 96.0).abs() < 0.01, "{}", mm(w));

        // Custom follows the scale field.
        image.fit = Fit::Custom;
        image.scale = 0.5;
        let (_, _, w, h) = image.image_rect(A4_W, A4_H);
        assert!((mm(w) - 500.0 * 25.4 / 96.0).abs() < 0.01, "{}", mm(w));
        assert!((mm(h) - 250.0 * 25.4 / 96.0).abs() < 0.01, "{}", mm(h));
    }

    #[test]
    fn rotating_swaps_the_binding_side_and_keeps_the_aspect_ratio() {
        let mut image = placement();
        image.rotate = true;
        let (_, _, w, h) = image.image_rect(A4_W, A4_H);
        // The 2:1 image stands on its side, so it reads 1:2 on the page
        // and the binding side flips with it: the 277 mm height now
        // limits the fit, where unrotated the 190 mm width did.
        assert!((mm(h) - 277.0).abs() < 0.01, "{}", mm(h));
        assert!((mm(w) - 138.5).abs() < 0.01, "{}", mm(w));
        assert!((w / h - 0.5).abs() < 0.001);
    }

    #[test]
    fn a_landscape_sheet_prints_the_same_image_larger() {
        let image = placement();
        let (_, _, portrait_w, _) = image.image_rect(A4_W, A4_H);
        let (_, _, landscape_w, _) = image.image_rect(A4_H, A4_W);
        assert!((mm(portrait_w) - 190.0).abs() < 0.01);
        assert!(
            (mm(landscape_w) - 277.0).abs() < 0.01,
            "{}",
            mm(landscape_w)
        );
    }

    #[test]
    fn alignment_and_dragging_place_the_image_and_keep_it_on_the_sheet() {
        let mut image = placement();

        image.align = (0.0, 0.0);
        let (x, y, _, _) = image.image_rect(A4_W, A4_H);
        assert!((mm(x) - 10.0).abs() < 0.01 && (mm(y) - 10.0).abs() < 0.01);

        image.align = (1.0, 1.0);
        let (x, y, w, h) = image.image_rect(A4_W, A4_H);
        assert!((mm(x + w) - 200.0).abs() < 0.01, "{}", mm(x + w));
        assert!((mm(y + h) - 287.0).abs() < 0.01, "{}", mm(y + h));

        // An explicit offset wins over the anchor…
        image.offset = Some((50.0, 60.0));
        let (x, y, _, _) = image.image_rect(A4_W, A4_H);
        assert_eq!((x, y), (50.0, 60.0));

        // …but dragging it clean off the sheet is pulled back to an
        // edge, so a job can never come out as an unexplained blank page.
        image.offset = Some((-10_000.0, 10_000.0));
        image.clamp_offset(A4_W, A4_H);
        let (x, y, w, _) = image.image_rect(A4_W, A4_H);
        assert!((x - (MIN_ON_PAGE - w)).abs() < 0.01, "{x}");
        assert!((y - (A4_H - MIN_ON_PAGE)).abs() < 0.01, "{y}");
    }

    /// End-to-end through GTK's own print plumbing: the same operation
    /// the Print button builds, exported to PDF instead of sent to a
    /// printer. Covers what the geometry tests cannot see — the page
    /// callback firing, the unit and full-page settings agreeing with
    /// the placement, and the pixbuf actually reaching the page.
    #[test]
    #[ignore = "requires a GTK display; run with --ignored"]
    fn exports_the_placed_image_as_a_one_page_pdf() {
        gtk::init().expect("gtk init");
        let image = Pixbuf::new(Colorspace::Rgb, false, 8, 400, 200).unwrap();
        image.fill(0xd23a2aff);

        let page = PageSetup::new();
        page.set_paper_size_and_default_margins(&PaperSize::new(Some("iso_a4")));
        let layout = Layout {
            page,
            image: Placement {
                image_px: (400.0, 200.0),
                ..placement()
            },
        };

        let path =
            std::env::temp_dir().join(format!("tensaku-print-test-{}.pdf", std::process::id()));
        let operation = build_operation(&layout, &image);
        operation.set_export_filename(&path);
        let outcome = operation
            .run(gtk::PrintOperationAction::Export, None::<&gtk::Window>)
            .expect("export");
        assert!(matches!(outcome, gtk::PrintOperationResult::Apply));

        let bytes = std::fs::read(&path).expect("exported pdf");
        std::fs::remove_file(&path).unwrap();
        assert!(bytes.starts_with(b"%PDF"), "not a PDF");
        // Cairo writes the pixbuf as an image XObject at its own pixel
        // size and scales it with the page transform, so these
        // dimensions are proof the image reached the page — a callback
        // that drew nothing still yields a valid, and blank, A4 sheet.
        let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        assert!(contains(b"/Subtype /Image"), "no image on the page");
        assert!(contains(b"/Width 400") && contains(b"/Height 200"));
    }

    #[test]
    fn effective_dpi_reports_what_the_printer_is_asked_for() {
        let mut image = placement();
        // Actual size is 96 dpi by definition.
        image.fit = Fit::Actual;
        assert!((image.effective_dpi(A4_W, A4_H) - 96.0).abs() < 0.01);
        // Half the size packs the same pixels into half the paper.
        image.fit = Fit::Custom;
        image.scale = 0.5;
        assert!((image.effective_dpi(A4_W, A4_H) - 192.0).abs() < 0.01);
        // Rotation measures the side that actually runs across the page.
        image.rotate = true;
        assert!((image.effective_dpi(A4_W, A4_H) - 192.0).abs() < 0.01);
    }
}
