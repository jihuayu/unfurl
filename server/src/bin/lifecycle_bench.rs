/// lifecycle_bench — 真实场景生命周期性能测试
///
/// 模拟服务器在生产环境中完整的负载生命周期：
///   1. 冷启动预热阶段   — 并发从 1 逐步爬升到 20（5 分钟）
///   2. 高负载峰值阶段   — 50 并发（5 分钟）
///   3. 负载下降-中等阶段 — 5 并发（5 分钟）
///   4. 空闲冷却阶段     — 每 3 秒 1 个请求（5 分钟），监控内存自主释放速度
///   5. 突发流量恢复阶段  — 50 并发（5 分钟）
///
/// 针对「低内存模式」和「标准模式」分别运行，输出 lifecycle-bench-results.json。
///
/// 运行方式：
///   cargo run --bin lifecycle_bench --release
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

use axum::{
    Router,
    body::Body,
    extract::Path as AxumPath,
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

// ── 场景常量 ─────────────────────────────────────────────────────────────

const MOCK_HOST: &str = "mock-lifecycle.example.test";

/// 缓存中预先填充的条目数量（用于模拟真实缓存命中）
const CACHE_SIZE: usize = 2000;

/// 预热并发（填充缓存阶段不计入测试阶段）
const PREWARM_CONCURRENCY: usize = 50;

/// 资源采样间隔
const RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

/// 等待监听地址文件的超时
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// 每个阶段最短持续时间（5 分钟）
const PHASE_DURATION: Duration = Duration::from_secs(5 * 60);

/// 空闲阶段两次请求之间的间隔
const IDLE_REQUEST_INTERVAL: Duration = Duration::from_secs(3);

/// 图片尺寸（与 mock 保持一致，image_proxy_url 辅助函数使用）
#[allow(dead_code)]
const IMAGE_WIDTH: u32 = 1200;
#[allow(dead_code)]
const IMAGE_HEIGHT: u32 = 630;

// ── 类型别名 ──────────────────────────────────────────────────────────────

type BenchError = Box<dyn std::error::Error + Send + Sync>;

// ── 阶段定义 ──────────────────────────────────────────────────────────────

/// 每个阶段的描述信息，用于 JSON 输出
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PhaseKind {
    /// 冷启动预热：并发从 1 逐步增加到 20
    ColdWarmup,
    /// 高负载峰值：50 并发
    PeakLoad,
    /// 负载下降到中等：5 并发
    ModerateLoad,
    /// 空闲冷却：每 3 秒 1 个请求
    Idle,
    /// 突发流量恢复：50 并发
    TrafficSpike,
}

impl PhaseKind {
    fn label(self) -> &'static str {
        match self {
            Self::ColdWarmup => "cold_warmup",
            Self::PeakLoad => "peak_load",
            Self::ModerateLoad => "moderate_load",
            Self::Idle => "idle",
            Self::TrafficSpike => "traffic_spike",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::ColdWarmup => "冷启动预热：并发从 1 逐步爬升到 20，模拟服务刚上线时的请求增长",
            Self::PeakLoad => "高负载峰值：50 并发持续压测",
            Self::ModerateLoad => "负载下降到中等：5 并发，观察资源使用是否随之下降",
            Self::Idle => "空闲冷却：每 3 秒 1 个请求，监控内存自主释放速度",
            Self::TrafficSpike => "突发流量恢复：50 并发，测试从空闲状态快速响应突发请求的能力",
        }
    }
}

// ── 内存模式定义 ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MemoryMode {
    Standard,
    LowMemory,
}

impl MemoryMode {
    fn label(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::LowMemory => "low_memory",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Standard => "标准模式：默认连接池大小，最大并发限制宽松",
            Self::LowMemory => "低内存模式：连接池缩小，并发限制收紧，HTTP 连接更快回收",
        }
    }

    fn low_memory_env(self) -> &'static str {
        match self {
            Self::Standard => "false",
            Self::LowMemory => "true",
        }
    }
}

