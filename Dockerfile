# syntax=docker/dockerfile:1.6

# --- builder ------------------------------------------------------------
FROM rust:1.85-bookworm AS builder

# protoc is required for tonic-build.
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Build with the handler repository as the Docker context. The iCommerce
# engine crates are fetched from the pinned Git revision in Cargo.toml.
COPY . .
RUN cargo build --release --bin stateset-icp-handler

# --- runtime ------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 1000 icp \
    && useradd  --system --uid 1000 --gid icp --shell /usr/sbin/nologin icp

WORKDIR /app
COPY --from=builder /build/target/release/stateset-icp-handler \
     /usr/local/bin/stateset-icp-handler

USER icp
EXPOSE 8082 50052
ENV HOST=0.0.0.0 PORT=8082 GRPC_HOST=0.0.0.0 GRPC_PORT=50052

ENTRYPOINT ["/usr/local/bin/stateset-icp-handler"]
