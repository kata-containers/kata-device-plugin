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

The chart's generated [`index.json`](deploy/helm/kata-device-plugin/index.json)
lists those rows for tools that need the resource names and PCI matches.

A resource is advertised iff matching devices are VFIO-bound — declared by
the node, not configured. Supporting a new device type is one table row; no
other code changes.

The plugin declares supply only. How devices are consumed, as a whole tray
(`nvidia.com/gpu: 4` on a GB200 compute tray) or as a subset, is a scheduling
decision expressed in pod specs, node labels, and taints; the plugin never
aggregates or partitions what the node declares.

For each present resource the plugin:

1. writes a host CDI spec mapping device indices to IOMMUFD cdev paths —
   `/var/run/cdi/kata.nvidia.com-gpu.yaml` for `nvidia.com/gpu`,
2. registers with the kubelet over its unix socket and serves the device
   plugin v1beta1 API,
3. answers `Allocate` with CDI device names (`nvidia.com/gpu=0`) — the
   container runtime resolves them against the host CDI registry, and the
   Kata shim wires the cold-plugged VM devices to the right container.

No config file, no Kubernetes API access, and a single flag,
`--resource-naming=alias|sku`, which picks between the table names and
hardware-identity names like `nvidia.com/GH100_H100_SXM5_80GB`. Everything
else is a kernel, kubelet, or CDI contract expressed as a constant. The
device set is rescanned periodically, so newly bound devices show up
without restarting the pod.

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

Releases publish a multi-arch image (amd64, arm64) and the Helm chart to
ghcr.io. Set `VERSION` to one of the
[releases](https://github.com/kata-containers/kata-device-plugin/releases)
and pin it with `--version`, which is also the only way to get a
pre-release such as `0.2.0-rc.0` since Helm skips those otherwise:

```sh
VERSION=0.2.0
helm install kata-device-plugin \
  oci://ghcr.io/kata-containers/kata-device-plugin-charts/kata-device-plugin \
  --version "${VERSION}" -n kube-system
```

From a checkout, `make deploy` installs the local chart instead.

The image and chart are signed keylessly with cosign by the release
workflow, and both carry GitHub build provenance, so either can be checked
before it gets anywhere near a node. The chart's provenance is recorded
against its tarball, which is why it's verified after a `helm pull`:

```sh
IDENTITY=https://github.com/kata-containers/kata-device-plugin/.github/workflows/release.yaml@refs/heads/main
ISSUER=https://token.actions.githubusercontent.com

cosign verify ghcr.io/kata-containers/kata-device-plugin:v${VERSION} \
  --certificate-identity "${IDENTITY}" --certificate-oidc-issuer "${ISSUER}"
gh attestation verify oci://ghcr.io/kata-containers/kata-device-plugin:v${VERSION} \
  --repo kata-containers/kata-device-plugin

cosign verify ghcr.io/kata-containers/kata-device-plugin-charts/kata-device-plugin:${VERSION} \
  --certificate-identity "${IDENTITY}" --certificate-oidc-issuer "${ISSUER}"
helm pull oci://ghcr.io/kata-containers/kata-device-plugin-charts/kata-device-plugin \
  --version "${VERSION}"
gh attestation verify "kata-device-plugin-${VERSION}.tgz" \
  --repo kata-containers/kata-device-plugin
```

The chart is the only deployment model.  It exposes only what varies per
cluster (image, nodeSelector, tolerations, resources, resource naming,
log filter); the security context and hostPath mounts are contracts, not
configuration, and are fixed in the template.

The chart includes `index.json` so consumers can read the resource names,
supported naming modes, and default chart image from a pinned chart release.
`aliasResources` lists the fixed names. In `sku` mode, the names come from the
PCI ID database at runtime and can fall back to an alias when a device ID is
unknown. The index therefore does not enumerate SKU names. The generated
`values.schema.json` checks `resourceNaming` when Helm reads the chart.

Both files are generated from the Rust resource table and the chart metadata.
After changing either source, run:

```sh
cargo run --locked --example generate-resource-contract
```

The Static checks workflow runs this command on every pull request and fails
if the committed files differ. The Release workflow checks them again before
publishing.

The DaemonSet mounts three host paths: the kubelet device-plugin socket
directory, `/dev/vfio` (read-only), and `/var/run/cdi`. It runs as uid 0
(pinned with `runAsUser: 0`) with every capability dropped and a read-only
root filesystem — root is required only because the kubelet owns its
socket directory.

## Releasing

Bump the version in `Cargo.toml` and in both `version` and `appVersion` of
the chart's `Chart.yaml`, merge that, and then dispatch the Release workflow
on `main`. It builds, signs, and attests the image and the chart, and only
then creates the tag and GitHub release, marking it as a pre-release (and
leaving `:latest` alone) whenever the version has a `-` suffix.

## See also

- [ARCHITECTURE.md](ARCHITECTURE.md) — scope, rationale, cluster shape
- [CLAUDE.md](CLAUDE.md) / [AGENTS.md](AGENTS.md) — contributor and agent guidance
