use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Endpoint, Server, Uri};
use tonic::{Request, Response, Status};
use tower::service_fn;
use tracing::info;

use std::collections::HashMap;
use std::sync::Arc;

use pcilibs_rs::fabric::{ENDPOINT_ENV, TOKEN_ENV};

use crate::cdi;
use crate::vfio::{self, Naming, Sysfs};
use crate::view::View;

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
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Every resource needs its own socket in the kubelet's shared plugin dir:
/// "nvidia.com/gpu" → "kata-gpu.sock".
fn socket_name(resource_name: &str) -> String {
    let base = resource_name.rsplit('/').next().unwrap_or(resource_name);
    format!("kata-{base}.sock")
}

/// Single concrete device plugin; tonic requires Clone.  One server per
/// resolved resource name — a RESOURCES row name in alias mode, a SKU name
/// like "nvidia.com/GH100_H100_SXM5_80GB" in sku mode.
#[derive(Clone)]
pub struct DeviceServer {
    name: String,
    naming: Naming,
    // Path fields rather than the constants so tests can inject temp dirs.
    device_dir: PathBuf,
    sysfs: Sysfs,
    socket_dir: PathBuf,
    cdi_dir: PathBuf,
    endpoint_shutdown: CancellationToken,
    /// Set on every server; only the fabric's uses it, at Allocate.
    view: Option<Arc<View>>,
    /// Where the fabric holder publishes to, when the node address is known.
    advertise: Option<String>,
}

impl DeviceServer {
    pub fn new(
        name: &str,
        naming: Naming,
        device_dir: &str,
        sysfs_root: &str,
        socket_dir: &str,
        cdi_dir: &str,
    ) -> Self {
        Self {
            name: name.to_owned(),
            naming,
            device_dir: PathBuf::from(device_dir),
            sysfs: Sysfs::new(Path::new(sysfs_root)),
            socket_dir: PathBuf::from(socket_dir),
            cdi_dir: PathBuf::from(cdi_dir),
            endpoint_shutdown: CancellationToken::new(),
            view: None,
            advertise: None,
        }
    }

    /// Make the fabric's Allocate hand its holder the publishing endpoint
    /// and a fresh token.  `advertise` is `http://<node ip>:<port>`, or
    /// `None` when the node address is unknown.
    pub fn with_view(mut self, view: Arc<View>, advertise: Option<String>) -> Self {
        self.view = Some(view);
        self.advertise = advertise;
        self
    }

    /// Whether this server hands out the node's fabric, the one device whose
    /// holder is the Service VM.
    fn serves_fabric(&self) -> bool {
        vfio::RESOURCES
            .iter()
            .any(|row| row.name == self.name && row.unit == vfio::Unit::Fabric)
    }

    /// The environment the fabric's new holder publishes with.  Rotating the
    /// token here is what makes the previous holder's copy stop working the
    /// moment the fabric changes hands.
    fn publisher_env(&self) -> Result<HashMap<String, String>, Status> {
        let Some(view) = self.view.as_ref().filter(|_| self.serves_fabric()) else {
            return Ok(HashMap::new());
        };
        let token = view.rotate_token().map_err(|e| {
            Status::unavailable(format!("fabric view token cannot be written: {e:#}"))
        })?;
        let mut envs = HashMap::from([(TOKEN_ENV.to_owned(), token)]);
        match &self.advertise {
            Some(endpoint) => {
                envs.insert(ENDPOINT_ENV.to_owned(), endpoint.clone());
                info!(
                    endpoint,
                    "fabric allocated: holder gets endpoint and a fresh token"
                );
            }
            None => info!("fabric allocated: holder gets a fresh token, no endpoint is known"),
        }
        Ok(envs)
    }

    /// This server's devices under the current scan: the discover() group
    /// matching our resolved name.
    fn my_devices(&self) -> Vec<vfio::Device> {
        vfio::discover(&self.device_dir, &self.sysfs, self.naming)
            .remove(&self.name)
            .unwrap_or_default()
    }

