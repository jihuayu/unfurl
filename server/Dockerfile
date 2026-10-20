FROM rust:1.95-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin unfurl-server --bin image_worker

FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/unfurl-server /usr/local/bin/unfurl-server
COPY --from=builder /app/target/release/image_worker /usr/local/bin/image_worker
RUN useradd --system --uid 10001 --home /app --shell /usr/sbin/nologin unfurl \
  && mkdir -p /data && chown unfurl:unfurl /data

ENV HOST=0.0.0.0 \
    PORT=8080 \
    CACHE_BACKEND=sqlite \
    IMAGE_CACHE_BACKEND=sqlite \
    IMAGE_CACHE_TTL=259200 \
    SQLITE_PATH=/data/unfurl.db

EXPOSE 8080
USER unfurl
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl --fail --silent --show-error http://127.0.0.1:8080/health || exit 1
CMD ["unfurl-server"]
