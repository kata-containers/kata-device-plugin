# fmclient

A command-line client for the NVIDIA Fabric Manager SDK. Manages GPU fabric
partitions, queries NVLink status, and supports TCP, Unix domain socket, and
vsock connections to Fabric Manager.

## Building

### With Cargo (requires `libnvfm` and headers)

```sh
cargo build --release
```

The binary links against `libnvfm.so` from the
`nvidia-fabricmanager-dev-<VERSION>` package.

### With Docker

```sh
docker build -t fmclient .
```

The FM driver version defaults to 590. Override with:

```sh
docker build --build-arg FM_VERSION=570 -t fmclient .
```

## Usage

```
fmclient [OPTIONS]

Options:
  -l                                 List supported fabric partitions
  -a <ID>                            Activate a fabric partition
  -d <ID>                            Deactivate a fabric partition
  -i, --hostname <HOST>              Hostname or IP address of Fabric Manager [default: 127.0.0.1]
  -s, --unix-domain-socket <PATH>    Unix domain socket path for Fabric Manager
  -v, --vsock <CID:PORT>             vsock address for host-to-guest VM communication
  -g, --get-nvlink-failed-devices    Query all NVLink failed devices
  -u, --list-unsupported-partitions  List unsupported fabric partitions
  -p, --set-activated-list <IDS>     Set activated partition list (comma-separated)
  -o, --output <FMT>                 Output format: table or json [default: table]
  -t, --timeout <SECS>              Timeout to wait for Fabric Manager [default: 30]
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

# List partitions as JSON.  The schema is kata.fabric.v1 from pcilibs-rs,
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

# On the host over vsock the plugin's unix socket is the default endpoint;
# no token there, the socket's permissions decide.
fmclient --vsock -a 2

# Anywhere else, name the endpoint.
fmclient --publish --publish-to unix:///var/run/kata-device-plugin/fabric-view.sock

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
in-band with `kubectl exec ... -- fmclient -a <id>`, which publishes the
view; the plugin advertises each active partition within one poll.
`fmclient --publish --unit=gpu` has the plugin hand out single GPUs instead
of whole partitions.

## Running in Kata Containers / Service VMs

With Kata Containers, the container runs inside a lightweight VM (the guest).
Fabric Manager runs in the guest VM, not in the container itself. The
container only has network access to the guest via the virtual network
interface.

### Connecting from the host

TCP port forwarding (`-p`) from the host does **not** work with Kata
Containers. FM's `FM_CMD_BIND_INTERFACE` only accepts a specific IPv4 address,
not `0.0.0.0` (wildcard). Since FM defaults to `127.0.0.1`, it only listens
on the guest's loopback. Port-forwarded traffic arrives on the guest's `eth0`
and gets rejected.

For host-to-guest communication, use **vsock** (see below) or run `fmclient`
inside the container.

### Connecting from inside the container

The container shares the guest's network namespace, so the default
`FM_CMD_BIND_INTERFACE=127.0.0.1` works:

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

### vsock (Virtual Socket)

vsock provides a direct communication channel between the host and a guest VM
without any network configuration. It uses two identifiers:

- **CID (Context Identifier)** -- an address for each endpoint:
  - CID `2` is the host
  - CID `3+` are guest VMs (assigned by the hypervisor)
  - CID `4294967295` (`VMADDR_CID_ANY`) means "accept from any CID"
- **Port** -- identifies the service, like a TCP port. FM defaults to
  `1447971919`.

#### Guest FM configuration

The following must be set in the guest VM's `/etc/nvidia-fabricmanager/fm.cfg`
(baked into the guest rootfs or initrd):

```
FM_CMD_VSOCK_CID=4294967295
FM_CMD_VSOCK_PORT_NUMBER=1447971919
```

`VMADDR_CID_ANY` (`4294967295`) allows FM to accept vsock connections from
any source, including the host.

#### The Service VM over vsock (out-of-band)

With kata runtime-rs the Service VM gets a fixed CID through the
`io.katacontainers.config.hypervisor.vsock_cid` annotation, and NVRC turns
FM's vsock listener on when the kernel command line carries
`nvrc.fm.vsock=on`. `deploy/service-vm-out-of-band.yaml` sets both; its
container only holds the fabric, the host does the rest. The runtime-rs
NVIDIA shim has no shared filesystem, so its pods need containerd's erofs
snapshotter, which kata-deploy's `try-kata-nvidia-gpu.values.yaml` profile
configures (`erofs-utils` on the host).

```sh
# partitions, from the host; each change is published to the plugin's host
# socket, where the socket's permissions decide and no token is needed
fmclient --vsock -l
fmclient --vsock -a 2
```

`--vsock` without a value is the contract address: pcilibs-rs
`fabric::SERVICE_VM_CID` (3, the first CID kata reserves for configured VMs)
and FM's default port. Give `CID:PORT` for anything else.

A vsock connect needs no privilege on the host, so with the listener on every
host process can reach the FM API. That is the provider-owned model; where the
host must have no Fabric Manager control path, run the in-band manifest, whose
guest opens nothing.

## Fabric Manager Configuration Reference

| FM config key               | Required for                              |
|-----------------------------|-------------------------------------------|
| `FM_CMD_BIND_INTERFACE`     | TCP connections (`--hostname`)            |
| `FM_CMD_UNIX_SOCKET_PATH`   | Unix socket connections (`--unix-domain-socket`) |
| `FM_CMD_VSOCK_CID`          | vsock connections (`--vsock`)             |
| `FM_CMD_VSOCK_PORT_NUMBER`  | vsock connections (`--vsock`)             |
| `FABRIC_MODE_RESTART`       | `--set-activated-list` (resiliency mode)  |
