#![forbid(unsafe_code)]

use kata_device_plugin::{plugin, vfio};

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use plugin::DeviceServer;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// The one flag.  An argument templates directly in the DaemonSet spec, so
/// no config file or ConfigMap returns (KISS).
fn parse_naming() -> anyhow::Result<vfio::Naming> {
    let mut naming = vfio::Naming::Alias;
    for arg in std::env::args().skip(1) {
        match arg.strip_prefix("--resource-naming=").map(str::trim) {
            Some(value) => naming = vfio::Naming::parse(value).map_err(anyhow::Error::msg)?,
            None => anyhow::bail!("usage: kata-device-plugin [--resource-naming=alias|sku]"),
        }
    }
    Ok(naming)
}

/// Spawn one DeviceServer for `name`.  The task removes itself from
/// `running` on exit, so a server that dies (bind failure, transient FS
/// error) is respawned by the rescan loop instead of being lost until the
/// pod restarts.
fn spawn_server(
    name: &str,
    naming: vfio::Naming,
    shutdown: &CancellationToken,
    running: &Arc<Mutex<HashSet<String>>>,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    info!(resource = %name, "starting plugin");
    let server = DeviceServer::new(
        name,
        naming,
        vfio::VFIO_DIR,
        vfio::SYSFS_ROOT,
        plugin::SOCKET_DIR,
        plugin::CDI_DIR,
    );
    let token = shutdown.clone();
    let label = name.to_owned();
    let set = running.clone();
    running.lock().unwrap().insert(name.to_owned());
    tasks.push(tokio::spawn(async move {
        if let Err(e) = server.run(token).await {
            // {:#} keeps the error's cause chain; bare Display drops it.
            tracing::warn!(resource = %label, "plugin error: {e:#}");
        }
        set.lock().unwrap().remove(&label);
    }));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kata_device_plugin=info".parse().unwrap()),
        )
        .init();
    let naming = parse_naming()?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("GIT_SHA"),
        naming = ?naming,
        "kata-device-plugin"
    );

    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("shutdown");
        sd.cancel();
    });

    let running: Arc<Mutex<HashSet<String>>> = Arc::default();
    let mut tasks = Vec::new();

    // Reconcile loop: spawn a server for every name that should exist but
    // has none running.  Alias row names are always desired so the kubelet
    // sees the resource (zero capacity included) regardless of deployment
    // ordering; SKU names only exist once a device is discovered.  Servers
    // are never stopped — a name whose devices vanish keeps advertising
    // zero capacity via its own ListAndWatch poller.
    loop {
        let mut desired: Vec<String> = vfio::discover(
            Path::new(vfio::VFIO_DIR),
            &vfio::Sysfs::new(Path::new(vfio::SYSFS_ROOT)),
            naming,
        )
        .into_keys()
        .collect();
        if matches!(naming, vfio::Naming::Alias) {
            desired.extend(vfio::RESOURCES.iter().map(|r| r.name.to_owned()));
        }
        for name in desired {
            if !running.lock().unwrap().contains(&name) {
                spawn_server(&name, naming, &shutdown, &running, &mut tasks);
            }
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(plugin::POLL_INTERVAL) => {}
        }
    }

    futures::future::join_all(tasks).await;
    Ok(())
}
