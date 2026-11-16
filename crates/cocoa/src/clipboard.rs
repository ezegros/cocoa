//! Local clipboard and image file reads, normalized to PNG attachments.

use std::{
    fs::OpenOptions,
    io::{self, Cursor, Read},
    path::Path,
};

use cocoa_core::model::Image;
use image::{ExtendedColorType, ImageDecoder, ImageEncoder, ImageReader, codecs::png::PngEncoder};

const IMAGE_BYTES_MAX: usize = 8 * 1024 * 1024;
const IMAGE_PIXELS_MAX: u32 = 16 * 1024 * 1024;

pub(crate) enum Content {
    Image(Image),
    Text(String),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClipboardError {
    #[error("Cannot read the local system clipboard: {0}")]
    Read(#[from] arboard::Error),
    #[error("Image exceeds 16 million pixels or has invalid dimensions.")]
    Dimensions,
    #[error("Cannot decode or encode the image: {0}")]
    Encode(#[from] image::ImageError),
    #[error(transparent)]
    Image(#[from] cocoa_core::model::ImageError),
    #[error("Clipboard text exceeds 8 MiB.")]
    TextLimit,
    #[error("Cannot read the image file: {0}")]
    File(#[from] io::Error),
    #[error("Image attachment must be a regular file no larger than 8 MiB.")]
    FileLimit,
}

/// Read the local clipboard synchronously, preferring PNG-normalized images.
///
/// # Errors
/// Reject inaccessible contents, oversized text or images, and codec failures.
pub(crate) fn read() -> Result<Content, ClipboardError> {
    let mut clipboard = arboard::Clipboard::new()?;
    match clipboard.get_image() {
        Ok(image) => {
            let width = u32::try_from(image.width).map_err(|_| ClipboardError::Dimensions)?;
            let height = u32::try_from(image.height).map_err(|_| ClipboardError::Dimensions)?;
            encode(width, height, &image.bytes)
        }
        Err(arboard::Error::ContentNotAvailable) => {
            let text = clipboard.get_text()?;
            if text.len() > 8 * 1024 * 1024 {
                return Err(ClipboardError::TextLimit);
            }
            Ok(Content::Text(text))
        }
        Err(error) => Err(error.into()),
    }
}

/// Read a bounded regular image file synchronously and normalize it to PNG.
///
/// # Errors
/// Reject inaccessible/nonregular files, byte or pixel overflow, and codec failures.
pub(crate) fn read_file(path: &Path) -> Result<Content, ClipboardError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > IMAGE_BYTES_MAX as u64 {
        return Err(ClipboardError::FileLimit);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(IMAGE_BYTES_MAX as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > IMAGE_BYTES_MAX {
        return Err(ClipboardError::FileLimit);
    }
    read_image(&bytes)
}

fn read_image(bytes: &[u8]) -> Result<Content, ClipboardError> {
    if bytes.len() > IMAGE_BYTES_MAX {
        return Err(ClipboardError::FileLimit);
    }
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(IMAGE_PIXELS_MAX);
    limits.max_image_height = Some(IMAGE_PIXELS_MAX);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    dimensions(width, height)?;
    let image = image::DynamicImage::from_decoder(decoder)?.to_rgba8();
    encode(width, height, image.as_raw())
}

fn encode(width: u32, height: u32, bytes: &[u8]) -> Result<Content, ClipboardError> {
    dimensions(width, height)?;
    if bytes.len() != width as usize * height as usize * 4 {
        return Err(ClipboardError::Dimensions);
    }
    let mut png = Vec::new();
    PngEncoder::new(&mut png).write_image(bytes, width, height, ExtendedColorType::Rgba8)?;
    Ok(Content::Image(Image::from_png(&png)?))
}

fn dimensions(width: u32, height: u32) -> Result<(), ClipboardError> {
    if width == 0
        || height == 0
        || u64::from(width) * u64::from(height) > u64::from(IMAGE_PIXELS_MAX)
    {
        return Err(ClipboardError::Dimensions);
    }
    Ok(())
}
