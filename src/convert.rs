use actix_multipart::Multipart;
use actix_rt::time::timeout;
use actix_web::http::StatusCode;
use actix_web::{HttpResponse, Responder, post, web};
use futures_util::{StreamExt, TryStreamExt};
use magick_rust::{AlphaChannelOption, FilterType, MagickWand, PixelWand};
use randomizer::Randomizer;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use crate::{ConversionLimiter, MaxDimensions, MaxFileSize, UploadLimiter};

const SUPPORTED_FORMATS: [&str; 14] = [
    "png", "jpeg", "webp", "gif", "bmp", "tiff", "ico", "avif", "heic",
    "hdr", "psd", "cr2", "pdf", "qoi"
];

fn format_to_mime(format: &str) -> &'static str {
    match format {
        "png" => "image/png",
        "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tiff" => "image/tiff",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "heic" => "image/heic",
        "hdr" => "image/vnd.radiance",
        "psd" => "image/vnd.adobe.photoshop",
        "cr2" => "image/x-canon-cr2",
        "pdf" => "application/pdf",
        "qoi" => "image/qoi",
        _ => "application/octet-stream",
    }
}

// le funny alternatives
fn normalize_format(raw: &str) -> String {
    match raw.trim().to_lowercase().as_str() {
        "jpg" => "jpeg".to_string(),
        "tif" => "tiff".to_string(),
        "heif" => "heic".to_string(),
        other => other.to_string(),
    }
}

// Accepts WIDTHxHEIGHT, a number (1:1 ratio) or nothing (same size)
fn parse_dimensions(raw_dimensions: &str) -> Result<Option<(usize, usize)>, ()> {
    let raw_dimensions = raw_dimensions.trim().to_lowercase();
    if raw_dimensions.is_empty() {
        return Ok(None);
    }

    let number = |value: &str| -> Option<usize> {
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            value.parse().ok()
        } else {
            None
        }
    };

    let parts: Vec<&str> = raw_dimensions.split('x').collect();
    match parts.as_slice() {
        [side] => number(side).map(|side| Some((side, side))).ok_or(()),
        [width, height] => match (number(width), number(height)) {
            (Some(width), Some(height)) => Ok(Some((width, height))),
            _ => Err(())
        },
        _ => Err(())
    }
}

// i would have never thought it would be called this many times
fn build_response(status: StatusCode, body: impl Into<String>) -> HttpResponse {
    HttpResponse::build(status)
        .content_type("text/plain")
        .body(body.into())
}

struct CleanupGuard(PathBuf);

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("Failed to delete file {:#?}: {}", self.0, error);
            }
        }
    }
}

