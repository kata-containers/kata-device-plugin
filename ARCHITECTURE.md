# Architecture

## What this plugin does and does not do

This is a **read-only** Kubernetes device plugin. It enumerates passthrough GPU devices on a node and advertises them to the Kubernetes scheduler as `nvidia.com/gpu`. It never binds, configures, or reconfigures any device. VFIO binding and all other infrastructure operations happen on the trusted side, outside the workload cluster.

A clash with another plugin advertising `nvidia.com/gpu` is intentional and detects misconfiguration: trusted and untrusted workloads do not share a cluster, so on a Kata/untrusted cluster nothing else exposes this resource.

## Supported platforms and use cases

### Plain GPU passthrough — `pgpu`

**Platforms:** HGX H100, HGX B200/B300

GPUs are passed through to Kata VMs via VFIO. The NVLink fabric, where present, is scoped within a single node and does not require any inter-VM coordination. The plugin enumerates `/dev/vfio/` numeric entries and advertises each as one `nvidia.com/gpu` device.

### Multi-node NVLink with IMEX — `pgpu-imex`

**Platforms:** GB200 NVL72, GB300, Vera Rubin

GPUs across multiple nodes share memory over the NVLink fabric through the IMEX service (`nvidia-imex`). One IMEX daemon runs per VM; together they form a full mesh. The plugin behaves identically to `pgpu` for device advertisement, with two additional constraints:

