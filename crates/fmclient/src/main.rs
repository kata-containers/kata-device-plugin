// SPDX-License-Identifier: Apache-2.0
// Copyright (c) NVIDIA CORPORATION

mod ffi;
mod fm;
mod publish;
mod types;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use pcilibs_rs::fabric;

use fm::{FmAddress, FmConnection, FmLib};

#[derive(Clone, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
}

const VERSION: &str = "2.0.0";
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_TIMEOUT_MS: u32 = 1000;

#[derive(Parser)]
#[command(version = VERSION, about = "NVIDIA Fabric Manager client")]
struct Cli {
    /// List supported fabric partitions
    #[arg(short = 'l')]
    list: bool,

    /// Activate a fabric partition
    #[arg(short = 'a', value_name = "ID")]
    activate: Option<u32>,

    /// Deactivate a fabric partition
    #[arg(short = 'd', value_name = "ID")]
    deactivate: Option<u32>,

    /// Hostname or IP address (TCP) of Fabric Manager.
    ///
    /// Requires FM_CMD_BIND_INTERFACE=<IP> in Fabric Manager configuration.
    #[arg(short = 'i', long, value_name = "HOST", default_value = DEFAULT_HOST)]
    hostname: Option<String>,

    /// Unix domain socket path for Fabric Manager connection.
    ///
    /// Overrides --hostname when specified.
    /// Requires FM_CMD_UNIX_SOCKET_PATH=<path> in Fabric Manager configuration.
    #[arg(short = 's', long = "unix-domain-socket", value_name = "PATH")]
    unix_socket: Option<String>,

    /// vsock address (CID:PORT) of Fabric Manager inside a VM, for the host.
    ///
    /// Without a value, the Service VM's contract address: the deterministic
    /// CID kata runtime-rs gives it and FM's default port. Overrides
    /// --hostname and --unix-domain-socket. Needs FM listening on vsock
    /// (NVRC: nvrc.fm.vsock=on).
    #[arg(short = 'v', long, value_name = "CID:PORT", num_args = 0..=1)]
    vsock: Option<Option<String>>,

    /// Query all NVLink failed devices
    #[arg(short = 'g', long = "get-nvlink-failed-devices")]
    nvlink_failed: bool,

    /// List unsupported fabric partitions
    #[arg(short = 'u', long = "list-unsupported-partitions")]
    list_unsupported: bool,

    /// Activate a partition with VFs (BDF format: PARTITION_ID=DDDD:BB:DD.F,...).
    ///
    /// Host PCI addresses of SR-IOV Virtual Functions passed through to guest VMs.
    /// The order of VFs must correspond to the physical GPUs in the partition.
    /// Requires FABRIC_MODE=2 in Fabric Manager configuration.
    #[arg(long = "activate-with-vfs", value_name = "ID=BDFs")]
    activate_with_vfs: Option<String>,

    /// Set activated partition list (comma-separated, no spaces).
    ///
    /// Used for Fabric Manager resiliency restart mode.
    /// Requires FABRIC_MODE_RESTART=1 in Fabric Manager configuration.
    #[arg(short = 'p', long = "set-activated-list", value_name = "IDS")]
    set_activated: Option<String>,

    /// Publish the partition view to kata-device-plugin and exit.
    ///
    /// Every command that changes partitions publishes the result when it
    /// knows where the plugin is; this resends the current view. Endpoint
    /// and token come from KATA_FABRIC_VIEW_ENDPOINT and
    /// KATA_FABRIC_VIEW_TOKEN as the plugin injects them into the container
    /// it gives the fabric to; with --vsock the plugin's host socket is the
    /// default; --publish-to overrides both.
    #[arg(long)]
    publish: bool,

    /// Endpoint to publish to: http://HOST:PORT or unix:///PATH.
    #[arg(long = "publish-to", value_name = "ENDPOINT")]
    publish_to: Option<String>,

    /// How the GPUs of active partitions are to be consumed: "partition"
    /// (one unit per active partition) or "gpu" (one unit per GPU).
    #[arg(long, value_name = "UNIT", default_value = "partition")]
    unit: String,

