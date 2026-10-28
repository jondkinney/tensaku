//! File format selection and encoding for saved annotations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use relm4::gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};

/// Rotate/flip `pixbuf` to match its embedded EXIF orientation tag.
///
/// `Pixbuf::from_file` and `PixbufLoader` decode raw sensor-orientation
/// pixels and silently ignore any EXIF orientation tag, unlike browsers
/// and most image viewers. A phone photo taken in portrait is commonly
/// stored as landscape pixels plus an orientation tag, so without this
/// it opens sideways. Screenshots carry no such tag, so this is a no-op
/// for anything Tensaku itself captured.
pub fn apply_exif_orientation(pixbuf: Pixbuf) -> Pixbuf {
    pixbuf.apply_embedded_orientation().unwrap_or(pixbuf)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
    Avif,
    Tiff,
    Bmp,
}

impl ImageFormat {
    pub const ALL: [Self; 6] = [
        Self::Png,
        Self::Jpeg,
        Self::Webp,
        Self::Avif,
        Self::Tiff,
        Self::Bmp,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Tiff => "tiff",
            Self::Bmp => "bmp",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Webp => "WebP",
            Self::Avif => "AVIF",
            Self::Tiff => "TIFF",
            Self::Bmp => "BMP",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            _ => self.name(),
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" | "jpe" => Some(Self::Jpeg),
            "webp" => Some(Self::Webp),
            "avif" => Some(Self::Avif),
            "tif" | "tiff" => Some(Self::Tiff),
            "bmp" => Some(Self::Bmp),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(Self::from_name)
    }

    /// Inspect the input bytes rather than trusting a possibly incorrect suffix.
    pub fn from_file(path: &Path) -> Option<Self> {
        Pixbuf::file_info(path)
            .and_then(|(format, _, _)| format.name())
            .and_then(|name| Self::from_name(&name))
    }

    /// Only offer formats that the installed GdkPixbuf codecs can write.
    pub fn available() -> Vec<Self> {
        let codecs = Pixbuf::formats();
        Self::ALL
            .into_iter()
            .filter(|format| {
                codecs.iter().any(|codec| {
                    codec.is_writable() && codec.name().as_deref() == Some(format.name())
                })
            })
            .collect()
    }

    pub fn output_path(self, path: &Path) -> PathBuf {
        if Self::from_path(path) == Some(self) {
            path.to_path_buf()
        } else {
            path.with_extension(self.extension())
        }
    }

    pub fn encode(self, image: &Pixbuf) -> Result<Vec<u8>> {
        // JPEG has no alpha channel; BMP readers also vary in alpha support.
        // Composite onto white instead of dropping alpha (which exposes hidden
        // RGB values) or passing RGBA to a JPEG encoder that rejects it.
        let opaque;
        let image = if image.has_alpha() && matches!(self, Self::Jpeg | Self::Bmp) {
            opaque = Pixbuf::new(Colorspace::Rgb, false, 8, image.width(), image.height())
                .context("Could not allocate the image for export")?;
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
            &opaque
        } else {
            image
        };
        let options: &[(&str, &str)] = if self == Self::Jpeg {
            &[("quality", "95")]
        } else {
            &[]
        };
        image
            .save_to_bufferv(self.name(), options)
            .with_context(|| format!("Could not encode the image as {}", self.label()))
    }
}

pub fn default_format(
    configured: Option<&Path>,
    source: ImageFormat,
    available: &[ImageFormat],
) -> ImageFormat {
    configured
        .and_then(ImageFormat::from_path)
        .filter(|format| available.contains(format))
        .or_else(|| available.contains(&source).then_some(source))
        .unwrap_or_default()
}

/// An explicit format choice wins over the suffix. In automatic mode a known
/// suffix selects the encoder; an absent suffix uses the document's default.
pub fn resolve_output(
    path: &Path,
    selected: Option<ImageFormat>,
    default: ImageFormat,
    available: &[ImageFormat],
) -> Result<(PathBuf, ImageFormat)> {
    let format = if let Some(format) = selected {
        format
    } else if let Some(extension) = path.extension().filter(|ext| !ext.is_empty()) {
        let Some(format) = extension.to_str().and_then(ImageFormat::from_name) else {
            bail!(
                "Unrecognized image extension '.{}'. Choose a file type in Save As, or use a supported extension.",
                extension.to_string_lossy()
            );
        };
        format
    } else {
        default
    };
    if !available.contains(&format) {
        bail!(
            "{} export is unavailable on this system. Choose another file type.",
            format.label()
        );
    }
    Ok((format.output_path(path), format))
}

