use std::{
    fmt,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

use crate::model::{RenditionFormat, RenditionRecord, SourceFormat};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

const IMAGE_MAGICK_MEMORY_LIMIT: &str = "256MiB";
const IMAGE_MAGICK_MAP_LIMIT: &str = "512MiB";
const IMAGE_MAGICK_DISK_LIMIT: &str = "512MiB";

/// The profile every rendition is converted to: the compact sRGB v4 profile
/// from Compact ICC Profiles, released under CC0.
pub(crate) const SRGB_PROFILE: &[u8] = include_bytes!("../assets/sRGB-v4.icc");

#[derive(Debug)]
pub enum MediaError {
    Invalid(String),
    LimitExceeded,
    TimedOut,
    ProcessUnavailable(std::io::Error),
    ProcessFailed,
    Io(std::io::Error),
}

impl fmt::Display for MediaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::LimitExceeded => formatter.write_str("image dimensions exceed configured limits"),
            Self::TimedOut => formatter.write_str("ImageMagick exceeded the configured time limit"),
            Self::ProcessUnavailable(error) | Self::Io(error) => error.fmt(formatter),
            Self::ProcessFailed => formatter.write_str("ImageMagick failed to process the image"),
        }
    }
}

impl std::error::Error for MediaError {}

impl From<std::io::Error> for MediaError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone)]
pub struct ImageProcessor {
    executable: String,
    srgb_profile: PathBuf,
    max_pixels: u64,
    max_dimension: u32,
    timeout: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
    pub opaque: bool,
}

impl ImageProcessor {
    pub fn new(
        executable: String,
        srgb_profile: PathBuf,
        max_pixels: u64,
        max_dimension: u32,
        timeout: Duration,
    ) -> Self {
        Self {
            executable,
            srgb_profile,
            max_pixels,
            max_dimension,
            timeout,
        }
    }

    pub async fn inspect(
        &self,
        input_path: &Path,
        input_format: SourceFormat,
        staging_dir: &Path,
    ) -> Result<ImageDimensions, MediaError> {
        let magick_format = input_format
            .magick_name()
            .ok_or_else(|| MediaError::Invalid("The file is not an image.".to_owned()))?;
        let input = format!("{magick_format}:{}", input_path.display());
        let output = self
            .run(
                [
                    input,
                    "-auto-orient".to_owned(),
                    "-format".to_owned(),
                    "%w %h %[opaque]".to_owned(),
                    "info:".to_owned(),
                ],
                staging_dir,
            )
            .await?;

        let dimensions = parse_dimensions(&output.stdout)?;
        let pixels = u64::from(dimensions.width)
            .checked_mul(u64::from(dimensions.height))
            .ok_or(MediaError::LimitExceeded)?;

        if dimensions.width > self.max_dimension
            || dimensions.height > self.max_dimension
            || pixels > self.max_pixels
        {
            return Err(MediaError::LimitExceeded);
        }

        Ok(dimensions)
    }

    pub async fn verify_formats(&self) -> Result<(), MediaError> {
        let mut command = Command::new(&self.executable);
        command
            .args(["-list", "format"])
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let output = timeout(self.timeout, command.output())
            .await
            .map_err(|_| MediaError::TimedOut)?
            .map_err(MediaError::ProcessUnavailable)?;
        if !output.status.success() {
            return Err(MediaError::ProcessFailed);
        }
        let available = read_write_formats(&String::from_utf8_lossy(&output.stdout));

        // Every format that is written out must also be readable back, and
        // now that uploads may be in AVIF, JXL or HEIC, they must be readable
        // too. A build without the delegate refuses to start rather than
        // failing on somebody's first upload.
        for required in ["JPEG", "PNG", "WEBP", "AVIF", "JXL", "HEIC"] {
            if !available.iter().any(|format| format == required) {
                return Err(MediaError::Invalid(format!(
                    "ImageMagick must provide read/write support for {required}."
                )));
            }
        }
        Ok(())
    }

