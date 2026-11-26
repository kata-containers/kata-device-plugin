# fmclient

A command-line client for the NVIDIA Fabric Manager SDK. Manages GPU fabric
partitions, queries NVLink status, and publishes the partition view to
kata-device-plugin. Connects to Fabric Manager over TCP or a Unix domain
socket.

## Building

From the repository root:

```sh
cargo build --release -p fmclient
```

The build needs no Fabric Manager SDK: `libnvfm.so.1` is loaded when the
program starts, from the `nvidia-fabricmanager-dev-<VERSION>` package or
wherever the loader finds it, and a missing library is one error at startup.
The test that touches the SDK runs only where it is installed.

### With Docker

```sh
docker build -f crates/fmclient/Dockerfile -t fmclient .
```

The FM version defaults to 590 and must match Fabric Manager in the guest
image, or every connect fails with a version mismatch. Override with:

```sh
docker build -f crates/fmclient/Dockerfile --build-arg FM_VERSION=570 -t fmclient .
```

## Usage

```text
fmclient [OPTIONS]

Options:
  -l                                 List supported fabric partitions
  -a <ID>                            Activate a fabric partition
  -d <ID>                            Deactivate a fabric partition
  -i, --hostname <HOST>              Hostname or IP address of Fabric Manager [default: 127.0.0.1]
  -s, --unix-domain-socket <PATH>    Unix domain socket path for Fabric Manager
  -g, --get-nvlink-failed-devices    Query all NVLink failed devices
  -u, --list-unsupported-partitions  List unsupported fabric partitions
  -p, --set-activated-list <IDS>     Set activated partition list (comma-separated)
      --activate-with-vfs <ID=BDFs>  Activate a partition with SR-IOV VFs (FABRIC_MODE=2)
      --publish                      Publish the partition view to kata-device-plugin
      --publish-to <ENDPOINT>        Endpoint to publish to: http://HOST:PORT
      --unit <UNIT>                  partition or gpu [default: partition]
  -o, --output <FMT>                 Output format: table or json [default: table]
  -t, --timeout <SECS>               Timeout to wait for Fabric Manager [default: 30]
```

### Examples

```sh
# List partitions as a table
fmclient -l
# Partitions (15 supported, max 15):
#   ID  Active  GPUs  NVLinks  Rate (MB/s)  GPU IDs
#    0      no     8   18/18         26562  1,2,3,4,5,6,7,8
#    1      no     4   18/18         26562  1,2,3,4
#    2      no     4   18/18         26562  5,6,7,8
#    3      no     2   18/18         26562  1,3
#    4      no     2   18/18         26562  2,4
#    5      no     2   18/18         26562  5,7
#    6      no     2   18/18         26562  6,8
#    7      no     1    0/0          26562  1
#    8      no     1    0/0          26562  2
#  ...

# List partitions as JSON.  The schema is kata.fabric.v1 from kata-fabric-api,
# the same types this client publishes to kata-device-plugin, with fmpm's
# field names: partitionInfo[].partitionId, isActive, gpuInfo[].physicalId.
# Only JSON reaches stdout; libnvfm's own log lines go to stderr.
fmclient -l -o json

# Activate partition 3
fmclient -a 3

# Connect via Unix socket
fmclient -s /var/run/nvidia-fabricmanager/fm.sock -l

# Wait up to 60s for FM to become available
fmclient -t 60 -l

# Fail immediately if FM is not available
fmclient -t 0 -l
```

## Publishing the view to kata-device-plugin

In shared NVSwitch mode the plugin on the host advertises each active
partition as a schedulable unit. It learns the partitions from this client
in one gRPC call and keeps that view until the next call or until the fabric
changes hands. Every command that changes partitions (`-a`, `-d`,
`--activate-with-vfs`, `--set-activated-list`) publishes the result when it
knows where the plugin is; `--publish` alone resends the current view.

```sh
# In the Service VM container.  The plugin injected the endpoint and token
# into this container's environment when it allocated the fabric.
fmclient -a 2
fmclient --publish

# Anywhere else, name the endpoint; the token still comes from the
# environment.
fmclient --publish --publish-to http://10.0.0.7:9877

# Tell the plugin to hand out single GPUs instead of whole partitions.
fmclient --publish --unit gpu
```

Log lines go to stderr. A publish that fails after a partition changed exits
non-zero: the change is in Fabric Manager and the node does not know yet; run
`--publish` again.

## Deploying as the Service VM

`deploy/service-vm.yaml` is the Service VM pod spec: a DaemonSet over the
nodes whose fabric the provisioner bound, running this image under the Kata
runtime class whose guest carries Fabric Manager, requesting
`nvidia.com/nvswitch-fabric: 1`; its container idles. The plugin injects
the endpoint and token when it allocates the fabric. Activate partitions
with `kubectl exec ... -- fmclient -a <id>`, which publishes the view; the
plugin advertises each active partition within one poll.
`fmclient --publish --unit=gpu` has the plugin hand out single GPUs instead
of whole partitions.

## Running in Kata Containers

With Kata Containers, the container runs inside a lightweight VM (the guest).
Fabric Manager runs in the guest VM, not in the container itself, and the
container shares the guest's network namespace, so the default
`FM_CMD_BIND_INTERFACE=127.0.0.1` works from inside the container:

```sh
sudo nerdctl run --runtime io.containerd.kata.v2 \
  --device /dev/vfio/devices/vfio8 \
  --device /dev/vfio/devices/vfio9 \
  --device /dev/vfio/devices/vfio10 \
  --device /dev/vfio/devices/vfio11 \
  -it fm-image

# Inside the container:
fmclient -l
```

TCP port forwarding from the host does not work: FM's `FM_CMD_BIND_INTERFACE`
accepts a specific IPv4 address, not `0.0.0.0`, and defaults to `127.0.0.1`,
so it listens on the guest's loopback only. The host has no path into
Fabric Manager; the Kubernetes API is the control plane, through
`kubectl exec` into the Service VM.

## Fabric Manager Configuration Reference

| FM config key             | Required for                                     |
| ------------------------- | ------------------------------------------------ |
| `FM_CMD_BIND_INTERFACE`   | TCP connections (`--hostname`)                   |
| `FM_CMD_UNIX_SOCKET_PATH` | Unix socket connections (`--unix-domain-socket`) |
| `FABRIC_MODE_RESTART`     | `--set-activated-list` (resiliency mode)         |