    pub async fn run(self, token: CancellationToken) -> anyhow::Result<()> {
        let result = self.serve_until_shutdown(&token).await;

        // Remove the CDI spec on clean shutdown so no stale device paths
        // remain on disk if the node is reconfigured while the plugin is absent.
        // On crash the file stays, but the ListAndWatch poller replaces it from
        // a fresh scan on the kubelet's first call after restart.
        let cdi_file = cdi::spec_path(&self.cdi_dir, &self.name);
        match tokio::fs::remove_file(&cdi_file).await {
            Ok(()) => info!(path = %cdi_file.display(), "removed CDI spec on shutdown"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(%e, path = %cdi_file.display(), "CDI spec removal on shutdown failed")
            }
        }

        result
    }

    async fn serve_until_shutdown(&self, token: &CancellationToken) -> anyhow::Result<()> {
        let sock_name = socket_name(&self.name);
        let socket = self.socket_dir.join(&sock_name);
        let kubelet = self.socket_dir.join("kubelet.sock");
        loop {
            if token.is_cancelled() {
                return Ok(());
            }
            match tokio::fs::remove_file(&socket).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("remove plugin socket"),
            }
            let listener = UnixListener::bind(&socket)
                .with_context(|| format!("bind {}", socket.display()))?;
            let identity = socket_identity(&socket).await?;
            info!(resource = %self.name, socket = %socket.display(), "plugin server starting");

            // Serve before registering: kubelet probes us inside Register.
            let endpoint_shutdown = token.child_token();
            let mut server = self.clone();
            server.endpoint_shutdown = endpoint_shutdown.clone();
            let mut serve = tokio::spawn(
                Server::builder()
                    .add_service(DevicePluginServer::new(server))
                    .serve_with_incoming(UnixListenerStream::new(listener)),
            );
            let result = tokio::select! {
                res = &mut serve => {
                    endpoint_shutdown.cancel();
                    return res.context("plugin server task")?.context("plugin server");
                },
                res = self.watch_registration(&kubelet, &socket, identity, &sock_name, token) => res,
            };
            // Aborting the listener alone leaves tonic's connection tasks alive.
            endpoint_shutdown.cancel();
            serve.abort();
            let _ = serve.await;
            result?;
            if token.is_cancelled() {
                return Ok(());
            }
            info!(resource = %self.name, "plugin socket removed or replaced; recreating endpoint");
        }
    }

    async fn watch_registration(
        &self,
        kubelet: &Path,
        socket: &Path,
        identity: Option<(u64, u64, i64, i64)>,
        endpoint: &str,
        token: &CancellationToken,
    ) -> anyhow::Result<()> {
        let mut registered = None;
        loop {
            if socket_identity(socket).await? != identity {
                return Ok(());
            }
            let current = socket_identity(kubelet).await?;
            if current.is_none() {
                registered = None;
            } else if registered != current {
                tokio::select! {
                    _ = token.cancelled() => return Ok(()),
                    result = tokio::time::timeout(
                        Duration::from_secs(15),
                        register(kubelet, &self.name, endpoint),
                    ) => match result {
                        Ok(Ok(())) => registered = current,
                        Ok(Err(e)) => tracing::warn!(resource = %self.name, "kubelet registration failed; retrying: {e:#}"),
                        Err(e) => tracing::warn!(resource = %self.name, "kubelet registration timed out; retrying: {e}"),
                    }
                }
            }
            tokio::select! {
                _ = token.cancelled() => return Ok(()),
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
    }

    /// Bring the on-disk CDI spec in line with `devs`: write it from this
    /// scan, or remove it when no devices remain.  Sync fs work runs off the
    /// async workers.  Callers decide the failure policy: the poller retries
    /// on the next tick, Allocate fails the RPC.
    async fn sync_cdi_spec(&self, devs: &[vfio::Device]) -> anyhow::Result<()> {
        if devs.is_empty() {
            let cdi_file = cdi::spec_path(&self.cdi_dir, &self.name);
            match tokio::fs::remove_file(&cdi_file).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| format!("remove {}", cdi_file.display())),
            }
        } else {
            let name = self.name.clone();
            let devs = devs.to_vec();
            let dir = self.cdi_dir.clone();
            tokio::task::spawn_blocking(move || cdi::write_cdi_spec(&name, &devs, &dir))
                .await
                .context("CDI spec write task")?
        }
    }
}

