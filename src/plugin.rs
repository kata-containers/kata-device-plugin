use std::path::PathBuf;

use anyhow::Context;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Endpoint, Server, Uri};
use tonic::{Request, Response, Status};
use tower::service_fn;
use tracing::info;

use crate::cdi;
use crate::dp::v1beta1::{
    device_plugin_server::{DevicePlugin, DevicePluginServer},
    registration_client::RegistrationClient,
    AllocateRequest, AllocateResponse, CdiDevice, ContainerAllocateResponse, Device,
    DevicePluginOptions, Empty, ListAndWatchResponse, PreStartContainerRequest,
    PreStartContainerResponse, PreferredAllocationRequest, PreferredAllocationResponse,
    RegisterRequest,
};

const HEALTHY: &str = "Healthy";

/// Derive a unique socket filename from the resource name.
/// "nvidia.com/gpu" → "kata-gpu.sock", "nvidia.com/nvswitch" → "kata-nvswitch.sock"
fn socket_name(resource_name: &str) -> String {
    let base = resource_name.rsplit('/').next().unwrap_or(resource_name);
    format!("kata-{base}.sock")
}

/// Which platform behaviour to activate.
/// Derived from the `nvidia.com/gpu.clique` label written by GFD.
#[derive(Clone, PartialEq)]
pub enum Mode {
    /// Plain GPU passthrough — no `nvidia.com/gpu.clique` label on the node.
    Pgpu,
    /// Multi-node NVLink — `nvidia.com/gpu.clique` is present and GFD confirmed
    /// GPU_FABRIC_STATE_COMPLETED.
    Imex { clique_id: String },
}

impl Mode {
    pub fn name(&self) -> &str {
        match self {
            Mode::Pgpu => "pgpu",
            Mode::Imex { .. } => "pgpu-imex",
        }
    }
}

/// Single concrete device plugin; tonic requires Clone.
#[derive(Clone)]
pub struct DeviceServer {
    resource_name: String,
    /// /dev/vfio — IOMMUFD devices are scanned under <device_dir>/devices/
    device_dir: PathBuf,
    socket_dir: PathBuf,
    cdi_dir: PathBuf,
    mode: Mode,
}

impl DeviceServer {
    pub fn new(
        resource_name: &str,
        device_dir: &str,
        socket_dir: &str,
        cdi_dir: &str,
        mode: Mode,
    ) -> Self {
        Self {
            resource_name: resource_name.to_owned(),
            device_dir: PathBuf::from(device_dir),
            socket_dir: PathBuf::from(socket_dir),
            cdi_dir: PathBuf::from(cdi_dir),
            mode,
        }
    }

