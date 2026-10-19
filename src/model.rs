use serde::Serialize;

use crate::ident::AssetId;

/// Which collection an asset belongs to: the caller's choice, recorded.
///
/// Not the same thing as the stored format. A PNG can be an `image` (inspected,
/// converted into renditions, served inline) or a `document` (stored untouched,
/// served as an attachment), and both take the identical path through storage,
/// hashing and delivery. Format decides what the bytes may be *done to*; kind
/// decides which operations exist at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Image,
    Document,
}

impl Kind {
    /// The URL segment for this collection.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Image => "images",
            Self::Document => "documents",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Document => "document",
        }
    }

    pub fn from_key(value: &str) -> Option<Self> {
        match value {
            "image" => Some(Self::Image),
            "document" => Some(Self::Document),
            _ => None,
        }
    }

    /// Parse a URL segment. Anything else is not a collection.
    pub fn from_slug(value: &str) -> Option<Self> {
        match value {
            "images" => Some(Self::Image),
            "documents" => Some(Self::Document),
            _ => None,
        }
    }

    /// What this collection accepts.
    ///
    /// Images are the convertible formats, so they are the ones that can have
    /// renditions. Documents accept the document formats and, deliberately,
    /// images too: storing a picture without converting it is the reason the
    /// collection exists.
    pub fn accepts(self, format: SourceFormat) -> bool {
        match self {
            Self::Image => format.is_image(),
            Self::Document => true,
        }
    }

    /// Whether uploads into this collection are inspected and converted.
    pub fn generates_renditions(self) -> bool {
        matches!(self, Self::Image)
    }
}

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

    /// Every format Atlas accepts, in one place.
    ///
    /// Queries about what an upload *could* be — "which media types may this
    /// collection contain" — are answered from this list rather than a second
    /// one written beside it, which is how the two start disagreeing.
    pub const ALL: [SourceFormat; 6] = [
        SourceFormat::Jpeg,
        SourceFormat::Png,
        SourceFormat::Webp,
        SourceFormat::Pdf,
        SourceFormat::Doc,
        SourceFormat::Docx,
    ];

    /// The collection a file of this format belongs in by default.
    ///
    /// This is the format's habit, not a rule: the documents collection also
    /// accepts images, because a caller may want a picture stored whole.
    pub fn kind(self) -> Kind {
        match self {
            Self::Jpeg | Self::Png | Self::Webp => Kind::Image,
            Self::Pdf | Self::Doc | Self::Docx => Kind::Document,
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
    /// The name the uploader gave the file, if it sent one. Never a path.
    pub original_name: Option<String>,
    /// Which collection this was uploaded into. Absent only for a row written
    /// before the column existed, which is backfilled from the format.
    pub kind: Option<Kind>,
    /// When the asset moved to trash; `None` while it is live.
    pub deleted_at: Option<i64>,
    /// When the trashed bytes were discarded; the row outlives them as a tombstone.
    pub purged_at: Option<i64>,
    /// Set by a consuming application that still uses this asset, so purge skips it.
    pub pinned: bool,
}

impl AssetRecord {
    pub fn state(&self) -> AssetState {
        if self.purged_at.is_some() {
            AssetState::Purged
        } else if self.deleted_at.is_some() {
            AssetState::Deleted
        } else {
            AssetState::Active
        }
    }
}

/// How Atlas should treat an asset: live, in trash, or purged to a tombstone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetState {
    Active,
    Deleted,
    Purged,
    /// Atlas has no row with this id. Only a state lookup reports this;
    /// `GET /api/v1/assets/{id}` answers `404` instead.
    Missing,
}

impl AssetState {
    /// Anything but a live asset has no bytes to publish, so public delivery
    /// answers with the placeholder instead.
    pub fn is_retired(self) -> bool {
        !matches!(self, Self::Active)
    }
}

#[derive(Clone, Debug)]
pub struct RenditionRecord {
    pub max_width: u32,
    pub width: u32,
    pub height: u32,
    pub format: RenditionFormat,
    pub bytes: u64,
    pub sha256: String,
    /// File name inside the asset directory, relative to it. Named after the
    /// actual output width, which may be smaller than `max_width`.
    pub file_name: String,
}

#[derive(Clone, Debug)]
pub struct StoredAsset {
    pub asset: AssetRecord,
    pub renditions: Vec<RenditionRecord>,
}