// Include ctime so rapid unlink/rebind cycles cannot hide inode reuse.
async fn socket_identity(path: &Path) -> anyhow::Result<Option<(u64, u64, i64, i64)>> {
    match tokio::fs::metadata(path).await {
        Ok(m) => Ok(Some((m.dev(), m.ino(), m.ctime(), m.ctime_nsec()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

async fn register(kubelet_sock: &Path, resource_name: &str, endpoint: &str) -> anyhow::Result<()> {
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
            // Two independent trackers: `sent` is what the kubelet last saw,
            // `synced` is what the on-disk CDI spec reflects.  A CDI failure
            // must retry next tick without re-sending an identical device
            // list every 5s (kubelet churn); a device change must reach the
            // kubelet even while the CDI registry is briefly unwritable.
            let mut sent: Option<Vec<Vec<u32>>> = None;
            let mut synced: Option<Vec<Vec<u32>>> = None;
            loop {
                // Exit when the kubelet drops the stream, even if the device
                // set never changes again — otherwise this task would poll
                // forever across kubelet reconnects.
                if tx.is_closed() {
                    break;
                }

                let vfio_devs = server.my_devices();
                // Compare the backing cdev numbers per device, not the
                // advertised Device list: IDs are just indices 0..n, so a
                // same-count swap (vfio7 gone, vfio9 new) would look identical
                // to the kubelet while the CDI spec's index → cdev mapping
                // went stale.
                let nums: Vec<Vec<u32>> = vfio_devs
                    .iter()
                    .map(|device| device.iter().map(|cdev| cdev.num).collect())
                    .collect();

                if Some(&nums) != synced.as_ref() {
                    match server.sync_cdi_spec(&vfio_devs).await {
                        Ok(()) => synced = Some(nums.clone()),
                        Err(e) => tracing::warn!("CDI spec sync failed, retrying: {e:#}"),
                    }
                }

                if Some(&nums) != sent.as_ref() {
                    info!(
                        count = nums.len(),
                        resource = %server.name,
                        "device list changed"
                    );
                    let devices = (0..nums.len())
                        .map(|idx| Device {
                            id: idx.to_string(),
                            health: "Healthy".to_owned(),
                            topology: None,
                        })
                        .collect();
                    sent = Some(nums);
                    tokio::select! {
                        _ = server.endpoint_shutdown.cancelled() => break,
                        result = tx.send(Ok(ListAndWatchResponse { devices })) => {
                            if result.is_err() {
                                break;
                            }
                        }
                    }
                }

                tokio::select! {
                    _ = server.endpoint_shutdown.cancelled() => break,
                    _ = tx.closed() => break,
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                }
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
        let resource_name = &self.name;
        // The device set can shrink between scheduling and Allocate.  Check
        // the requested IDs against a fresh enumeration so a stale ID fails
        // loudly here instead of as an unresolvable CDI name in the shim.
        let devs = self.my_devices();
        let present = devs.len();
        // The names below only work if the shim can resolve them at
        // SandboxCreate.  Sync the spec from the very enumeration we validate
        // against; if the CDI registry can't be written, fail the RPC now
        // instead of succeeding and producing an opaque runtime failure.
        if let Err(e) = self.sync_cdi_spec(&devs).await {
            return Err(Status::unavailable(format!(
                "CDI registry for {resource_name} is not writable: {e:#}"
            )));
        }
        let envs = self.publisher_env()?;
        let mut responses = Vec::new();
        for cr in req.into_inner().container_requests {
            let mut cdi_devices = Vec::new();
            for id in cr.devices_i_ds {
                match id.parse::<usize>() {
                    Ok(idx) if idx < present => cdi_devices.push(CdiDevice {
                        name: format!("{resource_name}={id}"),
                    }),
                    _ => {
                        return Err(Status::not_found(format!(
                            "device {id} of {resource_name} is no longer present \
                             ({present} devices bound)"
                        )))
                    }
                }
            }
            responses.push(ContainerAllocateResponse {
                cdi_devices,
                envs: envs.clone(),
                ..Default::default()
            });
        }
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