    /// Without LCMS, `-profile` is a warning on stderr, which is discarded,
    /// and every wide-gamut photo would come out washed out without a word.
    pub async fn verify_colour_management(&self) -> Result<(), MediaError> {
        let mut command = Command::new(&self.executable);
        command
            .args(["-list", "configure"])
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let output = timeout(self.timeout, command.output())
            .await
            .map_err(|_| MediaError::TimedOut)?
            .map_err(MediaError::ProcessUnavailable)?;
        if !output.status.success() {
            return Err(MediaError::ProcessFailed);
        }
        if !has_delegate(&String::from_utf8_lossy(&output.stdout), "lcms") {
            return Err(MediaError::Invalid(
                "ImageMagick must be built with LCMS to convert colour profiles.".to_owned(),
            ));
        }
        Ok(())
    }

    /// The settings every rendition is written with, up to the point where
    /// the image has been resized.
    ///
    /// The fidelity test builds its reference from this same list, on
    /// purpose: the reference has to be the image the encoder was given, or
    /// the measurement covers the resampling instead of the encoding.
    fn base_args(&self, input: &str, width: u32) -> Vec<String> {
        vec![
            input.to_owned(),
            "-auto-orient".to_owned(),
            // Convert to sRGB before anything is stripped. A photo from a
            // phone is usually Display P3, and `-strip` below drops the
            // profile that says so: without converting first, the P3
            // values would be relabelled as sRGB and the rendition would
            // look washed out. ImageMagick already calls such a photo
            // sRGB, so `-colorspace` alone does nothing to it; only
            // `-profile` converts through the embedded profile. An
            // untagged source is only labelled, its pixels untouched, and
            // `-colorspace` still turns untagged CMYK into RGB.
            "-profile".to_owned(),
            format!("ICC:{}", self.srgb_profile.display()),
            "-colorspace".to_owned(),
            "sRGB".to_owned(),
            "-resize".to_owned(),
            format!("{width}x>"),
            "-strip".to_owned(),
        ]
    }

    /// Where the renditions of one width are written, and what they are
    /// called.
    ///
    /// The order is the order they are written in: JPEG XL first, then AVIF,
    /// then WebP, then the conventional fallback.
    fn output_paths(
        staging_dir: &Path,
        width: u32,
        fallback: RenditionFormat,
    ) -> Vec<(RenditionFormat, PathBuf, String)> {
        let named = |format: RenditionFormat| {
            let extension = format.extension();
            (
                format,
                staging_dir.join(format!("w{width}.{extension}")),
                format!("w{width}.{extension}"),
            )
        };
        vec![
            named(RenditionFormat::Jxl),
            named(RenditionFormat::Avif),
            named(RenditionFormat::Webp),
            named(fallback),
        ]
    }

    /// The ImageMagick arguments that write the renditions of one width.
    ///
    /// Split out of [`Self::generate`] so that what the encoders are *told*
    /// can be looked at, not only what comes out. The order of the writes is
    /// part of that: a quality setting applies to every write after it, and
    /// ImageMagick's JPEG XL coder turns one into a libjxl distance.
    fn rendition_args(
        &self,
        input: &str,
        width: u32,
        outputs: &[(RenditionFormat, PathBuf, String)],
    ) -> Vec<String> {
        let path = |index: usize| outputs[index].1.display().to_string();
        let mut args = self.base_args(input, width);

        // JPEG XL is written first, and with no quality setting in force.
        //
        // ImageMagick's JPEG XL coder does not take a quality: it turns one
        // into a libjxl *distance*, where a larger distance means more loss,
        // and libjxl's own default is a distance of 1.0. A quality left over
        // from an earlier write therefore decides the distance without saying
        // so — `-quality 55`, which the AVIF write below needs, lands in much
        // the same place as `-quality 85`, well past visually lossless. The
        // rendition comes out a third smaller and measurably worse, and
        // nothing in the response says that anything was lost. Writing this
        // format before any quality is set lets libjxl choose.
        args.extend([
            "-define".to_owned(),
            "jxl:effort=3".to_owned(),
            "-write".to_owned(),
            format!("{}:{}", RenditionFormat::Jxl.magick_name(), path(0)),
        ]);

        args.extend([
            "-quality".to_owned(),
            "80".to_owned(),
            "-define".to_owned(),
            "webp:method=4".to_owned(),
            "-write".to_owned(),
            format!("{}:{}", RenditionFormat::Webp.magick_name(), path(2)),
            "-quality".to_owned(),
            "55".to_owned(),
            "-define".to_owned(),
            "heic:speed=8".to_owned(),
            "-write".to_owned(),
            format!("{}:{}", RenditionFormat::Avif.magick_name(), path(1)),
        ]);

        match outputs[3].0 {
            RenditionFormat::Jpeg => {
                args.extend([
                    "-quality".to_owned(),
                    "85".to_owned(),
                    "-write".to_owned(),
                    format!("{}:{}", RenditionFormat::Jpeg.magick_name(), path(3)),
                ]);
            }
            RenditionFormat::Png => {
                args.extend([
                    "-define".to_owned(),
                    "png:compression-level=8".to_owned(),
                    "-write".to_owned(),
                    format!("{}:{}", RenditionFormat::Png.magick_name(), path(3)),
                ]);
            }
            _ => unreachable!("fallback format is JPEG or PNG"),
        }
        args.push("null:".to_owned());
        args
    }

