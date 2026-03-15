# Unfurl Server

A standalone Rust server version of `unfurl` that does not depend on Cloudflare.

It keeps the same public routes as the Worker version:
- `GET|HEAD /health`
- `GET|HEAD /api`
- `GET|HEAD /proxy/image`

Default metadata and image cache backend is SQLite. Redis is optional for metadata, and S3 is optional for processed image cache.
Image proxying and basic transform/format negotiation are handled inside the server.

## Features

- Compatible JSON response shape with the Worker version
- OG/Twitter/meta extraction from `<head>` only
- Public URL validation with SSRF guardrails
- SQLite cache by default
- Redis cache via environment switch
- Image proxy with:
  - width/height resize
  - `fit` modes
  - `auto` format negotiation
  - forced `referer` from query param
  - processed image cache in SQLite by default
  - optional S3-backed processed image cache with `302` redirect to public object URL
- OTLP trace export support
- Docker deployment support
- Built-in benchmark binary with local mock upstream

## Project Layout

- `src/main.rs`: server entrypoint
- `src/routes.rs`: `/health`, `/api`, `/proxy/image`
- `src/cache.rs`: cache abstraction + SQLite/Redis backends
- `src/extract.rs`: HTML head metadata extraction and merge logic
- `src/image_proxy.rs`: image transform pipeline
- `src/telemetry.rs`: tracing and OTLP initialization
- `compose.yaml`: local container deployment example
- `Dockerfile`: production image build

## Requirements

Local development:
- Rust 1.93+

Container deployment:
- Docker
- Docker Compose plugin if you want to use `compose.yaml`

## Configuration

Environment variables:

| Name | Default | Description |
| --- | --- | --- |
| `HOST` | `0.0.0.0` | Bind address |
| `PORT` | `8080` | Listen port |
| `LOW_MEMORY_MODE` | `false` | Enable aggressive low-memory defaults and external image worker processing |
| `CACHE_BACKEND` | `sqlite` | Metadata cache backend: `sqlite` or `redis` |
| `IMAGE_CACHE_BACKEND` | `sqlite` | Processed image cache backend: `sqlite` or `s3` |
| `SQLITE_PATH` | `/data/unfurl.db` | SQLite file path |
| `REDIS_URL` | empty | Required when `CACHE_BACKEND=redis` |
| `API_RESPONSE_CACHE_TTL` | `3600` | Browser/API JSON cache TTL |
| `IMAGE_CACHE_TTL` | `86400` | Browser image cache TTL |
| `OG_CACHE_TTL` | `43200` | Metadata cache TTL |
| `FETCH_TIMEOUT_MS` | `8000` | Upstream fetch timeout |
| `API_MISS_MAX_CONCURRENCY` | dynamic | Maximum concurrent `/api` cache misses |
| `IMAGE_MISS_MAX_CONCURRENCY` | dynamic | Maximum concurrent `/proxy/image` cache misses |
| `HTTP_POOL_MAX_IDLE_PER_HOST` | dynamic | Maximum idle upstream HTTP connections kept per host |
| `HTTP_POOL_IDLE_TIMEOUT_SECS` | dynamic | Idle timeout for upstream HTTP connections |
| `SQLITE_META_MAX_CONNECTIONS` | dynamic | Maximum SQLite connections for metadata cache |
| `SQLITE_IMAGE_MAX_CONNECTIONS` | dynamic | Maximum SQLite connections for image cache |
| `SQLITE_IDLE_TIMEOUT_SECS` | dynamic | Idle timeout for SQLite pooled connections |
| `IMAGE_WORKER_BIN` | sibling binary | Optional explicit path to the external image worker binary |
| `S3_ENDPOINT` | empty | Optional custom S3 endpoint, useful for MinIO/R2-compatible gateways |
| `S3_REGION` | `us-east-1` | S3 region |
| `S3_BUCKET` | empty | Required when `IMAGE_CACHE_BACKEND=s3` |
| `S3_ACCESS_KEY_ID` | empty | Optional static access key |
| `S3_SECRET_ACCESS_KEY` | empty | Optional static secret |
| `S3_PUBLIC_BASE_URL` | empty | Required when `IMAGE_CACHE_BACKEND=s3`, used for `302` redirect target |
| `S3_FORCE_PATH_STYLE` | `false` | Enable path-style addressing for MinIO or compatible services |
| `S3_PREFIX` | `image-cache` | Object key prefix for processed image cache |
| `OTEL_SERVICE_NAME` | `unfurl-server` | OTLP service name |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | empty | Enable OTLP export when set |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | exporter default | Usually `grpc` when sending to `4317` |
| `OTEL_EXPORTER_OTLP_HEADERS` | empty | Optional OTLP auth headers |

A sample env file is provided at `.env.example`.