// ── 资源统计 ──────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize)]
struct ResourceStats {
    avg_cpu_percent: f32,
    peak_cpu_percent: f32,
    avg_memory_mb: f64,
    peak_memory_mb: f64,
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

// ── 延迟统计 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct LatencyStats {
    count: usize,
    avg_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

fn latency_stats(mut latencies: Vec<f64>) -> LatencyStats {
    if latencies.is_empty() {
        return LatencyStats {
            count: 0,
            avg_ms: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
            max_ms: 0.0,
        };
    }
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let count = latencies.len();
    let avg_ms = latencies.iter().sum::<f64>() / count as f64;
    let max_ms = *latencies.last().unwrap();
    LatencyStats {
        count,
        avg_ms,
        p50_ms: percentile(&latencies, 0.50),
        p95_ms: percentile(&latencies, 0.95),
        p99_ms: percentile(&latencies, 0.99),
        max_ms,
    }
}

fn percentile(sorted: &[f64], ratio: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * ratio).round() as usize;
    sorted[index]
}

// ── 内存释放跟踪 ──────────────────────────────────────────────────────────

/// 内存释放效率报告（空闲阶段专用）
#[derive(Debug, Clone, Serialize)]
struct MemoryReleaseReport {
    /// 阶段开始时的内存（前一个高负载阶段结束时的峰值）
    memory_at_phase_start_mb: f64,
    /// 阶段结束时的平均内存
    memory_at_phase_end_mb: f64,
    /// 释放量（MB）
    memory_released_mb: f64,
    /// 释放比例（0.0–1.0）
    release_ratio: f64,
    /// 达到 50% 释放所用时间（秒），如果在阶段内未达到则为 None
    time_to_50pct_release_secs: Option<f64>,
    /// 达到 80% 释放所用时间（秒），如果在阶段内未达到则为 None
    time_to_80pct_release_secs: Option<f64>,
    /// 内存快照序列（每 15 秒一个数据点）
    memory_timeline_mb: Vec<f64>,
    /// 对应的时间轴（距阶段开始的秒数）
    timeline_secs: Vec<f64>,
}

// ── 阶段报告 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct PhaseReport {
    phase: PhaseKind,
    description: &'static str,
    duration_secs: f64,
    total_requests: usize,
    avg_rps: f64,
    resources: ResourceStats,
    latency: LatencyStats,
    /// 成功率（0.0–1.0）
    success_ratio: f64,
    /// 仅在 Idle 阶段填充
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_release: Option<MemoryReleaseReport>,
}

// ── 完整测试结果 ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct MemoryModeReport {
    memory_mode: MemoryMode,
    description: &'static str,
    phases: Vec<PhaseReport>,
}

#[derive(Debug, Serialize)]
struct LifecycleBenchReport {
    generated_at: String,
    phase_duration_secs: u64,
    cache_size: usize,
    note: &'static str,
    results: Vec<MemoryModeReport>,
}

// ── 进程管理 ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct MockState {
    png: Vec<u8>,
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

// ── 入口 ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), BenchError> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("mock") => run_mock_process(&args[1..]).await,
        Some("server") => run_server_process(&args[1..]).await,
        Some(other) => Err(format!("unsupported subcommand: {other}").into()),
        None => run_orchestrator().await,
    }
}

// ── Orchestrator ──────────────────────────────────────────────────────────

async fn run_orchestrator() -> Result<(), BenchError> {
    let executable = std::env::current_exe()?;
    println!("=== Lifecycle Benchmark 开始 ===");
    println!(
        "每阶段持续时间: {}分钟，缓存大小: {}",
        PHASE_DURATION.as_secs() / 60,
        CACHE_SIZE
    );

    let mock = spawn_mock_process(&executable).await?;
    println!("Mock 服务器已启动: {}", mock.base_url());

    let mut results = Vec::new();

    for &mode in &[MemoryMode::Standard, MemoryMode::LowMemory] {
        println!("\n>>> 运行内存模式: {} <<<", mode.label());
        let report = run_lifecycle_for_mode(&executable, mock.address.port(), mode).await?;
        results.push(report);
    }

    mock.shutdown().await;

    let report = LifecycleBenchReport {
        generated_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?,
        phase_duration_secs: PHASE_DURATION.as_secs(),
        cache_size: CACHE_SIZE,
        note: concat!(
            "CPU 和内存采样自 unfurl-server 进程。",
            "内存释放效率仅在空闲阶段（idle phase）统计。",
            "每阶段至少持续 5 分钟以确保测试可信度。"
        ),
        results,
    };

    let output = serde_json::to_string_pretty(&report)?;
    let output_path = "lifecycle-bench-results.json";
    std::fs::write(output_path, &output)?;
    println!("\n=== 结果已写入 {output_path} ===");
    println!("{output}");
    Ok(())
}

