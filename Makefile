IMAGE ?= kata-device-plugin
TAG   ?= latest
# Commit of this build, with -dirty when the tree has uncommitted changes.
GIT_SHA := $(shell git rev-parse --short HEAD)$(shell git diff-index --quiet HEAD -- || echo -dirty)

.PHONY: build image push clippy fmt test test-unit test-integration deploy undeploy clean

build:
	cargo build --release

clippy:
	cargo clippy --workspace -- -D warnings

fmt:
	cargo fmt --all --check

# Unit tests only (no sockets, no gRPC) — fast, no setup needed.
test-unit:
	cargo test --workspace --lib

# Integration tests: mock kubelet socket + fake VFIO devices.
# No GPU or Kubernetes cluster required.
test-integration:
	cargo test --test server --test fabric --test partition

# All tests.
test: test-unit test-integration

# .git is not in the docker build context — hand the commit in explicitly.
image:
	docker build -t $(IMAGE):$(TAG) --build-arg GIT_SHA=$(GIT_SHA) .

push:
	docker push $(IMAGE):$(TAG)

# Install into the current kubectl context.
deploy:
	helm upgrade --install kata-device-plugin deploy/helm/kata-device-plugin -n kube-system

undeploy:
	helm uninstall kata-device-plugin -n kube-system

clean:
	cargo clean
