# Multi-stage build: build on a full rust image, run on distroless.
# Result: ~15 MB final image, no shell, no package manager, no root.

FROM rust:1.82-slim-bookworm AS builder
WORKDIR /build

# Layer-cache deps separately from source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main(){}" > src/main.rs && \
    cargo build --release && \
    rm -rf src target/release/solmux target/release/deps/solmux*

COPY src ./src
RUN cargo build --release --locked && \
    strip target/release/solmux

FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /build/target/release/solmux /usr/local/bin/solmux
COPY config.example.yaml /app/config.example.yaml

# 8899 = JSON-RPC listener, 9090 = Prometheus
EXPOSE 8899 9090

# Use the binary itself to healthcheck (distroless has no shell/curl).
HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/solmux", "--healthcheck"]

USER nonroot
ENTRYPOINT ["/usr/local/bin/solmux"]
CMD ["--config", "/app/config.yaml"]
