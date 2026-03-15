use std::{
    net::SocketAddr,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3::Client as S3Client;
use axum::{
    Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{Response, StatusCode, header},
    routing::get,
};
use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    sync::Mutex,
    task::JoinSet,
};
use unfurl_server::{build_app_with_client, config::Config};

const MOCK_HOST: &str = "mock-bench.example.test";
const CONCURRENCY_LEVELS: [usize; 3] = [10, 100, 200];
const CACHE_SIZES: [usize; 3] = [200, 1000, 5000];
const HIT_MODES: [HitMode; 3] = [HitMode::HitOnly, HitMode::MissOnly, HitMode::Mixed80_20];
const IMAGE_WIDTH: u32 = 1200;
const IMAGE_HEIGHT: u32 = 630;
const REQUEST_COUNT_FLOOR: usize = 1000;
const PREWARM_CONCURRENCY: usize = 50;
const RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

type BenchError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
struct MockState {
    png: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum EndpointKind {
    Api,
    Image,
}

impl EndpointKind {
    fn label(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Image => "image",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HitMode {
    HitOnly,
    MissOnly,
    Mixed80_20,
}

impl HitMode {
    fn label(self) -> &'static str {
        match self {
            Self::HitOnly => "hit_only",
            Self::MissOnly => "miss_only",
            Self::Mixed80_20 => "mixed_80_20",
        }
    }

    fn configured_hit_ratio(self) -> f64 {
        match self {
            Self::HitOnly => 1.0,
            Self::MissOnly => 0.0,
            Self::Mixed80_20 => 0.8,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ImageBackendKind {
    Sqlite,
    S3,
}

impl ImageBackendKind {
    fn label(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::S3 => "s3",
        }
    }

    fn env_value(self) -> &'static str {
        self.label()
    }
}

#[derive(Debug, Clone)]
struct RequestSpec {
    url: String,
    expected_hit: bool,
    expected_status: StatusCode,
}

#[derive(Debug, Clone, Default)]
struct ParsedServerTimings {
    cache_read_ms: Option<f64>,
    fetch_upstream_ms: Option<f64>,
    transform_ms: Option<f64>,
    cache_write_ms: Option<f64>,
}

#[derive(Debug, Clone)]
struct RequestMeasurement {
    actual_hit: bool,
    latency_ms: f64,
    stages: ParsedServerTimings,
}

#[derive(Debug, Default, Clone, Serialize)]
struct ResourceStats {
    avg_cpu_percent: f32,
    peak_cpu_percent: f32,
    avg_memory_mb: f64,
    peak_memory_mb: f64,
}

#[derive(Debug, Clone, Serialize)]
struct LatencyStats {
    count: usize,
    avg_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
struct StageLatencyStats {
    cache_read: LatencyStats,
    fetch_upstream: LatencyStats,
    transform: LatencyStats,
    cache_write: LatencyStats,
}

#[derive(Debug, Clone, Serialize)]
struct ScenarioReport {
    endpoint: EndpointKind,
    hit_mode: HitMode,
    image_cache_backend: ImageBackendKind,
    cache_size: usize,
    concurrency: usize,
    total_requests: usize,
    configured_hit_ratio: f64,
    observed_hit_ratio: f64,
    resources: ResourceStats,
    total_latency: LatencyStats,
    hit_latency: LatencyStats,
    miss_latency: LatencyStats,
    hit_stage_latency: StageLatencyStats,
    miss_stage_latency: StageLatencyStats,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    generated_at: String,
    note: &'static str,
    scenarios: Vec<ScenarioReport>,
}

struct BenchmarkFilter {
    cache_sizes: Option<Vec<usize>>,
    concurrencies: Option<Vec<usize>>,
    hit_modes: Option<Vec<HitMode>>,
    endpoints: Option<Vec<EndpointKind>>,
    image_backends: Option<Vec<ImageBackendKind>>,
}

#[derive(Clone)]
struct BenchmarkS3Config {
    endpoint: String,
    region: String,
    bucket: String,
    access_key_id: String,
    secret_access_key: String,
    public_base_url: String,
    force_path_style: bool,
    prefix: String,
}

struct SpawnedProcess {
    child: Child,
    address: SocketAddr,
    _guard: TempDir,
}

impl SpawnedProcess {
    fn id(&self) -> Result<u32, BenchError> {
        self.child
            .id()
            .ok_or_else(|| "spawned process has no pid".into())
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

#[derive(Default)]
struct ResourceAccumulator {
    samples: usize,
    cpu_sum: f64,
    cpu_peak: f32,
    memory_sum_mb: f64,
    memory_peak_mb: f64,
}

impl ResourceAccumulator {
    fn push(&mut self, cpu_percent: f32, memory_mb: f64) {
        self.samples += 1;
        self.cpu_sum += cpu_percent as f64;
        self.cpu_peak = self.cpu_peak.max(cpu_percent);
        self.memory_sum_mb += memory_mb;
        self.memory_peak_mb = self.memory_peak_mb.max(memory_mb);
    }

    fn snapshot(&self) -> ResourceStats {
        if self.samples == 0 {
            return ResourceStats::default();
        }

        ResourceStats {
            avg_cpu_percent: (self.cpu_sum / self.samples as f64) as f32,
            peak_cpu_percent: self.cpu_peak,
            avg_memory_mb: self.memory_sum_mb / self.samples as f64,
            peak_memory_mb: self.memory_peak_mb,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), BenchError> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("mock") => run_mock_process(&args[1..]).await,
        Some("server") => run_server_process(&args[1..]).await,
        Some(other) => Err(format!("unsupported benchmark subcommand: {other}").into()),
        None => run_orchestrator().await,
    }
}

async fn run_orchestrator() -> Result<(), BenchError> {
    let executable = std::env::current_exe()?;
    let mock = spawn_mock_process(&executable).await?;
    let s3_config = BenchmarkS3Config::from_env();
    let filter = BenchmarkFilter::from_env()?;

    if let Some(config) = &s3_config {
        ensure_bucket_public(config).await?;
    }

    let mut scenarios = Vec::new();
    for &cache_size in filter.cache_sizes().iter() {
        scenarios.extend(
            run_profile(
                &executable,
                mock.address.port(),
                cache_size,
                ImageBackendKind::Sqlite,
                None,
                &filter,
            )
            .await?,
        );

        if let Some(config) = &s3_config {
            scenarios.extend(
                run_profile(
                    &executable,
                    mock.address.port(),
                    cache_size,
                    ImageBackendKind::S3,
                    Some(config.clone()),
                    &filter,
                )
                .await?,
            );
        }
    }

    mock.shutdown().await;

    let report = BenchmarkReport {
        generated_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?,
        note: "CPU and memory are sampled from the unfurl server process only. S3 scenarios exclude MinIO resource usage from the server resource stats.",
        scenarios,
    };

    let output = serde_json::to_string_pretty(&report)?;
    std::fs::write("benchmark-results.json", &output)?;
    println!("{output}");
    Ok(())
}

async fn run_profile(
    executable: &Path,
    mock_port: u16,
    cache_size: usize,
    image_backend: ImageBackendKind,
    s3_config: Option<BenchmarkS3Config>,
    filter: &BenchmarkFilter,
) -> Result<Vec<ScenarioReport>, BenchError> {
    println!(
        "starting profile: image_backend={} cache_size={}",
        image_backend.label(),
        cache_size
    );

    let temp_dir = TempDir::new()?;
    let sqlite_path = temp_dir.path().join("bench.db");
    let s3_prefix = format!(
        "{}/{}/{cache_size}",
        s3_config
            .as_ref()
            .map(|config| config.prefix.as_str())
            .unwrap_or("benchmark"),
        image_backend.label(),
    );
    let server = spawn_server_process(
        executable,
        mock_port,
        &sqlite_path,
        image_backend,
        s3_config.as_ref(),
        &s3_prefix,
    )
    .await?;

    let client = benchmark_client()?;
    prewarm_cache(&client, &server.base_url(), cache_size, image_backend).await?;

    let mut reports = Vec::new();
    let mut scenario_index = 0usize;
    let endpoints = match image_backend {
        ImageBackendKind::Sqlite => vec![EndpointKind::Api, EndpointKind::Image],
        ImageBackendKind::S3 => vec![EndpointKind::Image],
    }
    .into_iter()
    .filter(|endpoint| filter.matches_endpoint(*endpoint))
    .collect::<Vec<_>>();

    if endpoints.is_empty() || !filter.matches_image_backend(image_backend) {
        server.shutdown().await;
        return Ok(Vec::new());
    }

    for endpoint in endpoints {
        for hit_mode in filter.hit_modes().iter().copied() {
            for &concurrency in filter.concurrencies().iter() {
                let total_requests = REQUEST_COUNT_FLOOR.max(cache_size);
                let requests = build_requests(
                    &server.base_url(),
                    cache_size,
                    total_requests,
                    endpoint,
                    hit_mode,
                    image_backend,
                    cache_size + (scenario_index + 1) * 100_000,
                );

                println!(
                    "running scenario: backend={} endpoint={} hit_mode={} cache_size={} concurrency={}",
                    image_backend.label(),
                    endpoint.label(),
                    hit_mode.label(),
                    cache_size,
                    concurrency,
                );
                let report = execute_scenario(
                    server.id()?,
                    client.clone(),
                    requests,
                    endpoint,
                    hit_mode,
                    image_backend,
                    cache_size,
                    concurrency,
                )
                .await?;
                println!(
                    "done: backend={} endpoint={} hit_mode={} cache_size={} concurrency={} total_avg={:.2}ms hit_avg={:.2}ms miss_avg={:.2}ms peak_mem={:.2}MB peak_cpu={:.2}%",
                    image_backend.label(),
                    endpoint.label(),
                    hit_mode.label(),
                    cache_size,
                    concurrency,
                    report.total_latency.avg_ms,
                    report.hit_latency.avg_ms,
                    report.miss_latency.avg_ms,
                    report.resources.peak_memory_mb,
                    report.resources.peak_cpu_percent,
                );
                reports.push(report);
                scenario_index += 1;
            }
        }
    }

    server.shutdown().await;
    Ok(reports)
}

impl BenchmarkFilter {
    fn from_env() -> Result<Self, BenchError> {
        Ok(Self {
            cache_sizes: parse_env_list("BENCH_CACHE_SIZES", parse_usize_item)?,
            concurrencies: parse_env_list("BENCH_CONCURRENCIES", parse_usize_item)?,
            hit_modes: parse_env_list("BENCH_HIT_MODES", parse_hit_mode_item)?,
            endpoints: parse_env_list("BENCH_ENDPOINTS", parse_endpoint_item)?,
            image_backends: parse_env_list("BENCH_IMAGE_BACKENDS", parse_image_backend_item)?,
        })
    }

    fn cache_sizes(&self) -> Vec<usize> {
        self.cache_sizes
            .clone()
            .unwrap_or_else(|| CACHE_SIZES.to_vec())
    }

    fn concurrencies(&self) -> Vec<usize> {
        self.concurrencies
            .clone()
            .unwrap_or_else(|| CONCURRENCY_LEVELS.to_vec())
    }

    fn hit_modes(&self) -> Vec<HitMode> {
        self.hit_modes.clone().unwrap_or_else(|| HIT_MODES.to_vec())
    }

    fn matches_endpoint(&self, endpoint: EndpointKind) -> bool {
        self.endpoints
            .as_ref()
            .map(|items| items.contains(&endpoint))
            .unwrap_or(true)
    }

    fn matches_image_backend(&self, image_backend: ImageBackendKind) -> bool {
        self.image_backends
            .as_ref()
            .map(|items| items.contains(&image_backend))
            .unwrap_or(true)
    }
}

fn parse_env_list<T>(
    name: &str,
    parser: impl Fn(&str) -> Result<T, BenchError>,
) -> Result<Option<Vec<T>>, BenchError> {
    let Some(raw) = std::env::var(name).ok() else {
        return Ok(None);
    };

    let mut items = Vec::new();
    for part in raw
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        items.push(parser(part)?);
    }

    if items.is_empty() {
        Ok(None)
    } else {
        Ok(Some(items))
    }
}

fn parse_usize_item(value: &str) -> Result<usize, BenchError> {
    value
        .parse::<usize>()
        .map_err(|error| format!("invalid numeric filter value `{value}`: {error}").into())
}

fn parse_hit_mode_item(value: &str) -> Result<HitMode, BenchError> {
    match value {
        "hit_only" => Ok(HitMode::HitOnly),
        "miss_only" => Ok(HitMode::MissOnly),
        "mixed_80_20" => Ok(HitMode::Mixed80_20),
        _ => Err(format!("invalid BENCH_HIT_MODES item `{value}`").into()),
    }
}

fn parse_endpoint_item(value: &str) -> Result<EndpointKind, BenchError> {
    match value {
        "api" => Ok(EndpointKind::Api),
        "image" => Ok(EndpointKind::Image),
        _ => Err(format!("invalid BENCH_ENDPOINTS item `{value}`").into()),
    }
}

fn parse_image_backend_item(value: &str) -> Result<ImageBackendKind, BenchError> {
    match value {
        "sqlite" => Ok(ImageBackendKind::Sqlite),
        "s3" => Ok(ImageBackendKind::S3),
        _ => Err(format!("invalid BENCH_IMAGE_BACKENDS item `{value}`").into()),
    }
}

async fn execute_scenario(
    server_pid: u32,
    client: reqwest::Client,
    requests: Vec<RequestSpec>,
    endpoint: EndpointKind,
    hit_mode: HitMode,
    image_cache_backend: ImageBackendKind,
    cache_size: usize,
    concurrency: usize,
) -> Result<ScenarioReport, BenchError> {
    let resource_stats = Arc::new(Mutex::new(ResourceAccumulator::default()));
    let running = Arc::new(AtomicBool::new(true));
    let sampler = tokio::spawn(sample_resources(
        server_pid,
        running.clone(),
        resource_stats.clone(),
    ));
    tokio::time::sleep(RESOURCE_SAMPLE_INTERVAL).await;

    let measurements = execute_requests(client, requests, concurrency).await?;

    running.store(false, Ordering::Relaxed);
    let _ = sampler.await;
    let resources = resource_stats.lock().await.snapshot();

    let observed_hit_count = measurements.iter().filter(|m| m.actual_hit).count();
    let observed_hit_ratio = observed_hit_count as f64 / measurements.len() as f64;
    let total_latency = latency_stats(measurements.iter().map(|m| m.latency_ms).collect());
    let hit_measurements = measurements
        .iter()
        .filter(|measurement| measurement.actual_hit)
        .cloned()
        .collect::<Vec<_>>();
    let miss_measurements = measurements
        .iter()
        .filter(|measurement| !measurement.actual_hit)
        .cloned()
        .collect::<Vec<_>>();

    Ok(ScenarioReport {
        endpoint,
        hit_mode,
        image_cache_backend,
        cache_size,
        concurrency,
        total_requests: measurements.len(),
        configured_hit_ratio: hit_mode.configured_hit_ratio(),
        observed_hit_ratio,
        resources,
        total_latency,
        hit_latency: latency_stats(
            hit_measurements
                .iter()
                .map(|measurement| measurement.latency_ms)
                .collect(),
        ),
        miss_latency: latency_stats(
            miss_measurements
                .iter()
                .map(|measurement| measurement.latency_ms)
                .collect(),
        ),
        hit_stage_latency: stage_latency_stats(&hit_measurements),
        miss_stage_latency: stage_latency_stats(&miss_measurements),
    })
}

async fn prewarm_cache(
    client: &reqwest::Client,
    app_base: &str,
    cache_size: usize,
    image_backend: ImageBackendKind,
) -> Result<(), BenchError> {
    let mut join_set = JoinSet::new();

    for id in 0..cache_size {
        let client = client.clone();
        let app_base = app_base.to_string();
        join_set.spawn(async move {
            let api_response = client.get(api_url(&app_base, id)).send().await?;
            if api_response.status() != StatusCode::OK {
                return Err(format!("prewarm api failed with {}", api_response.status()).into());
            }

            let image_response = client.get(image_proxy_url(&app_base, id)).send().await?;
            let expected_image_status = match image_backend {
                ImageBackendKind::Sqlite => StatusCode::OK,
                ImageBackendKind::S3 => StatusCode::FOUND,
            };
            if image_response.status() != expected_image_status {
                return Err(format!(
                    "prewarm image failed with {} for backend {}",
                    image_response.status(),
                    image_backend.label()
                )
                .into());
            }

            Ok::<(), BenchError>(())
        });

        if join_set.len() >= PREWARM_CONCURRENCY {
            drain_one(&mut join_set).await?;
        }
    }

    while !join_set.is_empty() {
        drain_one(&mut join_set).await?;
    }

    Ok(())
}

async fn execute_requests(
    client: reqwest::Client,
    requests: Vec<RequestSpec>,
    concurrency: usize,
) -> Result<Vec<RequestMeasurement>, BenchError> {
    let mut results = Vec::with_capacity(requests.len());
    let mut join_set = JoinSet::new();
    let mut iter = requests.into_iter();

    loop {
        while join_set.len() < concurrency {
            let Some(spec) = iter.next() else {
                break;
            };
            let client = client.clone();
            join_set.spawn(async move {
                let started_at = Instant::now();
                let response = client.get(&spec.url).send().await?;
                if response.status() != spec.expected_status {
                    return Err(format!(
                        "unexpected status for {}: got {}, want {}",
                        spec.url,
                        response.status(),
                        spec.expected_status
                    )
                    .into());
                }

                let actual_hit = response
                    .headers()
                    .get("x-cache-status")
                    .and_then(|value| value.to_str().ok())
                    .map(|value| value.eq_ignore_ascii_case("hit"))
                    .unwrap_or(spec.expected_hit);
                let timings = parse_server_timing(
                    response
                        .headers()
                        .get("server-timing")
                        .and_then(|value| value.to_str().ok()),
                );
                let _ = response.bytes().await?;

                Ok::<RequestMeasurement, BenchError>(RequestMeasurement {
                    actual_hit,
                    latency_ms: started_at.elapsed().as_secs_f64() * 1000.0,
                    stages: timings,
                })
            });
        }

        if join_set.is_empty() {
            break;
        }

        let measurement = join_set
            .join_next()
            .await
            .unwrap()
            .map_err(|error| -> BenchError { Box::new(error) })??;
        results.push(measurement);
    }

    Ok(results)
}

async fn drain_one(join_set: &mut JoinSet<Result<(), BenchError>>) -> Result<(), BenchError> {
    join_set
        .join_next()
        .await
        .unwrap()
        .map_err(|error| -> BenchError { Box::new(error) })??;
    Ok(())
}

fn build_requests(
    app_base: &str,
    cache_size: usize,
    total_requests: usize,
    endpoint: EndpointKind,
    hit_mode: HitMode,
    image_backend: ImageBackendKind,
    miss_base: usize,
) -> Vec<RequestSpec> {
    let expected_status = match endpoint {
        EndpointKind::Api => StatusCode::OK,
        EndpointKind::Image => match image_backend {
            ImageBackendKind::Sqlite => StatusCode::OK,
            ImageBackendKind::S3 => StatusCode::FOUND,
        },
    };

    (0..total_requests)
        .map(|index| {
            let expected_hit = match hit_mode {
                HitMode::HitOnly => true,
                HitMode::MissOnly => false,
                HitMode::Mixed80_20 => index % 5 != 0,
            };
            let id = if expected_hit {
                index % cache_size
            } else {
                miss_base + index
            };
            let url = match endpoint {
                EndpointKind::Api => api_url(app_base, id),
                EndpointKind::Image => image_proxy_url(app_base, id),
            };
            RequestSpec {
                url,
                expected_hit,
                expected_status,
            }
        })
        .collect()
}

fn api_url(app_base: &str, id: usize) -> String {
    format!(
        "{app_base}/api?url={}",
        urlencoding::encode(&page_target_url(id))
    )
}

fn image_proxy_url(app_base: &str, id: usize) -> String {
    format!(
        "{app_base}/proxy/image?url={}&referer={}&w={IMAGE_WIDTH}&h={IMAGE_HEIGHT}&fit=cover&f=auto&q=80",
        urlencoding::encode(&image_target_url(id)),
        urlencoding::encode(&page_target_url(id))
    )
}

fn page_target_url(id: usize) -> String {
    format!("http://{MOCK_HOST}/page/{id}")
}

fn image_target_url(id: usize) -> String {
    format!("http://{MOCK_HOST}/image/{id}")
}

fn benchmark_client() -> Result<reqwest::Client, BenchError> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

fn parse_server_timing(value: Option<&str>) -> ParsedServerTimings {
    let mut timings = ParsedServerTimings::default();

    for part in value.unwrap_or_default().split(',') {
        let segment = part.trim();
        if segment.is_empty() {
            continue;
        }

        let mut pieces = segment.split(';');
        let name = pieces.next().unwrap_or_default().trim();
        let duration = pieces.find_map(|piece| {
            piece
                .trim()
                .strip_prefix("dur=")
                .and_then(|raw| raw.parse::<f64>().ok())
        });

        match name {
            "cache-read" => timings.cache_read_ms = duration,
            "fetch-upstream" => timings.fetch_upstream_ms = duration,
            "transform" => timings.transform_ms = duration,
            "cache-write" => timings.cache_write_ms = duration,
            _ => {}
        }
    }

    timings
}

fn stage_latency_stats(measurements: &[RequestMeasurement]) -> StageLatencyStats {
    StageLatencyStats {
        cache_read: latency_stats(
            measurements
                .iter()
                .filter_map(|measurement| measurement.stages.cache_read_ms)
                .collect(),
        ),
        fetch_upstream: latency_stats(
            measurements
                .iter()
                .filter_map(|measurement| measurement.stages.fetch_upstream_ms)
                .collect(),
        ),
        transform: latency_stats(
            measurements
                .iter()
                .filter_map(|measurement| measurement.stages.transform_ms)
                .collect(),
        ),
        cache_write: latency_stats(
            measurements
                .iter()
                .filter_map(|measurement| measurement.stages.cache_write_ms)
                .collect(),
        ),
    }
}

fn latency_stats(mut latencies: Vec<f64>) -> LatencyStats {
    if latencies.is_empty() {
        return LatencyStats {
            count: 0,
            avg_ms: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
        };
    }

    latencies.sort_by(|left, right| left.partial_cmp(right).unwrap());
    let count = latencies.len();
    let avg_ms = latencies.iter().sum::<f64>() / count as f64;

    LatencyStats {
        count,
        avg_ms,
        p50_ms: percentile(&latencies, 0.50),
        p95_ms: percentile(&latencies, 0.95),
        p99_ms: percentile(&latencies, 0.99),
    }
}

fn percentile(latencies: &[f64], ratio: f64) -> f64 {
    if latencies.is_empty() {
        return 0.0;
    }
    let index = ((latencies.len() - 1) as f64 * ratio).round() as usize;
    latencies[index]
}

async fn sample_resources(
    pid: u32,
    running: Arc<AtomicBool>,
    accumulator: Arc<Mutex<ResourceAccumulator>>,
) {
    let pid = Pid::from_u32(pid);
    let mut system = System::new_all();

    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cpu().with_memory(),
    );

    while running.load(Ordering::Relaxed) {
        tokio::time::sleep(RESOURCE_SAMPLE_INTERVAL).await;
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        if let Some(process) = system.process(pid) {
            let memory_mb = process.memory() as f64 / 1024.0 / 1024.0;
            accumulator
                .lock()
                .await
                .push(process.cpu_usage(), memory_mb);
        }
    }
}

async fn spawn_mock_process(executable: &Path) -> Result<SpawnedProcess, BenchError> {
    let guard = TempDir::new()?;
    let addr_file = guard.path().join("mock.addr");
    let child = Command::new(executable)
        .arg("mock")
        .arg("--listen-addr-file")
        .arg(&addr_file)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let address = wait_for_listen_addr(&addr_file).await?;
    if child.id().is_none() {
        return Err("mock process exited before startup".into());
    }

    Ok(SpawnedProcess {
        child,
        address,
        _guard: guard,
    })
}

async fn spawn_server_process(
    executable: &Path,
    mock_port: u16,
    sqlite_path: &Path,
    image_backend: ImageBackendKind,
    s3_config: Option<&BenchmarkS3Config>,
    s3_prefix: &str,
) -> Result<SpawnedProcess, BenchError> {
    let guard = TempDir::new()?;
    let addr_file = guard.path().join("server.addr");
    let mut command = Command::new(executable);
    command
        .arg("server")
        .arg("--listen-addr-file")
        .arg(&addr_file)
        .arg("--mock-port")
        .arg(mock_port.to_string())
        .env("HOST", "127.0.0.1")
        .env("PORT", "0")
        .env("CACHE_BACKEND", "sqlite")
        .env("IMAGE_CACHE_BACKEND", image_backend.env_value())
        .env("SQLITE_PATH", sqlite_path)
        .env("API_RESPONSE_CACHE_TTL", "3600")
        .env("IMAGE_CACHE_TTL", "86400")
        .env("OG_CACHE_TTL", "43200")
        .env("FETCH_TIMEOUT_MS", "8000")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    if let Some(config) = s3_config {
        command
            .env("S3_ENDPOINT", &config.endpoint)
            .env("S3_REGION", &config.region)
            .env("S3_BUCKET", &config.bucket)
            .env("S3_ACCESS_KEY_ID", &config.access_key_id)
            .env("S3_SECRET_ACCESS_KEY", &config.secret_access_key)
            .env("S3_PUBLIC_BASE_URL", &config.public_base_url)
            .env(
                "S3_FORCE_PATH_STYLE",
                if config.force_path_style {
                    "true"
                } else {
                    "false"
                },
            )
            .env("S3_PREFIX", s3_prefix);
    }

    let child = command.spawn()?;
    let address = wait_for_listen_addr(&addr_file).await?;
    if child.id().is_none() {
        return Err("server process exited before startup".into());
    }

    Ok(SpawnedProcess {
        child,
        address,
        _guard: guard,
    })
}

async fn wait_for_listen_addr(path: &Path) -> Result<SocketAddr, BenchError> {
    let started_at = Instant::now();
    loop {
        if let Ok(raw) = std::fs::read_to_string(path) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return trimmed.parse::<SocketAddr>().map_err(Into::into);
            }
        }

        if started_at.elapsed() > STARTUP_TIMEOUT {
            return Err(
                format!("timed out waiting for listen address at {}", path.display()).into(),
            );
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run_mock_process(args: &[String]) -> Result<(), BenchError> {
    let listen_addr_file = required_arg(args, "--listen-addr-file")?;
    let mock_state = MockState { png: sample_png() };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    std::fs::write(&listen_addr_file, listener.local_addr()?.to_string())?;
    axum::serve(listener, mock_router(mock_state)).await?;
    Ok(())
}

async fn run_server_process(args: &[String]) -> Result<(), BenchError> {
    let listen_addr_file = required_arg(args, "--listen-addr-file")?;
    let mock_port = required_arg(args, "--mock-port")?.parse::<u16>()?;

    let config = Config::from_env()?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .resolve(MOCK_HOST, SocketAddr::from(([127, 0, 0, 1], mock_port)))
        .build()?;
    let app = build_app_with_client(config.clone(), client).await?;
    let address = SocketAddr::new(config.host.parse()?, config.port);
    let listener = TcpListener::bind(address).await?;
    std::fs::write(&listen_addr_file, listener.local_addr()?.to_string())?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn required_arg(args: &[String], name: &str) -> Result<String, BenchError> {
    let mut index = 0usize;
    while index < args.len() {
        if args[index] == name {
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("missing value for {name}"))?;
            return Ok(value.clone());
        }
        index += 1;
    }

    Err(format!("missing argument {name}").into())
}

fn mock_router(state: MockState) -> Router {
    Router::new()
        .route("/page/{id}", get(mock_page))
        .route("/image/{id}", get(mock_image))
        .with_state(state)
}

async fn mock_page(AxumPath(id): AxumPath<usize>) -> Response<Body> {
    let page_url = page_target_url(id);
    let image_url = image_target_url(id);
    let html = format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta property="og:title" content="Bench Title {id}" />
    <meta property="og:description" content="Bench Description {id}" />
    <meta property="og:image" content="{image_url}" />
    <meta property="og:image:width" content="1200" />
    <meta property="og:image:height" content="630" />
    <meta property="og:url" content="{page_url}" />
    <meta property="og:site_name" content="Bench Publisher" />
    <title>Bench Title {id}</title>
  </head>
  <body>Bench {id}</body>
</html>"#
    );

    let mut response = Response::new(Body::from(html));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

async fn mock_image(
    State(state): State<MockState>,
    AxumPath(_id): AxumPath<usize>,
) -> Response<Body> {
    let mut response = Response::new(Body::from(state.png));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("image/png"),
    );
    response
}

fn sample_png() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .unwrap();
    cursor.into_inner()
}

impl BenchmarkS3Config {
    fn from_env() -> Option<Self> {
        let endpoint = std::env::var("BENCH_S3_ENDPOINT").ok()?;
        let bucket = std::env::var("BENCH_S3_BUCKET").ok()?;
        let access_key_id = std::env::var("BENCH_S3_ACCESS_KEY_ID").ok()?;
        let secret_access_key = std::env::var("BENCH_S3_SECRET_ACCESS_KEY").ok()?;
        let public_base_url = std::env::var("BENCH_S3_PUBLIC_BASE_URL").ok()?;

        Some(Self {
            endpoint,
            region: std::env::var("BENCH_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string()),
            bucket,
            access_key_id,
            secret_access_key,
            public_base_url,
            force_path_style: std::env::var("BENCH_S3_FORCE_PATH_STYLE")
                .ok()
                .map(|value| {
                    matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "1" | "true" | "yes"
                    )
                })
                .unwrap_or(true),
            prefix: std::env::var("BENCH_S3_PREFIX").unwrap_or_else(|_| "benchmark".to_string()),
        })
    }
}

async fn ensure_bucket_public(config: &BenchmarkS3Config) -> Result<(), BenchError> {
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(config.region.clone()))
        .credentials_provider(Credentials::new(
            config.access_key_id.clone(),
            config.secret_access_key.clone(),
            None,
            None,
            "benchmark",
        ))
        .load()
        .await;
    let mut builder = aws_sdk_s3::config::Builder::from(&shared).endpoint_url(&config.endpoint);
    if config.force_path_style {
        builder = builder.force_path_style(true);
    }
    let client = S3Client::from_conf(builder.build());

    if client
        .head_bucket()
        .bucket(&config.bucket)
        .send()
        .await
        .is_err()
    {
        client.create_bucket().bucket(&config.bucket).send().await?;
    }

    let policy = format!(
        r#"{{"Version":"2012-10-17","Statement":[{{"Sid":"PublicRead","Effect":"Allow","Principal":"*","Action":["s3:GetObject"],"Resource":["arn:aws:s3:::{bucket}/*"]}}]}}"#,
        bucket = config.bucket
    );
    client
        .put_bucket_policy()
        .bucket(&config.bucket)
        .policy(policy)
        .send()
        .await?;

    Ok(())
}