async fn run_lifecycle_for_mode(
    executable: &Path,
    mock_port: u16,
    mode: MemoryMode,
) -> Result<MemoryModeReport, BenchError> {
    let temp_dir = TempDir::new()?;
    let sqlite_path = temp_dir.path().join("lifecycle-bench.db");

    let server = spawn_server_process(executable, mock_port, &sqlite_path, mode).await?;
    let client = build_http_client()?;
    let server_pid = server.id()?;
    let base_url = server.base_url();

    println!("  [{}] 服务器已启动: {}", mode.label(), base_url);
    println!("  [{}] 开始预热缓存 ({} 条目)…", mode.label(), CACHE_SIZE);
    prewarm_cache(&client, &base_url, CACHE_SIZE).await?;
    println!("  [{}] 缓存预热完成", mode.label());

    // 各阶段顺序执行
    let mut phases = Vec::new();
    // ── 阶段 1: 冷启动预热 ──────────────────────────────────────────────
    println!("  [{}] 阶段 1/5: 冷启动预热 (并发 1→20)…", mode.label());
    let phase1 = run_rampup_phase(
        server_pid,
        client.clone(),
        &base_url,
        PhaseKind::ColdWarmup,
        /* start_concurrency */ 1,
        /* end_concurrency */ 20,
        PHASE_DURATION,
    )
    .await?;
    let mut prev_peak_memory_mb = phase1.resources.peak_memory_mb;
    print_phase_summary(&phase1, mode);
    phases.push(phase1);

    // ── 阶段 2: 高负载峰值 ──────────────────────────────────────────────
    println!("  [{}] 阶段 2/5: 高负载峰值 (50 并发)…", mode.label());
    let phase2 = run_constant_concurrency_phase(
        server_pid,
        client.clone(),
        &base_url,
        PhaseKind::PeakLoad,
        50,
        PHASE_DURATION,
        None,
    )
    .await?;
    prev_peak_memory_mb = prev_peak_memory_mb.max(phase2.resources.peak_memory_mb);
    print_phase_summary(&phase2, mode);
    phases.push(phase2);

    // ── 阶段 3: 负载下降-中等 ────────────────────────────────────────────
    println!("  [{}] 阶段 3/5: 中等负载 (5 并发)…", mode.label());
    let phase3 = run_constant_concurrency_phase(
        server_pid,
        client.clone(),
        &base_url,
        PhaseKind::ModerateLoad,
        5,
        PHASE_DURATION,
        None,
    )
    .await?;
    print_phase_summary(&phase3, mode);
    phases.push(phase3);

    // ── 阶段 4: 空闲冷却 ─────────────────────────────────────────────────
    println!(
        "  [{}] 阶段 4/5: 空闲冷却 (每 {}s 1 个请求)…",
        mode.label(),
        IDLE_REQUEST_INTERVAL.as_secs()
    );
    let phase4 =
        run_idle_phase(server_pid, client.clone(), &base_url, prev_peak_memory_mb).await?;
    print_phase_summary(&phase4, mode);
    if let Some(ref mr) = phase4.memory_release {
        println!(
            "    内存释放: {:.2}MB → {:.2}MB (释放 {:.2}MB, {:.1}%)",
            mr.memory_at_phase_start_mb,
            mr.memory_at_phase_end_mb,
            mr.memory_released_mb,
            mr.release_ratio * 100.0
        );
        if let Some(t) = mr.time_to_50pct_release_secs {
            println!("    达到 50% 释放用时: {t:.1}s");
        }
        if let Some(t) = mr.time_to_80pct_release_secs {
            println!("    达到 80% 释放用时: {t:.1}s");
        }
    }
    phases.push(phase4);

    // ── 阶段 5: 突发流量恢复 ──────────────────────────────────────────────
    println!("  [{}] 阶段 5/5: 突发流量恢复 (50 并发)…", mode.label());
    let phase5 = run_constant_concurrency_phase(
        server_pid,
        client.clone(),
        &base_url,
        PhaseKind::TrafficSpike,
        50,
        PHASE_DURATION,
        None,
    )
    .await?;
    print_phase_summary(&phase5, mode);
    phases.push(phase5);

    server.shutdown().await;

    Ok(MemoryModeReport {
        memory_mode: mode,
        description: mode.description(),
        phases,
    })
}

