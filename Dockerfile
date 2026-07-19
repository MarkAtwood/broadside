# Build stage — statically linked musl binary
FROM docker.io/library/rust:1.96-bookworm AS builder

RUN rustup target add x86_64-unknown-linux-musl && \
    apt-get update && apt-get install -y musl-tools && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release --target x86_64-unknown-linux-musl && \
    strip target/x86_64-unknown-linux-musl/release/broadside

# Runtime stage — scratch (just the binary + CA certs)
FROM scratch

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/broadside /broadside

# Default data directory
VOLUME /data

# Default bind port
EXPOSE 3000

# Environment variables
ENV BROADSIDE_DATA_DIR=/data
ENV RUST_LOG=broadside=info
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

# Labels
ARG VERSION=dev
LABEL org.opencontainers.image.title="broadside" \
      org.opencontainers.image.description="One-way ActivityPub server for organizations" \
      org.opencontainers.image.url="https://github.com/MarkAtwood/broadside" \
      org.opencontainers.image.source="https://github.com/MarkAtwood/broadside" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.version="${VERSION}"

ENTRYPOINT ["/broadside"]
CMD ["--data-dir", "/data", "serve"]
