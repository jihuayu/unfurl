use std::{path::PathBuf, time::Duration};

use axum::http::StatusCode;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

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
#[serde(tag = "status", rename_all = "snake_case")]
enum WorkerResponse {
    Ok {
        content_type: String,
        optimized: bool,
        bytes_base64: String,
    },
    Error {
        status_code: u16,
        code: String,
        message: String,
    },
}

pub async fn process_image_with_helper(
    worker_binary: Option<&PathBuf>,
    bytes: &[u8],
    content_type: &str,
    request: &ImageRequest,
    timeout_ms: u64,
) -> Result<ProcessedImage, AppError> {
    if bytes.len() > 32 * 1024 * 1024 {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "IMAGE_RESOURCE_LIMIT",
            "Image input exceeds the limit",
        ));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms.max(1000));
    let worker_path = resolve_worker_binary(worker_binary)?;
    let request_payload = serde_json::to_vec(&WorkerRequest {
        content_type: content_type.to_string(),
        bytes_base64: STANDARD.encode(bytes),
        image_request: request.clone(),
    })?;
    let mut child = Command::new(worker_path)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            AppError::internal_with_message(format!("failed to spawn image worker: {error}"))
        })?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::internal_with_message("image worker stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::internal_with_message("image worker stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::internal_with_message("image worker stderr unavailable"))?;
    // Writes and reads participate in the same deadline. Concurrent reads also
    // prevent stdout/stderr pipe pressure from deadlocking the child.
    let (_, stdout, stderr, status) = tokio::time::timeout_at(deadline, async {
        tokio::try_join!(
            async {
                stdin.write_all(&request_payload).await?;
                drop(stdin);
                Ok::<_, std::io::Error>(())
            },
            read_worker_output(stdout, 96 * 1024 * 1024),
            read_worker_output(stderr, 1024 * 1024),
            child.wait()
        )
    })
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

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(AppError::internal_with_message(format!(
            "image worker failed: {}",
            stderr.trim()
        )));
    }

    let response: WorkerResponse = serde_json::from_slice(&stdout).map_err(|error| {
        AppError::internal_with_message(format!("failed to decode image worker output: {error}"))
    })?;
    match response {
        WorkerResponse::Error {
            status_code,
            code,
            message,
        } => Err(AppError::new(
            StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            code,
            message,
        )),
        WorkerResponse::Ok {
            content_type,
            optimized,
            bytes_base64,
        } => {
            let bytes = STANDARD.decode(bytes_base64).map_err(|error| {
                AppError::internal_with_message(format!(
                    "failed to decode image worker bytes: {error}"
                ))
            })?;
            Ok(ProcessedImage {
                bytes,
                content_type,
                optimized,
            })
        }
    }
}

pub fn run_worker() -> Result<(), Box<dyn std::error::Error>> {
    let mut stdin = std::io::stdin().lock();
    let mut payload = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(&mut stdin, 48 * 1024 * 1024 + 1),
        &mut payload,
    )?;
    if payload.len() > 48 * 1024 * 1024 {
        return Err("image worker input exceeds the limit".into());
    }

    let request: WorkerRequest = serde_json::from_slice(&payload)?;
    let input_bytes = STANDARD.decode(request.bytes_base64)?;
    let response = match process_image(&input_bytes, &request.content_type, &request.image_request)
    {
        Ok(processed) => WorkerResponse::Ok {
            content_type: processed.content_type,
            optimized: processed.optimized,
            bytes_base64: STANDARD.encode(processed.bytes),
        },
        Err(error) => WorkerResponse::Error {
            status_code: error.status.as_u16(),
            code: error.code,
            message: error.message,
        },
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

async fn read_worker_output(
    stream: impl tokio::io::AsyncRead + Unpin,
    limit: u64,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    stream.take(limit + 1).read_to_end(&mut output).await?;
    if output.len() as u64 > limit {
        return Err(std::io::Error::other(
            "image worker output exceeds the limit",
        ));
    }
    Ok(output)
}
#[cfg(all(test, unix))]
mod worker_budget_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[tokio::test]
    async fn helper_stdin_backpressure_has_a_deadline_and_child_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("stalled-helper");
        let pid_path = dir.path().join("pid");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n",
                pid_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let request = ImageRequest {
            width: Some(20),
            height: Some(20),
            quality: 80,
            format: crate::models::ImageFormat::Png,
            fit: crate::models::ImageFit::Contain,
        };
        let start = std::time::Instant::now();
        let error = process_image_with_helper(
            Some(&script),
            &vec![0; 1024 * 1024],
            "image/png",
            &request,
            1000,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "IMAGE_TRANSFORM_TIMEOUT");
        assert!(start.elapsed() < Duration::from_secs(3));
        let pid = std::fs::read_to_string(pid_path).unwrap();
        // Tokio reaps killed children asynchronously; allow the event loop to run.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if !std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
            {
                return;
            }
        }
        panic!("timed-out helper still alive");
    }
}