// ── 阶段实现 ──────────────────────────────────────────────────────────────

/// 斜坡升压阶段：在 `duration` 内将并发从 `start` 线性增加到 `end`
async fn run_rampup_phase(
    server_pid: u32,
    client: reqwest::Client,
    base_url: &str,
    phase: PhaseKind,
    start_concurrency: usize,
    end_concurrency: usize,
    duration: Duration,
) -> Result<PhaseReport, BenchError> {
    let resource_stats = Arc::new(Mutex::new(ResourceAccumulator::default()));
    let running = Arc::new(AtomicBool::new(true));
    let sampler = tokio::spawn(sample_resources(
        server_pid,
        running.clone(),
        resource_stats.clone(),
    ));

    let phase_start = Instant::now();
    let mut latencies = Vec::new();
    let mut success_count = 0usize;
    let mut total_count = 0usize;

    // 将 duration 分成若干个窗口，每个窗口内递进一级并发
    let steps = end_concurrency.saturating_sub(start_concurrency) + 1;
    let window = duration / steps as u32;

    for step in 0..steps {
        let concurrency = start_concurrency + step;
        let window_end = phase_start + window * (step as u32 + 1);

        while Instant::now() < window_end {
            // 在当前并发级别下运行一批请求（批次大小 = 并发数）
            let batch = concurrency.max(1);
            let batch_results = execute_batch(&client, base_url, batch, false).await;
            for result in batch_results {
                total_count += 1;
                match result {
                    Ok(latency_ms) => {
                        success_count += 1;
                        latencies.push(latency_ms);
                    }
                    Err(_) => {}
                }
            }
        }
    }

    // 如果 duration 尚未完全消化（极端情况），继续最大并发直到超时
    while phase_start.elapsed() < duration {
        let batch_results = execute_batch(&client, base_url, end_concurrency, false).await;
        for result in batch_results {
            total_count += 1;
            if let Ok(latency_ms) = result {
                success_count += 1;
                latencies.push(latency_ms);
            }
        }
    }

    let elapsed = phase_start.elapsed().as_secs_f64();
    running.store(false, Ordering::Relaxed);
    let _ = sampler.await;
    let resources = resource_stats.lock().await.snapshot();

    Ok(PhaseReport {
        phase,
        description: phase.description(),
        duration_secs: elapsed,
        total_requests: total_count,
        avg_rps: total_count as f64 / elapsed,
        resources,
        latency: latency_stats(latencies),
        success_ratio: if total_count == 0 {
            1.0
        } else {
            success_count as f64 / total_count as f64
        },
        memory_release: None,
    })
}

/// 固定并发阶段：在 `duration` 内以固定并发级别持续发送请求
async fn run_constant_concurrency_phase(
    server_pid: u32,
    client: reqwest::Client,
    base_url: &str,
    phase: PhaseKind,
    concurrency: usize,
    duration: Duration,
    _memory_release_baseline: Option<f64>,
) -> Result<PhaseReport, BenchError> {
    let resource_stats = Arc::new(Mutex::new(ResourceAccumulator::default()));
    let running = Arc::new(AtomicBool::new(true));
    let sampler = tokio::spawn(sample_resources(
        server_pid,
        running.clone(),
        resource_stats.clone(),
    ));

    let phase_start = Instant::now();
    let mut latencies = Vec::new();
    let mut success_count = 0usize;
    let mut total_count = 0usize;

    while phase_start.elapsed() < duration {
        let batch_results = execute_batch(&client, base_url, concurrency, false).await;
        for result in batch_results {
            total_count += 1;
            if let Ok(latency_ms) = result {
                success_count += 1;
                latencies.push(latency_ms);
            }
        }
    }

    let elapsed = phase_start.elapsed().as_secs_f64();
    running.store(false, Ordering::Relaxed);
    let _ = sampler.await;
    let resources = resource_stats.lock().await.snapshot();

    Ok(PhaseReport {
        phase,
        description: phase.description(),
        duration_secs: elapsed,
        total_requests: total_count,
        avg_rps: total_count as f64 / elapsed,
        resources,
        latency: latency_stats(latencies),
        success_ratio: if total_count == 0 {
            1.0
        } else {
            success_count as f64 / total_count as f64
        },
        memory_release: None,
    })
}

