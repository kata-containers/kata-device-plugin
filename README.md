# kata-device-plugin

A read-only Kubernetes device plugin for GPU passthrough into Kata VMs.
Its whole interface is `/dev/vfio/devices/vfio*`: whatever the trusted side
has VFIO-bound gets classified by sysfs PCI identity and advertised to the
kubelet — nothing is ever bound, configured, or reconfigured by this plugin.

## How it works

Every device this plugin can advertise is one row in a compile-time table
([`src/vfio.rs`](src/vfio.rs)): a Kubernetes extended-resource name plus the
PCI vendor and class prefix that identify it in `/sys/class/vfio-dev`.

| Resource | PCI identity |
| --- | --- |
| `nvidia.com/gpu` | `0x10de`, class `0x0302xx` (3D controller) |
| `nvidia.com/nvswitch` | `0x10de`, class `0x0680xx` (bridge: other) |

A resource is advertised iff matching devices are VFIO-bound — declared by
the node, not configured. Supporting a new device type is one table row; no
other code changes.

For each present resource the plugin:

1. writes a host CDI spec mapping device indices to IOMMUFD cdev paths —
   `/var/run/cdi/kata.nvidia.com-gpu.yaml` for `nvidia.com/gpu`,
2. registers with the kubelet over its unix socket and serves the device
   plugin v1beta1 API,
3. answers `Allocate` with CDI device names (`nvidia.com/gpu=0`) — the
   container runtime resolves them against the host CDI registry, and the
   Kata shim wires the cold-plugged VM devices to the right container.

No config file, no CLI arguments, no Kubernetes API access, no modes: all
paths and names are kernel, kubelet, or CDI contracts expressed as
constants. The advertised device set is fixed for the plugin's lifetime — a
device-set change on the node means the pod restarts.

## Building and testing

Requires `protoc` and the Rust toolchain pinned in
[`rust-toolchain.toml`](rust-toolchain.toml).

```sh
make build   # cargo build --release
make test    # unit + integration tests: no GPU or cluster required
make image   # container image (bakes the git commit into the startup log)
```

The test suite mocks VFIO with temp directories (fake cdevs + sysfs
identities) and runs a mock kubelet on unix sockets that probes the plugin
endpoint exactly like the real device manager — so registration ordering,
stream lifecycle, and CDI output are all covered without hardware.

## Deploying

```sh
make deploy  # kubectl apply -f deploy/daemonset.yaml
```

The DaemonSet mounts three host paths: the kubelet device-plugin socket
directory, `/dev/vfio` (read-only), and `/var/run/cdi`. It runs as uid 0
(pinned with `runAsUser: 0`) with every capability dropped and a read-only
root filesystem — root is required only because the kubelet owns its
socket directory.

## See also

- [ARCHITECTURE.md](ARCHITECTURE.md) — scope, rationale, cluster shape
- [CLAUDE.md](CLAUDE.md) / [AGENTS.md](AGENTS.md) — contributor and agent guidance
