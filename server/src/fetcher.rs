use std::time::Duration;

use axum::http::StatusCode;
use reqwest::{Client, Response, header};

use crate::error::AppError;

const USER_AGENTS: [&str; 5] = [
    "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36",
    "WhatsApp/2.24.0 i",
    "Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)",
    "Twitterbot/1.0",
];

pub async fn fetch_page(client: &Client, url: &str, timeout_ms: u64) -> Result<Response, AppError> {
    let max_attempts = USER_AGENTS.len().min(3);
    let mut last_response: Option<Response> = None;
    let mut last_timeout = false;

    for (index, user_agent) in USER_AGENTS.iter().take(max_attempts).enumerate() {
        match client
            .get(url)
            .timeout(Duration::from_millis(timeout_ms))
            .header(header::USER_AGENT, *user_agent)
            .header(
                header::ACCEPT,
                "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8",
            )
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                if (status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS)
                    && index + 1 < max_attempts
                {
                    last_response = Some(response);
                    continue;
                }
                return Ok(response);
            }
            Err(error) => {
                last_timeout = error.is_timeout();
                if index + 1 == max_attempts {
                    break;
                }
            }
        }
    }

    if let Some(response) = last_response {
        return Ok(response);
    }

    if last_timeout {
        return Err(AppError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "FETCH_TIMEOUT",
            format!("Fetching {url} timed out after {timeout_ms}ms"),
        ));
    }

    Err(AppError::new(
        StatusCode::BAD_GATEWAY,
        "FETCH_FAILED",
        format!("Unable to fetch {url}"),
    ))
}