/// 空闲冷却阶段：每隔 `IDLE_REQUEST_INTERVAL` 发送 1 个请求，同时密集采样内存以追踪释放曲线
async fn run_idle_phase(
    server_pid: u32,
    client: reqwest::Client,
    base_url: &str,
    peak_memory_before_idle_mb: f64,
) -> Result<PhaseReport, BenchError> {
    let phase = PhaseKind::Idle;
    let resource_stats = Arc::new(Mutex::new(ResourceAccumulator::default()));
    let running = Arc::new(AtomicBool::new(true));

    // 时间线快照（每 15 秒记录一次内存）
    let timeline_snapshots: Arc<Mutex<Vec<(f64, f64)>>> = Arc::new(Mutex::new(Vec::new()));
    let timeline_snapshots_clone = timeline_snapshots.clone();

    let sampler = tokio::spawn(sample_resources_with_timeline(
        server_pid,
        running.clone(),
        resource_stats.clone(),
        timeline_snapshots_clone,
        Duration::from_secs(15),
    ));

    let phase_start = Instant::now();
    let mut latencies = Vec::new();
    let mut success_count = 0usize;
    let mut total_count = 0usize;

    // 空闲阶段：每 `IDLE_REQUEST_INTERVAL` 发一个混合请求（缓存命中为主）
    while phase_start.elapsed() < PHASE_DURATION {
        let batch_results = execute_batch(&client, base_url, 1, true).await;
        for result in batch_results {
            total_count += 1;
            if let Ok(latency_ms) = result {
                success_count += 1;
                latencies.push(latency_ms);
            }
        }
        tokio::time::sleep(IDLE_REQUEST_INTERVAL).await;
    }

    let elapsed = phase_start.elapsed().as_secs_f64();
    running.store(false, Ordering::Relaxed);
    let _ = sampler.await;
    let resources = resource_stats.lock().await.snapshot();

    // 分析内存释放曲线
    let snapshots = timeline_snapshots.lock().await.clone();
    let memory_release = build_memory_release_report(
        peak_memory_before_idle_mb,
        resources.avg_memory_mb,
        &snapshots,
    );

    Ok(PhaseReport {
        phase,
        description: phase.description(),
        duration_secs: elapsed,
        total_requests: total_count,
        avg_rps: total_count as f64 / elapsed,
        resources,
        latency: latency_stats(latencies),
        success_ratio: if total_count == 0 {
            1.0
        } else {
            success_count as f64 / total_count as f64
        },
        memory_release: Some(memory_release),
    })
}

// ── 内存释放分析 ──────────────────────────────────────────────────────────

fn build_memory_release_report(
    start_mb: f64,
    end_mb: f64,
    snapshots: &[(f64, f64)], // (elapsed_secs, memory_mb)
) -> MemoryReleaseReport {
    let released = (start_mb - end_mb).max(0.0);
    let release_ratio = if start_mb > 0.0 {
        released / start_mb
    } else {
        0.0
    };

    let target_50pct = start_mb - start_mb * 0.50;
    let target_80pct = start_mb - start_mb * 0.80;

    let time_to_50pct = snapshots
        .iter()
        .find(|(_, mem)| *mem <= target_50pct)
        .map(|(t, _)| *t);
    let time_to_80pct = snapshots
        .iter()
        .find(|(_, mem)| *mem <= target_80pct)
        .map(|(t, _)| *t);

    let timeline_secs = snapshots.iter().map(|(t, _)| *t).collect();
    let memory_timeline_mb = snapshots.iter().map(|(_, m)| *m).collect();

    MemoryReleaseReport {
        memory_at_phase_start_mb: start_mb,
        memory_at_phase_end_mb: end_mb,
        memory_released_mb: released,
        release_ratio,
        time_to_50pct_release_secs: time_to_50pct,
        time_to_80pct_release_secs: time_to_80pct,
        memory_timeline_mb,
        timeline_secs,
    }
}