    pub async fn generate(
        &self,
        input_path: &Path,
        source_format: SourceFormat,
        dimensions: ImageDimensions,
        widths: &[u32],
        staging_dir: &Path,
    ) -> Result<Vec<RenditionRecord>, MediaError> {
        let magick_format = source_format
            .magick_name()
            .ok_or_else(|| MediaError::Invalid("The file is not an image.".to_owned()))?;
        let input = format!("{magick_format}:{}", input_path.display());
        let mut records = Vec::new();
        let mut previous_width = 0;

        for max_width in widths.iter().copied() {
            let width = max_width.min(dimensions.width);
            if width == previous_width {
                continue;
            }
            previous_width = width;
            let fallback = source_format.fallback(dimensions.opaque);
            let height = ((u64::from(dimensions.height) * u64::from(width)
                + u64::from(dimensions.width) / 2)
                / u64::from(dimensions.width)) as u32;

            let mut outputs = Self::output_paths(staging_dir, width, fallback);
            let args = self.rendition_args(&input, width, &outputs);

            self.run(args, staging_dir).await?;

            for (format, path, file_name) in outputs.drain(..) {
                let bytes = tokio::fs::metadata(&path).await?.len();
                if bytes == 0 {
                    return Err(MediaError::ProcessFailed);
                }
                let sha256 = sha256_file(&path).await?;
                records.push(RenditionRecord {
                    max_width,
                    width,
                    height,
                    format,
                    bytes,
                    sha256,
                    file_name,
                });
            }
        }

        Ok(records)
    }

    async fn run<I>(&self, args: I, staging_dir: &Path) -> Result<Output, MediaError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut command = Command::new(&self.executable);
        command
            .args([
                "-limit",
                "memory",
                IMAGE_MAGICK_MEMORY_LIMIT,
                "-limit",
                "map",
                IMAGE_MAGICK_MAP_LIMIT,
                "-limit",
                "disk",
                IMAGE_MAGICK_DISK_LIMIT,
                "-limit",
                "thread",
                "1",
                "-limit",
                "width",
                &self.max_dimension.to_string(),
                "-limit",
                "height",
                &self.max_dimension.to_string(),
                "-limit",
                "time",
                &self.timeout.as_secs().to_string(),
            ])
            .args(args)
            .env("MAGICK_TEMPORARY_PATH", staging_dir)
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let output = timeout(self.timeout, command.output())
            .await
            .map_err(|_| MediaError::TimedOut)?
            .map_err(MediaError::ProcessUnavailable)?;
        if !output.status.success() {
            tracing::warn!(status = %output.status, "ImageMagick rejected or failed to process an upload");
            return Err(MediaError::ProcessFailed);
        }
        Ok(output)
    }
}

