//! The fabric view: what the Service VM's Fabric Manager client publishes,
//! held for `discover()` to resolve against the node's GPUs.
//!
//! Only the holder of `nvidia.com/nvswitch-fabric` may publish.  Allocate
//! hands that one container an endpoint and a token through its environment,
//! and the TCP listener admits only that token.  The unix socket on the host
//! admits whoever can open it: its permissions are the credential there.
//!
//! One call replaces the view and the plugin keeps it, on disk next to the
//! token so a plugin restart keeps the node's capacity, until the next call
//! or until the fabric changes hands, when the partitions it describes were
//! the previous holder's.  The plugin is a scheduling ledger and Fabric
//! Manager is the enforcement point: a stale view sends a pod to GPUs whose
//! fabric is not ready, where CUDA init fails, and cannot hand out another
//! partition's GPUs.  So the command that changes partitions publishes the
//! result and exits; nothing stays connected.

use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use kata_fabric_api::grpc::{require_token, FabricViewService, FabricViewServiceServer};
use kata_fabric_api::v1::{FabricView, PublishResponse};
use kata_fabric_api::HOST_SOCKET;
use tokio::net::{TcpListener, UnixListener};
use tokio_stream::wrappers::{TcpListenerStream, UnixListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

/// The plugin's own state on the host: the token, the last view and the unix
/// socket.  A pod restart keeps it; a node reboot clears it along with every
/// Service VM that held a token.
pub const STATE_DIR: &str = "/var/run/kata-device-plugin";
/// The host socket, a kata-fabric-api contract shared with the publisher.
pub const SOCKET: &str = HOST_SOCKET;
const TOKEN_FILE: &str = "fabric-view.token";
/// The last view published, as JSON with fmpm's field names, so an operator
/// can read what the node is working from.
const VIEW_FILE: &str = "fabric-view.json";
/// Where in-band publishers reach the plugin: the pod's port, exposed on the
/// node by the chart.  Fixed, like the kubelet's socket directory.
pub const PORT: u16 = 9877;
/// The node address the chart passes in from the downward API
/// (`status.hostIP`).  The plugin advertises `http://<ip>:PORT` to the
/// fabric holder; without it the holder gets a token and no endpoint.
pub const HOST_IP_ENV: &str = "KATA_DEVICE_PLUGIN_HOST_IP";

struct State {
    token: String,
    current: Option<FabricView>,
}

/// The published view and the token that gates publishing, shared by the
/// listeners, Allocate, and discover().
pub struct View {
    state: Mutex<State>,
    token_path: PathBuf,
    view_path: PathBuf,
}

impl View {
    /// Load the token the last plugin instance issued, or mint one, and the
    /// view it last kept, so a Service VM that outlives a plugin restart keeps
    /// publishing and the node keeps its capacity.
    pub fn load(state_dir: &Path) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create {}", state_dir.display()))?;
        let token_path = state_dir.join(TOKEN_FILE);
        let token = match std::fs::read_to_string(&token_path) {
            Ok(token) if is_token(token.trim()) => token.trim().to_owned(),
            Ok(_) => {
                warn!(path = %token_path.display(), "token file is not a token, minting a new one");
                write_token(&token_path)?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => write_token(&token_path)?,
            Err(e) => return Err(e).with_context(|| format!("read {}", token_path.display())),
        };
        let view_path = state_dir.join(VIEW_FILE);
        let current = match std::fs::read(&view_path) {
            Ok(bytes) => match serde_json::from_slice::<FabricView>(&bytes) {
                Ok(view) => {
                    info!("fabric view: {view}, kept from the last run");
                    Some(view)
                }
                Err(e) => {
                    warn!(path = %view_path.display(), "view file is not a view, ignored: {e}");
                    None
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("read {}", view_path.display())),
        };
        Ok(Arc::new(Self {
            state: Mutex::new(State { token, current }),
            token_path,
            view_path,
        }))
    }

    /// The token a publisher must present on the TCP listener.
    pub fn token(&self) -> String {
        self.state.lock().unwrap().token.clone()
    }

    /// A new fabric holder gets a new token, so the previous holder's copy
    /// stops working the moment the fabric changes hands.  Its view goes too:
    /// the partitions were the previous holder's.
    pub fn rotate_token(&self) -> anyhow::Result<String> {
        let mut state = self.state.lock().unwrap();
        let token = write_token(&self.token_path)?;
        state.token = token.clone();
        if state.current.take().is_some() {
            remove(&self.view_path)?;
            info!("fabric view: withdrawn, the fabric has a new holder");
        }
        Ok(token)
    }

    /// The view discover() acts on: the last one published.
    pub fn current(&self) -> Option<FabricView> {
        self.state.lock().unwrap().current.clone()
    }

    /// Replace the view, on disk first: a view the plugin cannot keep is
    /// refused, so the file and the ledger agree at the next start.
    fn publish(&self, view: FabricView) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        let json = serde_json::to_vec_pretty(&view).context("encode the fabric view")?;
        write(&self.view_path, &json, 0o644)?;
        if state.current.as_ref() != Some(&view) {
            info!("fabric view: {view}");
        }
        state.current = Some(view);
        Ok(())
    }
}

/// The endpoint an in-band publisher reaches this node at, from the node
/// address the chart passes in; `None` when that is not an IP address.
pub fn endpoint(host_ip: &str) -> Option<String> {
    let ip: IpAddr = host_ip.trim().parse().ok()?;
    // SocketAddr brackets an IPv6 address, as a URL authority requires.
    Some(format!("http://{}", SocketAddr::new(ip, PORT)))
}

fn is_token(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 256 random bits as hex, written readable by the owner only.
fn write_token(path: &Path) -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut bytes))
        .context("read /dev/urandom")?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    write(path, token.as_bytes(), 0o600)?;
    Ok(token)
}