#[cfg(test)]
mod tests {
    use super::*;
    use relm4::gtk::gdk_pixbuf::{PixbufLoader, prelude::*};

    #[test]
    fn output_format_follows_the_source_filename_or_explicit_choice() {
        use ImageFormat::*;
        for (path, selected, default, expected_path, expected_format) in [
            ("abc", None, Jpeg, "abc.jpg", Jpeg),
            ("abc.", None, Jpeg, "abc.jpg", Jpeg),
            ("abc", None, Png, "abc.png", Png),
            ("abc.JPEG", None, Png, "abc.JPEG", Jpeg),
            ("abc.tif", None, Jpeg, "abc.tif", Tiff),
            ("abc.webp", None, Jpeg, "abc.webp", Webp),
            ("abc.png", Some(Jpeg), Png, "abc.jpg", Jpeg),
            ("abc.jpg", Some(Webp), Jpeg, "abc.webp", Webp),
            ("abc.txt", Some(Png), Jpeg, "abc.png", Png),
            ("a.b/abc", None, Jpeg, "a.b/abc.jpg", Jpeg),
            (".hidden", None, Jpeg, ".hidden.jpg", Jpeg),
        ] {
            assert_eq!(
                resolve_output(Path::new(path), selected, default, &ImageFormat::ALL).unwrap(),
                (PathBuf::from(expected_path), expected_format),
                "{path}, {selected:?}"
            );
        }
        // Unknown suffixes must never receive bytes in a different format.
        assert!(resolve_output(Path::new("abc.gif"), None, Jpeg, &ImageFormat::ALL).is_err());
        assert!(resolve_output(Path::new("abc.webp"), None, Jpeg, &[Png, Jpeg]).is_err());
        assert!(resolve_output(Path::new("abc"), Some(Webp), Jpeg, &[Png, Jpeg]).is_err());
    }

    #[test]
    fn defaults_honor_configured_output_and_fall_back_for_unavailable_encoders() {
        use ImageFormat::*;
        assert_eq!(default_format(None, Jpeg, &ImageFormat::ALL), Jpeg);
        assert_eq!(
            default_format(Some(Path::new("out.webp")), Jpeg, &ImageFormat::ALL),
            Webp
        );
        assert_eq!(
            default_format(Some(Path::new("out")), Jpeg, &ImageFormat::ALL),
            Jpeg
        );
        assert_eq!(default_format(None, Avif, &[Png, Jpeg]), Png);
        assert_eq!(
            default_format(Some(Path::new("out.avif")), Jpeg, &[Png, Jpeg]),
            Jpeg
        );
    }

    fn decode(bytes: &[u8]) -> (ImageFormat, Pixbuf) {
        let loader = PixbufLoader::new();
        loader.write(bytes).unwrap();
        loader.close().unwrap();
        let name = loader.format().unwrap().name().unwrap();
        (
            ImageFormat::from_name(&name).unwrap(),
            loader.pixbuf().unwrap(),
        )
    }

    #[test]
    fn every_available_encoder_writes_the_selected_format_from_rgb_and_rgba() {
        let available = ImageFormat::available();
        assert!(available.contains(&ImageFormat::Png));
        assert!(available.contains(&ImageFormat::Jpeg));
        for format in available {
            for alpha in [false, true] {
                // The odd width also exercises RGB row padding.
                let image = Pixbuf::new(Colorspace::Rgb, alpha, 8, 13, 9).unwrap();
                image.fill(0x80402080);
                let data = format.encode(&image).unwrap();
                let (decoded_format, decoded) = decode(&data);
                assert_eq!(decoded_format, format);
                assert_eq!((decoded.width(), decoded.height()), (13, 9));
                let pixel = decoded.read_pixel_bytes();
                if alpha && matches!(format, ImageFormat::Jpeg | ImageFormat::Bmp) {
                    assert!(!decoded.has_alpha());
                    for (actual, expected) in pixel[..3].iter().zip([191u8, 159, 143]) {
                        assert!(actual.abs_diff(expected) <= 3, "{format:?}: {pixel:?}");
                    }
                } else {
                    for (actual, expected) in pixel[..3].iter().zip([128u8, 64, 32]) {
                        assert!(actual.abs_diff(expected) <= 4, "{format:?}: {pixel:?}");
                    }
                    if alpha {
                        assert!(decoded.has_alpha(), "{format:?} lost alpha");
                        assert!(pixel[3].abs_diff(128) <= 1, "{format:?} changed alpha");
                    }
                }
            }
        }
    }

