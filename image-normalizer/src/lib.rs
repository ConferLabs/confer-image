//! Decodes one untrusted image into 8-bit RGB the way vLLM's own loader does: EXIF orientation
//! applied, transparency flattened onto white, first frame only. The proxy hands vLLM the result
//! as a PPM, so the model sees the same pixels without vLLM running an image decoder.

use std::fmt;
use std::io::Cursor;

use image::imageops::{self, FilterType};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits, RgbImage};

#[cfg(target_os = "linux")]
pub mod child;
#[cfg(target_os = "linux")]
pub mod connection;

/// The proxy refuses larger encrypted objects, so no legitimate input exceeds this.
pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;

/// Pillow's decompression-bomb error threshold, the largest image vLLM accepts today.
pub const MAX_DECODED_PIXELS: u64 = 178_956_970;

/// The served model's image processor shrinks anything larger to this many pixels, so
/// downscaling here only changes how that shrink is done.
pub const MAX_OUTPUT_PIXELS: u64 = 16_777_216;

/// The served model's image processor rejects wider aspect ratios.
pub const MAX_ASPECT_RATIO: u64 = 200;

const MAX_DECODER_ALLOCATION: u64 = 1536 * 1024 * 1024;
const BACKGROUND: u32 = 255;

#[derive(Debug)]
pub enum NormalizeError {
  InputTooLarge,
  UnsupportedFormat,
  InvalidDimensions(u32, u32),
  Decode(ImageError),
}

impl fmt::Display for NormalizeError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      NormalizeError::InputTooLarge => write!(formatter, "input exceeds {MAX_INPUT_BYTES} bytes"),
      NormalizeError::UnsupportedFormat => write!(formatter, "unsupported image format"),
      NormalizeError::InvalidDimensions(width, height) => {
        write!(formatter, "unsupported image dimensions {width}x{height}")
      }
      NormalizeError::Decode(error) => write!(formatter, "decode failed: {error}"),
    }
  }
}

impl From<ImageError> for NormalizeError {
  fn from(error: ImageError) -> Self {
    NormalizeError::Decode(error)
  }
}

pub fn normalize(input: &[u8]) -> Result<RgbImage, NormalizeError> {
  if input.len() > MAX_INPUT_BYTES {
    return Err(NormalizeError::InputTooLarge);
  }

  let format = detect_format(input).ok_or(NormalizeError::UnsupportedFormat)?;
  let mut reader = ImageReader::with_format(Cursor::new(input), format);
  reader.limits(decoder_limits());

  let mut decoder = reader.into_decoder()?;
  let (width, height) = decoder.dimensions();
  check_dimensions(width, height, MAX_DECODED_PIXELS)?;

  // vLLM applies Pillow's exif_transpose and ignores EXIF it cannot read.
  let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
  let mut image = DynamicImage::from_decoder(decoder)?;
  image.apply_orientation(orientation);

  let flattened = flatten(image);
  let (fitted_width, fitted_height) = fitted_dimensions(flattened.width(), flattened.height());

  if (fitted_width, fitted_height) == flattened.dimensions() {
    Ok(flattened)
  } else {
    Ok(imageops::resize(
      &flattened,
      fitted_width,
      fitted_height,
      FilterType::CatmullRom,
    ))
  }
}

/// Whether `normalize` can produce an image of these dimensions.
pub fn is_valid_output(width: u32, height: u32) -> bool {
  check_dimensions(width, height, MAX_OUTPUT_PIXELS).is_ok()
}

/// The same signatures the proxy admits; anything else never reaches a decoder.
fn detect_format(input: &[u8]) -> Option<ImageFormat> {
  if input.starts_with(&[0xff, 0xd8, 0xff]) {
    Some(ImageFormat::Jpeg)
  } else if input.starts_with(b"\x89PNG\r\n\x1a\n") {
    Some(ImageFormat::Png)
  } else if input.starts_with(b"GIF87a") || input.starts_with(b"GIF89a") {
    Some(ImageFormat::Gif)
  } else if input.len() >= 12 && input.starts_with(b"RIFF") && &input[8..12] == b"WEBP" {
    Some(ImageFormat::WebP)
  } else {
    None
  }
}

fn decoder_limits() -> Limits {
  let mut limits = Limits::default();
  limits.max_image_width = Some(u32::try_from(MAX_DECODED_PIXELS).unwrap_or(u32::MAX));
  limits.max_image_height = Some(u32::try_from(MAX_DECODED_PIXELS).unwrap_or(u32::MAX));
  limits.max_alloc = Some(MAX_DECODER_ALLOCATION);
  limits
}

fn check_dimensions(width: u32, height: u32, max_pixels: u64) -> Result<(), NormalizeError> {
  let (long, short) = (u64::from(width.max(height)), u64::from(width.min(height)));

  if short == 0 || long * short > max_pixels || long > short * MAX_ASPECT_RATIO {
    Err(NormalizeError::InvalidDimensions(width, height))
  } else {
    Ok(())
  }
}

/// vLLM converts any image with transparency to RGBA and pastes it onto white.
fn flatten(image: DynamicImage) -> RgbImage {
  if !image.color().has_alpha() {
    return image.into_rgb8();
  }

  let rgba = image.into_rgba8();
  let mut rgb = RgbImage::new(rgba.width(), rgba.height());

  for (output, input) in rgb.pixels_mut().zip(rgba.pixels()) {
    let alpha = u32::from(input[3]);
    for channel in 0..3 {
      output[channel] = blend(input[channel], alpha);
    }
  }

  rgb
}

