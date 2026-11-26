//! Integration tests for the fabric: the NVSwitch fabric as one device,
//! what its Allocate hands the Service VM, and the partitions a published
//! view turns into devices.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use container_device_interface::specs::config::Spec;
use kata_device_plugin::dp::v1beta1::{
    AllocateRequest, ContainerAllocateRequest, ContainerAllocateResponse,
};
use kata_device_plugin::plugin::DeviceServer;
use kata_device_plugin::vfio::Naming;
use kata_device_plugin::view::View;
use kata_fabric_api::v1;
use kata_fabric_api::{ENDPOINT_ENV, TOKEN_ENV};
use pcilibs_rs::testfs::{add as add_dev, add_connectx_cdev};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Status};

use common::*;

#[tokio::test]
async fn management_pfs_are_served_as_one_nvswitch_fabric() {
    // HGX B200/B300: the NVSwitches are reached through the management
    // ConnectX, whose PFs are what the Service VM is given.  Two of the four
    // carry the VPD marker, all four belong, and they are one fabric, so the
    // Service VM requests nvidia.com/nvswitch-fabric: 1 and gets every
    // function.  A data-path ConnectX of the same vendor and class,
    // VFIO-bound for some other purpose, must not be advertised.
    let vfio = fake_vfio(2);
    for (n, function) in [(2, 0), (3, 1), (4, 2), (5, 3)] {
        add_connectx_cdev(
            vfio.path(),
            n,
            &format!("0000:05:00.{function}"),
            function < 2,
        );
    }
    add_connectx_cdev(vfio.path(), 6, "0000:18:00.0", false);

    let owner = published(fabric_view("gpu", &[])).await;
    let node = Node::start_with_view(
        vfio,
        &["nvidia.com/gpu", "nvidia.com/nvswitch-fabric"],
        Naming::Alias,
        Some(owner.view.clone()),
    )
    .await;

    let (_gpu_stream, gpus) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(gpus.devices.len(), 2);
    let (_fabric_stream, fabric) = node.snapshot("kata-nvswitch-fabric.sock").await;
    assert_eq!(fabric.devices.len(), 1);

    // The spec's shape, one CDI device with a node per function, is the
    // cdi module's test; here only that each spec holds its own cdevs.
    let sw = node.spec("kata.nvidia.com-nvswitch-fabric.yaml").unwrap();
    assert!(sw.contains("devices/vfio2") && sw.contains("devices/vfio5"));
    assert!(!sw.contains("devices/vfio6"));
    assert!(!node
        .spec("kata.nvidia.com-gpu.yaml")
        .unwrap()
        .contains("devices/vfio2"));

    let resp = node
        .client("kata-nvswitch-fabric.sock")
        .await
        .allocate(Request::new(AllocateRequest {
            container_requests: vec![ContainerAllocateRequest {
                devices_i_ds: vec!["0000:05:00".to_owned()],
            }],
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        resp.container_responses[0].cdi_devices[0].name,
        "nvidia.com/nvswitch-fabric=0000:05:00"
    );

    node.shutdown().await;
    owner.stop.cancel();
}

/// A server on `name` over `vfio`, given the view, serving without a kubelet.
async fn server_with_view(
    name: &str,
    vfio: &TempDir,
    sockets: &TempDir,
    view: &Arc<View>,
    endpoint: Option<&str>,
) -> (
    CancellationToken,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let cdi = TempDir::new().unwrap();
    let server = DeviceServer::new(
        name,
        Naming::Alias,
        vfio.path().to_str().unwrap(),
        vfio.path().to_str().unwrap(),
        sockets.path().to_str().unwrap(),
        cdi.path().to_str().unwrap(),
    )
    .with_view(view.clone(), endpoint.map(str::to_owned));
    let token = CancellationToken::new();
    let handle = tokio::spawn(server.run(token.clone()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::mem::forget(cdi);
    (token, handle)
}

async fn allocate(sock: PathBuf, ids: &[&str]) -> Result<Vec<ContainerAllocateResponse>, Status> {
    unix_client(sock)
        .await
        .allocate(Request::new(AllocateRequest {
            container_requests: vec![ContainerAllocateRequest {
                devices_i_ds: ids.iter().map(|id| id.to_string()).collect(),
            }],
        }))
        .await
        .map(|response| response.into_inner().container_responses)
}

/// The fabric at slot 0000:05:00, as every test here lays it out.
async fn allocate_fabric(sock: PathBuf) -> ContainerAllocateResponse {
    allocate(sock, &["0000:05:00"]).await.unwrap().remove(0)
}

#[tokio::test]
async fn fabric_allocate_hands_its_holder_endpoint_and_a_fresh_token() {
    // The Service VM must learn where to publish and with which token, and
    // only it: a GPU's holder learns nothing.  Each fabric allocation rotates
    // the token so the previous holder's copy stops working.
    let vfio = TempDir::new().unwrap();
    for (n, function, marker) in [(1, 0, true), (3, 1, true), (4, 2, false), (5, 3, false)] {
        add_connectx_cdev(vfio.path(), n, &format!("0000:05:00.{function}"), marker);
    }
    add_dev(vfio.path(), 2, 0x10de, 0x2330, 0x030200);
    let state = TempDir::new().unwrap();
    let view = View::load(state.path()).unwrap();
    let before = view.token();
    let sockets = TempDir::new().unwrap();

    let (fabric_stop, fabric) = server_with_view(
        "nvidia.com/nvswitch-fabric",
        &vfio,
        &sockets,
        &view,
        Some("http://10.0.0.7:9877"),
    )
    .await;
    let resp = allocate_fabric(sockets.path().join("kata-nvswitch-fabric.sock")).await;
    assert_eq!(
        resp.cdi_devices[0].name,
        "nvidia.com/nvswitch-fabric=0000:05:00"
    );
    let token = view.token();
    assert_ne!(
        token, before,
        "the token rotates on every fabric allocation"
    );
    assert_eq!(resp.envs[TOKEN_ENV], token);
    assert_eq!(resp.envs[ENDPOINT_ENV], "http://10.0.0.7:9877");
    assert_eq!(resp.envs.len(), 2);

    let again = allocate_fabric(sockets.path().join("kata-nvswitch-fabric.sock")).await;
    assert_ne!(again.envs[TOKEN_ENV], token);
    assert_eq!(again.envs[TOKEN_ENV], view.token());

    // A GPU's holder learns nothing, with or without a node address.  The
    // fabric is bound, so the GPU exists only under a view: a second View
    // instance over its own state publishes unit gpu for it.
    let owner = published(fabric_view("gpu", &[])).await;
    let (gpu_stop, gpu) =
        server_with_view("nvidia.com/gpu", &vfio, &sockets, &owner.view, None).await;
    let resp = allocate(sockets.path().join("kata-gpu.sock"), &["0000:02:00"])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(resp.cdi_devices[0].name, "nvidia.com/gpu=0000:02:00");
    assert!(resp.envs.is_empty());
    assert_eq!(view.token(), again.envs[TOKEN_ENV]);
    owner.stop.cancel();

    // Without a node address the holder still gets the token.
    fabric_stop.cancel();
    fabric.await.unwrap().unwrap();
    let (fabric_stop, fabric) =
        server_with_view("nvidia.com/nvswitch-fabric", &vfio, &sockets, &view, None).await;
    let resp = allocate_fabric(sockets.path().join("kata-nvswitch-fabric.sock")).await;
    assert_eq!(resp.envs.len(), 1);
    assert_eq!(resp.envs[TOKEN_ENV], view.token());

    for (stop, handle) in [(fabric_stop, fabric), (gpu_stop, gpu)] {
        stop.cancel();
        handle.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn a_failed_or_empty_allocate_keeps_the_holders_token() {
    // The token rotates only when the fabric changes hands.  A request for
    // a device that is gone fails, and one that asks for nothing hands the
    // fabric to nobody; neither revokes the current holder's token.
    let vfio = TempDir::new().unwrap();
    for (n, function, marker) in [(0, 0, true), (1, 1, true), (2, 2, false), (3, 3, false)] {
        add_connectx_cdev(vfio.path(), n, &format!("0000:05:00.{function}"), marker);
    }
    let state = TempDir::new().unwrap();
    let view = View::load(state.path()).unwrap();
    let sockets = TempDir::new().unwrap();
    let (stop, server) =
        server_with_view("nvidia.com/nvswitch-fabric", &vfio, &sockets, &view, None).await;
    let sock = sockets.path().join("kata-nvswitch-fabric.sock");

    let holder = allocate_fabric(sock.clone()).await.envs[TOKEN_ENV].clone();
    assert_eq!(holder, view.token());

    let status = allocate(sock.clone(), &["0000:06:00"]).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::NotFound);
    assert_eq!(view.token(), holder);

    let empty = allocate(sock.clone(), &[]).await.unwrap().remove(0);
    assert!(empty.cdi_devices.is_empty() && empty.envs.is_empty());
    assert_eq!(view.token(), holder);

    // Two containers in one request, one of them taking the fabric: only
    // that one gets the environment.
    let mut client = unix_client(sock).await;
    let both = client
        .allocate(Request::new(AllocateRequest {
            container_requests: vec![
                ContainerAllocateRequest {
                    devices_i_ds: vec!["0000:05:00".to_owned()],
                },
                ContainerAllocateRequest {
                    devices_i_ds: vec![],
                },
            ],
        }))
        .await
        .unwrap()
        .into_inner()
        .container_responses;
    assert_eq!(both[0].envs[TOKEN_ENV], view.token());
    assert!(both[1].cdi_devices.is_empty() && both[1].envs.is_empty());

    stop.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn active_partitions_are_served_as_nvlink_units() {
    // Four labeled GPUs and a bound fabric, carved by the Service VM into two
    // pairs: the kubelet sees two nvidia.com/nvlink-2gpu devices and no
    // plain GPU, each unit's CDI entry carries both GPUs' functions, and
    // Allocate names the unit.
    let vfio = TempDir::new().unwrap();
    for (n, address, module) in [
        (0, "0000:1a:00.0", 7),
        (1, "0000:3c:00.0", 6),
        (2, "0000:62:00.0", 8),
        (3, "0000:73:00.0", 5),
    ] {
        add_labeled_gpu(vfio.path(), n, address, module);
    }
    for n in 4..8u32 {
        add_dev(vfio.path(), n, 0x10de, 0x22a3, 0x068000);
    }
    let pairs: &[(u32, bool, &[u32])] = &[(6, true, &[5, 6]), (7, true, &[7, 8])];
    let owner = published(fabric_view("partition", pairs)).await;
    let node = Node::start_with_view(
        vfio,
        &[
            "nvidia.com/nvlink-2gpu",
            "nvidia.com/gpu",
            "nvidia.com/nvswitch-fabric",
        ],
        Naming::Alias,
        Some(owner.view.clone()),
    )
    .await;

    let (_units, units) = node.snapshot("kata-nvlink-2gpu.sock").await;
    assert_eq!(units.devices.len(), 2);
    let (_gpus, gpus) = node.snapshot("kata-gpu.sock").await;
    assert!(gpus.devices.is_empty());
    let (_fabric, fabric) = node.snapshot("kata-nvswitch-fabric.sock").await;
    assert_eq!(fabric.devices.len(), 1);

    let spec: Spec =
        serde_saphyr::from_str(&node.spec("kata.nvidia.com-nvlink-2gpu.yaml").unwrap()).unwrap();
    assert_eq!(spec.kind, "nvidia.com/nvlink-2gpu");
    let cdevs = |i: usize| -> Vec<String> {
        spec.devices[i]
            .container_edits
            .device_nodes
            .as_ref()
            .unwrap()
            .iter()
            .map(|node| node.path.rsplit('/').next().unwrap().to_owned())
            .collect()
    };
    // Partition 6 is GPUs 5 and 6, partition 7 is 7 and 8, in FM's order.
    assert_eq!(spec.devices[0].name, "p6");
    assert_eq!(cdevs(0), ["vfio3", "vfio1"]);
    assert_eq!(spec.devices[1].name, "p7");
    assert_eq!(cdevs(1), ["vfio0", "vfio2"]);

    let resp = node
        .client("kata-nvlink-2gpu.sock")
        .await
        .allocate(Request::new(AllocateRequest {
            container_requests: vec![ContainerAllocateRequest {
                devices_i_ds: vec!["p7".to_owned()],
            }],
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        resp.container_responses[0].cdi_devices[0].name,
        "nvidia.com/nvlink-2gpu=p7"
    );
    assert!(resp.container_responses[0].envs.is_empty());

    node.shutdown().await;
    owner.stop.cancel();
}

#[tokio::test]
async fn an_incomplete_partition_is_served_unhealthy() {
    // GPU 6 is not on this host, so pair 6 is Unhealthy beside the healthy
    // pair 7: the kubelet counts it in capacity and not in allocatable, the
    // CDI spec names only the healthy unit, and Allocate refuses its ID.
    let vfio = TempDir::new().unwrap();
    for (n, address, module) in [
        (0, "0000:1a:00.0", 7),
        (2, "0000:62:00.0", 8),
        (3, "0000:73:00.0", 5),
    ] {
        add_labeled_gpu(vfio.path(), n, address, module);
    }
    for n in 4..8u32 {
        add_dev(vfio.path(), n, 0x10de, 0x22a3, 0x068000);
    }
    let pairs: &[(u32, bool, &[u32])] = &[(6, true, &[5, 6]), (7, true, &[7, 8])];
    let owner = published(fabric_view("partition", pairs)).await;
    let node = Node::start_with_view(
        vfio,
        &["nvidia.com/nvlink-2gpu", "nvidia.com/nvswitch-fabric"],
        Naming::Alias,
        Some(owner.view.clone()),
    )
    .await;

    let (_units, units) = node.snapshot("kata-nvlink-2gpu.sock").await;
    let health: Vec<(&str, &str)> = units
        .devices
        .iter()
        .map(|d| (d.id.as_str(), d.health.as_str()))
        .collect();
    assert_eq!(health, [("p6", "Unhealthy"), ("p7", "Healthy")]);

    let spec: Spec =
        serde_saphyr::from_str(&node.spec("kata.nvidia.com-nvlink-2gpu.yaml").unwrap()).unwrap();
    assert_eq!(spec.devices.len(), 1);
    assert_eq!(spec.devices[0].name, "p7");

    let err = node
        .client("kata-nvlink-2gpu.sock")
        .await
        .allocate(Request::new(AllocateRequest {
            container_requests: vec![ContainerAllocateRequest {
                devices_i_ds: vec!["p6".to_owned()],
            }],
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);

    node.shutdown().await;
    owner.stop.cancel();
}

#[tokio::test]
async fn a_partition_that_stops_being_supported_goes_unhealthy_and_loses_its_cdi_name() {
    // The cdevs behind the unit do not change, only its health, so the key
    // the poller compares must carry the health: the kubelet gets a new list
    // with the unit Unhealthy and the CDI spec no longer names it.
    let vfio = TempDir::new().unwrap();
    for (n, address, module) in [
        (0, "0000:1a:00.0", 7),
        (1, "0000:3c:00.0", 6),
        (2, "0000:62:00.0", 8),
        (3, "0000:73:00.0", 5),
    ] {
        add_labeled_gpu(vfio.path(), n, address, module);
    }
    for n in 4..8u32 {
        add_dev(vfio.path(), n, 0x10de, 0x22a3, 0x068000);
    }
    let pairs: &[(u32, bool, &[u32])] = &[(6, true, &[5, 6]), (7, true, &[7, 8])];
    let owner = published(fabric_view("partition", pairs)).await;
    let node = Node::start_with_view(
        vfio,
        &["nvidia.com/nvlink-2gpu", "nvidia.com/nvswitch-fabric"],
        Naming::Alias,
        Some(owner.view.clone()),
    )
    .await;

    let (mut stream, first) = node.snapshot("kata-nvlink-2gpu.sock").await;
    assert!(first.devices.iter().all(|d| d.health == "Healthy"));
    assert!(node
        .spec("kata.nvidia.com-nvlink-2gpu.yaml")
        .unwrap()
        .contains("name: p7"));

    let mut unsupported = fabric_view("partition", pairs);
    unsupported.unsupported = Some(v1::UnsupportedPartitionList {
        version: 1,
        partition_info: vec![v1::UnsupportedPartition {
            partition_id: 7,
            gpu_physical_ids: vec![7, 8],
        }],
    });
    owner.publish(unsupported).await;

    let updated = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("timed out waiting for the health change")
        .unwrap()
        .expect("stream closed");
    let health: Vec<(&str, &str)> = updated
        .devices
        .iter()
        .map(|d| (d.id.as_str(), d.health.as_str()))
        .collect();
    assert_eq!(health, [("p6", "Healthy"), ("p7", "Unhealthy")]);
    let spec = node.spec("kata.nvidia.com-nvlink-2gpu.yaml").unwrap();
    assert!(spec.contains("name: p6") && !spec.contains("name: p7"));

    node.shutdown().await;
    owner.stop.cancel();
}