    pub async fn run(self, token: CancellationToken) -> anyhow::Result<()> {
        if let Mode::Imex { ref clique_id } = self.mode {
            info!(clique_id, "IMEX mode");
        }

        // Write the host-side CDI spec before binding the socket so the Kata
        // shim can resolve our CDI device names as soon as we register.
        if let Err(e) = cdi::write_cdi_spec(
            &self.resource_name,
            &self.device_dir,
            &self.cdi_dir,
        ) {
            tracing::warn!(%e, "CDI spec write failed, continuing without it");
        }

        let sock_name = socket_name(&self.resource_name);
        let socket = self.socket_dir.join(&sock_name);
        let _ = tokio::fs::remove_file(&socket).await;
        let stream = UnixListenerStream::new(
            UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?,
        );

        info!(resource = %self.resource_name, %socket, "plugin server starting");

        let kubelet = self.socket_dir.join("kubelet.sock").to_string_lossy().into_owned();
        let resource = self.resource_name.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Err(e) = register(&kubelet, &resource, &sock_name).await {
                tracing::warn!(%e, "kubelet registration failed");
            }
        });

        Server::builder()
            .add_service(DevicePluginServer::new(self.clone()))
            .serve_with_incoming_shutdown(stream, token.cancelled())
            .await?;

        // Remove the CDI spec on clean shutdown so no stale device paths
        // remain on disk if the node is reconfigured while the plugin is absent.
        // On crash the file stays, but startup always overwrites it with a fresh scan.
        //
        // TODO: NVLink / IMEX mode needs more thought here.  In a multi-node
        // NVLink partition, VFIO renumbering on one node may invalidate CDI
        // specs on peer nodes that share the same clique.  A clean shutdown of
        // this plugin does not signal peer nodes, and the IMEX mesh may be in
        // a partially-torn state.  Revisit when NVLink partition lifecycle
        // (join / leave / reconfigure) is fully specced.
        let cdi_file = self.cdi_dir.join(
            format!("{}.yaml", self.resource_name.replace('/', "-"))
        );
        if let Err(e) = tokio::fs::remove_file(&cdi_file).await {
            tracing::warn!(%e, path = %cdi_file.display(), "CDI spec removal on shutdown failed");
        } else {
            info!(path = %cdi_file.display(), "removed CDI spec on shutdown");
        }

        Ok(())
    }

    /// Enumerate IOMMUFD char devices under `<device_dir>/devices/`.
    /// Entries matching `vfio[0-9]+` are sorted numerically and assigned
    /// sequential device IDs `"0"`, `"1"`, ... that match CDI spec indices.
    fn devices(&self) -> Vec<Device> {
        let devices_dir = self.device_dir.join("devices");
        let Ok(rd) = std::fs::read_dir(&devices_dir) else {
            return vec![];
        };
        let mut vfio_nums: Vec<u32> = rd
            .flatten()
            .filter_map(|e| {
                e.file_name().to_str()?.strip_prefix("vfio")?.parse::<u32>().ok()
            })
            .collect();
        vfio_nums.sort();
        let devs: Vec<Device> = vfio_nums
            .iter()
            .enumerate()
            .map(|(idx, _)| Device {
                id: idx.to_string(),
                health: HEALTHY.to_owned(),
                topology: None,
            })
            .collect();
        info!(count = devs.len(), "discovered IOMMUFD devices");
        devs
    }
}

async fn register(kubelet_sock: &str, resource_name: &str, endpoint: &str) -> anyhow::Result<()> {
    let sock = kubelet_sock.to_owned();
    let channel = Endpoint::try_from("http://[::]:0")?
        .connect_with_connector(service_fn(move |_: Uri| {
            tokio::net::UnixStream::connect(sock.clone())
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
        let resp = ListAndWatchResponse { devices: self.devices() };
        let stream = futures::stream::once(futures::future::ready(Ok(resp)));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_preferred_allocation(
        &self,
        _: Request<PreferredAllocationRequest>,
    ) -> Result<Response<PreferredAllocationResponse>, Status> {
        Ok(Response::new(PreferredAllocationResponse::default()))
    }

    /// Return CDI device names so the container runtime (and Kata's shim)
    /// know which device index maps to each container.
    /// Kata's shim resolves these via the host CDI spec we wrote at startup.
    async fn allocate(
        &self,
        req: Request<AllocateRequest>,
    ) -> Result<Response<AllocateResponse>, Status> {
        let resource_name = self.resource_name.clone();
        let responses = req
            .into_inner()
            .container_requests
            .into_iter()
            .map(|cr| ContainerAllocateResponse {
                cdi_devices: cr
                    .devices_i_ds
                    .into_iter()
                    .map(|id| CdiDevice { name: format!("{resource_name}={id}") })
                    .collect(),
                ..Default::default()
            })
            .collect();
        Ok(Response::new(AllocateResponse { container_responses: responses }))
    }

    async fn pre_start_container(
        &self,
        _: Request<PreStartContainerRequest>,
    ) -> Result<Response<PreStartContainerResponse>, Status> {
        Ok(Response::new(PreStartContainerResponse {}))
    }
}