    #[test]
    fn jpeg_and_bmp_flatten_fully_transparent_pixels_to_white() {
        for format in [ImageFormat::Jpeg, ImageFormat::Bmp] {
            if !ImageFormat::available().contains(&format) {
                continue;
            }
            let image = Pixbuf::new(Colorspace::Rgb, true, 8, 8, 8).unwrap();
            image.fill(0x00000000);
            let (_, decoded) = decode(&format.encode(&image).unwrap());
            assert_eq!(&decoded.read_pixel_bytes()[..3], &[255, 255, 255]);
        }
    }

    #[test]
    fn source_format_is_detected_from_bytes_even_with_a_wrong_extension() {
        let image = Pixbuf::new(Colorspace::Rgb, false, 8, 8, 8).unwrap();
        image.fill(0x804020ff);
        let path =
            std::env::temp_dir().join(format!("tensaku-source-format-{}.png", std::process::id()));
        std::fs::write(&path, ImageFormat::Jpeg.encode(&image).unwrap()).unwrap();
        let detected = ImageFormat::from_file(&path);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(detected, Some(ImageFormat::Jpeg));
    }

    /// `testdata/exif-orientation-6.jpg` is a 24×16 grid of six 8×8
    /// blocks (red, green, blue / yellow, magenta, cyan, in that
    /// raster order) whose *stored* pixels are rotated 90° from that
    /// layout and tagged EXIF `Orientation=6` ("rotate 90° CW to
    /// display correctly") -- the layout a phone actually writes for a
    /// portrait photo. Regenerated with:
    ///
    /// ```python
    /// from PIL import Image
    /// import piexif
    /// img = Image.new("RGB", (24, 16))
    /// colors = [(255,0,0),(0,255,0),(0,0,255),(255,255,0),(255,0,255),(0,255,255)]
    /// for i, c in enumerate(colors):
    ///     cx, cy = i % 3, i // 3
    ///     for y in range(cy*8, cy*8+8):
    ///         for x in range(cx*8, cx*8+8):
    ///             img.putpixel((x, y), c)
    /// exif = piexif.dump({"0th": {piexif.ImageIFD.Orientation: 6}})
    /// img.rotate(90, expand=True).save(
    ///     "exif-orientation-6.jpg", quality=100, subsampling=0, exif=exif
    /// )
    /// ```
    ///
    /// `ImageOps.exif_transpose` (Pillow's own EXIF-orientation
    /// correction, used as the independent reference) confirms this
    /// file corrects back to the intended 24×16 layout above.
    #[test]
    fn apply_exif_orientation_rotates_pixels_to_match_the_tag() {
        let bytes = include_bytes!("testdata/exif-orientation-6.jpg");
        let path = std::env::temp_dir().join(format!(
            "tensaku-exif-orientation-{}.jpg",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        let raw = Pixbuf::from_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        // Unapplied, the file's raw pixel grid is portrait: this is
        // exactly what a viewer ignoring the EXIF tag would show, i.e.
        // the bug being fixed.
        assert_eq!((raw.width(), raw.height()), (16, 24));

        let corrected = apply_exif_orientation(raw);
        assert_eq!((corrected.width(), corrected.height()), (24, 16));

        let expected = [
            (255u8, 0u8, 0u8),
            (0, 255, 0),
            (0, 0, 255),
            (255, 255, 0),
            (255, 0, 255),
            (0, 255, 255),
        ];
        let pixels = corrected.read_pixel_bytes();
        let (stride, channels) = (corrected.rowstride(), corrected.n_channels());
        for (i, expected_color) in expected.iter().enumerate() {
            let (block_x, block_y) = (i % 3, i / 3);
            let (x, y) = (block_x * 8 + 4, block_y * 8 + 4);
            let offset = (y as i32 * stride + x as i32 * channels) as usize;
            let actual = (pixels[offset], pixels[offset + 1], pixels[offset + 2]);
            let close = |a: u8, b: u8| (a as i16 - b as i16).abs() <= 20;
            assert!(
                close(actual.0, expected_color.0)
                    && close(actual.1, expected_color.1)
                    && close(actual.2, expected_color.2),
                "block {i} at ({x},{y}): expected {expected_color:?}, got {actual:?}"
            );
        }
    }
}
