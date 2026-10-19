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
    pub fn new(executable: String, max_pixels: u64, max_dimension: u32, timeout: Duration) -> Self {
        Self {
            executable,
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
        let available: Vec<_> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut columns = line.split_whitespace();
                let name = columns.next()?.trim_end_matches('*');
                let mode = columns.next()?;
                (mode.contains('r') && mode.contains('w')).then_some(name.to_ascii_uppercase())
            })
            .collect();

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

            let mut outputs: Vec<(RenditionFormat, PathBuf, String)> = vec![
                (
                    RenditionFormat::Jxl,
                    staging_dir.join(format!("w{width}.jxl")),
                    format!("w{width}.jxl"),
                ),
                (
                    RenditionFormat::Avif,
                    staging_dir.join(format!("w{width}.avif")),
                    format!("w{width}.avif"),
                ),
                (
                    RenditionFormat::Webp,
                    staging_dir.join(format!("w{width}.webp")),
                    format!("w{width}.webp"),
                ),
                (
                    fallback,
                    staging_dir.join(format!("w{width}.{}", fallback.extension())),
                    format!("w{width}.{}", fallback.extension()),
                ),
            ];

            let mut args = vec![
                input.clone(),
                "-auto-orient".to_owned(),
                // Convert to sRGB before anything is stripped. A photo from a
                // phone is usually Display P3, and `-strip` below drops the
                // profile that says so: without converting first, the P3
                // values would be relabelled as sRGB and the rendition would
                // look washed out. An untagged or already-sRGB source is
                // unaffected, so this is safe to do unconditionally.
                "-colorspace".to_owned(),
                "sRGB".to_owned(),
                "-resize".to_owned(),
                format!("{width}x>"),
                "-strip".to_owned(),
                "-quality".to_owned(),
                "80".to_owned(),
                "-define".to_owned(),
                "webp:method=4".to_owned(),
                "-write".to_owned(),
                format!("WEBP:{}", outputs[2].1.display()),
                "-quality".to_owned(),
                "55".to_owned(),
                "-define".to_owned(),
                "heic:speed=8".to_owned(),
                "-write".to_owned(),
                format!("AVIF:{}", outputs[1].1.display()),
                "-define".to_owned(),
                "jxl:effort=3".to_owned(),
                "-define".to_owned(),
                "jxl:distance=1.5".to_owned(),
                "-write".to_owned(),
                format!("JXL:{}", outputs[0].1.display()),
            ];

            match fallback {
                RenditionFormat::Jpeg => {
                    args.extend([
                        "-quality".to_owned(),
                        "85".to_owned(),
                        "-write".to_owned(),
                        format!("JPEG:{}", outputs[3].1.display()),
                    ]);
                }
                RenditionFormat::Png => {
                    args.extend([
                        "-define".to_owned(),
                        "png:compression-level=8".to_owned(),
                        "-write".to_owned(),
                        format!("PNG:{}", outputs[3].1.display()),
                    ]);
                }
                _ => unreachable!("fallback format is JPEG or PNG"),
            }
            args.push("null:".to_owned());

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

#[cfg(test)]
mod tests {
    use super::{parse_dimensions, validate_signature};
    use crate::model::SourceFormat;

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
}
