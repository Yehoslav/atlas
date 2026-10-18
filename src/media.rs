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

        for required in ["JPEG", "PNG", "WEBP", "AVIF", "JXL"] {
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
        SourceFormat::Pdf => prefix
            .windows(5)
            .take(1024)
            .any(|window| window == b"%PDF-"),
        SourceFormat::Doc => prefix.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"),
        SourceFormat::Docx => prefix.starts_with(b"PK\x03\x04"),
    }
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
}