/// The formats ImageMagick can both read and write, from `magick -list format`.
///
/// The listing has more than one shape. A build without loadable modules
/// prints the name, the mode and the description:
///
/// ```text
///      JPEG* rw-   Joint Photographic Experts Group JFIF format
/// ```
///
/// A build with them prints the module the format lives in as well, which is
/// how Debian and Ubuntu ship ImageMagick:
///
/// ```text
///      JPEG  JPEG      rw-   Joint Photographic Experts Group JFIF format
/// ```
///
/// The mode is therefore found by what it looks like rather than by where it
/// sits: three letters, the first for reading, the second for writing and the
/// third for holding more than one image. Module names are upper case, so one
/// of them can never be mistaken for a mode.
fn read_write_formats(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let name = columns.next()?.trim_end_matches('*');
            let mode = columns.find(|column| is_format_mode(column))?;
            (mode.contains('r') && mode.contains('w')).then(|| name.to_ascii_uppercase())
        })
        .collect()
}

/// Whether a column is ImageMagick's mode column.
///
/// `rw+` reads and writes and holds a sequence, `r--` only reads, `-w-` only
/// writes, and `---` neither.
fn is_format_mode(column: &str) -> bool {
    let mut letters = column.chars();
    matches!(letters.next(), Some('r' | '-'))
        && matches!(letters.next(), Some('w' | '-'))
        && matches!(letters.next(), Some('+' | '-'))
        && letters.next().is_none()
}

pub fn validate_signature(format: SourceFormat, prefix: &[u8]) -> bool {
    match format {
        SourceFormat::Jpeg => prefix.starts_with(&[0xff, 0xd8, 0xff]),
        SourceFormat::Png => prefix.starts_with(b"\x89PNG\r\n\x1a\n"),
        SourceFormat::Webp => {
            prefix.len() >= 12 && prefix.starts_with(b"RIFF") && &prefix[8..12] == b"WEBP"
        }
        // JPEG XL has two serialisations and both are in circulation: a bare
        // codestream, which is what ImageMagick writes, and the ISOBMFF
        // container, which is what most other encoders write. Accepting only
        // one would break the round trip in one direction.
        SourceFormat::Jxl => {
            prefix.starts_with(&[0xff, 0x0a])
                || prefix.starts_with(&[
                    0x00, 0x00, 0x00, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a,
                ])
        }
        // AVIF and HEIC are both ISOBMFF and both start with an `ftyp` box, so
        // only the brand inside it tells them apart. Without that, an AVIF
        // renamed to .heic would pass as HEIC and be decoded as whatever its
        // payload actually is.
        SourceFormat::Avif => iso_brand(prefix, &["avif", "avis"]),
        SourceFormat::Heic => iso_brand(
            prefix,
            &[
                "heic", "heix", "heim", "heis", "hevc", "hevx", "hevm", "hevs",
            ],
        ),
        SourceFormat::Pdf => prefix
            .windows(5)
            .take(1024)
            .any(|window| window == b"%PDF-"),
        SourceFormat::Doc => prefix.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"),
        SourceFormat::Docx => prefix.starts_with(b"PK\x03\x04"),
    }
}

/// Whether the file is an ISO base media file carrying one of `brands`.
///
/// The major brand sits at bytes 8..12, but a file may declare `mif1` as its
/// major brand and list `heic` among its compatible brands instead — real files
/// from phones do exactly that — so the whole brand list is searched, up to
/// the size the `ftyp` box declares for itself.
fn iso_brand(prefix: &[u8], brands: &[&str]) -> bool {
    if prefix.len() < 12 || &prefix[4..8] != b"ftyp" {
        return false;
    }
    let declared = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    let end = declared.clamp(12, prefix.len());
    prefix[8..end]
        .chunks_exact(4)
        .any(|brand| brands.iter().any(|wanted| brand == wanted.as_bytes()))
}

