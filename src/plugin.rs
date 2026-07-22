use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Endpoint, Server, Uri};
use tonic::{Request, Response, Status};
use tower::service_fn;
use tracing::info;

use crate::cdi;
use crate::vfio::{self, Resource};

use crate::dp::v1beta1::{
    device_plugin_server::{DevicePlugin, DevicePluginServer},
    registration_client::RegistrationClient,
    AllocateRequest, AllocateResponse, CdiDevice, ContainerAllocateResponse, Device,
    DevicePluginOptions, Empty, ListAndWatchResponse, PreStartContainerRequest,
    PreStartContainerResponse, PreferredAllocationRequest, PreferredAllocationResponse,
    RegisterRequest,
};

/// Where the kubelet serves its registration socket and scans for plugin
/// sockets.  Not configurable — the device plugin API decides.
/// Tests inject a temp dir via `DeviceServer::new` instead.
pub const SOCKET_DIR: &str = "/var/lib/kubelet/device-plugins";

/// Where the host CDI registry lives; the Kata shim reads it at
/// SandboxCreate.  A runtime contract, not configuration.
pub const CDI_DIR: &str = "/var/run/cdi";

/// How often ListAndWatch re-scans /dev/vfio/devices/.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Every resource needs its own socket in the kubelet's shared plugin dir:
/// "nvidia.com/gpu" → "kata-gpu.sock".
fn socket_name(resource_name: &str) -> String {
    let base = resource_name.rsplit('/').next().unwrap_or(resource_name);
    format!("kata-{base}.sock")
}

/// Single concrete device plugin; tonic requires Clone.
#[derive(Clone)]
pub struct DeviceServer {
    resource: Resource,
    // Path fields rather than the constants so tests can inject temp dirs.
    device_dir: PathBuf,
    sysfs_dir: PathBuf,
    socket_dir: PathBuf,
    cdi_dir: PathBuf,
}

impl DeviceServer {
    pub fn new(
        resource: Resource,
        device_dir: &str,
        sysfs_dir: &str,
        socket_dir: &str,
        cdi_dir: &str,
    ) -> Self {
        Self {
            resource,
            device_dir: PathBuf::from(device_dir),
            sysfs_dir: PathBuf::from(sysfs_dir),
            socket_dir: PathBuf::from(socket_dir),
            cdi_dir: PathBuf::from(cdi_dir),
        }
    }

    pub async fn run(self, token: CancellationToken) -> anyhow::Result<()> {
        let sock_name = socket_name(self.resource.name);
        let socket = self.socket_dir.join(&sock_name);
        let _ = tokio::fs::remove_file(&socket).await;
        let stream = UnixListenerStream::new(
            UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?,
        );

        info!(resource = self.resource.name, socket = %socket.display(), "plugin server starting");

        // Serve first, register after: the kubelet dials back and probes the
        // endpoint inside the Register RPC itself, so registering before the
        // server is live deadlocks until kubelet's 10s dial deadline expires.
        let mut serve = tokio::spawn(
            Server::builder()
                .add_service(DevicePluginServer::new(self.clone()))
                .serve_with_incoming(stream),
        );

        let kubelet = self
            .socket_dir
            .join("kubelet.sock")
            .to_string_lossy()
            .into_owned();
        if let Err(e) = register(&kubelet, self.resource.name, &sock_name).await {
            tracing::warn!("kubelet registration failed: {e:#}");
        }

        // No graceful drain on shutdown: we hold the kubelet's ListAndWatch
        // stream open for the plugin's whole lifetime, so draining would wait
        // forever.  Abort the server task and clean up instead.
        tokio::select! {
            res = &mut serve => res.context("plugin server task")?.context("plugin server")?,
            _ = token.cancelled() => serve.abort(),
        }

        // Remove the CDI spec on clean shutdown so no stale device paths
        // remain on disk if the node is reconfigured while the plugin is absent.
        // On crash the file stays, but the ListAndWatch poller replaces it from
        // a fresh scan on the kubelet's first call after restart.
        let cdi_file = cdi::spec_path(&self.cdi_dir, self.resource.name);
        match tokio::fs::remove_file(&cdi_file).await {
            Ok(()) => info!(path = %cdi_file.display(), "removed CDI spec on shutdown"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(%e, path = %cdi_file.display(), "CDI spec removal on shutdown failed")
            }
        }

        Ok(())
    }
}

