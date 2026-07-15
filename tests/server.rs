/// Integration tests for DeviceServer.
///
/// These run without a GPU or a Kubernetes cluster.  They require:
///   - A temp dir that mimics /dev/vfio with a devices/ subdir containing
///     synthetic IOMMUFD char device entries (vfio0, vfio1, ...).
///   - A mock kubelet gRPC Registration service on a Unix socket.
///
/// The DeviceServer is started, writes a CDI spec, connects to the mock
/// kubelet, registers, and serves ListAndWatch.  We then call ListAndWatch
/// and Allocate ourselves to verify device IDs and CDI device names.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kata_device_plugin::dp::v1beta1::{
    device_plugin_client::DevicePluginClient,
    registration_server::{Registration, RegistrationServer},
    AllocateRequest, ContainerAllocateRequest, Empty, RegisterRequest,
};
use kata_device_plugin::plugin::{DeviceServer, Mode};
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Endpoint, Server, Uri};
use tonic::{Request, Response, Status};
use tower::service_fn;

// ── mock kubelet ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct MockKubelet {
    registered: Arc<Mutex<Option<RegisterRequest>>>,
}

#[tonic::async_trait]
impl Registration for MockKubelet {
    async fn register(
        &self,
        req: Request<RegisterRequest>,
    ) -> Result<Response<kata_device_plugin::dp::v1beta1::Empty>, Status> {
        *self.registered.lock().unwrap() = Some(req.into_inner());
        Ok(Response::new(kata_device_plugin::dp::v1beta1::Empty {}))
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Create a temp dir that mimics /dev/vfio with n IOMMUFD char devices.
/// Layout: <dir>/devices/vfio0, vfio1, ..., vfio(n-1)
fn fake_vfio(n: u32) -> TempDir {
    let dir = TempDir::new().unwrap();
    let devices = dir.path().join("devices");
    std::fs::create_dir(&devices).unwrap();
    for i in 0..n {
        std::fs::write(devices.join(format!("vfio{i}")), b"").unwrap();
    }
    dir
}

async fn serve_unix<S>(socket_dir: &std::path::Path, name: &str, svc: S) -> PathBuf
where
    S: tonic::server::NamedService
        + tower::Service<
            http::Request<tonic::transport::Body>,
            Response = http::Response<tonic::body::BoxBody>,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    <S as tower::Service<http::Request<tonic::transport::Body>>>::Future: Send,
{
    let path = socket_dir.join(name);
    let listener = UnixListener::bind(&path).unwrap();
    let stream = UnixListenerStream::new(listener);
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(stream)
            .await
            .ok();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    path
}

async fn unix_client(socket_path: PathBuf) -> DevicePluginClient<tonic::transport::Channel> {
    let channel = Endpoint::try_from("http://[::]:0")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            tokio::net::UnixStream::connect(socket_path.clone())
        }))
        .await
        .unwrap();
    DevicePluginClient::new(channel)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn registers_with_kubelet_and_lists_devices() {
    let socket_dir = TempDir::new().unwrap();
    let cdi_dir = TempDir::new().unwrap();
    let vfio_dir = fake_vfio(4); // pretend there are 4 GPUs (vfio0..vfio3)

    let registered = Arc::new(Mutex::new(None::<RegisterRequest>));
    let mock = MockKubelet { registered: registered.clone() };
    serve_unix(socket_dir.path(), "kubelet.sock", RegistrationServer::new(mock)).await;

    let token = CancellationToken::new();
    let server = DeviceServer::new(
        "nvidia.com/gpu",
        vfio_dir.path().to_str().unwrap(),
        socket_dir.path().to_str().unwrap(),
        cdi_dir.path().to_str().unwrap(),
        Mode::Pgpu,
    );
    let tok = token.clone();
    tokio::spawn(async move { server.run(tok).await.ok(); });

    tokio::time::sleep(Duration::from_millis(400)).await;

    let reg = registered.lock().unwrap().clone().expect("plugin did not register");
    assert_eq!(reg.resource_name, "nvidia.com/gpu");
    assert_eq!(reg.endpoint, "kata-gpu.sock");
    assert_eq!(reg.version, "v1beta1");

    let plugin_sock = socket_dir.path().join("kata-gpu.sock");
    let mut client = unix_client(plugin_sock).await;
    let mut stream = client
        .list_and_watch(Request::new(Empty {}))
        .await
        .unwrap()
        .into_inner();

    let resp = tokio::time::timeout(Duration::from_secs(2), stream.message())
        .await
        .expect("timeout")
        .unwrap()
        .expect("stream closed");

    assert_eq!(resp.devices.len(), 4, "expected 4 devices");
    // IDs must be sequential integers matching CDI spec indices.
    let mut ids: Vec<u32> = resp.devices.iter()
        .map(|d| d.id.parse::<u32>().expect("device ID must be a number"))
        .collect();
    ids.sort();
    assert_eq!(ids, vec![0, 1, 2, 3]);
    for d in &resp.devices {
        assert_eq!(d.health, "Healthy");
    }

    // CDI spec must have been written.
    assert!(cdi_dir.path().join("nvidia.com-gpu.yaml").exists(), "CDI spec not written");

    token.cancel();
}