pub async fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn parse_dimensions(bytes: &[u8]) -> Result<ImageDimensions, MediaError> {
    let output = std::str::from_utf8(bytes)
        .map_err(|_| MediaError::Invalid("Image dimensions could not be read.".to_owned()))?;
    let mut parts = output.split_whitespace();
    let width = parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| MediaError::Invalid("Image dimensions could not be read.".to_owned()))?;
    let height = parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| MediaError::Invalid("Image dimensions could not be read.".to_owned()))?;
    let opaque = parts
        .next()
        .map(|value| value.eq_ignore_ascii_case("true"))
        .ok_or_else(|| MediaError::Invalid("Image opacity could not be read.".to_owned()))?;
    if width == 0 || height == 0 {
        return Err(MediaError::Invalid(
            "The image has invalid dimensions.".to_owned(),
        ));
    }
    Ok(ImageDimensions {
        width,
        height,
        opaque,
    })
}

/// Whether `magick -list configure` names `wanted` among the delegates.
fn has_delegate(configure: &str, wanted: &str) -> bool {
    configure
        .lines()
        .filter_map(|line| line.strip_prefix("DELEGATES"))
        .flat_map(str::split_whitespace)
        .any(|delegate| delegate == wanted)
}

#[cfg(test)]
mod tests {
    use super::{
        ImageProcessor, SRGB_PROFILE, has_delegate, parse_dimensions, read_write_formats,
        validate_signature,
    };
    use crate::model::{RenditionFormat, SourceFormat};
    use std::path::Path;
    use std::time::Duration;

    /// The listing a build without loadable modules prints.
    const STATIC_BUILD_LISTING: &str = "   Format  Mode  Description\n\
        -------------------------------------------------------------------------------\n\
        \x20    3FR  r--   Hasselblad CFV/H3D39II Raw Format (0.22.1-Release)\n\
        \x20   AVIF* HEIC      rw+   AV1 Image File Format\n\
        \x20   HEIC* HEIC      rw+   High Efficiency Image Format\n\
        \x20    JXL* rw+   JPEG XL (ISO/IEC 18181) (libjxl 0.12.0)\n\
        \x20   JPEG* rw-   Joint Photographic Experts Group JFIF format\n\
        \x20    PNG* rw-   Portable Network Graphics (libpng 1.6.58)\n\
        \x20   WEBP* rw+   WebP Image Format (libwebp 1.6.0 [0210])\n";

    /// The listing a build with loadable modules prints, which is how Debian
    /// and Ubuntu ship ImageMagick: the module the format lives in sits
    /// between the name and the mode.
    const MODULE_BUILD_LISTING: &str = " Format  Module    Mode  Description\n\
        -------------------------------------------------------------------------------\n\
        \x20    3FR  DNG       r--   Hasselblad CFV/H3D39II Raw Format (0.22.1-Release)\n\
        \x20   AVIF  HEIC      rw+   AV1 Image File Format\n\
        \x20   HEIC  HEIC      rw+   High Efficiency Image Format\n\
        \x20    JXL  JXL       rw+   JPEG XL (ISO/IEC 18181) (libjxl 0.12.0)\n\
        \x20   JPEG  JPEG      rw-   Joint Photographic Experts Group JFIF format\n\
        \x20    PNG  PNG       rw-   Portable Network Graphics (libpng 1.6.58)\n\
        \x20   WEBP  WEBP      rw+   WebP Image Format (libwebp 1.6.0 [0210])\n";

    /// The mode column is found by its shape, not by its position: a module
    /// build puts the module's name where a static build puts the mode, and
    /// reading that column as the mode finds nothing that reads and writes.
    #[test]
    fn the_codec_listing_is_read_the_same_way_in_both_shapes() {
        for listing in [STATIC_BUILD_LISTING, MODULE_BUILD_LISTING] {
            let available = read_write_formats(listing);
            for required in ["AVIF", "HEIC", "JXL", "JPEG", "PNG", "WEBP"] {
                assert!(
                    available.iter().any(|format| format == required),
                    "{required} is missing from {available:?}"
                );
            }
            // A format that only reads is not a format Atlas can write a
            // rendition in.
            assert!(!available.iter().any(|format| format == "3FR"));
        }
    }

    /// The header of the listing is not a format, and neither is a line
    /// without a mode.
    #[test]
    fn a_listing_without_modes_yields_no_formats() {
        assert!(read_write_formats("   Format  Mode  Description\n").is_empty());
        assert!(read_write_formats("").is_empty());
    }

