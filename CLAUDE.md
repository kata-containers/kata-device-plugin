# kata-device-plugin

Rust. No Go. No exceptions.

## What this is

A Kubernetes device plugin DaemonSet that watches its own node's NFD labels and starts the right passthrough GPU plugin variant. Read-only toward the infrastructure — it only declares what exists, never binds or reconfigures anything (ADR 1000, boundary doc).

## Project layout

```text
src/
  main.rs      entry point, CLI args, node label watcher loop, mode selection
  labels.rs    label key/value constants only
  plugin.rs    DeviceServer (tonic gRPC impl) + Mode enum
proto/
  deviceplugin.proto  kubelet device plugin v1beta1 API
build.rs       prost/tonic codegen from proto
deploy/
  rbac.yaml
  daemonset.yaml
```

## Principles

- **KISS** — fewer files, fewer abstractions. No trait objects, no dynamic dispatch, no registry pattern. A `match` on a `Mode` enum is enough.
- **No privileged pods** — the DaemonSet mounts `/dev/vfio` read-only and `/var/lib/kubelet/device-plugins` for the kubelet socket. Nothing else. Isolation is the VM boundary (ADR 10000).
- **No reconfiguration** — VFIO binding happens on the trusted side. This plugin only reads `/dev/vfio/` to enumerate numeric group entries and report them as healthy `nvidia.com/gpu` devices.

## Resource name

The plugin advertises `nvidia.com/gpu` — the same name the standard NVIDIA device plugin would use.  This is intentional: trusted and untrusted workloads never share a cluster, so on a Kata/untrusted cluster nothing else exposes `nvidia.com/gpu`.  A clash means two device plugins are running on the same node, which is a misconfiguration that should be detected loudly.

## Label scheme

No custom labels. The plugin reads standard GFD labels only.

| Label | Written by | Meaning |
| --- | --- | --- |
| `nvidia.com/gpu.present` | GFD | Node has GPUs; used as DaemonSet `nodeSelector` |
| `nvidia.com/gpu.clique` | GFD (k8s-device-plugin ≥ v0.17.0) | `{ClusterUUID}.{CliqueID}` — node is fabric-attached and `GPU_FABRIC_STATE_COMPLETED`; triggers `Mode::Imex` |

Mode selection: clique label present → `pgpu-imex` (multi-node NVLink). Absent → `pgpu` (plain passthrough).

## Building

Requires `protoc` (`apt install protobuf-compiler`).

```sh
cargo build --release
cargo clippy -- -D warnings
cargo fmt --check
make image   # docker build
```

## Adding a new platform variant

1. Add a constant to `src/labels.rs` if a new label value is needed.
2. Add an arm to the `Mode` enum in `src/plugin.rs`.
3. Add an arm to `select_mode()` in `src/main.rs`.
4. Add platform-specific behaviour inside the matching `Mode` arm in `DeviceServer::run()`.

## See also

- [ARCHITECTURE.md](ARCHITECTURE.md) — supported use-cases, rationale, cluster shape
- [AGENTS.md](AGENTS.md) — AI agent guidance for this repo
- Design docs: "Runtime Deployment Models…" and "Multi-Node NVLink for Kata"
