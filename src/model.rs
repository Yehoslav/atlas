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
    ///
    /// The stored key is accepted as well as the slug. A caller that read
    /// `kind` out of a response and put it straight into a route gets the
    /// collection it meant instead of a 404 that reads like a missing asset,
    /// which is the confusing way to be wrong.
    pub fn from_slug(value: &str) -> Option<Self> {
        match value {
            "images" | "image" => Some(Self::Image),
            "documents" | "document" => Some(Self::Document),
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
    Avif,
    Jxl,
    Heic,
    Pdf,
    Doc,
    Docx,
}

impl SourceFormat {
    /// The format a media type names, if Atlas accepts that media type.
    pub fn parse_content_type(content_type: &str) -> Option<Self> {
        let wanted = content_type.split(';').next()?.trim().to_ascii_lowercase();
        SourceFormat::ALL.into_iter().find(|format| {
            format
                .accepted_content_types()
                .iter()
                .any(|accepted| *accepted == wanted)
        })
    }

    /// Every media type this format may be declared as.
    ///
    /// Most formats have exactly one spelling, but HEIF has two and the
    /// desktop decides which one a browser sends: freedesktop.org makes
    /// `image/heif` the canonical type for a `.heic` file and keeps
    /// `image/heic` as an alias of it, so a browser, a file manager and a
    /// command line all disagree about the string to put on the wire.
    ///
    /// Declaring either is the same request. The bytes still decide what the
    /// file is — `crate::media::validate_signature` requires an HEVC brand —
    /// so this widens what may be *declared*, never what may be *decoded*: an
    /// AV1-in-HEIF file is refused whatever it calls itself.
    pub fn accepted_content_types(self) -> &'static [&'static str] {
        match self {
            Self::Jpeg => &["image/jpeg"],
            Self::Png => &["image/png"],
            Self::Webp => &["image/webp"],
            Self::Avif => &["image/avif"],
            Self::Jxl => &["image/jxl"],
            Self::Heic => &["image/heic", "image/heif"],
            Self::Pdf => &["application/pdf"],
            Self::Doc => &["application/msword"],
            Self::Docx => {
                &["application/vnd.openxmlformats-officedocument.wordprocessingml.document"]
            }
        }
    }

    pub fn from_database(value: &str) -> Option<Self> {
        match value {
            "jpeg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "webp" => Some(Self::Webp),
            "avif" => Some(Self::Avif),
            "jxl" => Some(Self::Jxl),
            "heic" => Some(Self::Heic),
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
            Self::Avif => "avif",
            Self::Jxl => "jxl",
            Self::Heic => "heic",
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
            Self::Avif => "avif",
            Self::Jxl => "jxl",
            Self::Heic => "heic",
            Self::Pdf => "pdf",
            Self::Doc => "doc",
            Self::Docx => "docx",
        }
    }

    /// The media type this format is stored and served as.
    ///
    /// The first accepted spelling is the canonical one, so a file uploaded
    /// as `image/heif` is stored as `image/heic`: the media type a caller
    /// reads back does not depend on which spelling it happened to send.
    pub fn mime_type(self) -> &'static str {
        self.accepted_content_types()[0]
    }

    pub fn magick_name(self) -> Option<&'static str> {
        match self {
            Self::Jpeg => Some("JPEG"),
            Self::Png => Some("PNG"),
            Self::Webp => Some("WEBP"),
            Self::Avif => Some("AVIF"),
            Self::Jxl => Some("JXL"),
            Self::Heic => Some("HEIC"),
            Self::Pdf | Self::Doc | Self::Docx => None,
        }
    }

    /// Every format Atlas accepts, in one place.
    ///
    /// Queries about what an upload *could* be — "which media types may this
    /// collection contain" — are answered from this list rather than a second
    /// one written beside it, which is how the two start disagreeing.
    pub const ALL: [SourceFormat; 9] = [
        SourceFormat::Jpeg,
        SourceFormat::Png,
        SourceFormat::Webp,
        SourceFormat::Avif,
        SourceFormat::Jxl,
        SourceFormat::Heic,
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
            Self::Jpeg | Self::Png | Self::Webp | Self::Avif | Self::Jxl | Self::Heic => {
                Kind::Image
            }
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
            // Everything else that can carry alpha needs a fallback that does
            // too; an opaque source is better served by JPEG's ubiquity.
            Self::Webp | Self::Avif | Self::Jxl | Self::Heic if opaque => RenditionFormat::Jpeg,
            Self::Webp | Self::Avif | Self::Jxl | Self::Heic => RenditionFormat::Png,
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
    /// A consumer's own opaque note. Atlas stores it, hands it back untouched
    /// and never looks inside; see the migration for why the column is named
    /// more plainly than the API field.
    pub consumer_data: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::SourceFormat;

    /// HEIF has two spellings and the desktop picks which one goes on the
    /// wire: freedesktop.org makes `image/heif` canonical and keeps
    /// `image/heic` as its alias.
    #[test]
    fn both_spellings_of_heif_name_the_same_format() {
        assert_eq!(
            SourceFormat::parse_content_type("image/heic"),
            Some(SourceFormat::Heic)
        );
        assert_eq!(
            SourceFormat::parse_content_type("image/heif"),
            Some(SourceFormat::Heic)
        );
        assert_eq!(
            SourceFormat::parse_content_type("IMAGE/HEIF"),
            Some(SourceFormat::Heic)
        );
    }

    /// The canonical spelling is what gets stored and served, whichever one
    /// the caller happened to send.
    #[test]
    fn the_stored_media_type_is_the_canonical_one() {
        assert_eq!(SourceFormat::Heic.mime_type(), "image/heic");
        assert_eq!(
            SourceFormat::Heic.accepted_content_types(),
            ["image/heic", "image/heif"]
        );
    }

    /// An image sequence is a grid of pictures, not a picture.
    #[test]
    fn an_image_sequence_is_not_accepted() {
        assert_eq!(
            SourceFormat::parse_content_type("image/heic-sequence"),
            None
        );
        assert_eq!(
            SourceFormat::parse_content_type("image/heif-sequence"),
            None
        );
    }

    #[test]
    fn a_content_type_outside_the_allowlist_names_no_format() {
        for content_type in [
            "image/tiff",
            "image/heif-sequence",
            "application/octet-stream",
            "text/plain",
            "",
        ] {
            assert_eq!(
                SourceFormat::parse_content_type(content_type),
                None,
                "{content_type} is not a format Atlas accepts"
            );
        }
    }

    /// The allowlist and the formats that back it cannot disagree: every
    /// accepted media type names a format, and every format is reachable.
    #[test]
    fn every_accepted_media_type_names_a_format() {
        for format in SourceFormat::ALL {
            for content_type in format.accepted_content_types() {
                assert_eq!(
                    SourceFormat::parse_content_type(content_type),
                    Some(format),
                    "{content_type} should name {format:?}"
                );
            }
        }
    }
}