#[post("/")]
pub async fn convert(mut payload: Multipart, conversion_path: web::Data<PathBuf>, upload_limiter: web::Data<UploadLimiter>, conversion_limiter: web::Data<ConversionLimiter>, max_file_size: web::Data<MaxFileSize>, max_dimensions: web::Data<MaxDimensions>) -> impl Responder {
    let upload_permit = match upload_limiter.0.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return build_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Server is too busy; please try again later",
            );
        }
    };

    let conversion_path = conversion_path.get_ref().clone();
    let max_file_size = max_file_size.0;
    let max_dimensions = max_dimensions.0;

    let conversion_id = Randomizer::ALPHANUMERIC(32).string().unwrap();
    let input_path = conversion_path.join(format!("{}-input.tmp", conversion_id));
    let _guard = CleanupGuard(input_path.clone());

    let mut format: Option<String> = None;
    let mut dimensions: Option<String> = None;
    let mut received_file = false;
    let mut infer_buffer: Vec<u8> = Vec::with_capacity(8192);
    let mut total_size: usize = 0;

    // Go through every field in the multipart form
    let read_result = timeout(Duration::from_secs(90), async {
        while let Ok(Some(mut field)) = payload.try_next().await {
            let field_name = field.name().unwrap_or_default().to_string();

            match field_name.as_str() {
                // Get the (optional) destination format and dimensions
                "format" | "dimensions" => {
                    let mut value = Vec::with_capacity(16);
                    while let Some(chunk) = field.next().await {
                        match chunk {
                            Ok(bytes) if value.len() + bytes.len() <= 16 => {
                                value.extend_from_slice(&bytes)
                            }
                            _ => return Some(build_response(StatusCode::BAD_REQUEST, "Invalid form data")),
                        }
                    }

                    let Ok(value) = String::from_utf8(value) else {
                        return Some(build_response(StatusCode::BAD_REQUEST, "Invalid form data"));
                    };

                    if field_name == "format" {
                        format = Some(value);
                    } else {
                        dimensions = Some(value);
                    }
                }

                // Get the file to convert
                "file" => {
                    if received_file {
                        while field.next().await.is_some() {}
                        continue;
                    }
                    received_file = true;

                    let mut file = match File::create(&input_path) {
                        Ok(file) => file,
                        Err(error) => {
                            eprintln!("Failed creating file: {}", error);
                            return Some(build_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Internal Server Error"
                            ));
                        }
                    };

                    // Stream the file to the disk (under a temporary name for now)
                    while let Some(chunk) = field.next().await {
                        let chunk = match chunk {
                            Ok(bytes) => bytes,
                            Err(_) => {
                                return Some(build_response(StatusCode::BAD_REQUEST, "Invalid form data"));
                            }
                        };

                        total_size += chunk.len();
                        if total_size > max_file_size {
                            return Some(build_response(StatusCode::PAYLOAD_TOO_LARGE, "File too large!"));
                        }

                        // Store the first 8kb of the file in memory for infer to detect the file type
                        if infer_buffer.len() < 8192 {
                            let take = (8192 - infer_buffer.len()).min(chunk.len());
                            infer_buffer.extend_from_slice(&chunk[..take]);
                        }

                        if let Err(error) = file.write_all(&chunk) {
                            eprintln!("Failed writing to file: {}", error);
                            return Some(build_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Internal Server Error",
                            ));
                        }
                    }
                }

                // Ignore other fields
                _ => while field.next().await.is_some() {}
            }
        }
        None
    })
    .await;

    match read_result {
        Ok(Some(early_response)) => return early_response,
        Ok(None) => {}
        Err(_) => return build_response(StatusCode::REQUEST_TIMEOUT, "Took too long to upload!"),
    }

    if !received_file || total_size == 0 {
        return build_response(StatusCode::BAD_REQUEST, "Empty file!");
    }

    drop(upload_permit);

    // Try parsing the dimensions for the converted image
    let dimensions = match parse_dimensions(dimensions.as_deref().unwrap_or("")) {
        Ok(dimensions) => dimensions,
        Err(_) => {
            return build_response(
                StatusCode::BAD_REQUEST,
                "Invalid dimensions! Use WIDTHxHEIGHT or a single number."
            );
        }
    };

    if let Some((width, height)) = dimensions {
        if width == 0 || height == 0 {
            return build_response(
                StatusCode::BAD_REQUEST,
                "Dimensions must be greater than 0!"
            );
        }
        if width > max_dimensions || height > max_dimensions {
            return build_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("Dimensions too large! Max is {0}x{0}px", max_dimensions),
            );
        }
    }

    // And also make sure that the format is valid if supplied
    let format = format
        .map(|format| normalize_format(&format))
        .filter(|format| !format.is_empty());
    if let Some(format) = &format {
        if !SUPPORTED_FORMATS.contains(&format.as_str()) || ["cr2"].contains(&format.as_str()) {
            return build_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("Unsupported output format: {format}")
            );
        }
    }

    // If neither dimensions nor a format is supplied, don't do anything lol
    if format.is_none() && dimensions.is_none() {
        return build_response(
            StatusCode::BAD_REQUEST,
            "Please supply a destination image format or width/height!",
        );
    }

    // Additional check on the input just to make sure ImageMagick doesn't get
    // any weird file
    let input_format = match infer::get(&infer_buffer) {
        Some(file_type) => {
            let extension = normalize_format(file_type.extension());
            if !SUPPORTED_FORMATS.contains(&extension.as_str()) || ["hdr", "pdf", "qoi"].contains(&extension.as_str()) {
                return build_response(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    format!(
                        "Unsupported input file type: {}",
                        extension
                    ),
                );
            }
            extension
        },

        // Apparently it rejects rejects TIFF with a first pixel looking like
        // a CR2 header. This happened while testing; not sure if anyone would
        // ever have this issue but just in case
        None if infer_buffer.starts_with(b"II*\0") || infer_buffer.starts_with(b"MM\0*") => {
            "tiff".to_string()
        },

        None => return build_response(StatusCode::BAD_REQUEST, "Could not detect input file type"),
    };

    let spec = format!("{}:{}[0]", input_format, input_path.display());
    let probe_spec = spec.clone();

    // Get the original image dimensions quickly
    let probe = web::block(move || {
        let wand = MagickWand::new();
        wand.ping_image(&probe_spec)
            .map_err(|error| format!("{:?}", error))?;
        Ok::<(usize, usize), String>((wand.get_image_width(), wand.get_image_height()))
    })
    .await;

    let (source_width, source_height) = match probe {
        Ok(Ok(size)) => size,
        Ok(Err(error)) => {
            eprintln!("Could not read image {}: {}", conversion_id, error);
            return build_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Could not read your image!",
            );
        }
        Err(_) => return build_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
    };

    if source_width > max_dimensions || source_height > max_dimensions {
        return build_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("Image too large! Max is {0}x{0}px", max_dimensions),
        );
    }

    // If the format isn't supplied, default to the original one
    let target_format = format.unwrap_or_else(|| input_format.clone());
    let (out_width, out_height) = dimensions.unwrap_or((source_width, source_height));

    // Check if the conversion/resize is even useful (pt. 2)
    if target_format == input_format && (out_width, out_height) == (source_width, source_height) {
        return build_response(
            StatusCode::BAD_REQUEST,
            "Please supply values different from the ones of the image!",
        );
    }

    // .ico must be 1:1 and not more than 256px
    if target_format == "ico" {
        if out_width != out_height {
            return build_response(
                StatusCode::BAD_REQUEST,
                "Please submit a 1:1 ratio image to convert to the ico format!",
            );
        }
        if out_width > 256 {
            return build_response(
                StatusCode::BAD_REQUEST,
                "The ico format supports a maximum of 256x256px!",
            );
        }
    }

    println!("New conversion request:");
    println!("- ID: {}", conversion_id);
    println!("- Size: {:.2} MB", total_size as f32 / 1024.0 / 1024.0);
    println!("- From: {} ({}x{})",input_format, source_width, source_height);
    println!("- To: {} ({}x{})", target_format, out_width, out_height);

    // If the amount of conversions reached MAX_CONCURRENT_CONVERSIONS then this will lock the request until one of them is done
    let conversion_permit = conversion_limiter.0.acquire().await.unwrap();

    // le long-awaited conversion
    let conversion = web::block({
        let spec = spec.clone();
        let target_format = target_format.clone();
        // Only resize when the dimensions actually changes
        let resize = dimensions.filter(|size| *size != (source_width, source_height));

        move || {
            let mut wand = MagickWand::new();
            wand.read_image(&spec)
                .map_err(|error| format!("read failed: {:?}", error))?;

            // Apply the EXIF rotation, then drop all metadata (EXIF, GPS, profiles...)
            wand.auto_orient();
            let _ = wand.strip_image();

            if let Some((width, height)) = resize {
                let _ = wand.resize_image(width, height, FilterType::Lanczos);
            }

            // For JPEG & BMP make sure the background is white
            if matches!(target_format.as_str(), "jpeg" | "bmp") {
                let mut white = PixelWand::new();
                white
                    .set_color("white")
                    .map_err(|error| format!("{:?}", error))?;
                wand.set_image_background_color(&white)
                    .map_err(|error| format!("{:?}", error))?;
                wand.set_image_alpha_channel(AlphaChannelOption::Remove)
                    .map_err(|error| format!("{:?}", error))?;
                wand.set_image_alpha_channel(AlphaChannelOption::Off)
                    .map_err(|error| format!("{:?}", error))?;
            }

            wand.set_image_format(&target_format)
                .map_err(|error| format!("unsupported format: {:?}", error))?;

            wand.write_image_blob(&target_format)
                .map_err(|error| format!("write failed: {:?}", error))
        }
    });

    let result = timeout(Duration::from_secs(120), conversion).await;

    drop(conversion_permit);

    let output = match result {
        Ok(Ok(Ok(bytes))) => bytes,
        Ok(Ok(Err(error))) => {
            eprintln!("Conversion {} failed: {}", conversion_id, error);
            return build_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not convert your image!",
            );
        }
        Ok(Err(_)) => return build_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
        Err(_) => {
            eprintln!("Conversion {} timed out", conversion_id);
            return build_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }
    };

    println!("Successfully converted {} from {} to {}!", conversion_id, input_format, target_format);

    HttpResponse::Ok().content_type(format_to_mime(&target_format)).body(output)
}