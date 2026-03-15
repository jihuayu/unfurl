use std::io::Cursor;

use axum::http::StatusCode;
use image::{
    DynamicImage, GenericImageView, ImageEncoder,
    codecs::{avif::AvifEncoder, jpeg::JpegEncoder, png::PngEncoder, webp::WebPEncoder},
    imageops::{self, FilterType},
};

use crate::{
    error::AppError,
    models::{ImageFit, ImageFormat, ImageRequest, ProcessedImage},
    utils::DEFAULT_IMAGE_QUALITY,
};

pub fn process_image(
    bytes: &[u8],
    content_type: &str,
    request: &ImageRequest,
) -> Result<ProcessedImage, AppError> {
    let source_format = image_format_from_content_type(content_type);
    let needs_resize = request.width.is_some() || request.height.is_some();
    let needs_reencode =
        source_format.as_ref() != Some(&request.format) || request.quality != DEFAULT_IMAGE_QUALITY;

    if !needs_resize && !needs_reencode {
        return Ok(ProcessedImage {
            bytes: bytes.to_vec(),
            content_type: content_type_for_format(
                source_format.as_ref().unwrap_or(&request.format),
            )
            .to_string(),
            optimized: false,
        });
    }

    let Some(source_format) = source_format else {
        return Err(AppError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED_MEDIA_TYPE",
            "Origin image format cannot be transformed",
        ));
    };

    let mut image = image::load_from_memory(bytes).map_err(|error| {
        AppError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED_MEDIA_TYPE",
            format!("Failed to decode image: {error}"),
        )
    })?;

    if needs_resize {
        image = transform_image(image, request);
    }

    let format = request.format.clone();
    let encoded = encode_image(image, &format, request.quality)?;
    Ok(ProcessedImage {
        bytes: encoded,
        content_type: content_type_for_format(&format).to_string(),
        optimized: source_format != format || needs_resize,
    })
}

fn transform_image(image: DynamicImage, request: &ImageRequest) -> DynamicImage {
    match request.fit {
        ImageFit::Contain => resize_contain(&image, request.width, request.height, false),
        ImageFit::ScaleDown => resize_contain(&image, request.width, request.height, true),
        ImageFit::Cover | ImageFit::Crop => resize_cover(&image, request.width, request.height),
        ImageFit::Pad => resize_pad(&image, request.width, request.height),
    }
}

fn resize_contain(
    image: &DynamicImage,
    width: Option<u32>,
    height: Option<u32>,
    scale_down_only: bool,
) -> DynamicImage {
    let (original_width, original_height) = image.dimensions();
    let Some((target_width, target_height)) = resize_dimensions(
        original_width,
        original_height,
        width,
        height,
        false,
        scale_down_only,
    ) else {
        return image.clone();
    };
    if target_width == original_width && target_height == original_height {
        return image.clone();
    }
    image.resize_exact(target_width, target_height, FilterType::Lanczos3)
}

fn resize_cover(image: &DynamicImage, width: Option<u32>, height: Option<u32>) -> DynamicImage {
    let (original_width, original_height) = image.dimensions();
    let (Some(target_width), Some(target_height)) = (width, height) else {
        return resize_contain(image, width, height, false);
    };
    let (resized_width, resized_height) = resize_dimensions(
        original_width,
        original_height,
        Some(target_width),
        Some(target_height),
        true,
        false,
    )
    .unwrap_or((target_width, target_height));
    let resized = image.resize_exact(resized_width, resized_height, FilterType::Lanczos3);
    let x = (resized_width.saturating_sub(target_width)) / 2;
    let y = (resized_height.saturating_sub(target_height)) / 2;
    DynamicImage::ImageRgba8(
        imageops::crop_imm(&resized.to_rgba8(), x, y, target_width, target_height).to_image(),
    )
}

