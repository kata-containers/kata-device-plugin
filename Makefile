IMAGE ?= kata-device-plugin
TAG   ?= latest

.PHONY: build image push clippy fmt test test-unit test-integration clean

build:
	cargo build --release

clippy:
	cargo clippy -- -D warnings

fmt:
	cargo fmt --check

# Unit tests only (no sockets, no gRPC) — fast, no setup needed.
test-unit:
	cargo test --lib

# Integration tests: mock kubelet socket + fake VFIO devices.
# No GPU or Kubernetes cluster required.
test-integration:
	cargo test --test server

# All tests.
test: test-unit test-integration

image:
	docker build -t $(IMAGE):$(TAG) .

push:
	docker push $(IMAGE):$(TAG)

clean:
	cargo clean