/// Pillow's paste-with-mask arithmetic, so flattened pixels match vLLM's exactly.
fn blend(value: u8, alpha: u32) -> u8 {
  let mixed = BACKGROUND * (255 - alpha) + u32::from(value) * alpha + 128;
  ((mixed + (mixed >> 8)) >> 8) as u8
}

/// Largest size with the same aspect ratio that fits MAX_OUTPUT_PIXELS and MAX_ASPECT_RATIO.
fn fitted_dimensions(width: u32, height: u32) -> (u32, u32) {
  let pixels = u64::from(width) * u64::from(height);
  if pixels <= MAX_OUTPUT_PIXELS {
    return (width, height);
  }

  let scale = (MAX_OUTPUT_PIXELS as f64 / pixels as f64).sqrt();
  let mut fitted_width = ((f64::from(width) * scale).floor() as u32).max(1);
  let mut fitted_height = ((f64::from(height) * scale).floor() as u32).max(1);
  let limit = MAX_ASPECT_RATIO as u32;

  if fitted_width > fitted_height * limit {
    fitted_width = fitted_height * limit;
  } else if fitted_height > fitted_width * limit {
    fitted_height = fitted_width * limit;
  }

  (fitted_width, fitted_height)
}

#[cfg(test)]
mod tests {
  use super::*;
  use image::codecs::png::PngEncoder;
  use image::{ExtendedColorType, ImageEncoder, Rgba, RgbaImage};

  fn png(image: &RgbaImage) -> Vec<u8> {
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
      .write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgba8,
      )
      .unwrap();
    bytes
  }

  #[test]
  fn refuses_formats_outside_the_allowlist() {
    for input in [
      &b""[..],
      b"BM\0\0\0\0",
      b"P6\n1 1\n255\n\0\0\0",
      b"II*\0",
      b"\0\0\0\x0cjP  ",
    ] {
      assert!(matches!(
        normalize(input),
        Err(NormalizeError::UnsupportedFormat)
      ));
    }
  }

  #[test]
  fn refuses_oversized_input_before_decoding() {
    let mut input = b"\x89PNG\r\n\x1a\n".to_vec();
    input.resize(MAX_INPUT_BYTES + 1, 0);
    assert!(matches!(
      normalize(&input),
      Err(NormalizeError::InputTooLarge)
    ));
  }

  #[test]
  fn flattens_transparency_onto_white_like_pillow() {
    let mut image = RgbaImage::new(4, 1);
    image.put_pixel(0, 0, Rgba([10, 20, 30, 255]));
    image.put_pixel(1, 0, Rgba([10, 20, 30, 0]));
    image.put_pixel(2, 0, Rgba([10, 20, 30, 128]));
    image.put_pixel(3, 0, Rgba([200, 100, 0, 1]));

    let rgb = normalize(&png(&image)).unwrap();

    // Pillow: DIV255(255 * (255 - a) + v * a)
    assert_eq!(
      rgb.as_raw(),
      &[10, 20, 30, 255, 255, 255, 132, 137, 142, 255, 254, 254]
    );
  }

  #[test]
  fn blend_matches_rounded_division_for_every_input() {
    for alpha in 0..=255u32 {
      for value in 0..=255u32 {
        let exact = (255 * (255 - alpha) + value * alpha) as f64 / 255.0;
        assert_eq!(u32::from(blend(value as u8, alpha)), exact.round() as u32);
      }
    }
  }

  #[test]
  fn refuses_aspect_ratios_the_model_rejects() {
    assert!(check_dimensions(200, 1, MAX_DECODED_PIXELS).is_ok());
    assert!(check_dimensions(201, 1, MAX_DECODED_PIXELS).is_err());
    assert!(check_dimensions(1, 201, MAX_DECODED_PIXELS).is_err());
    assert!(check_dimensions(0, 1, MAX_DECODED_PIXELS).is_err());
    assert!(check_dimensions(13_000, 13_765, MAX_DECODED_PIXELS).is_ok());
    assert!(check_dimensions(13_000, 13_766, MAX_DECODED_PIXELS).is_err());
  }

  #[test]
  fn keeps_images_within_the_output_budget_unchanged() {
    assert_eq!(fitted_dimensions(4096, 4096), (4096, 4096));
    assert_eq!(fitted_dimensions(1536, 1021), (1536, 1021));
  }

  #[test]
  fn fits_large_images_within_the_output_budget_and_aspect_limit() {
    for (width, height) in [
      (8064, 6048),
      (13_000, 13_766),
      (57_928, 290),
      (290, 57_928),
      (4097, 4097),
    ] {
      let (fitted_width, fitted_height) = fitted_dimensions(width, height);
      let fitted_pixels = u64::from(fitted_width) * u64::from(fitted_height);

      assert!(
        fitted_pixels <= MAX_OUTPUT_PIXELS,
        "{width}x{height} -> {fitted_width}x{fitted_height}"
      );
      assert!(
        fitted_pixels > MAX_OUTPUT_PIXELS * 99 / 100,
        "{width}x{height} shrank too far"
      );
      assert!(check_dimensions(fitted_width, fitted_height, MAX_OUTPUT_PIXELS).is_ok());
    }
  }

  #[test]
  fn downscales_oversized_images() {
    let image = RgbaImage::from_pixel(4200, 4200, Rgba([0, 128, 255, 255]));
    let rgb = normalize(&png(&image)).unwrap();
    let pixels = u64::from(rgb.width()) * u64::from(rgb.height());

    assert_eq!(rgb.width(), rgb.height());
    assert!(pixels <= MAX_OUTPUT_PIXELS && pixels > MAX_OUTPUT_PIXELS * 99 / 100);
    assert_eq!(
      rgb.get_pixel(rgb.width() / 2, rgb.height() / 2).0,
      [0, 128, 255]
    );
  }
}
