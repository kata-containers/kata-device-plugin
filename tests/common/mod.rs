//! Fixtures shared by the integration tests: a mock kubelet, a node of
//! DeviceServers over a fake /dev/vfio, and a published fabric view.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use kata_device_plugin::dp::v1beta1::{
    device_plugin_client::DevicePluginClient,
    registration_server::{Registration, RegistrationServer},
    Empty, ListAndWatchResponse, RegisterRequest,
};
use kata_device_plugin::plugin::DeviceServer;
use kata_device_plugin::vfio::Naming;
use kata_device_plugin::view::{self, View, GRACE};
use pcilibs_rs::fabric::grpc::FabricViewServiceClient;
use pcilibs_rs::fabric::v1::{self, FabricView};
use pcilibs_rs::testfs::{add as add_dev, add_at};
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint, Server, Uri};
use tonic::{Request, Response, Status, Streaming};
use tower::service_fn;

// ── mock kubelet ──────────────────────────────────────────────────────────────

pub struct MockKubelet {
    pub socket_dir: PathBuf,
    pub registered: Arc<Mutex<Option<RegisterRequest>>>,
}

#[tonic::async_trait]
impl Registration for MockKubelet {
    async fn register(&self, req: Request<RegisterRequest>) -> Result<Response<Empty>, Status> {
        let req = req.into_inner();
        // The real kubelet dials the endpoint and probes it inside Register —
        // a plugin that registers before serving deadlocks right here.
        let sock = self.socket_dir.join(&req.endpoint);
        let probe = async {
            let mut client = unix_client(sock).await;
            client
                .get_device_plugin_options(Request::new(Empty {}))
                .await
        };
        tokio::time::timeout(Duration::from_secs(2), probe)
            .await
            .map_err(|_| Status::deadline_exceeded("failed to dial device plugin"))?
            .map_err(|e| Status::unknown(format!("endpoint probe failed: {e}")))?;
        *self.registered.lock().unwrap() = Some(req);
        Ok(Response::new(Empty {}))
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// n GPU cdevs (vfio0..vfio(n-1)); n = 0 gives an empty devices dir.
pub fn fake_vfio(n: u32) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("devices")).unwrap();
    for i in 0..n {
        add_dev(dir.path(), i, 0x10de, 0x2330, 0x030200);
    }
    dir
}

pub async fn unix_client(socket_path: PathBuf) -> DevicePluginClient<Channel> {
    let channel = Endpoint::try_from("http://[::]:0")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket_path = socket_path.clone();
            async move {
                // tonic 0.12 speaks hyper 1.0 IO traits, not tokio's.
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(socket_path).await?,
                ))
            }
        }))
        .await
        .unwrap();
    DevicePluginClient::new(channel)
}

/// A view and the pieces that keep it current: the service it was published
/// through and the publisher's still-open stream.
pub struct Published {
    pub view: Arc<View>,
    pub _tx: tokio::sync::mpsc::Sender<FabricView>,
    pub stop: CancellationToken,
    pub _state: TempDir,
}

/// Publish `fabric_view` the way fmclient does, over the host socket, and
/// keep the stream open so the view stays current.
pub async fn published(fabric_view: FabricView) -> Published {
    let state = TempDir::new().unwrap();
    let view = View::with_grace(state.path(), GRACE).unwrap();
    let socket = state.path().join("fabric-view.sock");
    let unix = UnixListener::bind(&socket).unwrap();
    let tcp = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let stop = CancellationToken::new();
    tokio::spawn(view::serve(view.clone(), unix, tcp, stop.clone()));
    let channel = Endpoint::try_from("http://[::]:0")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                tokio::net::UnixStream::connect(socket)
                    .await
                    .map(hyper_util::rt::TokioIo::new)
            }
        }))
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel::<FabricView>(2);
    let mut client = FabricViewServiceClient::new(channel);
    tokio::spawn(async move {
        client
            .publish(Request::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )))
            .await
    });
    tx.send(fabric_view).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while view.current().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the published view did not become current");
    Published {
        view,
        _tx: tx,
        stop,
        _state: state,
    }
}

/// A view: unit word and `(id, active, member physical ids)` partitions.
pub fn fabric_view(unit: &str, partitions: &[(u32, bool, &[u32])]) -> FabricView {
    FabricView {
        partitions: Some(v1::PartitionList {
            version: 1,
            max_num_partitions: 15,
            partition_info: partitions
                .iter()
                .map(|&(id, active, members)| v1::Partition {
                    partition_id: id,
                    is_active: active,
                    gpu_info: members
                        .iter()
                        .map(|&physical_id| v1::PartitionGpu {
                            physical_id,
                            ..Default::default()
                        })
                        .collect(),
                })
                .collect(),
        }),
        unsupported: None,
        unit: unit.to_owned(),
    }
}

/// A GPU as cdev `n` at `address`, labeled with module slot `module` the way
/// DGX firmware does.
pub fn add_labeled_gpu(root: &std::path::Path, n: u32, address: &str, module: u8) {
    let function = add_at(root, n, address, 0x10de, 0x2330, 0x030200);
    std::fs::write(function.join("label"), format!("GPU{n}, SMX{module}\n")).unwrap();
}

