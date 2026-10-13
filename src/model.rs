use serde::Serialize;

use crate::ident::AssetId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceFormat {
    Jpeg,
    Png,
    Webp,
    Pdf,
    Doc,
    Docx,
}

impl SourceFormat {
    pub fn parse_content_type(content_type: &str) -> Option<Self> {
        match content_type
            .split(';')
            .next()?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "image/jpeg" => Some(Self::Jpeg),
            "image/png" => Some(Self::Png),
            "image/webp" => Some(Self::Webp),
            "application/pdf" => Some(Self::Pdf),
            "application/msword" => Some(Self::Doc),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
                Some(Self::Docx)
            }
            _ => None,
        }
    }

    pub fn from_database(value: &str) -> Option<Self> {
        match value {
            "jpeg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "webp" => Some(Self::Webp),
            "pdf" => Some(Self::Pdf),
            "doc" => Some(Self::Doc),
            "docx" => Some(Self::Docx),
            _ => None,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Pdf => "pdf",
            Self::Doc => "doc",
            Self::Docx => "docx",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Pdf => "pdf",
            Self::Doc => "doc",
            Self::Docx => "docx",
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
            Self::Pdf => "application/pdf",
            Self::Doc => "application/msword",
            Self::Docx => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        }
    }

    pub fn magick_name(self) -> Option<&'static str> {
        match self {
            Self::Jpeg => Some("JPEG"),
            Self::Png => Some("PNG"),
            Self::Webp => Some("WEBP"),
            Self::Pdf | Self::Doc | Self::Docx => None,
        }
    }

    pub fn is_image(self) -> bool {
        self.magick_name().is_some()
    }

    pub fn fallback(self, opaque: bool) -> RenditionFormat {
        match self {
            Self::Jpeg => RenditionFormat::Jpeg,
            Self::Png => RenditionFormat::Png,
            Self::Webp if opaque => RenditionFormat::Jpeg,
            Self::Webp => RenditionFormat::Png,
            Self::Pdf | Self::Doc | Self::Docx => RenditionFormat::Png,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RenditionFormat {
    Jpeg,
    Png,
    Webp,
    Avif,
    Jxl,
}

impl RenditionFormat {
    pub fn key(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Jxl => "jxl",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Jxl => "jxl",
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
            Self::Avif => "image/avif",
            Self::Jxl => "image/jxl",
        }
    }

    pub fn from_database(value: &str) -> Option<Self> {
        match value {
            "jpeg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "webp" => Some(Self::Webp),
            "avif" => Some(Self::Avif),
            "jxl" => Some(Self::Jxl),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AssetRecord {
    pub id: AssetId,
    pub source_format: SourceFormat,
    pub bytes: u64,
    pub sha256: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct RenditionRecord {
    pub max_width: u32,
    pub width: u32,
    pub height: u32,
    pub format: RenditionFormat,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct StoredAsset {
    pub asset: AssetRecord,
    pub renditions: Vec<RenditionRecord>,
}
