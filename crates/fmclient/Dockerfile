# Stage 1: Build
FROM ubuntu:24.04 AS builder

ARG FM_VERSION=590

# libnvfm ships only in NVIDIA's CUDA repository; the keyring package adds
# the repository and its signing key.
RUN apt-get update && apt-get install -y --no-install-recommends \
        curl \
        ca-certificates \
    && curl -fsSLO https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb \
    && dpkg -i cuda-keyring_1.1-1_all.deb \
    && rm cuda-keyring_1.1-1_all.deb \
    && apt-get update && apt-get install -y --no-install-recommends \
        gcc \
        libc6-dev \
        nvidia-fabricmanager-dev-${FM_VERSION} \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --locked

# Stage 2: Runtime
FROM ubuntu:24.04

COPY --from=builder /build/target/release/fmclient /usr/local/bin/fmclient
COPY --from=builder /usr/lib/x86_64-linux-gnu/libnvfm* /usr/lib/x86_64-linux-gnu/
RUN ldconfig

ENTRYPOINT ["/bin/bash"]
