# kata-device-plugin

Rust. No Go. No exceptions.

## What this is

A Kubernetes device plugin DaemonSet that enumerates the node's VFIO-bound passthrough devices and declares them to the kubelet. Read-only toward the infrastructure — it only declares what exists, never binds or reconfigures anything (ADR 1000, boundary doc). It talks only to the kubelet over unix sockets; it has no Kubernetes API access.

## Project layout

```text
src/
  main.rs      entry point: start one DeviceServer per present resource, wait for shutdown
  plugin.rs    DeviceServer (tonic gRPC impl) + socket/CDI-dir constants
  vfio.rs      IOMMUFD cdev enumeration + the RESOURCES table (PCI identity → resource name)
  cdi.rs       host-side CDI spec writer (read by the Kata shim)
proto/
  deviceplugin.proto  kubelet device plugin v1beta1 API
build.rs       prost/tonic codegen from proto
deploy/
  daemonset.yaml
```

## Principles

- **KISS** — fewer files, fewer abstractions. No trait objects, no dynamic dispatch, no registry pattern, no modes, no config file, no CLI args. Behaviour is identical on every platform; everything the plugin needs is a kernel, kubelet, or CDI contract expressed as a constant. The advertised device set is fixed for the plugin's lifetime — a device-set change on the node means the pod restarts.
- **No privileged pods** — the DaemonSet mounts `/dev/vfio` read-only and `/var/lib/kubelet/device-plugins` for the kubelet socket. Nothing else. Isolation is the VM boundary (ADR 10000).
- **No reconfiguration, no platform knowledge** — VFIO binding happens on the trusted side. The plugin's whole interface is `/dev/vfio/devices/vfio*`: whatever is bound gets consumed. Each cdev is matched against the `RESOURCES` table in `src/vfio.rs` — one row per resource: (name, PCI vendor, PCI class prefix), read from `/sys/class/vfio-dev/vfioN/device`. A resource is advertised iff matching devices are present; unmatched devices are ignored. Supporting a new device type is one table row — no other code changes. IOMMUFD only — no legacy VFIO group backend. All paths are kernel contracts, not configuration.
- **Supply, not demand** — the plugin declares what exists; how it is consumed is the scheduling layer's decision. Whole node vs subset (`nvidia.com/gpu: 4` vs `nvidia.com/gpu: 2`), exclusive tenancy, tray semantics: all of that is expressed in pod specs, node labels, and taints, never encoded here. No aggregate resources, no consumption modes (see ARCHITECTURE.md).

## Resource name

The plugin advertises `nvidia.com/gpu` — the same name the standard NVIDIA device plugin would use.  This is intentional: trusted and untrusted workloads never share a cluster, so on a Kata/untrusted cluster nothing else exposes `nvidia.com/gpu`.  A clash means two device plugins are running on the same node, which is a misconfiguration that should be detected loudly.  The names live in the `RESOURCES` table in `src/vfio.rs`, not configuration.

## Label scheme

The plugin reads no labels at runtime. The only label in play is `nvidia.com/gpu.present` (written by GFD), used as the DaemonSet `nodeSelector` so the plugin only lands on GPU nodes. Multi-node NVLink / IMEX orchestration is out of scope for this component — see ARCHITECTURE.md.

## Building

Requires `protoc` (`apt install protobuf-compiler`).

```sh
cargo build --release
cargo clippy -- -D warnings
cargo fmt --check
make image   # docker build
```

## See also

- [ARCHITECTURE.md](ARCHITECTURE.md) — supported use-cases, rationale, cluster shape
- [AGENTS.md](AGENTS.md) — AI agent guidance for this repo
- Design docs: "Runtime Deployment Models…" and "Multi-Node NVLink for Kata"
