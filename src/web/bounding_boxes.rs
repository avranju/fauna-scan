//! Render validated image-relative animal boxes into a JPEG response.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, Rgb, RgbImage};

use crate::classifier::BoundingBox;

const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_PIXELS: u64 = 32_000_000;

pub(super) fn render_jpeg_with_boxes(
    jpeg: &[u8],
    boxes: &[BoundingBox],
    color: [u8; 3],
    stroke_width: u32,
) -> Result<Vec<u8>, &'static str> {
    let mut reader = ImageReader::with_format(Cursor::new(jpeg), ImageFormat::Jpeg);
    let mut limits = Limits::default();
    limits.max_image_width = Some(32_768);
    limits.max_image_height = Some(32_768);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);

    let mut decoder = reader.into_decoder().map_err(|_| "JPEG decoding failed")?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODED_BYTES
    {
        return Err("image exceeds the drawing size limit");
    }
    let orientation = decoder
        .orientation()
        .map_err(|_| "JPEG orientation is invalid")?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| "JPEG decoding failed")?;
    image.apply_orientation(orientation);
    let mut pixels = image.to_rgb8();

    for bounding_box in boxes {
        draw_box(&mut pixels, bounding_box, Rgb(color), stroke_width);
    }

    let mut output = Vec::with_capacity(jpeg.len());
    JpegEncoder::new_with_quality(&mut output, 90)
        .encode_image(&pixels)
        .map_err(|_| "JPEG encoding failed")?;
    Ok(output)
}

fn draw_box(image: &mut RgbImage, box_: &BoundingBox, color: Rgb<u8>, stroke_width: u32) {
    let (width, height) = image.dimensions();
    if width == 0
        || height == 0
        || ![box_.x_min, box_.y_min, box_.x_max, box_.y_max]
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
        || box_.x_min >= box_.x_max
        || box_.y_min >= box_.y_max
    {
        return;
    }

    let left = ((box_.x_min * f64::from(width)).floor() as u32).min(width - 1);
    let top = ((box_.y_min * f64::from(height)).floor() as u32).min(height - 1);
    let right = ((box_.x_max * f64::from(width)).ceil() as u32)
        .saturating_sub(1)
        .min(width - 1);
    let bottom = ((box_.y_max * f64::from(height)).ceil() as u32)
        .saturating_sub(1)
        .min(height - 1);

    for inset in 0..stroke_width {
        if left + inset > right.saturating_sub(inset) || top + inset > bottom.saturating_sub(inset)
        {
            break;
        }
        let x0 = left + inset;
        let y0 = top + inset;
        let x1 = right - inset;
        let y1 = bottom - inset;
        for x in x0..=x1 {
            image.put_pixel(x, y0, color);
            image.put_pixel(x, y1, color);
        }
        for y in y0..=y1 {
            image.put_pixel(x0, y, color);
            image.put_pixel(x1, y, color);
        }
    }
}

/// A bounded, orientation-correct gallery image. Shares the render semaphore.
pub(super) fn thumbnail(jpeg: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut reader = ImageReader::with_format(Cursor::new(jpeg), ImageFormat::Jpeg);
    let mut limits = Limits::default();
    limits.max_image_width = Some(32_768);
    limits.max_image_height = Some(32_768);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| "JPEG decoding failed")?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODED_BYTES
    {
        return Err("image exceeds the thumbnail size limit");
    }
    let orientation = decoder.orientation().map_err(|_| "invalid orientation")?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| "JPEG decoding failed")?;
    image.apply_orientation(orientation);
    let image = image.thumbnail(480, 360);
    let mut output = Vec::new();
    JpegEncoder::new_with_quality(&mut output, 80)
        .encode_image(&image)
        .map_err(|_| "JPEG encoding failed")?;
    Ok(output)
}