Cache behavior:
- OG metadata is cached by `CACHE_BACKEND`
- processed `/proxy/image` output is cached by `IMAGE_CACHE_BACKEND`
- when `IMAGE_CACHE_BACKEND=sqlite`, the server returns cached image bytes directly
- when `IMAGE_CACHE_BACKEND=s3`, the server uploads processed bytes to S3 and returns `302 Found` to `S3_PUBLIC_BASE_URL/<object-key>`
- cache hit and miss paths are isolated with dedicated miss limiters, so heavy miss traffic does not consume the same execution slots as fast cache hits
- image transforms run on Tokio's blocking pool instead of the async runtime worker threads
- reqwest and SQLite pools can shrink when idle through their idle timeout settings

Low-memory mode:
- enabling `LOW_MEMORY_MODE=true` applies lower defaults for HTTP idle pool size, HTTP idle timeout, SQLite pool size, SQLite idle timeout, and miss concurrency
- in low-memory mode, image transforms are moved to an external helper process instead of staying inside the server process
- the helper process is spawned only when an image miss needs processing, then exits after the request completes, so idle memory stays lower at the cost of more miss latency
- if you ship a custom worker binary path, set `IMAGE_WORKER_BIN`

## OTLP Support

By default the server only writes local logs.

OTLP export is enabled automatically when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.

Example collector settings:

```env
OTEL_SERVICE_NAME=unfurl-server
OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4317
OTEL_EXPORTER_OTLP_PROTOCOL=grpc
```

Current behavior:
- local structured logging stays enabled
- traces are also exported through OTLP
- if OTLP is not configured, the service still runs normally

## Run Locally

```bash
cd server
cargo run
```

The service listens on `http://127.0.0.1:8080` by default.

Run tests:

```bash
cd server
cargo test
```

## Docker Deployment

### Option 1: SQLite only

Build and run:

```bash
cd server
docker build -t unfurl-server .
docker run --rm -p 8080:8080 -v %cd%/data:/data unfurl-server
```

Linux/macOS shell equivalent:

```bash
cd server
docker build -t unfurl-server .
docker run --rm -p 8080:8080 -v "$(pwd)/data:/data" unfurl-server
```

Notes:
- SQLite database is stored at `/data/unfurl.db`
- Mount `/data` if you want cache persistence across restarts

### Option 2: Docker Compose with SQLite

```bash
cd server
docker compose up --build
```

This starts the app on `http://127.0.0.1:8080`.

### Option 3: Docker Compose with Redis

Start Redis profile and switch backend:

PowerShell:

```powershell
cd server
$env:CACHE_BACKEND='redis'
$env:REDIS_URL='redis://redis:6379/0'
docker compose --profile redis up --build
```

POSIX shell:

```bash
cd server
CACHE_BACKEND=redis REDIS_URL=redis://redis:6379/0 docker compose --profile redis up --build
```

### Option 4: Docker with S3 image cache

Example with a public bucket or CDN origin in front of the bucket:

```bash
cd server
docker run --rm -p 8080:8080 \
  -v "$(pwd)/data:/data" \
  -e IMAGE_CACHE_BACKEND=s3 \
  -e S3_REGION=us-east-1 \
  -e S3_BUCKET=unfurl-images \
  -e S3_PUBLIC_BASE_URL=https://cdn.example.com/unfurl-images \
  -e S3_ACCESS_KEY_ID=your-key \
  -e S3_SECRET_ACCESS_KEY=your-secret \
  unfurl-server
```

Notes:
- `S3_PUBLIC_BASE_URL` must point to a publicly fetchable preview/CDN path
- `/proxy/image` will answer with `302` after S3 hit or upload success
- SQLite is still used for metadata cache unless `CACHE_BACKEND=redis`

### Option 5: Docker with low-memory mode

```bash
cd server
docker run --rm -p 8080:8080 \
  -v "$(pwd)/data:/data" \
  -e LOW_MEMORY_MODE=true \
  unfurl-server
```

Behavior:
- lower idle HTTP pool size
- shorter idle HTTP timeout
- SQLite metadata/image pools reduced to `1`
- shorter SQLite idle timeout
- image transforms handled by the bundled `image_worker` process on demand

### Option 6: Docker with OTLP collector

If your collector is reachable as `otel-collector:4317` from the container network:

```bash
cd server
docker run --rm -p 8080:8080 \
  -v "$(pwd)/data:/data" \
  -e OTEL_SERVICE_NAME=unfurl-server \
  -e OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4317 \
  -e OTEL_EXPORTER_OTLP_PROTOCOL=grpc \
  unfurl-server
```

## Deployment Notes

### Reverse Proxy

If you put Nginx, Caddy, Traefik, or another proxy in front of the service, pass through:
- `Host`
- `X-Forwarded-Proto`
- `X-Forwarded-Host`

The server uses these headers to build image proxy URLs returned by `/api`.

### Persistence

- SQLite mode: persist `/data`
- Redis mode: persistence depends on your Redis configuration, not the app container

### Scaling

- SQLite mode is best for single-instance deployment
- Redis mode is better when you run multiple app instances

### Observability

