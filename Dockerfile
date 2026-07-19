# Build stage — compile the release binary
FROM docker.io/library/rust:1.96-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

# Build release binary with static linking for a scratch-compatible image
RUN cargo build --release && strip target/release/broadside

# Runtime stage — minimal image
FROM docker.io/library/debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    # Create non-root user
    groupadd -r broadside && \
    useradd -r -g broadside -d /data -s /sbin/nologin broadside && \
    mkdir -p /data/media && \
    chown -R broadside:broadside /data

COPY --from=builder /build/target/release/broadside /usr/local/bin/broadside

# Default data directory
VOLUME /data

# Default bind port
EXPOSE 3000

# Environment variables for configuration
ENV BROADSIDE_DATA_DIR=/data
ENV RUST_LOG=broadside=info

# Health check — uses the built-in /health endpoint
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/broadside", "--data-dir", "/data", "status"]

# Run as non-root
USER broadside

# Labels for container registries and tooling
ARG VERSION=dev
LABEL org.opencontainers.image.title="broadside" \
      org.opencontainers.image.description="One-way ActivityPub server for organizations" \
      org.opencontainers.image.url="https://github.com/MarkAtwood/broadside" \
      org.opencontainers.image.source="https://github.com/MarkAtwood/broadside" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.version="${VERSION}"

ENTRYPOINT ["/usr/local/bin/broadside"]
CMD ["--data-dir", "/data", "serve"]