1. **Clique label required.** Devices are only advertised once `kata.nvidia.com/nvlink-clique-id` is present on the node (see [NVLink partition identification](#nvlink-partition-identification) below). This ensures the VM is placed on a known NVLink partition before workloads are scheduled.
2. **In-guest mesh setup.** After the VM starts, the in-guest mesh agent fetches an overlay credential from Trustee, waits for the member roster to reach the expected IMEX mesh size, writes `nodes_config.cfg`, and then hands off to `nvidia-imex`. This is all in-guest work; the device plugin is not involved.

The IMEX mesh size reaches the guest via a pod annotation → CRI `RunPodSandbox` → Kata runtime → one-way host-to-guest channel (`fw_cfg`). It is not carried by the device plugin `Allocate` response.

### IMEX mesh sizes by VM width (one NVL72 rack)

| VM width | VMs per rack | IMEX daemons |
| -------- | ------------ | ------------ |
| 4-way    | 18           | 18           |
| 2-way    | 36           | 36           |
| 1-way    | 72           | 72           |

## NVLink partition identification

An NVLink partition is identified by a **Cluster UUID** and a **Clique ID**, both read from `nvmlDeviceGetGpuFabricInfo()`. GFD (GPU Feature Discovery, bundled in `k8s-device-plugin` ≥ v0.17.0) writes these as the node label `nvidia.com/gpu.clique={ClusterUUID}.{CliqueID}` once `GPU_FABRIC_STATE_COMPLETED` is confirmed. The NVIDIA Fabric Manager must be running and have finished fabric initialisation before GFD can read this state.

Under Kata, no GPU driver runs on the host, so GFD cannot run in its standard form on Kata nodes. The discovery flow requires a short-lived Kata probe Job:

```text
Fabric Manager (host) reaches COMPLETED state
  └── Clique probe (short-lived Kata Job)
        └── loads driver inside VM, calls nvmlDeviceGetGpuFabricInfo()
            └── writes nvidia.com/gpu.clique label via NodeFeature → NFD
                └── device plugin sees label → starts Mode::Imex
```

The clique probe is a prerequisite for `pgpu-imex` on Kata nodes. Until `nvidia.com/gpu.clique` appears, the dispatcher runs `Mode::Pgpu` (plain passthrough, no IMEX).

## Plugin selection

The dispatcher watches the node's own labels via the Kubernetes API. Mode is derived from a single standard GFD label — no custom labels required.

| `nvidia.com/gpu.clique` | Mode | Platform |
| --- | --- | --- |
| present and non-empty | `Mode::Imex { clique_id }` | GB200 NVL72, GB300, Vera Rubin |
| absent | `Mode::Pgpu` | HGX H100, HGX B200/B300 |

The value format is `{ClusterUUID}.{CliqueID}`, e.g. `7b968a6d-c8aa-45e1-9e70-e1e51be99c31.1`. GFD only writes this label once `GPU_FABRIC_STATE_COMPLETED` is confirmed via `nvmlDeviceGetGpuFabricInfo()`, which requires the NVIDIA Fabric Manager to have finished fabric initialisation.

On a label change the running plugin is cancelled and the new mode is started. The DaemonSet uses `nvidia.com/gpu.present=true` as a `nodeSelector` so it only lands on nodes where GFD has confirmed GPU presence.

## Threat models

The plugin runs identically under both threat models described in the design documents.

**Trusted-host:** The host and operator are trusted. Kata provides workload isolation from the host. The clique label is informational; a wrong value is a liveness failure, not a security break (a misplaced VM cannot reach peers across NVLink clique boundaries).

**Untrusted-host (confidential computing):** The host is adversarial. The VM contents are opaque to the host. The device plugin still only reads and declares — it holds no keys and performs no attestation. Attestation and key release are the in-guest components' responsibility (attestation agent, CDH, mesh agent). The plugin's security posture is unchanged.

## Rationale

### Why `nvidia.com/gpu` and not a distinct resource name

Trusted and untrusted workloads do not share a cluster. On a Kata/untrusted cluster the standard NVIDIA device plugin and GPU Operator do not run, so `nvidia.com/gpu` is uncontested. Using the same name means workload pod specs need no modification when moving between a trusted cluster (standard plugin) and an untrusted cluster (this plugin). A resource name clash is a detectable misconfiguration, not a silent conflict.

### Why a device plugin and not DRA

DRA earns its place when there are real topology constraints — heterogeneous fleets, cross-device matching, per-claim bind/unbind lifecycle. None of those are load-bearing here. Nodes are single-tenant with GPUs in a fixed passthrough state. Vera Rubin removes intra-node board alignment, so VM width is a plain count. The NVLink partition is flat, so placement within a partition is unconstrained. Clique selection is a single label match. A device plugin is the right tool (ADR 1000).

### Why the plugin does not bind VFIO devices

Binding a device is a reconfiguration of the infrastructure. The workload cluster is read-only toward the infrastructure; reconfiguration authority lives in the admin cluster. Placing a component that can bind devices inside the workload cluster would put infrastructure control next to the untrusted workloads it is meant to contain (boundary document, ADR 1000). The device plugin only declares what already exists.

### Why advertisement switches to `pgpu-imex` on `nvidia.com/gpu.clique`

GFD only writes `nvidia.com/gpu.clique` after `nvmlDeviceGetGpuFabricInfo()` returns `GPU_FABRIC_STATE_COMPLETED`, meaning the Fabric Manager has confirmed the node's partition identity. Switching to IMEX mode before that is confirmed would allow a VM to be scheduled onto a node whose NVLink partition membership is unknown. IMEX cannot cross clique boundaries, so a misplaced VM fails to converge rather than leaking — but it still wastes a slot. Gating on the GFD label is an early-failure optimization (ADR 8000).

### Why the IMEX mesh size is not carried by the device plugin

The mesh size is a per-job value set by whoever submits the workload. The device plugin does not know it at allocation time and should not — mixing job-level scheduling metadata with device-level hardware declaration is a category error. The mesh size travels via pod annotation → CRI `RunPodSandbox` → Kata runtime → `fw_cfg`, a path designed specifically for dynamic, untrusted, non-secret scalars (ADR 5000, configuration delivery section).

### Why no privileged pods

The isolation primitive for untrusted workloads is the VM or CVM boundary, not pod privilege. A privileged DaemonSet in the workload cluster is a standing high-privilege target next to untrusted workloads. Deleting the capability is stronger than fencing it. The device plugin reads `/dev/vfio/` to enumerate devices (read-only host path mount) and writes to the kubelet socket directory — both are narrow, named mounts, not broad privilege (ADR 10000).

## Cluster shape

This plugin runs in the **workload cluster**, which is read-only toward the infrastructure. It never runs in the admin cluster. The admin cluster holds the components that bind VFIO devices and configure the NVLink fabric.

```text
Admin cluster                        Workload cluster
─────────────────────────────        ──────────────────────────────────
GPU Operator (driver, MIG, fabric)   kata-device-plugin (declares only)
VFIO binder                          Kata VMs
Clique probe launcher          →     NFD label on node
                                     Workload pods requesting nvidia.com/gpu
```