/// Mock kubelet plus one running DeviceServer per resource name.
pub struct Node {
    // Keeps the fake /dev/vfio alive for the servers' lifetime.
    pub _vfio: TempDir,
    pub sockets: TempDir,
    pub cdi: TempDir,
    pub registered: Arc<Mutex<Option<RegisterRequest>>>,
    pub token: CancellationToken,
    pub servers: Vec<tokio::task::JoinHandle<()>>,
    pub kubelet: Option<tokio::task::JoinHandle<()>>,
}

impl Node {
    pub async fn start(vfio: TempDir, resources: &[&str]) -> Node {
        Self::start_with(vfio, resources, Naming::Alias).await
    }

    pub async fn start_with(vfio: TempDir, resources: &[&str], naming: Naming) -> Node {
        Self::start_with_view(vfio, resources, naming, None).await
    }

    /// With a view, the servers advertise GPUs as the view says; without
    /// one, a bound fabric withholds them.
    pub async fn start_with_view(
        vfio: TempDir,
        resources: &[&str],
        naming: Naming,
        view: Option<Arc<View>>,
    ) -> Node {
        let sockets = TempDir::new().unwrap();
        let cdi = TempDir::new().unwrap();
        let registered = Arc::new(Mutex::new(None));

        let listener = UnixListener::bind(sockets.path().join("kubelet.sock")).unwrap();
        let mock = MockKubelet {
            socket_dir: sockets.path().to_path_buf(),
            registered: registered.clone(),
        };
        let kubelet = tokio::spawn(async move {
            Server::builder()
                .add_service(RegistrationServer::new(mock))
                .serve_with_incoming(UnixListenerStream::new(listener))
                .await
                .ok();
        });

        let token = CancellationToken::new();
        let servers = resources
            .iter()
            .map(|name| {
                let mut server = DeviceServer::new(
                    name,
                    naming,
                    vfio.path().to_str().unwrap(),
                    vfio.path().to_str().unwrap(),
                    sockets.path().to_str().unwrap(),
                    cdi.path().to_str().unwrap(),
                );
                if let Some(view) = &view {
                    server = server.with_view(view.clone(), None);
                }
                let tok = token.clone();
                tokio::spawn(async move {
                    server.run(tok).await.ok();
                })
            })
            .collect();
        // Let registration and the kubelet dial-back settle.
        tokio::time::sleep(Duration::from_millis(400)).await;

        Node {
            _vfio: vfio,
            sockets,
            cdi,
            registered,
            token,
            servers,
            kubelet: Some(kubelet),
        }
    }

    pub async fn stop_kubelet(&mut self, remove_plugin_socket: bool) -> anyhow::Result<()> {
        if let Some(task) = self.kubelet.take() {
            task.abort();
            let _ = task.await;
        }
        std::fs::remove_file(self.sockets.path().join("kubelet.sock"))?;
        if remove_plugin_socket {
            std::fs::remove_file(self.sockets.path().join("kata-gpu.sock"))?;
        }
        *self.registered.lock().map_err(|e| anyhow::anyhow!("{e}"))? = None;
        Ok(())
    }

    pub fn start_kubelet(&mut self) -> anyhow::Result<()> {
        let listener = UnixListener::bind(self.sockets.path().join("kubelet.sock"))?;
        let mock = MockKubelet {
            socket_dir: self.sockets.path().to_path_buf(),
            registered: self.registered.clone(),
        };
        self.kubelet = Some(tokio::spawn(async move {
            Server::builder()
                .add_service(RegistrationServer::new(mock))
                .serve_with_incoming(UnixListenerStream::new(listener))
                .await
                .ok();
        }));
        Ok(())
    }

    pub async fn wait_for_registration(&self) -> anyhow::Result<RegisterRequest> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let registration = self
                    .registered
                    .lock()
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .clone();
                if let Some(registration) = registration {
                    return Ok(registration);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("plugin did not register with restarted kubelet")?
    }

    pub async fn client(&self, sock: &str) -> DevicePluginClient<Channel> {
        unix_client(self.sockets.path().join(sock)).await
    }

    /// First ListAndWatch message plus the still-open stream.
    pub async fn snapshot(
        &self,
        sock: &str,
    ) -> (Streaming<ListAndWatchResponse>, ListAndWatchResponse) {
        let mut stream = self
            .client(sock)
            .await
            .list_and_watch(Request::new(Empty {}))
            .await
            .unwrap()
            .into_inner();
        let resp = tokio::time::timeout(Duration::from_secs(2), stream.message())
            .await
            .expect("ListAndWatch timeout")
            .unwrap()
            .expect("stream closed");
        (stream, resp)
    }

    pub fn spec(&self, file: &str) -> Option<String> {
        std::fs::read_to_string(self.cdi.path().join(file)).ok()
    }

    /// Cancel and require prompt exit: a graceful drain would hang forever on
    /// the held-open ListAndWatch streams.
    pub async fn shutdown(self) {
        self.token.cancel();
        for task in self.servers {
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("server did not shut down after cancel")
                .unwrap();
        }
        if let Some(task) = self.kubelet {
            task.abort();
            let _ = task.await;
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────