async fn register(kubelet_sock: &str, resource_name: &str, endpoint: &str) -> anyhow::Result<()> {
    let sock = kubelet_sock.to_owned();
    let channel = Endpoint::try_from("http://[::]:0")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let sock = sock.clone();
            async move {
                // tonic 0.12 speaks hyper 1.0 IO traits, not tokio's.
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(sock).await?,
                ))
            }
        }))
        .await
        .context("connect to kubelet")?;

    RegistrationClient::new(channel)
        .register(RegisterRequest {
            version: "v1beta1".to_owned(),
            endpoint: endpoint.to_owned(),
            resource_name: resource_name.to_owned(),
            options: None,
        })
        .await
        .context("register")?;

    info!(resource = resource_name, "registered with kubelet");
    Ok(())
}

// ── tonic gRPC service ────────────────────────────────────────────────────────

#[tonic::async_trait]
impl DevicePlugin for DeviceServer {
    async fn get_device_plugin_options(
        &self,
        _: Request<Empty>,
    ) -> Result<Response<DevicePluginOptions>, Status> {
        Ok(Response::new(DevicePluginOptions::default()))
    }

    type ListAndWatchStream =
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<ListAndWatchResponse, Status>> + Send>>;

    async fn list_and_watch(
        &self,
        _: Request<Empty>,
    ) -> Result<Response<Self::ListAndWatchStream>, Status> {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let server = self.clone();
        tokio::spawn(async move {
            let mut last: Option<Vec<u32>> = None;
            loop {
                // Exit when the kubelet drops the stream, even if the device
                // set never changes again — otherwise this task would poll
                // forever across kubelet reconnects.
                if tx.is_closed() {
                    break;
                }

                let vfio_devs =
                    vfio::enumerate(&server.device_dir, &server.sysfs_dir, &server.resource);
                // Compare the backing cdev numbers, not the advertised Device
                // list: IDs are just indices 0..n, so a same-count swap
                // (vfio7 gone, vfio9 new) would look identical to the kubelet
                // while the CDI spec's index → cdev mapping went stale.
                let nums: Vec<u32> = vfio_devs.iter().map(|d| d.num).collect();

                if Some(&nums) != last.as_ref() {
                    info!(
                        count = nums.len(),
                        resource = server.resource.name,
                        "device list changed"
                    );
                    if vfio_devs.is_empty() {
                        // The last device vanished: remove the spec so it
                        // can't resolve to device nodes that no longer exist.
                        let cdi_file = cdi::spec_path(&server.cdi_dir, server.resource.name);
                        if let Err(e) = std::fs::remove_file(&cdi_file) {
                            if e.kind() != std::io::ErrorKind::NotFound {
                                tracing::warn!(%e, path = %cdi_file.display(), "stale CDI spec removal failed");
                            }
                        }
                    } else if let Err(e) =
                        cdi::write_cdi_spec(server.resource.name, &vfio_devs, &server.cdi_dir)
                    {
                        tracing::warn!("CDI spec write failed: {e:#}");
                    }
                    let devices = (0..nums.len())
                        .map(|idx| Device {
                            id: idx.to_string(),
                            health: "Healthy".to_owned(),
                            topology: None,
                        })
                        .collect();
                    last = Some(nums);
                    if tx.send(Ok(ListAndWatchResponse { devices })).await.is_err() {
                        break;
                    }
                }

                tokio::time::sleep(POLL_INTERVAL).await;
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }

    async fn get_preferred_allocation(
        &self,
        _: Request<PreferredAllocationRequest>,
    ) -> Result<Response<PreferredAllocationResponse>, Status> {
        Ok(Response::new(PreferredAllocationResponse::default()))
    }

    /// Only CDI names go back to the kubelet — actual device injection is the
    /// Kata shim's job, resolving them against the host CDI spec.
    async fn allocate(
        &self,
        req: Request<AllocateRequest>,
    ) -> Result<Response<AllocateResponse>, Status> {
        let resource_name = self.resource.name;
        let responses = req
            .into_inner()
            .container_requests
            .into_iter()
            .map(|cr| ContainerAllocateResponse {
                cdi_devices: cr
                    .devices_i_ds
                    .into_iter()
                    .map(|id| CdiDevice {
                        name: format!("{resource_name}={id}"),
                    })
                    .collect(),
                ..Default::default()
            })
            .collect();
        Ok(Response::new(AllocateResponse {
            container_responses: responses,
        }))
    }

    async fn pre_start_container(
        &self,
        _: Request<PreStartContainerRequest>,
    ) -> Result<Response<PreStartContainerResponse>, Status> {
        Ok(Response::new(PreStartContainerResponse {}))
    }
}