// ── 请求执行 ──────────────────────────────────────────────────────────────

/// 执行一批并发请求，返回每个请求的延迟（成功）或错误
async fn execute_batch(
    client: &reqwest::Client,
    base_url: &str,
    concurrency: usize,
    hit_biased: bool,
) -> Vec<Result<f64, BenchError>> {
    let mut join_set = JoinSet::new();
    for i in 0..concurrency {
        let client = client.clone();
        // 命中偏向：hit_biased 时只用缓存内的 id，否则混合 miss
        let id = if hit_biased || i % 5 != 0 {
            i % CACHE_SIZE
        } else {
            // cache miss：使用超出缓存大小范围的 id
            CACHE_SIZE + i * 1000 + rand_offset()
        };
        let url = api_url(base_url, id);
        join_set.spawn(async move {
            let start = Instant::now();
            let resp = client
                .get(&url)
                .send()
                .await
                .map_err(|e| -> BenchError { Box::new(e) })?;
            let status = resp.status();
            let _ = resp.bytes().await;
            if status != StatusCode::OK {
                return Err(format!("unexpected status {status} for {url}").into());
            }
            Ok(start.elapsed().as_secs_f64() * 1000.0)
        });
    }

    let mut results = Vec::with_capacity(concurrency);
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(r) => results.push(r),
            Err(e) => results.push(Err(Box::new(e) as BenchError)),
        }
    }
    results
}

/// 简单的轻量随机偏移（基于当前时间纳秒的低位，避免引入 rand crate）
fn rand_offset() -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.subsec_nanos() as usize).wrapping_mul(6364136223846793005))
        .unwrap_or(0)
        % 100_000
}

// ── 资源采样 ──────────────────────────────────────────────────────────────

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
        if let Some(proc) = system.process(pid) {
            let memory_mb = proc.memory() as f64 / 1024.0 / 1024.0;
            accumulator.lock().await.push(proc.cpu_usage(), memory_mb);
        }
    }
}

/// 资源采样 + 定时写入内存时间线快照
async fn sample_resources_with_timeline(
    pid: u32,
    running: Arc<AtomicBool>,
    accumulator: Arc<Mutex<ResourceAccumulator>>,
    timeline: Arc<Mutex<Vec<(f64, f64)>>>,
    snapshot_interval: Duration,
) {
    let pid_sys = Pid::from_u32(pid);
    let mut system = System::new_all();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid_sys]),
        true,
        ProcessRefreshKind::nothing().with_cpu().with_memory(),
    );

    let start = Instant::now();
    let mut last_snapshot = Instant::now();

    while running.load(Ordering::Relaxed) {
        tokio::time::sleep(RESOURCE_SAMPLE_INTERVAL).await;
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid_sys]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        if let Some(proc) = system.process(pid_sys) {
            let memory_mb = proc.memory() as f64 / 1024.0 / 1024.0;
            accumulator.lock().await.push(proc.cpu_usage(), memory_mb);

            if last_snapshot.elapsed() >= snapshot_interval {
                let elapsed_secs = start.elapsed().as_secs_f64();
                timeline.lock().await.push((elapsed_secs, memory_mb));
                last_snapshot = Instant::now();
            }
        }
    }
}

// ── 缓存预热 ──────────────────────────────────────────────────────────────

async fn prewarm_cache(
    client: &reqwest::Client,
    base_url: &str,
    cache_size: usize,
) -> Result<(), BenchError> {
    let mut join_set = JoinSet::new();
    for id in 0..cache_size {
        let client = client.clone();
        let url = api_url(base_url, id);
        join_set.spawn(async move {
            let resp = client
                .get(&url)
                .send()
                .await
                .map_err(|e| -> BenchError { Box::new(e) })?;
            if resp.status() != StatusCode::OK {
                return Err(
                    format!("prewarm failed for id={id}: status={}", resp.status()).into(),
                );
            }
            Ok::<(), BenchError>(())
        });

        if join_set.len() >= PREWARM_CONCURRENCY {
            if let Some(result) = join_set.join_next().await {
                result.map_err(|e| -> BenchError { Box::new(e) })??;
            }
        }
    }
    while let Some(result) = join_set.join_next().await {
        result.map_err(|e| -> BenchError { Box::new(e) })??;
    }
    Ok(())
}