    #[test]
    fn signature_allowlist_rejects_raw_image_bytes() {
        assert!(validate_signature(SourceFormat::Jpeg, b"\xff\xd8\xff\xe0"));
        assert!(!validate_signature(SourceFormat::Jpeg, b"II*\0raw data"));
        assert!(validate_signature(
            SourceFormat::Png,
            b"\x89PNG\r\n\x1a\nmore"
        ));
        assert!(validate_signature(SourceFormat::Webp, b"RIFF\0\0\0\0WEBP"));
    }

    #[test]
    fn image_dimensions_are_parsed() {
        let dimensions = parse_dimensions(b"640 480 true").unwrap();
        assert_eq!(dimensions.width, 640);
        assert_eq!(dimensions.height, 480);
        assert!(dimensions.opaque);
    }

    #[test]
    fn lcms_is_found_only_on_the_delegates_line() {
        let configure = "NAME           ImageMagick\n\
                         DELEGATES      bzlib heic jpeg lcms png webp zlib\n\
                         FEATURES       Cipher DPC HDRI\n";
        assert!(has_delegate(configure, "lcms"));
        assert!(!has_delegate(
            "DELEGATES      bzlib heic jpeg png webp zlib\nLIBS           -llcms2\n",
            "lcms"
        ));
    }

    /// The two bytes that start a JPEG XL, from the two encoders that exist.
    const JXL_CODESTREAM: &[u8] = b"\xff\x0a\x68\x06\x7e\x0e";
    const JXL_CONTAINER: &[u8] = b"\x00\x00\x00\x0cJXL \x0d\x0a\x87\x0a";

