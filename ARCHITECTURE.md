# Architecture

## What this plugin does and does not do

This is a **read-only** Kubernetes device plugin. It enumerates VFIO-bound passthrough devices on a node, classifies them by sysfs PCI identity, and advertises them to the Kubernetes scheduler as `nvidia.com/gpu` (and `nvidia.com/nvswitch-fabric` where the node's NVSwitch fabric is bound: the switch chips on HGX H100/H200, the management ConnectX PFs that represent them on HGX B200/B300). It never binds, configures, or reconfigures any device. VFIO binding and all other infrastructure operations happen on the trusted side, outside the workload cluster.

A clash with another plugin advertising `nvidia.com/gpu` is intentional and detects misconfiguration: trusted and untrusted workloads do not share a cluster, so on a Kata/untrusted cluster nothing else exposes this resource.

The plugin talks to the kubelet over unix sockets and hosts one more endpoint, the fabric view service described below, which only the node's Service VM can feed. It has no Kubernetes API access and no RBAC. Behaviour is identical on every platform; what differs per node is what the fabric owner publishes.

## Supported platforms

**HGX H100, HGX B200/B300, GB200 NVL72, GB300, Vera Rubin.**

GPUs (and NVSwitches, where the platform has them) are passed through to Kata VMs via VFIO. The plugin enumerates IOMMUFD cdevs under `/dev/vfio/devices/`, classifies each via `/sys/class/vfio-dev/vfioN/device` PCI vendor/class, and advertises each as one device of the matching resource.

## Service VM

NVSwitch fabrics are never passed to tenant VMs. They go to one Service VM per node that runs Fabric Manager in Shared NVSwitch mode, and that Service VM is itself a Kata pod: it requests `nvidia.com/nvswitch-fabric: 1`, the same on every platform. On HGX H100/H200 that one device holds the four NVSwitch chips. On HGX B200/B300 it holds the management ConnectX whose PFs NVIDIA's integration guide passes to the Service VM together; telling those PFs from the node's data-path ConnectX NICs takes the VPD role marker the guide documents, which pcilibs-rs reads, and the plugin stays a sysfs reader and binds nothing. The fabric is advertised only once every switch function on the node is bound, so a Service VM never starts with part of one. Inside the VM, NVRC selects the H100-style or ConnectX-style fabric services from the same pcilibs-rs discovery, so one Service VM image and one pod spec serve both platforms. GPU partition activation then flows through Fabric Manager's API as in the guide.

## Fabric partitions

In shared NVSwitch mode Fabric Manager in the Service VM activates GPU partitions, and a tenant VM must receive exactly one active partition's GPUs: GPUs that share an active partition reach each other over NVLink, so this is isolation, not convenience. Fabric Manager names a GPU by its physical ID, the module slot on the baseboard, never by PCI address; the host names it by PCI address. The plugin bridges the two through the firmware's slot label on the PCI function (`/sys/bus/pci/devices/<bdf>/label`, "GPU0, SMX7" on DGX systems), verified against Fabric Manager's own records on a DGX NVL8; a GPU without a parsable label resolves to no partition, so an unknown board advertises nothing rather than a wrong partition. The shared schema and the resolution live in pcilibs-rs (`fabric`), used by the plugin and by the Fabric Manager client that publishes.

**The view.** The plugin cannot read Fabric Manager, by design. The Fabric Manager client next to it publishes the partition lists over a gRPC stream to the plugin's view service. The open stream is the view's liveness: a closed stream withdraws it after a grace period. Only the fabric holder may publish: the fabric's Allocate injects an endpoint and a freshly minted token into that container's environment, the TCP listener admits only that token, and a unix socket on the host admits a publisher there without one, its permissions being the credential. A newer stream replaces the current one.

**The unit.** An active partition is advertised as its own extended resource named by size, `nvidia.com/nvlink-4gpu`, one device holding every function of every member GPU. A count under `nvidia.com/gpu` was rejected for partitions because it is ambiguous, four NVLinked GPUs read the same as four standalone ones, and only a per-shape ledger lets the scheduler see how many partitions remain. This is not the aggregate the plugin refuses to invent below: the fabric owner defines the partition in the Service VM and it has exactly one valid consumption shape. A single-GPU partition is a plain `nvidia.com/gpu`. A partition is advertised only when active, supported by Fabric Manager, exclusive (no other active partition names one of its GPUs) and complete (every member bound and labeled on this host).

**What the node advertises.**

| node | view | unit | GPUs |
|---|---|---|---|
| fabric not VFIO-bound (host Fabric Manager, or no fabric) | none | | one device each, as before |
| fabric VFIO-bound (a Service VM owns it) | none | | none, until the Service VM publishes |
| any | present | `partition` or unset | one device per advertisable partition |
| any | present | `gpu` | one device each: the owner holds the whole node and says so |

The view always wins; without one, the fabric's binding decides. The unit word travels in the view, so the owner of the fabric, the infrastructure or a colocation tenant, chooses it in their own Service VM pod spec.

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

The unit of advertisement is the smallest thing a consumer can take. For GPUs that is the PCI device: a multi-function device is one device whose CDI entry carries every function, because no consumer can take a function without its siblings. The NVSwitch fabric goes one step further and is one device per node, spanning the four switch chips on H100/H200 or the management ConnectX's PFs on B200/B300, because it has exactly one consumer, the Service VM, and exactly one valid shape, all of it. A subset of a fabric is never a consumption choice the layer above could make, which is what separates it from GPUs, whose shapes are many; grouping devices that have more than one valid consumption shape stays out.

### Why the plugin does not bind VFIO devices

Binding a device is a reconfiguration of the infrastructure. The workload cluster is read-only toward the infrastructure; reconfiguration authority lives in the admin cluster. Placing a component that can bind devices inside the workload cluster would put infrastructure control next to the untrusted workloads it is meant to contain (boundary document, ADR 1000). The device plugin only declares what already exists.

### Why no privileged pods

The isolation primitive for untrusted workloads is the VM or CVM boundary, not pod privilege. A privileged DaemonSet in the workload cluster is a standing high-privilege target next to untrusted workloads. Deleting the capability is stronger than fencing it. The device plugin reads `/dev/vfio/` to enumerate devices (read-only host path mount) and writes to the kubelet socket directory — both are narrow, named mounts, not broad privilege (ADR 10000).

## Cluster shape

This plugin runs in the **workload cluster**, which is read-only toward the infrastructure. It never runs in the admin cluster. The admin cluster holds the components that bind VFIO devices. NVLink fabric configuration runs in the Service VM, a Kata pod in the workload cluster that holds the node's `nvidia.com/nvswitch-fabric` device.

```text
Admin cluster                        Workload cluster
─────────────────────────────        ──────────────────────────────────
GPU Operator (driver, MIG, fabric)   kata-device-plugin (declares only)
VFIO binder                          Kata VMs
                                     Workload pods requesting nvidia.com/gpu
                                     Service VM pod requesting nvidia.com/nvswitch-fabric
```