#[tokio::test]
async fn allocate_returns_cdi_device_names() {
    let socket_dir = TempDir::new().unwrap();
    let cdi_dir = TempDir::new().unwrap();
    let vfio_dir = fake_vfio(2);

    let mock = MockKubelet::default();
    serve_unix(socket_dir.path(), "kubelet.sock", RegistrationServer::new(mock)).await;

    let token = CancellationToken::new();
    let server = DeviceServer::new(
        "nvidia.com/gpu",
        vfio_dir.path().to_str().unwrap(),
        socket_dir.path().to_str().unwrap(),
        cdi_dir.path().to_str().unwrap(),
        Mode::Pgpu,
    );
    let tok = token.clone();
    tokio::spawn(async move { server.run(tok).await.ok(); });

    tokio::time::sleep(Duration::from_millis(400)).await;

    let plugin_sock = socket_dir.path().join("kata-gpu.sock");
    let mut client = unix_client(plugin_sock).await;

    let resp = client
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
    // No DeviceSpec — Kata uses CDI + Pod Resources API, not raw device paths.
    assert!(resp.container_responses[0].devices.is_empty());

    token.cancel();
}

#[tokio::test]
async fn imex_mode_lists_devices() {
    let socket_dir = TempDir::new().unwrap();
    let cdi_dir = TempDir::new().unwrap();
    let vfio_dir = fake_vfio(4);

    let mock = MockKubelet::default();
    serve_unix(socket_dir.path(), "kubelet.sock", RegistrationServer::new(mock)).await;

    let token = CancellationToken::new();
    let server = DeviceServer::new(
        "nvidia.com/gpu",
        vfio_dir.path().to_str().unwrap(),
        socket_dir.path().to_str().unwrap(),
        cdi_dir.path().to_str().unwrap(),
        Mode::Imex { clique_id: "7b968a6d-c8aa-45e1-9e70-e1e51be99c31.1".to_owned() },
    );
    let tok = token.clone();
    tokio::spawn(async move { server.run(tok).await.ok(); });

    tokio::time::sleep(Duration::from_millis(400)).await;

    let plugin_sock = socket_dir.path().join("kata-gpu.sock");
    let mut client = unix_client(plugin_sock).await;
    let mut stream = client
        .list_and_watch(Request::new(Empty {}))
        .await
        .unwrap()
        .into_inner();

    let resp = tokio::time::timeout(Duration::from_secs(2), stream.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert_eq!(resp.devices.len(), 4);
    token.cancel();
}

#[tokio::test]
async fn empty_vfio_dir_advertises_no_devices() {
    let socket_dir = TempDir::new().unwrap();
    let cdi_dir = TempDir::new().unwrap();
    let vfio_dir = fake_vfio(0);

    let mock = MockKubelet::default();
    serve_unix(socket_dir.path(), "kubelet.sock", RegistrationServer::new(mock)).await;

    let token = CancellationToken::new();
    let server = DeviceServer::new(
        "nvidia.com/gpu",
        vfio_dir.path().to_str().unwrap(),
        socket_dir.path().to_str().unwrap(),
        cdi_dir.path().to_str().unwrap(),
        Mode::Pgpu,
    );
    let tok = token.clone();
    tokio::spawn(async move { server.run(tok).await.ok(); });

    tokio::time::sleep(Duration::from_millis(400)).await;

    let plugin_sock = socket_dir.path().join("kata-gpu.sock");
    let mut client = unix_client(plugin_sock).await;
    let mut stream = client
        .list_and_watch(Request::new(Empty {}))
        .await
        .unwrap()
        .into_inner();

    let resp = tokio::time::timeout(Duration::from_secs(2), stream.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert_eq!(resp.devices.len(), 0);
    // No CDI spec written when there are no devices.
    assert!(!cdi_dir.path().join("nvidia.com-gpu.yaml").exists());

    token.cancel();
}
