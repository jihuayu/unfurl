use std::env;

use opentelemetry::{global, trace::TracerProvider};
use opentelemetry_sdk::{Resource, trace::SdkTracerProvider};
use tracing_subscriber::{EnvFilter, Registry, layer::SubscriberExt, util::SubscriberInitExt};

use crate::error::AppError;

pub struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
}

impl TelemetryGuard {
    pub fn shutdown(self) {
        if let Some(provider) = self.tracer_provider {
            let _ = provider.shutdown();
        }
    }
}

pub fn init() -> Result<TelemetryGuard, AppError> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer();

    if otlp_enabled() {
        let service_name =
            env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "unfurl-server".to_string());
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()
            .map_err(|error| {
                AppError::internal_with_message(format!("failed to build otlp exporter: {error}"))
            })?;
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                Resource::builder_empty()
                    .with_service_name(service_name)
                    .build(),
            )
            .build();
        let tracer = provider.tracer("unfurl-server");

        Registry::default()
            .with(env_filter)
            .with(fmt_layer)
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .init();
        global::set_tracer_provider(provider.clone());

        Ok(TelemetryGuard {
            tracer_provider: Some(provider),
        })
    } else {
        Registry::default().with(env_filter).with(fmt_layer).init();
        Ok(TelemetryGuard {
            tracer_provider: None,
        })
    }
}

fn otlp_enabled() -> bool {
    env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}
