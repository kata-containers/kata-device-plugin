# The Fabric Manager SDK library, from NVIDIA's CUDA repository.  The build
# does not link it; the runtime image carries it for fmclient to load, and
# its version must match Fabric Manager in the guest image.
FROM ubuntu:24.04 AS sdk

ARG FM_VERSION=590

RUN apt-get update && apt-get install -y --no-install-recommends \
        curl \
        ca-certificates \
    && curl -fsSLO https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb \
    && dpkg -i cuda-keyring_1.1-1_all.deb \
    && rm cuda-keyring_1.1-1_all.deb \
    && apt-get update && apt-get install -y --no-install-recommends \
        nvidia-fabricmanager-dev-${FM_VERSION} \
    && rm -rf /var/lib/apt/lists/*

# Built from the repository root, so the workspace's lock file and the
# fabric-api crate are in the context.
FROM rust:1.94-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto/ proto/
COPY src/ src/
COPY crates/ crates/

RUN cargo build --release --locked -p fmclient

FROM ubuntu:24.04

COPY --from=builder /build/target/release/fmclient /usr/local/bin/fmclient
COPY --from=sdk /usr/lib/x86_64-linux-gnu/libnvfm* /usr/lib/x86_64-linux-gnu/
RUN ldconfig

ENTRYPOINT ["/bin/bash"]
