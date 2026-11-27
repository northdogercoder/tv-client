//! Bounded artwork decoding.
//!
//! The client never trusts `Content-Type` and never hands bytes to a decoder
//! that could allocate from attacker-controlled dimensions. It parses only the
//! small format header, rejects excessive sides or pixel counts, and caps the
//! encoded payload before it is stored.

use crate::jellyfin::JellyfinError;
use crate::jellyfin::models::ArtworkClass;

/// Largest encoded artwork payload accepted from the server.
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Largest single image side accepted.
pub const MAX_IMAGE_DIMENSION: u32 = 8192;
/// Largest accepted decoded pixel count.
pub const MAX_IMAGE_PIXELS: u64 = 33_554_432;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Read the dimensions from a PNG, JPEG, GIF or WebP header.
pub(crate) fn image_dimensions(bytes: &[u8]) -> Result<(u32, u32), JellyfinError> {
    let dimensions = if bytes.starts_with(&PNG_SIGNATURE) {
        png_dimensions(bytes)
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        jpeg_dimensions(bytes)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        gif_dimensions(bytes)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(&b"WEBP"[..]) {
        webp_dimensions(bytes)
    } else {
        return Err(JellyfinError::InvalidImage);
    };
    dimensions.and_then(|(width, height)| {
        validate_dimensions(width, height)?;
        Ok((width, height))
    })
}

/// Reject zero-sized, oversized or excessively large images.
pub(crate) fn validate_dimensions(width: u32, height: u32) -> Result<(), JellyfinError> {
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err(JellyfinError::ImageTooLarge { width, height });
    }
    Ok(())
}

/// Parse and validate an artwork payload against a class's byte, pixel and
/// side caps.
pub(crate) fn validate_artwork(
    bytes: &[u8],
    class: ArtworkClass,
) -> Result<(u32, u32), JellyfinError> {
    if bytes.len() > class.max_bytes() {
        return Err(JellyfinError::ArtworkTooLarge);
    }
    let (width, height) = image_dimensions(bytes)?;
    if width > class.max_width()
        || height > class.max_height()
        || u64::from(width) * u64::from(height) > class.max_pixels()
    {
        return Err(JellyfinError::ImageTooLarge { width, height });
    }
    Ok((width, height))
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), JellyfinError> {
    if bytes.len() < 24 || bytes.get(12..16) != Some(&b"IHDR"[..]) {
        return Err(JellyfinError::InvalidImage);
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Ok((width, height))
}

fn gif_dimensions(bytes: &[u8]) -> Result<(u32, u32), JellyfinError> {
    if bytes.len() < 10 {
        return Err(JellyfinError::InvalidImage);
    }
    let width = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]));
    let height = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]));
    Ok((width, height))
}

fn jpeg_dimensions(bytes: &[u8]) -> Result<(u32, u32), JellyfinError> {
    let mut offset = 2;
    while offset + 1 < bytes.len() {
        if bytes[offset] != 0xff {
            return Err(JellyfinError::InvalidImage);
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        let Some(&marker) = bytes.get(offset) else {
            return Err(JellyfinError::InvalidImage);
        };
        offset += 1;
        if marker == 0xd9 || marker == 0xda {
            return Err(JellyfinError::InvalidImage);
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length_bytes = bytes
            .get(offset..offset + 2)
            .ok_or(JellyfinError::InvalidImage)?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 || offset + length > bytes.len() {
            return Err(JellyfinError::InvalidImage);
        }
        let is_sof = matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        );
        if is_sof {
            let data = &bytes[offset + 2..offset + length];
            if data.len() < 5 {
                return Err(JellyfinError::InvalidImage);
            }
            let height = u32::from(u16::from_be_bytes([data[1], data[2]]));
            let width = u32::from(u16::from_be_bytes([data[3], data[4]]));
            return Ok((width, height));
        }
        offset += length;
    }
    Err(JellyfinError::InvalidImage)
}

fn webp_dimensions(bytes: &[u8]) -> Result<(u32, u32), JellyfinError> {
    let chunk = bytes.get(12..16).ok_or(JellyfinError::InvalidImage)?;
    if chunk.starts_with(b"VP8X") {
        if bytes.len() < 30 {
            return Err(JellyfinError::InvalidImage);
        }
        let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
        let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);
        return Ok((width, height));
    }
    if chunk.starts_with(b"VP8L") {
        if bytes.len() < 25 || bytes[20] != 0x2f {
            return Err(JellyfinError::InvalidImage);
        }
        let packed = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
        let width = 1 + (packed & 0x3fff);
        let height = 1 + ((packed >> 14) & 0x3fff);
        return Ok((width, height));
    }
    if chunk.starts_with(b"VP8 ") {
        if bytes.len() < 30 || bytes.get(23..26) != Some(&[0x9d, 0x01, 0x2a][..]) {
            return Err(JellyfinError::InvalidImage);
        }
        let width = u32::from(u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff);
        let height = u32::from(u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff);
        return Ok((width, height));
    }
    Err(JellyfinError::InvalidImage)
}

#[cfg(test)]
mod tests {
    use super::{MAX_IMAGE_DIMENSION, image_dimensions};
    use crate::jellyfin::JellyfinError;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0]);
        bytes
    }

    #[test]
    fn png_dimensions_are_read_from_the_header() {
        assert_eq!(image_dimensions(&png(320, 180)).unwrap(), (320, 180));
    }

    #[test]
    fn oversized_png_is_rejected_before_storage() {
        let error = image_dimensions(&png(MAX_IMAGE_DIMENSION + 1, 1)).unwrap_err();
        assert!(matches!(error, JellyfinError::ImageTooLarge { .. }));
    }

    #[test]
    fn unknown_formats_are_rejected() {
        assert!(matches!(
            image_dimensions(b"not an image at all"),
            Err(JellyfinError::InvalidImage)
        ));
    }
}