    /// Output format [possible: table, json]
    #[arg(
        short = 'o',
        long,
        value_name = "FMT",
        default_value = "table",
        hide_possible_values = true
    )]
    output: OutputFormat,

    /// Timeout in seconds to wait for Fabric Manager to become available
    #[arg(short = 't', long, value_name = "SECS", default_value = "30")]
    timeout: u64,
}

fn parse_partition_ids(input: &str) -> Result<Vec<u32>, String> {
    input
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid partition id: {s}"))
        })
        .collect::<Result<BTreeSet<_>, _>>()
        .map(|set| set.into_iter().collect())
}

fn parse_bdf(bdf: &str) -> Result<ffi::FmPciDevice, String> {
    // DDDD:BB:DD.F
    let parts: Vec<&str> = bdf.split(':').collect();
    if parts.len() != 3 {
        return Err(format!("invalid BDF format: {bdf} (expected DDDD:BB:DD.F)"));
    }
    let domain =
        u32::from_str_radix(parts[0], 16).map_err(|_| format!("invalid domain in BDF: {bdf}"))?;
    let bus =
        u32::from_str_radix(parts[1], 16).map_err(|_| format!("invalid bus in BDF: {bdf}"))?;
    let dev_fn: Vec<&str> = parts[2].split('.').collect();
    if dev_fn.len() != 2 {
        return Err(format!("invalid device.function in BDF: {bdf}"));
    }
    let device =
        u32::from_str_radix(dev_fn[0], 16).map_err(|_| format!("invalid device in BDF: {bdf}"))?;
    let function = u32::from_str_radix(dev_fn[1], 16)
        .map_err(|_| format!("invalid function in BDF: {bdf}"))?;
    Ok(ffi::FmPciDevice {
        domain,
        bus,
        device,
        function,
    })
}

fn parse_activate_with_vfs(input: &str) -> Result<(u32, Vec<ffi::FmPciDevice>), String> {
    let (id_str, bdfs_str) = input
        .split_once('=')
        .ok_or_else(|| format!("expected PARTITION_ID=BDF,BDF,...  got: {input}"))?;
    let partition_id = id_str
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("invalid partition id: {id_str}"))?;
    let vfs = bdfs_str
        .split(',')
        .map(|s| parse_bdf(s.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((partition_id, vfs))
}

/// libnvfm logs to the process's stdout when it connects, whatever
/// __FMLIB_LOG_LEVEL says, which corrupts `-o json` the moment it is piped
/// or written to a file.  Give the library stderr as fd 1 and keep the
/// real stdout for this program's output.  Called before the library is
/// initialized and before any other thread exists.
fn claim_stdout() -> std::io::Result<File> {
    // SAFETY: fcntl and dup2 on the standard descriptors; the duplicate is
    // owned by the returned File and closed with it.  The floor of 3 keeps
    // the copy off the standard descriptors: with stderr closed, dup() would
    // return 2 and the dup2 below would point stdout back at the original
    // output, defeating the redirection; this way dup2 fails instead.
    let saved = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if saved < 0 || unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(saved) })
}

fn output<T: types::Render>(
    out: &mut File,
    value: &T,
    format: &OutputFormat,
) -> std::io::Result<()> {
    match format {
        OutputFormat::Json => writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(value).expect("serialization cannot fail")
        ),
        OutputFormat::Table => out.write_all(value.table().as_bytes()),
    }
}

