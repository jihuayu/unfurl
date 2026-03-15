# Unfurl Server

A standalone Rust server version of `unfurl` that does not depend on Cloudflare.

It keeps the same public routes as the Worker version:
- `GET|HEAD /health`
- `GET|HEAD /api`
- `GET|HEAD /proxy/image`

Default cache backend is SQLite. Redis is optional and pluggable.
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
- Docker deployment support

## Project Layout

- `src/main.rs`: server entrypoint
- `src/routes.rs`: `/health`, `/api`, `/proxy/image`
- `src/cache.rs`: cache abstraction + SQLite/Redis backends
- `src/extract.rs`: HTML head metadata extraction and merge logic
- `src/image_proxy.rs`: image transform pipeline
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
| `CACHE_BACKEND` | `sqlite` | `sqlite` or `redis` |
| `SQLITE_PATH` | `/data/unfurl.db` | SQLite file path |
| `REDIS_URL` | empty | Required when `CACHE_BACKEND=redis` |
| `API_RESPONSE_CACHE_TTL` | `3600` | Browser/API JSON cache TTL |
| `IMAGE_CACHE_TTL` | `86400` | Browser image cache TTL |
| `OG_CACHE_TTL` | `43200` | Metadata cache TTL |
| `FETCH_TIMEOUT_MS` | `8000` | Upstream fetch timeout |

A sample env file is provided at `.env.example`.

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

## Suggested Production Setup

1. Run behind a reverse proxy with TLS termination.
2. Mount `/data` if using SQLite.
3. Use Redis if you deploy multiple instances.
4. Add external rate limiting if the service is public.
5. Monitor upstream fetch failures and image transform errors.
