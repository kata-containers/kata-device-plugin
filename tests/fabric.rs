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
use kata_device_plugin::view::{View, GRACE};
use pcilibs_rs::fabric::{ENDPOINT_ENV, TOKEN_ENV};
use pcilibs_rs::testfs::{add as add_dev, add_connectx};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tonic::Request;

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
        add_connectx(
            vfio.path(),
            n,
            &format!("0000:05:00.{function}"),
            function < 2,
        );
    }
    add_connectx(vfio.path(), 6, "0000:18:00.0", false);

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

    let spec: Spec =
        serde_saphyr::from_str(&node.spec("kata.nvidia.com-nvswitch-fabric.yaml").unwrap())
            .unwrap();
    assert_eq!(spec.devices.len(), 1);
    let nodes = spec.devices[0]
        .container_edits
        .device_nodes
        .as_ref()
        .unwrap();
    let cdevs: Vec<&str> = nodes
        .iter()
        .map(|node| node.path.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(cdevs, ["vfio2", "vfio3", "vfio4", "vfio5"]);
    assert!(!node
        .spec("kata.nvidia.com-gpu.yaml")
        .unwrap()
        .contains("devices/vfio2"));

    let resp = node
        .client("kata-nvswitch-fabric.sock")
        .await
        .allocate(Request::new(AllocateRequest {
            container_requests: vec![ContainerAllocateRequest {
                devices_i_ds: vec!["0".to_owned()],
            }],
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        resp.container_responses[0].cdi_devices[0].name,
        "nvidia.com/nvswitch-fabric=0"
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
    advertise: Option<&str>,
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
    .with_view(view.clone(), advertise.map(str::to_owned));
    let token = CancellationToken::new();
    let handle = tokio::spawn(server.run(token.clone()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::mem::forget(cdi);
    (token, handle)
}

async fn allocate_first(sock: PathBuf) -> ContainerAllocateResponse {
    unix_client(sock)
        .await
        .allocate(Request::new(AllocateRequest {
            container_requests: vec![ContainerAllocateRequest {
                devices_i_ds: vec!["0".to_owned()],
            }],
        }))
        .await
        .unwrap()
        .into_inner()
        .container_responses
        .remove(0)
}

#[tokio::test]
async fn fabric_allocate_hands_its_holder_endpoint_and_a_fresh_token() {
    // The Service VM must learn where to publish and with which token, and
    // only it: a GPU's holder learns nothing.  Each fabric allocation rotates
    // the token so the previous holder's copy stops working.
    let vfio = TempDir::new().unwrap();
    for (n, function, marker) in [(1, 0, true), (3, 1, true), (4, 2, false), (5, 3, false)] {
        add_connectx(vfio.path(), n, &format!("0000:05:00.{function}"), marker);
    }
    add_dev(vfio.path(), 2, 0x10de, 0x2330, 0x030200);
    let state = TempDir::new().unwrap();
    let view = View::with_grace(state.path(), GRACE).unwrap();
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
    let resp = allocate_first(sockets.path().join("kata-nvswitch-fabric.sock")).await;
    assert_eq!(resp.cdi_devices[0].name, "nvidia.com/nvswitch-fabric=0");
    let token = view.token();
    assert_ne!(
        token, before,
        "the token rotates on every fabric allocation"
    );
    assert_eq!(resp.envs[TOKEN_ENV], token);
    assert_eq!(resp.envs[ENDPOINT_ENV], "http://10.0.0.7:9877");
    assert_eq!(resp.envs.len(), 2);

    let again = allocate_first(sockets.path().join("kata-nvswitch-fabric.sock")).await;
    assert_ne!(again.envs[TOKEN_ENV], token);
    assert_eq!(again.envs[TOKEN_ENV], view.token());

    // A GPU's holder learns nothing, with or without a node address.  The
    // fabric is bound, so the GPU exists only under a view: a second View
    // instance over the same state publishes unit gpu for it.
    let owner = published(fabric_view("gpu", &[])).await;
    let (gpu_stop, gpu) =
        server_with_view("nvidia.com/gpu", &vfio, &sockets, &owner.view, None).await;
    let resp = allocate_first(sockets.path().join("kata-gpu.sock")).await;
    assert_eq!(resp.cdi_devices[0].name, "nvidia.com/gpu=0");
    assert!(resp.envs.is_empty());
    assert_eq!(view.token(), again.envs[TOKEN_ENV]);
    owner.stop.cancel();

    // Without a node address the holder still gets the token.
    fabric_stop.cancel();
    fabric.await.unwrap().unwrap();
    let (fabric_stop, fabric) =
        server_with_view("nvidia.com/nvswitch-fabric", &vfio, &sockets, &view, None).await;
    let resp = allocate_first(sockets.path().join("kata-nvswitch-fabric.sock")).await;
    assert_eq!(resp.envs.len(), 1);
    assert_eq!(resp.envs[TOKEN_ENV], view.token());

    for (stop, handle) in [(fabric_stop, fabric), (gpu_stop, gpu)] {
        stop.cancel();
        handle.await.unwrap().unwrap();
    }
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
    assert_eq!(cdevs(0), ["vfio3", "vfio1"]);
    assert_eq!(cdevs(1), ["vfio0", "vfio2"]);

    let resp = node
        .client("kata-nvlink-2gpu.sock")
        .await
        .allocate(Request::new(AllocateRequest {
            container_requests: vec![ContainerAllocateRequest {
                devices_i_ds: vec!["1".to_owned()],
            }],
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        resp.container_responses[0].cdi_devices[0].name,
        "nvidia.com/nvlink-2gpu=1"
    );
    assert!(resp.container_responses[0].envs.is_empty());

    node.shutdown().await;
    owner.stop.cancel();
}
