//! File format selection and encoding for saved annotations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use relm4::gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};

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
}
