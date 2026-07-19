# Build stage — statically linked musl binary
FROM docker.io/clux/muslrust:stable AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release && strip target/x86_64-unknown-linux-musl/release/broadside

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
