FROM rust:latest AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN cargo build --release

FROM debian:stable-slim
RUN apt-get update \
  && apt-get upgrade -y \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/unfurl-server /usr/local/bin/unfurl-server
RUN mkdir -p /data

ENV HOST=0.0.0.0 \
    PORT=8080 \
    CACHE_BACKEND=sqlite \
    SQLITE_PATH=/data/unfurl.db

EXPOSE 8080
VOLUME ["/data"]
CMD ["unfurl-server"]