fn run(cli: Cli, out: &mut File) -> Result<(), Box<dyn std::error::Error>> {
    let lib = FmLib::init()?;

    let service_vm;
    let address = if let Some(ref vsock) = cli.vsock {
        service_vm = vsock.clone().unwrap_or_else(|| {
            format!(
                "{}:{}",
                pcilibs_rs::fabric::SERVICE_VM_CID,
                pcilibs_rs::fabric::FM_VSOCK_PORT
            )
        });
        FmAddress::Vsock(&service_vm)
    } else if let Some(ref path) = cli.unix_socket {
        FmAddress::Unix(path)
    } else {
        FmAddress::Tcp(cli.hostname.as_deref().unwrap_or(DEFAULT_HOST))
    };

    // Checked before Fabric Manager is touched: a bad --unit must not
    // change the partitions and then fail to publish the result.
    let unit = fabric::Unit::from_view(&cli.unit)
        .ok_or_else(|| format!("--unit must be partition or gpu, got {:?}", cli.unit))?;

    let conn = FmConnection::connect(
        &lib,
        address,
        DEFAULT_TIMEOUT_MS,
        Duration::from_secs(cli.timeout),
    )?;

    let changed = cli.activate.is_some()
        || cli.deactivate.is_some()
        || cli.activate_with_vfs.is_some()
        || cli.set_activated.is_some();
    if cli.list {
        let list = conn.list_partitions()?;
        output(out, &list, &cli.output)?;
    } else if let Some(id) = cli.activate {
        conn.activate_partition(id)?;
    } else if let Some(id) = cli.deactivate {
        conn.deactivate_partition(id)?;
    } else if let Some(ref vf_str) = cli.activate_with_vfs {
        let (id, vfs) = parse_activate_with_vfs(vf_str)?;
        conn.activate_partition_with_vfs(id, &vfs)?;
    } else if cli.list_unsupported {
        let list = conn.list_unsupported_partitions()?;
        output(out, &list, &cli.output)?;
    } else if cli.nvlink_failed {
        let devices = conn.get_nvlink_failed_devices()?;
        output(out, &devices, &cli.output)?;
    } else if let Some(ref ids_str) = cli.set_activated {
        let ids = parse_partition_ids(ids_str)?;
        conn.set_activated_partitions(&ids)?;
    }

    // The plugin keeps the last view it was given: the command that changed
    // the partitions hands it the result when it knows where the plugin is,
    // and --publish alone resends it.
    match (cli.publish || changed, target(&cli)) {
        (true, Some(target)) => publish_view(&conn, &target, &unit)?,
        (true, None) if cli.publish => {
            return Err(format!("no endpoint: set {} or --publish-to", fabric::ENDPOINT_ENV).into())
        }
        _ => {}
    }

    Ok(())
}

/// Where the plugin is: the flag, the environment the plugin injected into
/// the fabric holder's container, or, for a client on the host that reaches
/// Fabric Manager over vsock, the plugin's host socket.
fn target(cli: &Cli) -> Option<publish::Target> {
    let endpoint = cli
        .publish_to
        .clone()
        .or_else(|| std::env::var(fabric::ENDPOINT_ENV).ok())
        .or_else(|| {
            cli.vsock
                .is_some()
                .then(|| format!("unix://{}", fabric::HOST_SOCKET))
        });
    publish::Target::from_vars(endpoint, std::env::var(fabric::TOKEN_ENV).ok())
}

/// A publish that fails after a partition changed exits non-zero: the
/// change is in Fabric Manager and the node does not know yet.
fn publish_view(
    conn: &FmConnection,
    target: &publish::Target,
    unit: &fabric::Unit,
) -> Result<(), Box<dyn std::error::Error>> {
    let view = conn.fabric_view(unit.as_str())?;
    let line = format!("published to {}: {view}", target.endpoint);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(publish::publish(target, view))
        .map_err(|e| format!("publish to {}: {e}", target.endpoint))?;
    eprintln!("fmclient: {line}");
    Ok(())
}

fn has_operation(cli: &Cli) -> bool {
    cli.list
        || cli.activate.is_some()
        || cli.deactivate.is_some()
        || cli.activate_with_vfs.is_some()
        || cli.nvlink_failed
        || cli.list_unsupported
        || cli.set_activated.is_some()
        || cli.publish
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if !has_operation(&cli) {
        Cli::parse_from(["fmclient", "--help"]);
    }

    let mut out = match claim_stdout() {
        Ok(out) => out,
        Err(e) => {
            eprintln!("error: cannot keep stdout for output: {e}");
            return ExitCode::FAILURE;
        }
    };
    match run(cli, &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
