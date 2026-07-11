//! Upload safety: what a client is allowed to put into public storage.
//!
//! Threat model. Files under [`Storage`](super::Storage) are served
//! read-only at `/public`, same-origin with the app. An attacker who can
//! upload a file the browser will *execute* — HTML, or an SVG (SVG is an
//! XML document that runs `<script>` and event handlers) — gets stored
//! cross-site scripting against every user who views it. Two things stop
//! that here:
//!
//! 1. **Content, not name.** The upload's bytes must actually be one of a
//!    small allowlist of raster image formats ([`guard_image`]). A file
//!    named `logo.png` whose body is `<svg onload=…>` is rejected, and
//!    the stored extension is derived from the sniffed format, not from
//!    the client's file name — so a `.png` on disk is always a PNG.
//! 2. **Defense in depth on the way out.** `/public` is served with
//!    `X-Content-Type-Options: nosniff` (the browser honours the
//!    declared image type instead of sniffing markup out of it) and a
//!    `Content-Security-Policy: default-src 'none'; sandbox` (a file
//!    opened as a top-level document can neither script nor fetch). See
//!    [`response_headers`].
//!
//! Not covered, by design: a decompression "pixel bomb" (a valid but
//! enormous image) — that needs a real decoder and belongs to whatever
//! resizes the image, not to the gatekeeper. The raw byte-size cap here
//! bounds the input regardless.

use crate::error::{Error, Result};

/// A raster image format the store accepts. Deliberately excludes SVG:
/// it is a scriptable document, not a picture, and has no place behind a
/// same-origin `/public`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageFormat {
    /// The canonical, safe file extension — what the file is stored as,
    /// regardless of what the client named it.
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Gif => "gif",
            ImageFormat::Webp => "webp",
        }
    }

    /// The MIME type for the served response.
    pub fn content_type(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
            ImageFormat::Webp => "image/webp",
        }
    }

    /// Identify a format from the leading bytes, or `None` if the content
    /// is not one of the allowed images. Only the file signature is
    /// trusted; the client's name and declared content type are ignored.
    pub fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(ImageFormat::Png);
        }
        if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some(ImageFormat::Jpeg);
        }
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return Some(ImageFormat::Gif);
        }
        // RIFF <4-byte length> "WEBP".
        if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            return Some(ImageFormat::Webp);
        }
        None
    }
}

/// Validate a client image upload before it is stored: non-empty, within
/// `max_bytes`, and genuinely one of the allowed raster formats by
/// content. Returns the format so the caller can store it under the
/// canonical extension. This is the single gate every client-supplied
/// image must pass through.
pub fn guard_image(bytes: &[u8], max_bytes: usize) -> Result<ImageFormat> {
    if bytes.is_empty() {
        return Err(Error::Validation("the uploaded file is empty".into()));
    }
    if bytes.len() > max_bytes {
        return Err(Error::Validation(format!(
            "the file is {} bytes; the limit is {max_bytes}",
            bytes.len()
        )));
    }
    ImageFormat::sniff(bytes).ok_or_else(|| {
        Error::Validation("the file is not a png, jpg, gif or webp image".into())
    })
}

/// Hardening headers for every `/public` response, so a stored file can
/// never be turned into executable content in a browser even if one
/// slipped past [`guard_image`]. `nosniff` pins the declared type;
/// the CSP neutralizes a file opened as a document.
pub fn response_headers() -> [(&'static str, &'static str); 2] {
    [
        ("x-content-type-options", "nosniff"),
        ("content-security-policy", "default-src 'none'; sandbox"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_real_images_and_rejects_smuggled_markup() {
        assert_eq!(ImageFormat::sniff(b"\x89PNG\r\n\x1a\n\x00"), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::sniff(b"GIF89a....."), Some(ImageFormat::Gif));
        let webp = b"RIFF\x24\x00\x00\x00WEBPVP8 ";
        assert_eq!(ImageFormat::sniff(webp), Some(ImageFormat::Webp));

        // The whole point: markup dressed as an image is not an image.
        assert_eq!(ImageFormat::sniff(b"<svg onload=alert(1)></svg>"), None);
        assert_eq!(ImageFormat::sniff(b"<!DOCTYPE html><script>"), None);
        assert_eq!(ImageFormat::sniff(b"GIF87a is a lie"), Some(ImageFormat::Gif));
        assert_eq!(ImageFormat::sniff(b"MZ\x90\x00"), None, "an exe is not an image");
    }

    #[test]
    fn guard_enforces_size_and_emptiness() {
        assert!(guard_image(b"", 1024).is_err());
        assert!(guard_image(b"\x89PNG\r\n\x1a\n", 4).is_err(), "over the cap");
        assert_eq!(guard_image(b"\x89PNG\r\n\x1a\n\x00", 1024).unwrap(), ImageFormat::Png);
    }
}
