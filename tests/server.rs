/// Integration tests: a real DeviceServer against a mock kubelet, no GPU or
/// cluster required.  The mock registration service probes the plugin's
/// endpoint before acknowledging, exactly like kubelet's device manager — so
/// ordering bugs (register before serve) fail here instead of on a node.
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use kata_device_plugin::dp::v1beta1::{
    registration_server::RegistrationServer, AllocateRequest, ContainerAllocateRequest, Empty,
    PreStartContainerRequest, PreferredAllocationRequest,
};
use kata_device_plugin::plugin::DeviceServer;
use kata_device_plugin::vfio::Naming;
use pcilibs_rs::testfs::add as add_dev;
use pcilibs_rs::Sysfs;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tonic::Request;

use common::*;

#[tokio::test]
async fn recovers_from_repeated_kubelet_restarts() -> anyhow::Result<()> {
    let mut node = Node::start(fake_vfio(2), &["nvidia.com/gpu"]).await;
    node.wait_for_registration().await?;
    let (mut old_stream, initial) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(initial.devices.len(), 2);

    for restart in 0..2 {
        // Kubelet unlinks plugin endpoints on startup. Keep the old stream
        // open to ensure endpoint recreation does not wait for it to drain.
        node.stop_kubelet(true).await?;
        add_dev(node._vfio.path(), restart + 2, 0x10de, 0x2330, 0x030200);
        if restart == 0 {
            tokio::time::sleep(Duration::from_secs(6)).await;
            assert!(node.sockets.path().join("kata-gpu.sock").exists());
            assert!(node.spec("kata.nvidia.com-gpu.yaml").is_some());
        }
        node.start_kubelet()?;
        let registration = node.wait_for_registration().await?;
        assert_eq!(registration.resource_name, "nvidia.com/gpu");
        assert_eq!(registration.endpoint, "kata-gpu.sock");
        assert!(node.servers.iter().all(|task| !task.is_finished()));
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Ok(Some(_)) = old_stream.message().await {}
        })
        .await
        .context("old ListAndWatch stream remained open after endpoint recreation")?;

        let (new_stream, snapshot) = node.snapshot("kata-gpu.sock").await;
        assert_eq!(snapshot.devices.len(), (restart + 3) as usize);
        let spec = node
            .spec("kata.nvidia.com-gpu.yaml")
            .context("missing CDI spec")?;
        assert!(spec.contains(&format!("vfio{}", restart + 2)));
        old_stream = new_stream;
    }

    node.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn reregisters_when_only_kubelet_socket_is_replaced() -> anyhow::Result<()> {
    let mut node = Node::start(fake_vfio(1), &["nvidia.com/gpu"]).await;
    node.wait_for_registration().await?;
    node.stop_kubelet(false).await?;
    node.start_kubelet()?;
    node.wait_for_registration().await?;
    assert!(node.servers.iter().all(|task| !task.is_finished()));
    let (_stream, snapshot) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(snapshot.devices.len(), 1);
    node.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn retries_registration_after_connection_failure() -> anyhow::Result<()> {
    let mut node = Node::start(fake_vfio(1), &["nvidia.com/gpu"]).await;
    node.wait_for_registration().await?;
    node.stop_kubelet(false).await?;
    // An orphaned socket exists but refuses connections, unlike an absent
    // kubelet socket. Registration must retry after the failed dial.
    let listener = UnixListener::bind(node.sockets.path().join("kubelet.sock"))?;
    drop(listener);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(node
        .registered
        .lock()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .is_none());
    assert!(node.servers.iter().all(|task| !task.is_finished()));
    std::fs::remove_file(node.sockets.path().join("kubelet.sock"))?;
    node.start_kubelet()?;
    node.wait_for_registration().await?;
    let (_stream, snapshot) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(snapshot.devices.len(), 1);
    node.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn registers_with_kubelet_and_lists_devices() {
    let node = Node::start(fake_vfio(4), &["nvidia.com/gpu"]).await;

    let reg = node
        .registered
        .lock()
        .unwrap()
        .clone()
        .expect("plugin did not register");
    assert_eq!(reg.resource_name, "nvidia.com/gpu");
    assert_eq!(reg.endpoint, "kata-gpu.sock");
    assert_eq!(reg.version, "v1beta1");

    let (mut stream, resp) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(resp.devices.len(), 4);

    // The stream must stay open after the snapshot: the kubelet treats a
    // completed ListAndWatch stream as endpoint death (resource unhealthy).
    let second = tokio::time::timeout(Duration::from_millis(300), stream.message()).await;
    assert!(second.is_err(), "ListAndWatch stream ended after snapshot");

    // IDs are positions in the sorted enumeration — the same indices the CDI
    // spec maps to cdev paths.
    let mut ids: Vec<u32> = resp.devices.iter().map(|d| d.id.parse().unwrap()).collect();
    ids.sort();
    assert_eq!(ids, vec![0, 1, 2, 3]);
    for d in &resp.devices {
        assert_eq!(d.health, "Healthy");
    }
    assert!(
        node.spec("kata.nvidia.com-gpu.yaml").is_some(),
        "CDI spec not written"
    );

    node.shutdown().await;
}

#[tokio::test]
async fn allocate_returns_cdi_device_names() {
    let node = Node::start(fake_vfio(2), &["nvidia.com/gpu"]).await;

    // No ListAndWatch stream is open, so no poller has written the CDI spec:
    // Allocate alone must sync it so the names it returns resolve the moment
    // the shim reads the registry.
    assert!(node.spec("kata.nvidia.com-gpu.yaml").is_none());

    let resp = node
        .client("kata-gpu.sock")
        .await
        .allocate(Request::new(AllocateRequest {
            container_requests: vec![ContainerAllocateRequest {
                devices_i_ds: vec!["0".to_owned(), "1".to_owned()],
            }],
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.container_responses.len(), 1);
    let cdi_names: Vec<&str> = resp.container_responses[0]
        .cdi_devices
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(cdi_names, vec!["nvidia.com/gpu=0", "nvidia.com/gpu=1"]);
    // No DeviceSpec — device injection is the Kata shim's job, resolving CDI
    // names against the host spec, not the kubelet's via device paths.
    assert!(resp.container_responses[0].devices.is_empty());
    assert!(
        node.spec("kata.nvidia.com-gpu.yaml").is_some(),
        "Allocate must sync the CDI spec so its names resolve immediately"
    );

    node.shutdown().await;
}

#[tokio::test]
async fn allocate_rejects_ids_beyond_present_devices() {
    // The device set can shrink between scheduling and Allocate; a stale ID
    // must fail as a clear gRPC error, not as an unresolvable CDI name that
    // the Kata shim trips over later.
    let node = Node::start(fake_vfio(2), &["nvidia.com/gpu"]).await;

    remove_dev(node._vfio.path(), 1);

    let err = node
        .client("kata-gpu.sock")
        .await
        .allocate(Request::new(AllocateRequest {
            container_requests: vec![ContainerAllocateRequest {
                devices_i_ds: vec!["0".to_owned(), "1".to_owned()],
            }],
        }))
        .await
        .expect_err("allocate of a vanished device must fail");
    assert_eq!(err.code(), tonic::Code::NotFound);

    node.shutdown().await;
}

#[tokio::test]
async fn empty_vfio_dir_advertises_no_devices() {
    let node = Node::start(fake_vfio(0), &["nvidia.com/gpu"]).await;

    let (_stream, resp) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(resp.devices.len(), 0);
    assert!(node.spec("kata.nvidia.com-gpu.yaml").is_none());

    node.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn devices_appearing_after_startup_are_published() {
    // Start with an empty dir, then add a GPU cdev — simulates VFIO binding
    // racing DP startup.  The watcher must pick up the new device and push an
    // updated ListAndWatch response.
    let vfio = TempDir::new().unwrap();
    std::fs::create_dir(vfio.path().join("devices")).unwrap();

    let node = Node::start(vfio, &["nvidia.com/gpu"]).await;

    let (mut stream, first) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(first.devices.len(), 0, "must start with zero devices");

    // Simulate the VFIO cdev appearing after the DP is already running.
    add_dev(node._vfio.path(), 0, 0x10de, 0x2330, 0x030200);

    // The poller ticks every POLL_INTERVAL (5 s).  Time is paused
    // (start_paused), so the tick auto-advances as soon as the runtime idles;
    // the 10 s timeout is virtual too and only fires on a real hang.
    let updated = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("timed out waiting for updated ListAndWatch response")
        .unwrap()
        .expect("stream closed");
    assert_eq!(updated.devices.len(), 1);
    assert!(
        node.spec("kata.nvidia.com-gpu.yaml").is_some(),
        "CDI spec written after device appeared"
    );

    node.shutdown().await;
}

/// Remove cdev `vfio<n>` and its fake sysfs entry — the inverse of `add_dev`.
fn remove_dev(root: &std::path::Path, n: u32) {
    std::fs::remove_file(root.join("devices").join(format!("vfio{n}"))).unwrap();
    std::fs::remove_dir_all(Sysfs::new(root).vfio_dev(&format!("vfio{n}"))).unwrap();
}

#[tokio::test(start_paused = true)]
async fn same_count_device_swap_refreshes_cdi_spec() {
    // vfio0 is replaced by vfio5 between polls.  The advertised Device list
    // is identical (one device, id "0"), but the CDI index → cdev mapping
    // changed, so the poller must detect the swap, rewrite the spec, and
    // push an update.
    let node = Node::start(fake_vfio(1), &["nvidia.com/gpu"]).await;

    let (mut stream, first) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(first.devices.len(), 1);
    assert!(node
        .spec("kata.nvidia.com-gpu.yaml")
        .unwrap()
        .contains("vfio0"));

    remove_dev(node._vfio.path(), 0);
    add_dev(node._vfio.path(), 5, 0x10de, 0x2330, 0x030200);

    let updated = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("timed out waiting for swap update")
        .unwrap()
        .expect("stream closed");
    assert_eq!(updated.devices.len(), 1, "count unchanged by design");
    let spec = node.spec("kata.nvidia.com-gpu.yaml").unwrap();
    assert!(spec.contains("vfio5"), "spec must point at the new cdev");
    assert!(!spec.contains("vfio0"), "spec must not keep the dead cdev");

    node.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn last_device_disappearing_removes_cdi_spec() {
    let node = Node::start(fake_vfio(1), &["nvidia.com/gpu"]).await;

    let (mut stream, first) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(first.devices.len(), 1);
    assert!(node.spec("kata.nvidia.com-gpu.yaml").is_some());

    remove_dev(node._vfio.path(), 0);

    let updated = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("timed out waiting for removal update")
        .unwrap()
        .expect("stream closed");
    assert_eq!(updated.devices.len(), 0);
    assert!(
        node.spec("kata.nvidia.com-gpu.yaml").is_none(),
        "stale CDI spec must be removed when the last device vanishes"
    );

    node.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn poller_exits_when_kubelet_drops_the_stream() {
    // A kubelet reconnect drops the old ListAndWatch stream.  The poller
    // behind it must notice (tx.is_closed()) and exit on its next tick, not
    // keep polling forever.  Exercised by dropping the stream and letting
    // one tick elapse; a leaked poller would show up as the next test's
    // spurious CDI writes and, in coverage, as the break never taken.
    let node = Node::start(fake_vfio(1), &["nvidia.com/gpu"]).await;

    let (stream, first) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(first.devices.len(), 1);

    drop(stream);
    // One full poll interval with the receiver gone: the poller wakes,
    // sees the closed channel, and exits.
    tokio::time::sleep(Duration::from_secs(6)).await;

    node.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn cdi_write_failure_is_retried_next_tick() {
    // Break the CDI spec path, change the device set: the poller must still
    // push the update to the kubelet but keep retrying the CDI write until
    // the path is writable again — a transient failure must not leave a
    // permanently stale spec.  A directory squatting on the spec path makes
    // std::fs::write fail with EISDIR, root or not (chmod tricks don't work
    // as root, and dir write bits don't stop overwriting an existing file).
    let node = Node::start(fake_vfio(1), &["nvidia.com/gpu"]).await;

    let (mut stream, first) = node.snapshot("kata-gpu.sock").await;
    assert_eq!(first.devices.len(), 1);

    let spec_path = node.cdi.path().join("kata.nvidia.com-gpu.yaml");
    std::fs::remove_file(&spec_path).unwrap();
    std::fs::create_dir(&spec_path).unwrap();
    add_dev(node._vfio.path(), 1, 0x10de, 0x2330, 0x030200);

    // The device update must reach the kubelet even while the CDI write fails.
    let updated = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("timed out waiting for device update")
        .unwrap()
        .expect("stream closed");
    assert_eq!(updated.devices.len(), 2);
    assert!(
        spec_path.is_dir(),
        "spec cannot have been written over the blocking dir"
    );

    // Path is writable again: a later tick retries and the spec catches up.
    std::fs::remove_dir(&spec_path).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(spec) = node.spec("kata.nvidia.com-gpu.yaml") {
            if spec.contains("vfio1") {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "CDI write never retried"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    node.shutdown().await;
}

#[tokio::test]
async fn registration_failure_is_not_fatal() -> anyhow::Result<()> {
    // Deployment order must not require a pod restart when kubelet becomes
    // available after the plugin has started.
    let vfio = fake_vfio(1);
    let sockets = TempDir::new().unwrap();
    let cdi = TempDir::new().unwrap();

    let server = DeviceServer::new(
        "nvidia.com/gpu",
        Naming::Alias,
        vfio.path().to_str().unwrap(),
        vfio.path().to_str().unwrap(),
        sockets.path().to_str().unwrap(),
        cdi.path().to_str().unwrap(),
    );
    let token = CancellationToken::new();
    let handle = tokio::spawn(server.run(token.clone()));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut stream = unix_client(sockets.path().join("kata-gpu.sock"))
        .await
        .list_and_watch(Request::new(Empty {}))
        .await
        .expect("server must serve despite failed registration")
        .into_inner();
    let resp = tokio::time::timeout(Duration::from_secs(2), stream.message())
        .await
        .expect("ListAndWatch timeout")
        .unwrap()
        .expect("stream closed");
    assert_eq!(resp.devices.len(), 1);

    let registered = Arc::new(Mutex::new(None));
    let listener = UnixListener::bind(sockets.path().join("kubelet.sock"))?;
    let mock = MockKubelet {
        socket_dir: sockets.path().to_path_buf(),
        registered: registered.clone(),
    };
    let kubelet = tokio::spawn(async move {
        Server::builder()
            .add_service(RegistrationServer::new(mock))
            .serve_with_incoming(UnixListenerStream::new(listener))
            .await
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if registered
                .lock()
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .is_some()
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("plugin did not retry registration after kubelet became available")??;
    assert!(!handle.is_finished());

    token.cancel();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("server did not shut down")
        .unwrap()
        .unwrap();
    kubelet.abort();
    let _ = kubelet.await;
    Ok(())
}

#[tokio::test]
async fn sku_mode_serves_under_the_sku_name() {
    // End to end under a SKU-resolved name: socket, registration, CDI spec
    // file, and device IDs must all follow the resolved name, not the row.
    let vfio = fake_vfio(2);
    let name = kata_device_plugin::vfio::discover(
        vfio.path(),
        &Sysfs::new(vfio.path()),
        Naming::Sku,
        None,
    )
    .into_keys()
    .next()
    .expect("two H100 cdevs resolve to one SKU name");
    assert!(name.starts_with("nvidia.com/GH100"), "{name}");

    let sock = format!("kata-{}.sock", name.rsplit('/').next().unwrap());
    let node = Node::start_with(vfio, &[name.as_str()], Naming::Sku).await;

    let reg = node
        .registered
        .lock()
        .unwrap()
        .clone()
        .expect("plugin did not register");
    assert_eq!(reg.resource_name, name);

    let (_stream, resp) = node.snapshot(&sock).await;
    assert_eq!(resp.devices.len(), 2);

    let spec_file = format!("kata.{}.yaml", name.replace('/', "-"));
    let spec = node.spec(&spec_file).expect("CDI spec under the SKU name");
    assert!(spec.contains(&format!("kind: {name}")));
    assert!(spec.contains("devices/vfio0") && spec.contains("devices/vfio1"));

    node.shutdown().await;
}

#[tokio::test]
async fn grpc_stubs_respond() {
    // GetPreferredAllocation and PreStartContainer are protocol stubs; the
    // kubelet may still call them, so they must answer instead of erroring.
    let node = Node::start(fake_vfio(1), &["nvidia.com/gpu"]).await;
    let mut client = node.client("kata-gpu.sock").await;

    client
        .get_preferred_allocation(Request::new(PreferredAllocationRequest::default()))
        .await
        .expect("GetPreferredAllocation must answer");
    client
        .pre_start_container(Request::new(PreStartContainerRequest::default()))
        .await
        .expect("PreStartContainer must answer");

    node.shutdown().await;
}

#[tokio::test]
async fn gpu_and_nvswitch_fabric_filtered_by_pci_class() {
    // A mixed node: 2 GPUs, 3 NVSwitch chips forming one fabric, and a
    // VFIO-bound NIC that matches no RESOURCES row and must not be
    // advertised anywhere.
    let vfio = TempDir::new().unwrap();
    for i in 0..2 {
        add_dev(vfio.path(), i, 0x10de, 0x2330, 0x030200);
    }
    for i in 2..5 {
        add_dev(vfio.path(), i, 0x10de, 0x22a3, 0x068000);
    }
    add_dev(vfio.path(), 5, 0x15b3, 0x101e, 0x020000);

    // The fabric is bound, so GPUs appear only once the owner's view says
    // how; unit gpu keeps them one device each.
    let owner = published(fabric_view("gpu", &[])).await;
    let node = Node::start_with_view(
        vfio,
        &["nvidia.com/gpu", "nvidia.com/nvswitch-fabric"],
        Naming::Alias,
        Some(owner.view.clone()),
    )
    .await;

    for (sock, expected) in [("kata-gpu.sock", 2), ("kata-nvswitch-fabric.sock", 1)] {
        let (_stream, resp) = node.snapshot(sock).await;
        assert_eq!(resp.devices.len(), expected, "{sock}");
    }

    // Each resource's CDI spec holds only its own devices.
    let gpu = node.spec("kata.nvidia.com-gpu.yaml").unwrap();
    let sw = node.spec("kata.nvidia.com-nvswitch-fabric.yaml").unwrap();
    assert!(gpu.contains("devices/vfio0") && gpu.contains("devices/vfio1"));
    assert!(sw.contains("devices/vfio2") && sw.contains("devices/vfio4"));
    assert!(!gpu.contains("devices/vfio2") && !sw.contains("devices/vfio0"));
    // The NIC appears in neither.
    assert!(!gpu.contains("devices/vfio5") && !sw.contains("devices/vfio5"));

    node.shutdown().await;
    owner.stop.cancel();
}
