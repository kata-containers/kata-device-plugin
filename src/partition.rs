//! Active Fabric Manager partitions as advertised devices.
//!
//! In shared NVSwitch mode GPUs that share an active partition reach each
//! other over NVLink, so a tenant must get exactly one partition, never a
//! subset.  A count under `nvidia.com/gpu` cannot express that: four
//! NVLinked GPUs read the same as four standalone ones, and the scheduler
//! cannot see how many partitions remain.  Each active partition is
//! therefore one device named by size, `nvidia.com/nvlink-4gpu`, holding
//! every function of every member, and a single-GPU partition is a plain
//! GPU.
//!
//! Fabric Manager names GPUs by module slot; the host resolves those through
//! the firmware's slot labels (pcilibs-rs, `fabric`).  A partition is
//! Healthy only when supported by Fabric Manager, exclusive and complete,
//! every member bound and labeled here.  Otherwise it is advertised
//! Unhealthy under the name of the size Fabric Manager gave it, so the
//! kubelet counts it in capacity and not in allocatable: the partition the
//! Service VM activated stays visible on the node while nothing can be
//! scheduled on it, and the plugin logs the reason.

use std::collections::BTreeMap;

use kata_fabric_api::v1::FabricView;
use kata_fabric_api::ViewUnit;
use pcilibs_rs::fabric;
use pcilibs_rs::platform::GpuFunction;
use pcilibs_rs::Sysfs;

use crate::vfio::{by_slot, Device, Entry, Health, DOMAIN};

/// The view's unit applied to the bound GPUs, `gpus` being every GPU row's
/// devices by name, then by PCI slot.  Unit `gpu` is the owner saying the
/// node is theirs whole, so GPUs stay one device each.  Unit `partition`
/// makes every active partition one device holding all its GPUs' functions,
/// named by size and identified by Fabric Manager's partition id.  Devices
/// are ordered by partition id, and a partition's functions by Fabric
/// Manager's member order.
///
/// A GPU without a parsable label resolves to nothing: an unknown board
/// advertises no partition rather than a wrong one.  A partition with no
/// member bound here is not advertised at all; there is no row to name it
/// by.
pub fn units(
    view: &FabricView,
    sysfs: &Sysfs,
    gpus: BTreeMap<String, BTreeMap<String, Device>>,
) -> BTreeMap<String, Vec<Entry>> {
    let Some(unit) = ViewUnit::from_view(&view.unit) else {
        tracing::warn!(
            unit = %view.unit,
            "partition view carries an unknown unit, advertising no GPU"
        );
        return BTreeMap::new();
    };
    if unit == ViewUnit::Gpu {
        return by_slot(gpus);
    }
    // Every bound GPU by PCI slot, with the name its row resolved to.
    let bound: BTreeMap<String, (String, Device)> = gpus
        .iter()
        .flat_map(|(name, devices)| {
            devices
                .iter()
                .map(move |(slot, device)| (slot.clone(), (name.clone(), device.clone())))
        })
        .collect();
    let functions: Vec<GpuFunction> = bound
        .values()
        .map(|(_, device)| GpuFunction {
            bdf: device[0].bdf.clone(),
            vendor: u32::from(device[0].vendor),
        })
        .collect();
    let modules = match fabric::gpu_modules(sysfs, &functions) {
        Ok(modules) => modules,
        Err(e) => {
            tracing::warn!("GPU slot labels are unreadable, advertising no GPU: {e}");
            return BTreeMap::new();
        }
    };
    let resolved = fabric::resolve(&partitions(view), &unsupported(view), &modules);
    let mut units: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    let mut reasons: Vec<String> = Vec::new();
    for partition in resolved.iter().filter(|p| p.active) {
        // The members this host has; `bdfs()` are functions passed in above,
        // so every one of them is bound.  A unit has all of them.
        let members: Vec<&(String, Device)> = partition
            .bdfs()
            .filter_map(|bdf| bound.get(pcilibs_rs::slot(bdf)))
            .collect();
        let Some((row_name, _)) = members.first() else {
            reasons.push(format!(
                "{}: no member bound here, not advertised",
                partition.id
            ));
            continue;
        };
        // Named by the size Fabric Manager gave the partition, so an
        // incomplete eight-GPU partition reads as the eight-GPU resource.
        let size = partition.members.len();
        let name = if size == 1 {
            row_name.clone()
        } else {
            format!("{DOMAIN}/nvlink-{size}gpu")
        };
        let health = if partition.advertisable() {
            Health::Healthy
        } else {
            let why = if !partition.supported {
                "unsupported"
            } else if !partition.exclusive {
                "shares a GPU with another active partition"
            } else {
                "not every GPU is bound and labeled here"
            };
            reasons.push(format!("{}: Unhealthy, {why}", partition.id));
            Health::Unhealthy
        };
        let device: Device = members
            .iter()
            .flat_map(|(_, device)| device.iter().cloned())
            .collect();
        units.entry(name).or_default().push(Entry {
            id: format!("p{}", partition.id),
            device,
            health,
        });
    }
    if !reasons.is_empty() {
        tracing::warn!(partitions = ?reasons, "active partitions that are not units");
    }
    units
}

/// Fabric Manager's lists as the resolver takes them: ids and physical IDs.
fn partitions(view: &FabricView) -> Vec<fabric::Partition> {
    view.partitions
        .iter()
        .flat_map(|list| list.partition_info.iter())
        .map(|partition| fabric::Partition {
            id: partition.partition_id,
            active: partition.is_active,
            physical_ids: partition
                .gpu_info
                .iter()
                .map(|gpu| gpu.physical_id)
                .collect(),
        })
        .collect()
}

fn unsupported(view: &FabricView) -> Vec<u32> {
    view.unsupported
        .iter()
        .flat_map(|list| list.partition_info.iter())
        .map(|partition| partition.partition_id)
        .collect()
}
