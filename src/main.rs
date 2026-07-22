#![forbid(unsafe_code)]

use kata_device_plugin::{plugin, vfio};

use plugin::DeviceServer;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kata_device_plugin=info".parse().unwrap()),
        )
        .init();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("GIT_SHA"),
        "kata-device-plugin"
    );

    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("shutdown");
        sd.cancel();
    });

    // Declared by the node, not configured: one server per RESOURCES row,
    // started unconditionally.  Each ListAndWatch stream polls for VFIO
    // devices and pushes updates as the bound set changes (0..n), so
    // late-appearing devices (VFIO binding races the DP startup) are picked
    // up without restarting the pod.
    let servers: Vec<_> = vfio::RESOURCES
        .iter()
        .map(|res| {
            info!(resource = res.name, "starting plugin");
            DeviceServer::new(
                *res,
                vfio::VFIO_DIR,
                vfio::SYSFS_DIR,
                plugin::SOCKET_DIR,
                plugin::CDI_DIR,
            )
        })
        .collect();

    let results =
        futures::future::join_all(servers.into_iter().map(|s| s.run(shutdown.clone()))).await;
    for res in results {
        if let Err(e) = res {
            // {:#} keeps the error's cause chain; bare Display drops it.
            warn!("plugin error: {e:#}");
        }
    }
    Ok(())
}