- OTLP export is off unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set
- Send traces to an OpenTelemetry Collector, Tempo, Jaeger, or another OTLP-compatible backend
- Keep local logs enabled even when OTLP is on, so startup and failure diagnostics remain visible

## API Usage

### Health Check

```bash
curl http://127.0.0.1:8080/health
```

### Unfurl a Page

```bash
curl "http://127.0.0.1:8080/api?url=https%3A%2F%2Fexample.com%2Fpost"
```

Response shape:

```json
{
  "status": "success",
  "data": {
    "title": "Example Title",
    "description": "Example Description",
    "image": {
      "url": "https://cdn.example.com/cover.png",
      "width": 1200,
      "height": 630,
      "proxy": "http://127.0.0.1:8080/proxy/image?..."
    },
    "url": "https://example.com/post",
    "author": null,
    "publisher": "example",
    "date": null,
    "lang": "en",
    "logo": null,
    "video": null,
    "audio": null
  },
  "headers": {
    "x-cache-status": "MISS",
    "x-response-time": "12ms"
  }
}
```

### Force Refresh

```bash
curl "http://127.0.0.1:8080/api?url=https%3A%2F%2Fexample.com%2Fpost&force=true"
```

### Override Metadata Cache TTL

```bash
curl "http://127.0.0.1:8080/api?url=https%3A%2F%2Fexample.com%2Fpost&ttl=600"
```

Valid range:
- minimum `60`
- maximum `604800`

### Proxy an Image

```bash
curl -L "http://127.0.0.1:8080/proxy/image?url=https%3A%2F%2Fcdn.example.com%2Fcover.png&referer=https%3A%2F%2Fexample.com%2Fpost&w=1200&h=630&fit=cover&f=auto&q=80" --output cover.img
```

Supported image query params:
- `url`: required
- `referer`: optional but used if present; overrides client `Referer`
- `w`: width, `1-4096`
- `h`: height, `1-4096`
- `q`: quality, `1-100`
- `f`: `auto|avif|webp|jpeg|png`
- `fit`: `scale-down|contain|cover|crop|pad`

Image cache behavior:
- `referer` is part of the processed image cache key
- identical source URL with different `referer` or transform params produces distinct cache entries
- SQLite image cache returns bytes inline
- S3 image cache returns `302` to the configured public object URL

## Tests And Benchmarks

Unit and integration tests:

```bash
cd server
cargo test
```

Benchmark command:

```bash
cd server
cargo run --release --bin benchmark
```

Benchmark characteristics:
- starts a local mock OG/image upstream server
- prewarms `200`, `1000`, and `5000` OG + image pairs
- runs mixed `/api` and `/proxy/image` traffic
- tests concurrency `10`, `100`, and `200`
- keeps an `80%` cache hit ratio and `20%` miss ratio
- records process-level CPU, memory, and hit/miss latency

Benchmark output:
- console summary per scenario
- JSON report written to `benchmark-results.json`

Latest local benchmark snapshot from `benchmark-results.json`:

| Cache Size | Concurrency | Hit Avg (ms) | Miss Avg (ms) | Peak Memory (MB) | Peak CPU (%) |
| --- | ---: | ---: | ---: | ---: | ---: |
| 200 | 10 | 0.35 | 33.89 | 76.15 | 628.57 |
| 200 | 100 | 15.65 | 82.09 | 125.70 | 1812.50 |
| 200 | 200 | 36.10 | 122.59 | 159.78 | 1730.34 |
| 1000 | 10 | 0.37 | 39.96 | 83.73 | 600.00 |
| 1000 | 100 | 13.55 | 84.44 | 157.13 | 1812.50 |
| 1000 | 200 | 32.11 | 108.39 | 173.30 | 1919.46 |
| 5000 | 10 | 0.37 | 41.39 | 112.59 | 685.71 |
| 5000 | 100 | 17.34 | 97.74 | 192.39 | 1908.05 |
| 5000 | 200 | 44.55 | 150.96 | 203.61 | 2085.71 |

Notes:
- hit ratio is `80%`, miss ratio is `20%`
- traffic mix is `50% /api` and `50% /proxy/image`
- CPU and memory are sampled at the benchmark process level, so they include the app server, mock upstream, and load generator

## Compatibility Notes

This server intentionally mirrors the current Worker behavior in the following areas:
- JSON response envelope
- route names
- cache header defaults
- `HEAD` returns headers only
- metadata is extracted from `<head>` only
- image proxy URLs generated by `/api` include `referer=<source page url>`

## Current Limits

- No auth or rate limiting
- SQLite mode is not designed for high-write clustered deployments
- Image transforms use the Rust `image` crate; behavior is practical and compatible, but not byte-identical to Cloudflare Image Resizing
- Current OTLP support exports traces only, not metrics or logs

## Suggested Production Setup

1. Run behind a reverse proxy with TLS termination.
2. Mount `/data` if using SQLite.
3. Use Redis if you deploy multiple instances.
4. Add external rate limiting if the service is public.
5. Export traces to an OTLP collector.
6. Monitor upstream fetch failures and image transform errors.
