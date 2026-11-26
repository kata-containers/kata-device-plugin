//! Integration tests for the fabric: the NVSwitch fabric as one device and
//! what its Allocate hands the Service VM.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kata_device_plugin::dp::v1beta1::{
    AllocateRequest, ContainerAllocateRequest, ContainerAllocateResponse,
};
use kata_device_plugin::plugin::DeviceServer;
use kata_device_plugin::vfio::Naming;
use kata_device_plugin::view::View;
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

    let node = Node::start(vfio, &["nvidia.com/gpu", "nvidia.com/nvswitch-fabric"]).await;

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

    // A GPU's holder learns nothing, with or without a node address.
    let (gpu_stop, gpu) = server_with_view("nvidia.com/gpu", &vfio, &sockets, &view, None).await;
    let resp = allocate(sockets.path().join("kata-gpu.sock"), &["0000:02:00"])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(resp.cdi_devices[0].name, "nvidia.com/gpu=0000:02:00");
    assert!(resp.envs.is_empty());
    assert_eq!(view.token(), again.envs[TOKEN_ENV]);

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
