# ── build stage ────────────────────────────────────────────────────────────────
FROM rust:1.94-slim AS builder

# The build context has no .git — the Makefile passes the commit in.
ARG GIT_SHA=unknown
ENV GIT_SHA=$GIT_SHA

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
# No glob on Cargo.lock: it is committed, and --locked below is meaningless
# if a missing lockfile silently passes the COPY.
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY proto/ proto/
COPY src/ src/

RUN cargo build --release --locked

# ── runtime stage ──────────────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

# Links the ghcr.io package to this repository automatically.
LABEL org.opencontainers.image.source="https://github.com/zvonkok/kata-device-plugin" \
      org.opencontainers.image.description="Kata device plugin: advertises VFIO-bound passthrough devices to the kubelet"

COPY --from=builder /build/target/release/kata-device-plugin /kata-device-plugin

ENTRYPOINT ["/kata-device-plugin"]