    #[test]
    fn jpeg_xl_is_accepted_in_both_of_its_serialisations() {
        // ImageMagick writes the bare codestream; most other encoders write the
        // ISOBMFF container. Rejecting either breaks a round trip.
        assert!(validate_signature(SourceFormat::Jxl, JXL_CODESTREAM));
        assert!(validate_signature(SourceFormat::Jxl, JXL_CONTAINER));
        assert!(!validate_signature(
            SourceFormat::Jxl,
            b"\xff\xd8\xff\xe0jpeg"
        ));
        assert!(!validate_signature(SourceFormat::Jxl, b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn avif_and_heic_are_told_apart_by_their_brand() {
        // Both are ISO base media files starting with an `ftyp` box, so the
        // brand inside it is the only thing that distinguishes them.
        assert!(validate_signature(
            SourceFormat::Avif,
            b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00"
        ));
        assert!(validate_signature(
            SourceFormat::Avif,
            b"\x00\x00\x00\x1cftypavis"
        ));
        assert!(validate_signature(
            SourceFormat::Heic,
            b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00"
        ));
        assert!(validate_signature(
            SourceFormat::Heic,
            // A major brand of mif1 with heic listed as compatible: a real
            // shape, not a contrived one.
            b"\x00\x00\x00\x20ftypmif1\x00\x00\x00\x00heicmif1"
        ));

        // An AVIF renamed to .heic, and the reverse, are the case this exists
        // for: the bytes would decode fine, just not as what was declared.
        assert!(!validate_signature(
            SourceFormat::Heic,
            b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00"
        ));
        assert!(!validate_signature(
            SourceFormat::Avif,
            b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00"
        ));
        // Neither is anything else that merely starts with four bytes.
        assert!(!validate_signature(
            SourceFormat::Avif,
            b"\x00\x00\x00\x1cftyp"
        ));
        assert!(!validate_signature(
            SourceFormat::Heic,
            b"\x89PNG\r\n\x1a\n"
        ));
    }

    #[test]
    fn every_accepted_image_format_recognises_its_own_bytes() {
        // A format that cannot be recognised from its own output would only be
        // found out about when somebody tried to upload a file Atlas made.
        for (format, bytes) in [
            (SourceFormat::Jpeg, &b"\xff\xd8\xff\xe0"[..]),
            (SourceFormat::Png, &b"\x89PNG\r\n\x1a\n"[..]),
            (SourceFormat::Webp, &b"RIFF\0\0\0\0WEBP"[..]),
            (SourceFormat::Avif, &b"\x00\x00\x00\x1cftypavif"[..]),
            (SourceFormat::Jxl, JXL_CODESTREAM),
            (SourceFormat::Heic, &b"\x00\x00\x00\x18ftypheic"[..]),
        ] {
            assert!(validate_signature(format, bytes), "{format:?}");
            assert!(
                !validate_signature(format, b"not an image at all"),
                "{format:?}"
            );
        }
    }

    /// The image the fidelity test converts.
    ///
    /// A synthetic photograph — smooth gradients, a soft-edged shape, a dark
    /// band and a patch of fine texture — committed rather than generated, so
    /// that the numbers stay comparable between runs and between machines.
    /// It is a JPEG, like most of what a camera produces, so the test also
    /// covers the conventional fallback rather than the lossless one.
    const REFERENCE_IMAGE: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/reference.jpg");

    /// The width the fidelity test converts at. One is enough: the encoders
    /// behave the same way at every size, and a second width would only double
    /// the time the test takes.
    const FIDELITY_WIDTH: u32 = 320;

    /// How close a rendition has to stay to the source, per format.
    ///
    /// These are floors, not exact values, so an encoder that legitimately
    /// does better does not break the build. They are calibrated against the
    /// ImageMagick the development shell pins, which measures about 39.7 dB
    /// for JPEG XL, 39.6 for AVIF, 40.2 for WebP and 40.9 for the JPEG
    /// fallback on the fixture below, and every floor sits a decibel or two
    /// under that. A floor is a floor: a build whose encoders measure lower
    /// than this has genuinely got worse, and the number to raise is here.
    const FIDELITY_FLOORS: &[(&str, f64)] = &[
        ("jxl", 38.0),
        ("avif", 36.0),
        ("webp", 37.0),
        ("jpeg", 37.0),
    ];

    /// A rendition has to be a faithful copy of the source.
    ///
    /// Every other test in this file checks that a rendition *exists*, that it
    /// has the right width, that it has the right signature. None of them
    /// notice that the encoder settings changed and the pictures quietly got
    /// worse: a flag that costs seven decibels passes every other check in the
    /// suite. This one measures the distance between the rendition and the
    /// image the encoder was given, and refuses to let it grow.
    #[tokio::test]
    async fn renditions_stay_close_to_the_source() {
        require_image_magick();

        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("sRGB.icc");
        std::fs::write(&profile, SRGB_PROFILE).unwrap();
        let processor = ImageProcessor::new(
            "magick".to_owned(),
            profile,
            40_000_000,
            12_000,
            Duration::from_secs(45),
        );

        let source = directory.path().join("source.jpg");
        std::fs::copy(REFERENCE_IMAGE, &source).unwrap();

        let dimensions = processor
            .inspect(&source, SourceFormat::Jpeg, directory.path())
            .await
            .expect("the reference image should be readable");
        let renditions = processor
            .generate(
                &source,
                SourceFormat::Jpeg,
                dimensions,
                &[FIDELITY_WIDTH],
                directory.path(),
            )
            .await
            .expect("the reference image should convert");

        // A JPEG source gets the conventional JPEG fallback as well as the
        // three modern formats.
        assert_eq!(renditions.len(), 4, "every format should be written");

        for rendition in &renditions {
            let key = rendition.format.key();
            let floor = FIDELITY_FLOORS
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, floor)| *floor)
                .unwrap_or_else(|| panic!("no fidelity floor for {key}"));

            // What the rendition would be if the encoder lost nothing: the
            // same image, resized the same way, written losslessly. It is
            // written with `-write` rather than as a second bare file name,
            // because a file name after the input is another *input* to
            // ImageMagick, not the output.
            let reference = directory.path().join(format!("reference-{key}.png"));
            let mut args =
                processor.base_args(&format!("JPEG:{}", source.display()), rendition.width);
            args.push("-write".to_owned());
            args.push(format!("PNG:{}", reference.display()));
            args.push("null:".to_owned());
            processor
                .run(args, directory.path())
                .await
                .expect("the reference should render");

            let expected = decode_to_rgb(
                &processor,
                &format!("PNG:{}", reference.display()),
                directory.path(),
            )
            .await;
            let actual = decode_to_rgb(
                &processor,
                &format!(
                    "{}:{}",
                    rendition.format.magick_name(),
                    directory.path().join(&rendition.file_name).display()
                ),
                directory.path(),
            )
            .await;

            let measured = peak_signal_to_noise_ratio(&expected, &actual);
            assert!(
                measured >= floor,
                "the {key} rendition is {measured:.2} dB from the source, \
                 below the {floor:.1} dB floor"
            );
        }
    }

    /// The raw 8-bit RGB pixels of an image, so two of them can be compared.
    async fn decode_to_rgb(processor: &ImageProcessor, input: &str, staging: &Path) -> Vec<u8> {
        processor
            .run(
                [
                    input.to_owned(),
                    "-depth".to_owned(),
                    "8".to_owned(),
                    "rgb:-".to_owned(),
                ],
                staging,
            )
            .await
            .expect("ImageMagick should decode the image it wrote")
            .stdout
    }

    /// The peak signal-to-noise ratio of two images, in decibels.
    ///
    /// The peak is 255, because the images are compared as 8-bit RGB, which
    /// is what every format Atlas writes carries.
    fn peak_signal_to_noise_ratio(expected: &[u8], actual: &[u8]) -> f64 {
        assert_eq!(
            expected.len(),
            actual.len(),
            "the two images are not the same size"
        );
        let mut squared = 0f64;
        for (left, right) in expected.iter().zip(actual) {
            let difference = f64::from(*left) - f64::from(*right);
            squared += difference * difference;
        }
        if squared == 0.0 {
            return f64::INFINITY;
        }
        let mean = squared / expected.len() as f64;
        10.0 * (255.0f64 * 255.0 / mean).log10()
    }

    /// The JPEG XL rendition is written before any quality or distance is set.
    ///
    /// A quality setting applies to every write after it, and ImageMagick's
    /// JPEG XL coder turns one into a libjxl *distance* rather than a
    /// quality. So a quality left in force by an earlier write decides how
    /// lossy the JPEG XL rendition is, without anything in the response
    /// saying so. The fidelity test above cannot see this on its own: the
    /// distance a stray quality produces is worth about a decibel and a half,
    /// which is inside the margin a floor has to leave for an encoder that
    /// legitimately improves. This test sees the flag itself.
    #[test]
    fn the_jpeg_xl_write_carries_no_quality() {
        let processor = ImageProcessor::new(
            "magick".to_owned(),
            std::path::PathBuf::from("/tmp/sRGB.icc"),
            40_000_000,
            12_000,
            Duration::from_secs(45),
        );
        let outputs =
            ImageProcessor::output_paths(std::path::Path::new("/tmp"), 320, RenditionFormat::Jpeg);
        let args = processor.rendition_args("JPEG:/tmp/source.jpg", 320, &outputs);

        let jxl = args
            .iter()
            .position(|argument| argument.starts_with("JXL:"))
            .expect("the JPEG XL rendition is written");
        assert!(
            !args[..jxl].iter().any(|argument| argument == "-quality"),
            "a quality is in force when the JPEG XL rendition is written, \
             and ImageMagick turns it into a libjxl distance: {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|argument| argument.starts_with("jxl:distance=")),
            "JPEG XL should use libjxl's own distance, not an unverified define: {args:?}"
        );
    }

    /// The fidelity test runs the real ImageMagick, so it needs the real
    /// ImageMagick.
    ///
    /// The development shell refuses to open without the codecs this needs,
    /// so a missing binary means the environment is broken rather than that
    /// the test has nothing to say. Failing says so out loud: a test that
    /// quietly passes without running is worse than no test at all.
    fn require_image_magick() {
        let available = std::process::Command::new("magick")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        assert!(
            available,
            "ImageMagick is required for the fidelity test; run the tests inside `nix develop`"
        );
    }
}