/// Write-then-rename so a reader never sees a partial file.
fn write(path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .and_then(|mut file| io::Write::write_all(&mut file, bytes))
        .and_then(|()| std::fs::rename(&tmp, path))
        .with_context(|| format!("write {}", path.display()))
}

fn remove(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// The Publish call's server end.
struct Service {
    view: Arc<View>,
}

#[tonic::async_trait]
impl FabricViewService for Service {
    async fn publish(
        &self,
        request: Request<FabricView>,
    ) -> Result<Response<PublishResponse>, Status> {
        self.view
            .publish(request.into_inner())
            .map_err(|e| Status::unavailable(format!("the view cannot be kept: {e:#}")))?;
        Ok(Response::new(PublishResponse {}))
    }
}

/// Bind the production listeners: the host socket, and the port for
/// in-band publishers on every interface, since the pod's address is
/// whatever the node gives it.
pub async fn bind(socket: &Path) -> anyhow::Result<(UnixListener, TcpListener)> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    remove(socket)?;
    let unix = UnixListener::bind(socket).with_context(|| format!("bind {}", socket.display()))?;
    // Linux binds the IPv6 wildcard dual-stack, so it takes IPv4 connections
    // too; a pod without IPv6 cannot bind it and gets the IPv4 wildcard.
    let tcp = match TcpListener::bind((Ipv6Addr::UNSPECIFIED, PORT)).await {
        Ok(tcp) => tcp,
        Err(_) => TcpListener::bind((Ipv4Addr::UNSPECIFIED, PORT))
            .await
            .with_context(|| format!("bind port {PORT}"))?,
    };
    Ok((unix, tcp))
}

