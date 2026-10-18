//! The name an uploader gave its file.
//!
//! This is untrusted input that ends up in a response header, so it is decoded
//! and sanitized in one place and never reaches a filesystem path: the file on
//! disk is always named after the asset ID and the detected format.
//!
//! Legitimate names are strange. `Отчет за 2026.pdf`, `Ünïcode (final).pdf`,
//! `100% pure.pdf` and `photo😀.png` are all real files that real people made,
//! and none of them is a reason to reject an upload. Two encodings are
//! therefore accepted:
//!
//! * raw UTF-8 bytes, which `curl -H "X-Original-Filename: Отчет.pdf"` sends
//!   and which is what a browser's `File.name` would produce;
//! * percent-encoded, for clients whose HTTP library refuses non-ASCII header
//!   values (Python's `http.client` encodes headers as Latin-1 and raises on
//!   anything else, so `urllib.parse.quote` is the one-line fix).
//!
//! A `%` that is not followed by two hex digits stays a literal `%`, so
//! `100% pure.pdf` survives both ways. Control characters are refused rather
//! than stripped: no filesystem permits them, and CR or LF in a response
//! header is a protocol problem, not a naming quirk.

use std::fmt;

/// Filesystems commonly cap a name at 255 characters; exceeding that on the
/// client's side would have failed anyway, so truncating here keeps the upload
/// instead of the file.
const MAX_CHARS: usize = 255;

#[derive(Debug, Eq, PartialEq)]
pub enum FilenameError {
    /// The bytes are not UTF-8 text, escaped or not.
    NotUtf8,
    /// A control character, including NUL, CR and LF.
    Control,
    /// Nothing usable left after dropping any path.
    Empty,
}

impl fmt::Display for FilenameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 => formatter
                .write_str("X-Original-Filename must be UTF-8 text, optionally percent-encoded"),
            Self::Control => {
                formatter.write_str("X-Original-Filename must not contain control characters")
            }
            Self::Empty => formatter.write_str("X-Original-Filename did not contain a file name"),
        }
    }
}

/// Decode and sanitize a name received from a client.
///
/// Any path is dropped, so a client that sends a full path does not turn the
/// name into a traversal attempt, and the result is truncated on a character
/// boundary rather than a byte one, so a Cyrillic name survives intact.
pub fn from_header_bytes(value: &[u8]) -> Result<String, FilenameError> {
    let decoded = percent_decode(value)?;
    if decoded.chars().any(char::is_control) {
        return Err(FilenameError::Control);
    }

    let name = decoded
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    if name.is_empty() || name == "." || name == ".." {
        return Err(FilenameError::Empty);
    }

    Ok(name.chars().take(MAX_CHARS).collect())
}

/// Decode `%XX` escapes where they are unambiguous, leaving everything else
/// alone. `%` followed by two hex digits is an escape; `%` followed by anything
/// else is a character that happens to be a percent sign.
fn percent_decode(value: &[u8]) -> Result<String, FilenameError> {
    let mut out = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%'
            && let Some(byte) = hex_pair(value, index)
        {
            out.push(byte);
            index += 3;
        } else {
            out.push(value[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| FilenameError::NotUtf8)
}

fn hex_pair(value: &[u8], index: usize) -> Option<u8> {
    let high = (*value.get(index + 1)? as char).to_digit(16)?;
    let low = (*value.get(index + 2)? as char).to_digit(16)?;
    Some((high * 16 + low) as u8)
}

/// Build a `Content-Disposition` value.
///
/// The quoted `filename` is an ASCII fallback for clients that do not read
/// `filename*`, and `filename*` carries the real UTF-8 name percent-encoded per
/// RFC 6266 / RFC 8187. The two together mean a name survives being exotic:
/// the modern form is exact, and the old form is at least recognisable.
pub fn disposition(kind: &str, name: Option<&str>, fallback_extension: &str) -> String {
    let Some(name) = name else {
        return format!("{kind}; filename=\"asset.{fallback_extension}\"");
    };

    format!(
        "{kind}; filename=\"{}\"; filename*=UTF-8''{}",
        ascii_fallback(name),
        percent_encode(name)
    )
}

/// Fold a name down to something a client that ignores `filename*` can still
/// show. Only the quoted form is degraded; the encoded form is exact.
fn ascii_fallback(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_graphic() || character == ' ' {
                match character {
                    '"' | '\\' => '_',
                    other => other,
                }
            } else {
                '_'
            }
        })
        .collect()
}