fn resize_pad(image: &DynamicImage, width: Option<u32>, height: Option<u32>) -> DynamicImage {
    let (Some(target_width), Some(target_height)) = (width, height) else {
        return resize_contain(image, width, height, false);
    };
    let resized = resize_contain(image, Some(target_width), Some(target_height), false);
    let (resized_width, resized_height) = resized.dimensions();
    let mut canvas =
        image::RgbaImage::from_pixel(target_width, target_height, image::Rgba([255, 255, 255, 0]));
    let x = ((target_width - resized_width) / 2) as i64;
    let y = ((target_height - resized_height) / 2) as i64;
    imageops::overlay(&mut canvas, &resized.to_rgba8(), x, y);
    DynamicImage::ImageRgba8(canvas)
}

fn resize_dimensions(
    original_width: u32,
    original_height: u32,
    width: Option<u32>,
    height: Option<u32>,
    cover: bool,
    scale_down_only: bool,
) -> Option<(u32, u32)> {
    match (width, height) {
        (None, None) => None,
        (Some(width), None) => {
            let ratio = width as f32 / original_width as f32;
            let ratio = if scale_down_only {
                ratio.min(1.0)
            } else {
                ratio
            };
            Some((
                scaled_dimension(original_width, ratio),
                scaled_dimension(original_height, ratio),
            ))
        }
        (None, Some(height)) => {
            let ratio = height as f32 / original_height as f32;
            let ratio = if scale_down_only {
                ratio.min(1.0)
            } else {
                ratio
            };
            Some((
                scaled_dimension(original_width, ratio),
                scaled_dimension(original_height, ratio),
            ))
        }
        (Some(width), Some(height)) => {
            let width_ratio = width as f32 / original_width as f32;
            let height_ratio = height as f32 / original_height as f32;
            let mut ratio = if cover {
                width_ratio.max(height_ratio)
            } else {
                width_ratio.min(height_ratio)
            };
            if scale_down_only {
                ratio = ratio.min(1.0);
            }
            Some((
                scaled_dimension(original_width, ratio),
                scaled_dimension(original_height, ratio),
            ))
        }
    }
}

fn scaled_dimension(value: u32, ratio: f32) -> u32 {
    ((value as f32 * ratio).round() as u32).max(1)
}

fn encode_image(
    image: DynamicImage,
    format: &ImageFormat,
    quality: u8,
) -> Result<Vec<u8>, AppError> {
    let mut output = Cursor::new(Vec::new());
    match format {
        ImageFormat::Jpeg => {
            let rgb = image.to_rgb8();
            JpegEncoder::new_with_quality(&mut output, quality)
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(image_error)?;
        }
        ImageFormat::Png => {
            let rgba = image.to_rgba8();
            PngEncoder::new(&mut output)
                .write_image(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(image_error)?;
        }
        ImageFormat::Webp => {
            let rgba = image.to_rgba8();
            WebPEncoder::new_lossless(&mut output)
                .write_image(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(image_error)?;
        }
        ImageFormat::Avif => {
            let rgba = image.to_rgba8();
            AvifEncoder::new_with_speed_quality(&mut output, 5, quality)
                .write_image(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(image_error)?;
        }
        ImageFormat::Auto => unreachable!("auto should be resolved before encoding"),
    }

    Ok(output.into_inner())
}

fn image_format_from_content_type(content_type: &str) -> Option<ImageFormat> {
    match content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/avif" => Some(ImageFormat::Avif),
        "image/webp" => Some(ImageFormat::Webp),
        "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
        "image/png" => Some(ImageFormat::Png),
        _ => None,
    }
}

pub fn content_type_for_format(format: &ImageFormat) -> &'static str {
    match format {
        ImageFormat::Avif => "image/avif",
        ImageFormat::Webp => "image/webp",
        ImageFormat::Jpeg | ImageFormat::Auto => "image/jpeg",
        ImageFormat::Png => "image/png",
    }
}

fn image_error(error: image::ImageError) -> AppError {
    AppError::new(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "UNSUPPORTED_MEDIA_TYPE",
        format!("Failed to process image: {error}"),
    )
}
