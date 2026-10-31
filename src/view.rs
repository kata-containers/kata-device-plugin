//! The fabric view: what the Service VM's Fabric Manager client publishes,
//! held for `discover()` to resolve against the node's GPUs.
//!
//! Only the holder of `nvidia.com/nvswitch-fabric` may publish.  Allocate
//! hands that one container an endpoint and a token through its environment,
//! and the TCP listener admits only that token.  The unix socket on the host
//! admits whoever can open it: its permissions are the credential there.
//!
//! One stream at a time.  A newer stream replaces the current one, which is
//! ended, so a publisher that restarts does not have to wait for its old
//! stream to time out.  The open stream is the view's liveness: when it
//! closes, the view is withdrawn after a grace period, long enough for the
//! publisher's reconnect and short enough that a dead Service VM does not
//! keep its partitions advertised.  A new fabric holder withdraws the view at
//! once: the partitions it describes were the previous holder's.

use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use pcilibs_rs::fabric::grpc::{require_token, FabricViewService, FabricViewServiceServer};
use pcilibs_rs::fabric::v1::{FabricView, PublishSummary};
use pcilibs_rs::fabric::HOST_SOCKET;
use tokio::net::{TcpListener, UnixListener};
use tokio_stream::wrappers::{TcpListenerStream, UnixListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};
use tracing::{info, warn};

/// The plugin's own state on the host: the token and the unix socket.  A
/// pod restart keeps it; a node reboot clears it along with every Service
/// VM that held a token.
pub const STATE_DIR: &str = "/var/run/kata-device-plugin";
/// The host socket, a pcilibs-rs contract shared with the publisher.
pub const SOCKET: &str = HOST_SOCKET;
const TOKEN_FILE: &str = "fabric-view.token";
/// Where in-band publishers reach the plugin: the pod's port, exposed on the
/// node by the chart.  Fixed, like the kubelet's socket directory.
pub const PORT: u16 = 9877;
/// The node address the chart passes in from the downward API
/// (`status.hostIP`).  The plugin advertises `http://<ip>:PORT` to the
/// fabric holder; without it the holder gets a token and no endpoint.
pub const HOST_IP_ENV: &str = "KATA_DEVICE_PLUGIN_HOST_IP";
/// Three of the publisher's five-second polls: a reconnect lands well
/// inside it, a dead publisher is noticed before the kubelet's next look.
pub const GRACE: Duration = Duration::from_secs(15);

/// The publisher's stream, which is the view's liveness.
enum Stream {
    None,
    Open,
    Closed(Instant),
}

struct State {
    token: String,
    current: Option<FabricView>,
    /// Advanced by every new stream and every new fabric holder, so a
    /// Publish call can tell that it has been superseded.
    generation: u64,
    stream: Stream,
}

impl State {
    fn alive(&self, grace: Duration) -> bool {
        match self.stream {
            Stream::Open => true,
            Stream::Closed(at) => at.elapsed() < grace,
            Stream::None => false,
        }
    }
}

/// The published view and the token that gates publishing, shared by the
/// listeners, Allocate, and discover().
pub struct View {
    state: Mutex<State>,
    token_path: PathBuf,
    grace: Duration,
}

impl View {
    /// Load the token the last plugin instance issued, or mint one, so a
    /// Service VM that outlives a plugin restart keeps publishing.
    pub fn load(state_dir: &Path) -> anyhow::Result<Arc<Self>> {
        Self::with_grace(state_dir, GRACE)
    }