/// Percent-encode everything outside the `attr-char` set of RFC 8187, so the
/// result is safe in a header value and in a URL context.
fn percent_encode(value: &str) -> String {
    const SAFE: &[u8] = b"!#$&+-.^_`|~";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || SAFE.contains(byte) {
            encoded.push(char::from(*byte));
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::{
        FilenameError, ascii_fallback, disposition, from_header_bytes, percent_decode,
        percent_encode,
    };

    fn decode(value: &str) -> Result<String, FilenameError> {
        from_header_bytes(value.as_bytes())
    }

    #[test]
    fn ordinary_names_pass_through() {
        assert_eq!(decode("menu-2026.pdf"), Ok("menu-2026.pdf".to_owned()));
        assert_eq!(
            decode("A holiday photo.jpg"),
            Ok("A holiday photo.jpg".to_owned())
        );
        assert_eq!(decode("a+b&c=d.pdf"), Ok("a+b&c=d.pdf".to_owned()));
        assert_eq!(decode("scan (1).png"), Ok("scan (1).png".to_owned()));
    }

    #[test]
    fn unicode_names_need_no_encoding() {
        for name in [
            "Отчет за 2026.pdf",
            "Jahresbericht (Ü).pdf",
            "家族写真 2026.png",
            "photo😀.png",
            "Binte-Ümit-Rechnung.pdf",
        ] {
            assert_eq!(decode(name), Ok(name.to_owned()), "{name}");
        }
    }

    #[test]
    fn percent_encoded_names_decode_to_the_same_text() {
        assert_eq!(
            decode("Jahresbericht%20%28%C3%9C%29.pdf"),
            Ok("Jahresbericht (Ü).pdf".to_owned())
        );
        assert_eq!(
            decode("%D0%9E%D1%82%D1%87%D0%B5%D1%82.pdf"),
            Ok("Отчет.pdf".to_owned())
        );
    }

    #[test]
    fn a_literal_percent_survives_both_encodings() {
        // `%` is only an escape when two hex digits follow it, so a real name
        // containing one is untouched.
        for name in [
            "100% pure.pdf",
            "50% done.pdf",
            "a%zzb.pdf",
            "%not a hex.pdf",
        ] {
            assert_eq!(decode(name), Ok(name.to_owned()), "{name}");
        }
        // ...while an unambiguous escape is still decoded.
        assert_eq!(decode("50%25%20done.pdf"), Ok("50% done.pdf".to_owned()));
    }

    #[test]
    fn any_path_is_dropped() {
        assert_eq!(decode("../../etc/passwd"), Ok("passwd".to_owned()));
        assert_eq!(
            decode("..%2F..%2Fsecret%2Fphoto.jpg"),
            Ok("photo.jpg".to_owned())
        );
        assert_eq!(
            decode("C%3A%5CUsers%5Cme%5Cphoto.png"),
            Ok("photo.png".to_owned())
        );
    }

    #[test]
    fn control_characters_are_refused_not_stripped() {
        assert_eq!(
            decode("evil%0D%0AX-Injected:%20yes"),
            Err(FilenameError::Control)
        );
        assert_eq!(decode("nul%00byte"), Err(FilenameError::Control));
        assert_eq!(decode("line\nbreak.pdf"), Err(FilenameError::Control));
    }

    #[test]
    fn bytes_that_are_not_text_are_refused() {
        assert_eq!(
            from_header_bytes(&[0xff, 0xfe]),
            Err(FilenameError::NotUtf8),
            "undecodable bytes are refused, not turned into an empty name"
        );
        assert_eq!(
            from_header_bytes(b"\xc3\x28"),
            Err(FilenameError::NotUtf8),
            "a truncated multi-byte sequence is not text"
        );
        assert_eq!(decode("nul\u{0}byte.pdf"), Err(FilenameError::Control));
    }

    #[test]
    fn empty_and_dot_names_are_refused() {
        assert_eq!(decode(""), Err(FilenameError::Empty));
        assert_eq!(decode(".."), Err(FilenameError::Empty));
        assert_eq!(decode("%2F%2F"), Err(FilenameError::Empty));
        assert_eq!(decode("   "), Err(FilenameError::Empty));
    }

    #[test]
    fn long_names_are_truncated_by_characters_not_bytes() {
        let cyrillic = "я".repeat(400);
        let decoded = decode(&cyrillic).unwrap();
        assert_eq!(
            decoded.chars().count(),
            255,
            "a full filesystem-length name survives"
        );
        assert!(decoded.is_char_boundary(decoded.len()));

        let mixed = decode(&format!("{}😀", "ä".repeat(300))).unwrap();
        assert!(mixed.chars().count() <= 255);
        assert!(!mixed.contains(char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn the_ascii_fallback_is_degraded_but_readable() {
        assert_eq!(ascii_fallback("plain.jpg"), "plain.jpg");
        assert_eq!(
            ascii_fallback("quote\"and\\slash.pdf"),
            "quote_and_slash.pdf"
        );
        assert_eq!(ascii_fallback("Отчет.pdf"), "_____.pdf");
        assert_eq!(
            ascii_fallback("Jahresbericht (Ü).pdf"),
            "Jahresbericht (_).pdf"
        );
    }

    #[test]
    fn disposition_without_a_name_uses_the_format() {
        assert_eq!(
            disposition("attachment", None, "pdf"),
            "attachment; filename=\"asset.pdf\""
        );
    }

    #[test]
    fn disposition_carries_the_exact_name_in_the_encoded_form() {
        assert_eq!(
            disposition("inline", Some("Jahresbericht (Ü).pdf"), "pdf"),
            "inline; filename=\"Jahresbericht (_).pdf\"; \
             filename*=UTF-8''Jahresbericht%20%28%C3%9C%29.pdf"
        );
        assert_eq!(
            disposition("inline", Some("Отчет за 2026.pdf"), "pdf"),
            "inline; filename=\"_____ __ 2026.pdf\"; \
             filename*=UTF-8''%D0%9E%D1%82%D1%87%D0%B5%D1%82%20%D0%B7%D0%B0%202026.pdf"
        );
    }

    #[test]
    fn disposition_values_are_always_valid_header_contents() {
        for name in [
            "a b.pdf",
            "Ünïcode.pdf",
            "Отчет.pdf",
            "quote\"and\\slash.pdf",
            "comma,semi;colon.pdf",
            "100% pure.pdf",
            "family😀.png",
        ] {
            let value = disposition("inline", Some(name), "pdf");
            assert!(value.is_ascii(), "{value} must be ASCII");
            assert!(
                !value.contains(['\r', '\n']),
                "{value} must not break a header"
            );
            assert!(
                HeaderValue::from_str(&value).is_ok(),
                "{value} must be a valid header value"
            );
        }
    }

    #[test]
    fn percent_encoding_round_trips() {
        for name in ["a b", "Ü.pdf", "x+y", "100%", "a/b", "Отчет.pdf", "😀.png"] {
            assert_eq!(
                percent_decode(percent_encode(name).as_bytes()).unwrap(),
                name
            );
        }
    }
}
