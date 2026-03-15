use std::{path::PathBuf, time::Duration};

use axum::http::StatusCode;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command};

use crate::{
    error::AppError,
    image_proxy::process_image,
    models::{ImageRequest, ProcessedImage},
};

#[derive(Debug, Serialize, Deserialize)]
struct WorkerRequest {
    content_type: String,
    bytes_base64: String,
    image_request: ImageRequest,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkerResponse {
    content_type: String,
    optimized: bool,
    bytes_base64: String,
}

pub async fn process_image_with_helper(
    worker_binary: Option<&PathBuf>,
    bytes: &[u8],
    content_type: &str,
    request: &ImageRequest,
    timeout_ms: u64,
) -> Result<ProcessedImage, AppError> {
    let worker_path = resolve_worker_binary(worker_binary)?;
    let mut child = Command::new(worker_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            AppError::internal_with_message(format!("failed to spawn image worker: {error}"))
        })?;

    let request_payload = serde_json::to_vec(&WorkerRequest {
        content_type: content_type.to_string(),
        bytes_base64: STANDARD.encode(bytes),
        image_request: request.clone(),
    })?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::internal_with_message("image worker stdin unavailable"))?;
    stdin.write_all(&request_payload).await.map_err(|error| {
        AppError::internal_with_message(format!("failed to write image worker stdin: {error}"))
    })?;
    drop(stdin);

    let timeout = Duration::from_millis(timeout_ms.max(1000));
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            AppError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "IMAGE_TRANSFORM_TIMEOUT",
                "Image worker timed out",
            )
        })?
        .map_err(|error| {
            AppError::internal_with_message(format!("image worker execution failed: {error}"))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::internal_with_message(format!(
            "image worker failed: {}",
            stderr.trim()
        )));
    }

    let response: WorkerResponse = serde_json::from_slice(&output.stdout).map_err(|error| {
        AppError::internal_with_message(format!("failed to decode image worker output: {error}"))
    })?;
    let bytes = STANDARD.decode(response.bytes_base64).map_err(|error| {
        AppError::internal_with_message(format!("failed to decode image worker bytes: {error}"))
    })?;

    Ok(ProcessedImage {
        bytes,
        content_type: response.content_type,
        optimized: response.optimized,
    })
}

pub fn run_worker() -> Result<(), Box<dyn std::error::Error>> {
    let mut stdin = std::io::stdin().lock();
    let mut payload = Vec::new();
    std::io::Read::read_to_end(&mut stdin, &mut payload)?;

    let request: WorkerRequest = serde_json::from_slice(&payload)?;
    let input_bytes = STANDARD.decode(request.bytes_base64)?;
    let processed = process_image(&input_bytes, &request.content_type, &request.image_request)?;
    let response = WorkerResponse {
        content_type: processed.content_type.to_string(),
        optimized: processed.optimized,
        bytes_base64: STANDARD.encode(processed.bytes),
    };

    let stdout = std::io::stdout();
    serde_json::to_writer(stdout.lock(), &response)?;
    Ok(())
}

fn resolve_worker_binary(worker_binary: Option<&PathBuf>) -> Result<PathBuf, AppError> {
    if let Some(path) = worker_binary {
        return Ok(path.clone());
    }

    let current = std::env::current_exe().map_err(|error| {
        AppError::internal_with_message(format!("failed to resolve current executable: {error}"))
    })?;
    let file_name = if cfg!(windows) {
        "image_worker.exe"
    } else {
        "image_worker"
    };
    let sibling = current.with_file_name(file_name);
    if sibling.exists() {
        Ok(sibling)
    } else {
        Err(AppError::internal_with_message(format!(
            "image worker binary not found at {}",
            sibling.display()
        )))
    }
}