    pub fn with_grace(state_dir: &Path, grace: Duration) -> anyhow::Result<Arc<Self>> {
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
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                token,
                current: None,
                generation: 0,
                stream: Stream::None,
            }),
            token_path,
            grace,
        }))
    }

    /// The token a publisher must present on the TCP listener.
    pub fn token(&self) -> String {
        self.state.lock().unwrap().token.clone()
    }

    /// A new fabric holder gets a new token and the previous holder's stream
    /// is superseded, so its copy stops working the moment the fabric changes
    /// hands.  Its view goes too: the partitions were the previous holder's.
    pub fn rotate_token(&self) -> anyhow::Result<String> {
        let mut state = self.state.lock().unwrap();
        let token = write_token(&self.token_path)?;
        state.token = token.clone();
        state.generation += 1;
        state.stream = Stream::None;
        if state.current.take().is_some() {
            info!("fabric view: withdrawn, the fabric has a new holder");
        }
        Ok(token)
    }

    /// The view discover() may act on: the last one published, while its
    /// stream is open or closed less than the grace period ago.
    pub fn current(&self) -> Option<FabricView> {
        let state = self.state.lock().unwrap();
        state
            .alive(self.grace)
            .then(|| state.current.clone())
            .flatten()
    }

    fn opened(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        // A view past its grace period is gone; the new stream must not
        // bring it back for the time until its first message arrives.
        if !state.alive(self.grace) {
            state.current = None;
        }
        if matches!(state.stream, Stream::Open) {
            info!("fabric view: a newer publisher replaces the current stream");
        }
        state.generation += 1;
        state.stream = Stream::Open;
        state.generation
    }

    /// False when `generation` is no longer the current stream.
    fn received(&self, generation: u64, view: FabricView) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return false;
        }
        if state.current.as_ref() != Some(&view) {
            info!("fabric view: {view}");
        }
        state.current = Some(view);
        true
    }

    fn closed(&self, generation: u64) {
        let mut state = self.state.lock().unwrap();
        if state.generation == generation {
            state.stream = Stream::Closed(Instant::now());
            info!(
                grace_secs = self.grace.as_secs(),
                "fabric view: stream closed"
            );
        }
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
    // Write-then-rename so a reader never sees a partial token.
    let tmp = path.with_extension("tmp");
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| io::Write::write_all(&mut file, token.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(token)
}

/// The Publish stream's server end.
struct Service {
    view: Arc<View>,
    shutdown: CancellationToken,
}

#[tonic::async_trait]
impl FabricViewService for Service {
    async fn publish(
        &self,
        request: Request<Streaming<FabricView>>,
    ) -> Result<Response<PublishSummary>, Status> {
        let generation = self.view.opened();
        let mut stream = request.into_inner();
        let mut received = 0u32;
        // Shutdown ends the stream from this side: the server's graceful
        // shutdown waits for open streams, and this one never ends on its own.
        let outcome = loop {
            let message = tokio::select! {
                message = stream.message() => message,
                () = self.shutdown.cancelled() => {
                    break Err(Status::unavailable("the plugin is shutting down"))
                }
            };
            match message {
                Ok(Some(view)) => {
                    if !self.view.received(generation, view) {
                        break Err(Status::aborted(
                            "superseded by a newer stream or a new fabric holder",
                        ));
                    }
                    received += 1;
                }
                Ok(None) => break Ok(()),
                Err(status) => break Err(status),
            }
        };
        self.view.closed(generation);
        outcome.map(|()| {
            Response::new(PublishSummary {
                views_received: received,
            })
        })
    }
}

/// Bind the production listeners: the host socket, and the port for
/// in-band publishers on every interface, since the pod's address is
/// whatever the node gives it.
pub async fn bind(socket: &Path) -> anyhow::Result<(UnixListener, TcpListener)> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove {}", socket.display())),
    }
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
    // Pings every third of the grace period, answered within another third:
    // a publisher that dies without closing its stream is closed within two
    // thirds of the grace period, and its view withdrawn one grace period
    // after that.
    let keepalive = view.grace / 3;
    let server = || {
        Server::builder()
            .http2_keepalive_interval(Some(keepalive))
            .http2_keepalive_timeout(Some(keepalive))
    };
    let service = || Service {
        view: view.clone(),
        shutdown: shutdown.clone(),
    };
    let host = server()
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
    let pod = server()
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

    use hyper_util::rt::TokioIo;
    use pcilibs_rs::fabric::grpc::{with_token, FabricViewServiceClient};
    use pcilibs_rs::fabric::v1;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
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
        _dir: TempDir,
    }

    async fn node(grace: Duration) -> Node {
        let dir = TempDir::new().unwrap();
        let view = View::with_grace(dir.path(), grace).unwrap();
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
            _dir: dir,
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

    /// A publisher: sends what it is given, reports how the stream ended.
    struct Publisher {
        tx: mpsc::Sender<FabricView>,
        done: tokio::task::JoinHandle<Result<PublishSummary, Status>>,
    }

    async fn publish(channel: Channel, token: Option<&str>) -> Publisher {
        let (tx, rx) = mpsc::channel(4);
        let mut request = Request::new(ReceiverStream::new(rx));
        if let Some(token) = token {
            request = with_token(request, token).unwrap();
        }
        let mut client = FabricViewServiceClient::new(channel);
        let done = tokio::spawn(async move {
            client
                .publish(request)
                .await
                .map(|response| response.into_inner())
        });
        Publisher { tx, done }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// Poll `view` until `pred` holds, within two seconds.
    async fn until(view: &View, pred: impl Fn(Option<FabricView>) -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !pred(view.current()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the view did not reach the expected state");
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
    async fn the_host_socket_takes_views_without_a_token() {
        let node = node(GRACE).await;
        assert_eq!(node.view.current(), None);
        let publisher = publish(unix_channel(&node.socket).await, None).await;
        publisher.tx.send(view("", &[1])).await.unwrap();
        settle().await;
        assert_eq!(node.view.current(), Some(view("", &[1])));
        publisher.tx.send(view("gpu", &[2, 3])).await.unwrap();
        settle().await;
        assert_eq!(node.view.current(), Some(view("gpu", &[2, 3])));
        drop(publisher.tx);
        let summary = publisher.done.await.unwrap().unwrap();
        assert_eq!(summary.views_received, 2);
        node.stop.cancel();
    }

    #[tokio::test]
    async fn the_port_admits_only_the_current_token() {
        let node = node(GRACE).await;
        let wrong = publish(tcp_channel(node.port).await, Some("wrong")).await;
        let status = wrong.done.await.unwrap().unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        let none = publish(tcp_channel(node.port).await, None).await;
        assert_eq!(
            none.done.await.unwrap().unwrap_err().code(),
            Code::Unauthenticated
        );

        let token = node.view.token();
        let right = publish(tcp_channel(node.port).await, Some(&token)).await;
        right.tx.send(view("", &[4])).await.unwrap();
        settle().await;
        assert_eq!(node.view.current(), Some(view("", &[4])));

        // A new fabric holder: the view is withdrawn at once, the previous
        // holder's open stream ends at its next message, and its token no
        // longer opens a stream.
        let rotated = node.view.rotate_token().unwrap();
        assert_eq!(node.view.current(), None);
        right.tx.send(view("", &[4])).await.unwrap();
        assert_eq!(right.done.await.unwrap().unwrap_err().code(), Code::Aborted);
        let stale = publish(tcp_channel(node.port).await, Some(&token)).await;
        assert_eq!(
            stale.done.await.unwrap().unwrap_err().code(),
            Code::Unauthenticated
        );
        let current = publish(tcp_channel(node.port).await, Some(&rotated)).await;
        current.tx.send(view("", &[5])).await.unwrap();
        settle().await;
        assert_eq!(node.view.current(), Some(view("", &[5])));
        node.stop.cancel();
    }

    #[tokio::test]
    async fn a_newer_stream_replaces_the_current_one() {
        let node = node(GRACE).await;
        let first = publish(unix_channel(&node.socket).await, None).await;
        first.tx.send(view("", &[1])).await.unwrap();
        settle().await;
        let second = publish(unix_channel(&node.socket).await, None).await;
        second.tx.send(view("", &[2])).await.unwrap();
        settle().await;
        assert_eq!(node.view.current(), Some(view("", &[2])));
        // The first stream's next message is refused and ends it.
        first.tx.send(view("", &[3])).await.unwrap();
        let status = first.done.await.unwrap().unwrap_err();
        assert_eq!(status.code(), Code::Aborted);
        assert_eq!(node.view.current(), Some(view("", &[2])));
        // Its end does not withdraw the second stream's view.
        drop(second.tx);
        second.done.await.unwrap().unwrap();
        node.stop.cancel();
    }

    #[tokio::test]
    async fn a_closed_stream_withdraws_the_view_after_the_grace_period() {
        let grace = Duration::from_millis(300);
        let node = node(grace).await;
        let publisher = publish(unix_channel(&node.socket).await, None).await;
        publisher.tx.send(view("", &[1])).await.unwrap();
        settle().await;
        drop(publisher.tx);
        publisher.done.await.unwrap().unwrap();
        // Within the grace period the view stands, so a reconnect does not
        // flap the node's capacity.
        assert_eq!(node.view.current(), Some(view("", &[1])));
        tokio::time::sleep(grace + settle_time()).await;
        assert_eq!(node.view.current(), None);

        // A stream that opens after the grace period does not bring the
        // old view back before it publishes its own.
        let late = publish(unix_channel(&node.socket).await, None).await;
        settle().await;
        assert_eq!(node.view.current(), None);
        late.tx.send(view("", &[1])).await.unwrap();
        settle().await;
        assert_eq!(node.view.current(), Some(view("", &[1])));
        drop(late.tx);
        late.done.await.unwrap().unwrap();

        // A reconnect inside the grace period keeps the view alive.
        let back = publish(unix_channel(&node.socket).await, None).await;
        back.tx.send(view("", &[1])).await.unwrap();
        settle().await;
        tokio::time::sleep(grace + settle_time()).await;
        assert_eq!(node.view.current(), Some(view("", &[1])));
        node.stop.cancel();
    }

    fn settle_time() -> Duration {
        Duration::from_millis(50)
    }

    #[tokio::test]
    async fn a_publisher_that_stops_answering_pings_is_closed() {
        // A Service VM that dies hard leaves its TCP connection half open:
        // nothing closes the stream.  The server's keepalive pings go
        // unanswered, the stream is closed, and the grace period runs out.
        let grace = Duration::from_millis(300);
        let node = node(grace).await;
        let socket = node.socket.clone();
        let shared = node.view.clone();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let client = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let publisher = runtime.block_on(async {
                let publisher = publish(unix_channel(&socket).await, None).await;
                publisher.tx.send(view("", &[1])).await.unwrap();
                until(&shared, |current| current.is_some()).await;
                publisher
            });
            // The runtime is not polled from here on, so no ping is answered
            // while the socket stays open.
            let _ = held.recv();
            drop(publisher);
            drop(runtime);
        });
        until(&node.view, |current| current.is_some()).await;
        until(&node.view, |current| current.is_none()).await;
        drop(release);
        client.join().unwrap();
        node.stop.cancel();
    }

    #[tokio::test]
    async fn shutdown_ends_the_open_stream_and_the_service() {
        let node = node(GRACE).await;
        let publisher = publish(unix_channel(&node.socket).await, None).await;
        publisher.tx.send(view("", &[1])).await.unwrap();
        settle().await;
        node.stop.cancel();
        let status = tokio::time::timeout(Duration::from_secs(2), publisher.done)
            .await
            .expect("the stream outlived the shutdown")
            .unwrap()
            .unwrap_err();
        assert_eq!(status.code(), Code::Unavailable);
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