/// Serve both listeners until shutdown.  The unix socket checks no token;
/// the TCP listener admits only the current one, read per request so a
/// rotation takes effect at once.
pub async fn serve(
    view: Arc<View>,
    unix: UnixListener,
    tcp: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    info!(socket = %HOST_SOCKET, port = PORT, "fabric view service starting");
    let service = || Service { view: view.clone() };
    let host = Server::builder()
        .add_service(FabricViewServiceServer::new(service()))
        .serve_with_incoming_shutdown(
            UnixListenerStream::new(unix),
            shutdown.clone().cancelled_owned(),
        );
    let gate = view.clone();
    let authenticated =
        FabricViewServiceServer::with_interceptor(service(), move |request: Request<()>| {
            require_token(&gate.token())(request)
        });
    let pod = Server::builder()
        .add_service(authenticated)
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(tcp),
            shutdown.clone().cancelled_owned(),
        );
    let (host, pod) = tokio::join!(host, pod);
    host.context("fabric view host socket")?;
    pod.context("fabric view port")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::FileTypeExt;
    use std::time::Duration;

    use hyper_util::rt::TokioIo;
    use kata_fabric_api::grpc::{with_token, FabricViewServiceClient};
    use kata_fabric_api::v1;
    use tempfile::TempDir;
    use tonic::transport::{Channel, Endpoint, Uri};
    use tonic::Code;
    use tower::service_fn;

    fn view(unit: &str, active: &[u32]) -> FabricView {
        FabricView {
            partitions: Some(v1::PartitionList {
                version: 1,
                max_num_partitions: 15,
                partition_info: active
                    .iter()
                    .map(|&id| v1::Partition {
                        partition_id: id,
                        is_active: true,
                        gpu_info: vec![v1::PartitionGpu::default(); 2],
                    })
                    .collect(),
            }),
            unsupported: None,
            unit: unit.to_owned(),
        }
    }

    struct Node {
        view: Arc<View>,
        socket: PathBuf,
        port: u16,
        stop: CancellationToken,
        served: tokio::task::JoinHandle<anyhow::Result<()>>,
        dir: TempDir,
    }

    async fn node() -> Node {
        let dir = TempDir::new().unwrap();
        let view = View::load(dir.path()).unwrap();
        let socket = dir.path().join("fabric-view.sock");
        let unix = UnixListener::bind(&socket).unwrap();
        let tcp = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        let stop = CancellationToken::new();
        let served = tokio::spawn(serve(view.clone(), unix, tcp, stop.clone()));
        Node {
            view,
            socket,
            port,
            stop,
            served,
            dir,
        }
    }

    async fn unix_channel(socket: &Path) -> Channel {
        let path = socket.to_owned();
        Endpoint::try_from("http://[::]:0")
            .unwrap()
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    tokio::net::UnixStream::connect(path)
                        .await
                        .map(TokioIo::new)
                }
            }))
            .await
            .unwrap()
    }

    async fn tcp_channel(port: u16) -> Channel {
        Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
            .unwrap()
            .connect()
            .await
            .unwrap()
    }

    /// One Publish call, as fmclient makes it.
    async fn publish(
        channel: Channel,
        token: Option<&str>,
        view: FabricView,
    ) -> Result<(), Status> {
        let mut request = Request::new(view);
        if let Some(token) = token {
            request = with_token(request, token).unwrap();
        }
        FabricViewServiceClient::new(channel)
            .publish(request)
            .await
            .map(drop)
    }

    #[test]
    fn token_is_minted_once_and_rotated_on_demand() {
        let dir = TempDir::new().unwrap();
        let first = View::load(dir.path()).unwrap();
        let token = first.token();
        assert!(is_token(&token));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(TOKEN_FILE)).unwrap(),
            token
        );
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(dir.path().join(TOKEN_FILE))
                .unwrap()
                .permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);

        // A restart keeps the token a Service VM already holds.
        let again = View::load(dir.path()).unwrap();
        assert_eq!(again.token(), token);

        let rotated = again.rotate_token().unwrap();
        assert_ne!(rotated, token);
        assert_eq!(again.token(), rotated);
        assert_eq!(View::load(dir.path()).unwrap().token(), rotated);

        // Garbage in the file is replaced, not trusted.
        std::fs::write(dir.path().join(TOKEN_FILE), "not a token").unwrap();
        let fresh = View::load(dir.path()).unwrap();
        assert!(is_token(&fresh.token()) && fresh.token() != rotated);
        assert!(!is_token("abc") && !is_token(&"g".repeat(64)));
    }

    #[test]
    fn load_fails_when_the_state_dir_cannot_be_made() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(View::load(&file.join("state")).is_err());
    }

    #[test]
    fn the_endpoint_is_a_url_for_either_address_family() {
        assert_eq!(
            endpoint("10.0.0.7").unwrap(),
            format!("http://10.0.0.7:{PORT}")
        );
        assert_eq!(
            endpoint("fd00::7\n").unwrap(),
            format!("http://[fd00::7]:{PORT}")
        );
        assert_eq!(endpoint(""), None);
        assert_eq!(endpoint("node-1"), None);
    }

    #[tokio::test]
    async fn the_host_socket_takes_views_without_a_token_and_the_last_one_stands() {
        let node = node().await;
        assert_eq!(node.view.current(), None);
        publish(unix_channel(&node.socket).await, None, view("", &[1]))
            .await
            .unwrap();
        assert_eq!(node.view.current(), Some(view("", &[1])));
        publish(unix_channel(&node.socket).await, None, view("gpu", &[2, 3]))
            .await
            .unwrap();
        assert_eq!(node.view.current(), Some(view("gpu", &[2, 3])));
        node.stop.cancel();
    }

    #[tokio::test]
    async fn the_port_admits_only_the_current_token() {
        let node = node().await;
        let wrong = publish(tcp_channel(node.port).await, Some("wrong"), view("", &[4])).await;
        assert_eq!(wrong.unwrap_err().code(), Code::Unauthenticated);
        let none = publish(tcp_channel(node.port).await, None, view("", &[4])).await;
        assert_eq!(none.unwrap_err().code(), Code::Unauthenticated);
        assert_eq!(node.view.current(), None);

        let token = node.view.token();
        publish(tcp_channel(node.port).await, Some(&token), view("", &[4]))
            .await
            .unwrap();
        assert_eq!(node.view.current(), Some(view("", &[4])));

        // A new fabric holder: the view is withdrawn at once and the previous
        // holder's token no longer publishes.
        let rotated = node.view.rotate_token().unwrap();
        assert_eq!(node.view.current(), None);
        let stale = publish(tcp_channel(node.port).await, Some(&token), view("", &[4])).await;
        assert_eq!(stale.unwrap_err().code(), Code::Unauthenticated);
        publish(tcp_channel(node.port).await, Some(&rotated), view("", &[5]))
            .await
            .unwrap();
        assert_eq!(node.view.current(), Some(view("", &[5])));
        node.stop.cancel();
    }

    #[tokio::test]
    async fn the_last_view_outlives_a_restart_and_not_a_new_holder() {
        let node = node().await;
        publish(unix_channel(&node.socket).await, None, view("", &[1]))
            .await
            .unwrap();
        let file = node.dir.path().join(VIEW_FILE);
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(json["partitions"]["partitionInfo"][0]["partitionId"], 1);

        // A plugin restart keeps the node's capacity.
        let restarted = View::load(node.dir.path()).unwrap();
        assert_eq!(restarted.current(), Some(view("", &[1])));

        // A new fabric holder withdraws the view, and a restart after that
        // does not bring it back.
        restarted.rotate_token().unwrap();
        assert_eq!(restarted.current(), None);
        assert!(!file.exists());
        assert_eq!(View::load(node.dir.path()).unwrap().current(), None);

        // A file that is not a view is ignored, not trusted.
        std::fs::write(&file, "not a view").unwrap();
        assert_eq!(View::load(node.dir.path()).unwrap().current(), None);
        node.stop.cancel();
    }

    #[tokio::test]
    async fn shutdown_ends_the_service() {
        let node = node().await;
        node.stop.cancel();
        tokio::time::timeout(Duration::from_secs(2), node.served)
            .await
            .expect("the service outlived the shutdown")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn bind_replaces_a_stale_socket_and_serves_the_port() {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("sub").join("fabric-view.sock");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(&socket, "stale").unwrap();
        match bind(&socket).await {
            Ok((unix, _tcp)) => {
                assert!(unix.local_addr().unwrap().as_pathname().is_some());
                assert!(std::fs::metadata(&socket).unwrap().file_type().is_socket());
            }
            // The fixed port may be taken on a shared test host; the socket
            // half must still have been replaced before the port was tried.
            Err(e) => assert!(e.to_string().contains(&PORT.to_string()), "{e:#}"),
        }
    }
}
