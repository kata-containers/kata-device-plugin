# ── build stage ────────────────────────────────────────────────────────────────
FROM rust:1.82-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY build.rs ./
COPY proto/ proto/
COPY src/ src/

RUN cargo build --release

# ── runtime stage ──────────────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /build/target/release/kata-device-plugin /kata-device-plugin

ENTRYPOINT ["/kata-device-plugin"]