// ── URL 构造 ───────────────────────────────────────────────────────────────

fn api_url(base_url: &str, id: usize) -> String {
    let target = format!("http://{MOCK_HOST}/page/{id}");
    format!("{base_url}/api?url={}", urlencoding::encode(&target))
}

#[allow(dead_code)]
fn image_proxy_url(base_url: &str, id: usize) -> String {
    let img = format!("http://{MOCK_HOST}/image/{id}");
    let referer = format!("http://{MOCK_HOST}/page/{id}");
    format!(
        "{base_url}/proxy/image?url={}&referer={}&w={IMAGE_WIDTH}&h={IMAGE_HEIGHT}&fit=cover&f=auto&q=80",
        urlencoding::encode(&img),
        urlencoding::encode(&referer)
    )
}

fn page_target_url(id: usize) -> String {
    format!("http://{MOCK_HOST}/page/{id}")
}

fn image_target_url(id: usize) -> String {
    format!("http://{MOCK_HOST}/image/{id}")
}

// ── HTTP 客户端 ───────────────────────────────────────────────────────────

fn build_http_client() -> Result<reqwest::Client, BenchError> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(64)
        .build()?)
}

// ── 进程启动 ──────────────────────────────────────────────────────────────

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
        return Err("mock process exited before being ready".into());
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
    mode: MemoryMode,
) -> Result<SpawnedProcess, BenchError> {
    let guard = TempDir::new()?;
    let addr_file = guard.path().join("server.addr");
    let child = Command::new(executable)
        .arg("server")
        .arg("--listen-addr-file")
        .arg(&addr_file)
        .arg("--mock-port")
        .arg(mock_port.to_string())
        .env("HOST", "127.0.0.1")
        .env("PORT", "0")
        .env("CACHE_BACKEND", "sqlite")
        .env("IMAGE_CACHE_BACKEND", "sqlite")
        .env("SQLITE_PATH", sqlite_path)
        .env("API_RESPONSE_CACHE_TTL", "3600")
        .env("IMAGE_CACHE_TTL", "86400")
        .env("OG_CACHE_TTL", "43200")
        .env("FETCH_TIMEOUT_MS", "8000")
        .env("LOW_MEMORY_MODE", mode.low_memory_env())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let address = wait_for_listen_addr(&addr_file).await?;
    if child.id().is_none() {
        return Err("server process exited before being ready".into());
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

// ── Mock 服务器 ───────────────────────────────────────────────────────────

async fn run_mock_process(args: &[String]) -> Result<(), BenchError> {
    let listen_addr_file = required_arg(args, "--listen-addr-file")?;
    let png = sample_png();
    let state = MockState { png };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    std::fs::write(&listen_addr_file, listener.local_addr()?.to_string())?;
    axum::serve(listener, mock_router(state)).await?;
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
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            return args
                .get(i + 1)
                .cloned()
                .ok_or_else(|| format!("missing value for {name}").into());
        }
        i += 1;
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
    axum::extract::State(state): axum::extract::State<MockState>,
    AxumPath(_id): AxumPath<usize>,
) -> Response<Body> {
    let mut response = Response::new(Body::from(state.png.clone()));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("image/png"),
    );
    response
}

fn sample_png() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .unwrap();
    cursor.into_inner()
}

// ── 控制台输出辅助 ────────────────────────────────────────────────────────

fn print_phase_summary(phase: &PhaseReport, mode: MemoryMode) {
    println!(
        "    [{}] {} 完成: {:.0}s, {} 请求, avg_rps={:.1}, avg_lat={:.2}ms, p99_lat={:.2}ms, avg_cpu={:.1}%, peak_mem={:.2}MB",
        mode.label(),
        phase.phase.label(),
        phase.duration_secs,
        phase.total_requests,
        phase.avg_rps,
        phase.latency.avg_ms,
        phase.latency.p99_ms,
        phase.resources.avg_cpu_percent,
        phase.resources.peak_memory_mb,
    );
}
