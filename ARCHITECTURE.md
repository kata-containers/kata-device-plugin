# Architecture

## What this plugin does and does not do

This is a **read-only** Kubernetes device plugin. It enumerates VFIO-bound passthrough devices on a node, classifies them by sysfs PCI identity, and advertises them to the Kubernetes scheduler as `nvidia.com/gpu` (and `nvidia.com/nvswitch` where NVSwitch devices are bound). It never binds, configures, or reconfigures any device. VFIO binding and all other infrastructure operations happen on the trusted side, outside the workload cluster.

A clash with another plugin advertising `nvidia.com/gpu` is intentional and detects misconfiguration: trusted and untrusted workloads do not share a cluster, so on a Kata/untrusted cluster nothing else exposes this resource.

The plugin talks only to the kubelet, over unix sockets. It has no Kubernetes API access, no RBAC, and no modes — behaviour is identical on every platform.

## Supported platforms

**HGX H100, HGX B200/B300, GB200 NVL72, GB300, Vera Rubin.**

GPUs (and NVSwitches, where the platform has them) are passed through to Kata VMs via VFIO. The plugin enumerates IOMMUFD cdevs under `/dev/vfio/devices/`, classifies each via `/sys/class/vfio-dev/vfioN/device` PCI vendor/class, and advertises each as one device of the matching resource.

## Out of scope: multi-node NVLink (IMEX)

On GB200 NVL72 / GB300 / Vera Rubin, GPUs across nodes share memory over the NVLink fabric through the IMEX service. None of that concerns this plugin:

- **NVLink partition identity** (cluster UUID / clique ID) is a scheduling and placement concern, handled outside device advertisement.
- **IMEX mesh setup** is in-guest work: the mesh agent fetches an overlay credential from Trustee, waits for the member roster, writes `nodes_config.cfg`, and hands off to `nvidia-imex`.
- **The IMEX mesh size** is a per-job value that travels via pod annotation → CRI `RunPodSandbox` → Kata runtime → `fw_cfg`. Mixing job-level scheduling metadata with device-level hardware declaration is a category error (ADR 5000).

The device plugin advertises the same devices the same way whether or not the node participates in a multi-node NVLink partition.

## Threat models

The plugin runs identically under both threat models described in the design documents.

**Trusted-host:** The host and operator are trusted. Kata provides workload isolation from the host.

**Untrusted-host (confidential computing):** The host is adversarial. The VM contents are opaque to the host. The device plugin still only reads and declares — it holds no keys and performs no attestation. Attestation and key release are the in-guest components' responsibility (attestation agent, CDH, mesh agent). The plugin's security posture is unchanged.

## Rationale

### Why `nvidia.com/gpu` and not a distinct resource name

Trusted and untrusted workloads do not share a cluster. On a Kata/untrusted cluster the standard NVIDIA device plugin and GPU Operator do not run, so `nvidia.com/gpu` is uncontested. Using the same name means workload pod specs need no modification when moving between a trusted cluster (standard plugin) and an untrusted cluster (this plugin). A resource name clash is a detectable misconfiguration, not a silent conflict.

### Why a device plugin and not DRA

DRA earns its place when there are real topology constraints — heterogeneous fleets, cross-device matching, per-claim bind/unbind lifecycle. None of those are load-bearing here. Nodes are single-tenant with GPUs in a fixed passthrough state. Vera Rubin removes intra-node board alignment, so VM width is a plain count. A device plugin is the right tool (ADR 1000).

### Why there is no tray-level or aggregate resource

Deciding what an aggregate *is* (a tray, a half-node, the whole node) is consumption policy, and that decision belongs to the layer above. The scheduler is free to consume the declared devices as a whole (`nvidia.com/gpu: 4` on a GB200 compute tray is the whole tray) or as a subset (`nvidia.com/gpu: 2`); the plugin does not pre-empt that choice by bundling devices into units. Whole-node intent is a count plus node labels and taints, expressed in pod specs and placement policy.

The mechanics are secondary: two extended resources over the same hardware also cannot be reconciled (independent scheduler ledgers, no deallocation signal in the device plugin API). Even if they could be, the aggregation decision still would not belong here. If both granularities on one node ever become a hard requirement, that is DRA's partitionable-device model: revisit ADR 1000.

### Why the plugin does not bind VFIO devices

Binding a device is a reconfiguration of the infrastructure. The workload cluster is read-only toward the infrastructure; reconfiguration authority lives in the admin cluster. Placing a component that can bind devices inside the workload cluster would put infrastructure control next to the untrusted workloads it is meant to contain (boundary document, ADR 1000). The device plugin only declares what already exists.

### Why no privileged pods

The isolation primitive for untrusted workloads is the VM or CVM boundary, not pod privilege. A privileged DaemonSet in the workload cluster is a standing high-privilege target next to untrusted workloads. Deleting the capability is stronger than fencing it. The device plugin reads `/dev/vfio/` to enumerate devices (read-only host path mount) and writes to the kubelet socket directory — both are narrow, named mounts, not broad privilege (ADR 10000).

## Cluster shape

This plugin runs in the **workload cluster**, which is read-only toward the infrastructure. It never runs in the admin cluster. The admin cluster holds the components that bind VFIO devices and configure the NVLink fabric.

```text
Admin cluster                        Workload cluster
─────────────────────────────        ──────────────────────────────────
GPU Operator (driver, MIG, fabric)   kata-device-plugin (declares only)
VFIO binder                          Kata VMs
                                     Workload pods requesting nvidia.com/gpu
```
